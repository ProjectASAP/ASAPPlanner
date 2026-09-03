//! Analytical, resource-dimensional cost estimates.
//!
//! This module deliberately keeps CPU work, retained state, and scan I/O
//! separate.  They become one planner objective only through an explicit
//! [`ResourceCalibration`]; without that calibration the dimensional
//! estimate is still useful for explanations, but is not silently comparable.

use std::collections::{HashMap, HashSet};

use asap_types::post_asap::{SketchAlgorithm, SketchParams};
use asap_types::workload::DataArrival;
use serde::{Deserialize, Serialize};

use crate::analytical_statistics::{
    validate_comparison_scopes, ComparisonScope, EdgeStatistics, OperatorStatistics,
    OperatorStatisticsProvider, PromqlBinaryOperandMode, SourceCoverage,
};

pub const ANALYTICAL_MODEL_VERSION: &str = "analytical-resource-at-rest-v1";

/// Conversion from physical dimensions to one deployment-specific objective.
/// Memory's coefficient means cost units per retained byte over this model's
/// explicit comparison scope; it is not mixed with a rate implicitly.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ResourceCalibration {
    pub cost_per_cpu_op: f64,
    pub cost_per_scan_byte: f64,
    pub cost_per_retained_byte: f64,
    pub version: String,
}

impl ResourceCalibration {
    pub fn validate(&self) -> Result<(), AnalyticalCostError> {
        for (name, value) in [
            ("cost_per_cpu_op", self.cost_per_cpu_op),
            ("cost_per_scan_byte", self.cost_per_scan_byte),
            ("cost_per_retained_byte", self.cost_per_retained_byte),
        ] {
            if !value.is_finite() || value < 0.0 {
                return Err(AnalyticalCostError::InvalidCalibration(name, value));
            }
        }
        if self.cost_per_cpu_op == 0.0
            && self.cost_per_scan_byte == 0.0
            && self.cost_per_retained_byte == 0.0
        {
            return Err(AnalyticalCostError::ZeroCalibration);
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct ResourceEstimate {
    pub cpu_ops: f64,
    pub peak_memory_bytes: u64,
    pub scan_bytes: u64,
}

/// Physical operator classes used to expose CPU, memory, and disk formulas
/// independently of a particular front-end IR spelling.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum PhysicalOperator {
    Scan,
    Filter {
        /// Scalar predicate operations evaluated for each input row.
        predicate_operations_per_row: u64,
    },
    Project {
        /// Scalar expression/copy operations evaluated for each input row.
        expression_operations_per_row: u64,
    },
    HashAggregate {
        grouping_key_count: u64,
        accumulator_count: u64,
    },
    InMemoryComparisonSort {
        ordering_key_count: u64,
        partitioned: bool,
    },
    /// Heap-based bounded ordering. The heap retains `limit + offset` rows
    /// while the operator returns at most `limit` rows after skipping offset.
    TopK {
        limit: u64,
        offset: u64,
        ordering_key_count: u64,
    },
    HashJoin {
        build_side: HashJoinBuildSide,
        equality_key_count: u64,
    },
    HashDeduplicate {
        key_count: u64,
    },
    Concat,
    /// SQL analytic window evaluated over ordered in-memory partitions. This
    /// is unrelated to streaming tumbling/sliding/EH window layouts.
    InMemoryAnalyticWindow {
        partition_key_count: u64,
        ordering_key_count: u64,
        function_operations_per_row: u64,
    },
    Limit {
        limit: u64,
        offset: u64,
    },
    PassThrough,
    PromqlRange,
    PromqlSubquery,
    PromqlVectorBinary,
    PromqlRelabel,
    PromqlInfoEnrich,
    PromqlSeriesSample,
    PromqlScalarToVector,
    PromqlVectorToScalar,
    PromqlScalarLeaf,
    PromqlPerSeries,
    PromqlPresence,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum HashJoinBuildSide {
    Left,
    Right,
}

/// One node in an already-selected physical DAG. Logical output cardinality,
/// transient edge buffering, and state retained across the horizon are
/// separate values.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PhysicalDagNode {
    pub id: String,
    pub operator: PhysicalOperator,
    pub children: Vec<String>,
    /// Exact comparison-scope coverage consumed by a scan. Non-scan nodes
    /// leave this empty. Reusing `SourceCoverage` prevents a physical plan
    /// from naming a source independently of its snapshot and predicates.
    pub source_coverage: Option<SourceCoverage>,
    /// Maximum transient edge buffer, distinct from logical `output_bytes`.
    pub output_buffer_bytes: u64,
    /// State that remains live after this node finishes (zero for ordinary
    /// streaming operators).
    pub retained_bytes: u64,
    pub execution: ExecutionMultiplicity,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ExecutionMultiplicity {
    Once,
    PerEvaluation,
}

/// Borrowed inputs for one physical-DAG estimate. This remains available
/// independently for diagnostics; plan selection should use
/// [`estimate_physical_dag_comparison`] so scope equality is mandatory.
pub struct PhysicalDagEstimateRequest<'a> {
    pub nodes: &'a [PhysicalDagNode],
    pub root: &'a str,
    pub scope: &'a ComparisonScope,
    pub statistics: &'a dyn OperatorStatisticsProvider,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct PhysicalDagComparisonEstimate {
    pub raw: ResourceEstimate,
    pub candidate: ResourceEstimate,
}

/// Estimate two plans only after proving that their source, snapshot,
/// predicate, event-time, recurrence, and horizon scopes are identical.
pub fn estimate_physical_dag_comparison(
    raw: PhysicalDagEstimateRequest<'_>,
    candidate: PhysicalDagEstimateRequest<'_>,
) -> Result<PhysicalDagComparisonEstimate, AnalyticalCostError> {
    validate_comparison_scopes(raw.scope, candidate.scope)?;
    Ok(PhysicalDagComparisonEstimate {
        raw: estimate_physical_dag(raw.nodes, raw.root, raw.scope, raw.statistics)?,
        candidate: estimate_physical_dag(
            candidate.nodes,
            candidate.root,
            candidate.scope,
            candidate.statistics,
        )?,
    })
}

/// Compose local operator estimates once per physical identity. CPU and disk
/// are additive; peak memory is simulated over a child-before-parent schedule
/// and releases transient child outputs after their last consumer.
pub fn estimate_physical_dag(
    nodes: &[PhysicalDagNode],
    root: &str,
    scope: &ComparisonScope,
    statistics: &(impl OperatorStatisticsProvider + ?Sized),
) -> Result<ResourceEstimate, AnalyticalCostError> {
    let evaluation_count = scope.validate()?;
    let by_id: HashMap<&str, &PhysicalDagNode> = nodes.iter().map(|n| (n.id.as_str(), n)).collect();
    if by_id.len() != nodes.len() {
        return Err(AnalyticalCostError::InvalidPhysicalDag("duplicate node id"));
    }
    let mut visiting = HashSet::new();
    let mut visited = HashSet::new();
    let mut order = Vec::new();
    fn visit<'a>(
        id: &'a str,
        nodes: &HashMap<&'a str, &'a PhysicalDagNode>,
        visiting: &mut HashSet<&'a str>,
        visited: &mut HashSet<&'a str>,
        order: &mut Vec<&'a str>,
    ) -> Result<(), AnalyticalCostError> {
        if visited.contains(id) {
            return Ok(());
        }
        if !visiting.insert(id) {
            return Err(AnalyticalCostError::InvalidPhysicalDag("cycle"));
        }
        let node = nodes
            .get(id)
            .ok_or(AnalyticalCostError::InvalidPhysicalDag("missing node"))?;
        for child in &node.children {
            visit(child, nodes, visiting, visited, order)?;
        }
        visiting.remove(id);
        visited.insert(id);
        order.push(id);
        Ok(())
    }
    visit(root, &by_id, &mut visiting, &mut visited, &mut order)?;

    // Resolve each reachable node exactly once. A provider may be backed by a
    // live catalog; one estimate must not mix observations from two refreshes.
    let resolved_statistics: HashMap<&str, OperatorStatistics> = order
        .iter()
        .map(|id| statistics.statistics(id).map(|value| (*id, value)))
        .collect::<Result<_, _>>()?;

    let mut consumed_sources = Vec::new();
    for id in &order {
        let node = by_id[id];
        let node_statistics = &resolved_statistics[id];
        match node.operator {
            PhysicalOperator::Scan => {
                let coverage = node.source_coverage.as_ref().ok_or_else(|| {
                    AnalyticalCostError::MissingScanSourceCoverage(node.id.clone())
                })?;
                if !scope.sources.contains(coverage) {
                    return Err(AnalyticalCostError::ScanOutsideComparisonScope(
                        node.id.clone(),
                    ));
                }
                if !consumed_sources.contains(&coverage) {
                    consumed_sources.push(coverage);
                }
            }
            _ if node.source_coverage.is_some() => {
                return Err(AnalyticalCostError::InvalidPhysicalDag(
                    "only scan nodes may declare source coverage",
                ));
            }
            _ => {}
        }
        validate_operator_statistics(node, node_statistics, &by_id, &resolved_statistics)?;
        if node.retained_bytes > 0 && matches!(node.execution, ExecutionMultiplicity::PerEvaluation)
        {
            return Err(AnalyticalCostError::InvalidPhysicalDag(
                "per-evaluation node cannot retain state across the horizon",
            ));
        }
        for child in &node.children {
            let child = by_id[child.as_str()];
            if matches!(node.execution, ExecutionMultiplicity::Once)
                && matches!(child.execution, ExecutionMultiplicity::PerEvaluation)
            {
                return Err(AnalyticalCostError::InvalidPhysicalDag(
                    "build-once node cannot consume a per-evaluation child",
                ));
            }
            if matches!(node.execution, ExecutionMultiplicity::PerEvaluation)
                && matches!(child.execution, ExecutionMultiplicity::Once)
                && child.retained_bytes == 0
            {
                return Err(AnalyticalCostError::InvalidPhysicalDag(
                    "per-evaluation node reads a non-retained build-once child",
                ));
            }
        }
    }
    if scope
        .sources
        .iter()
        .any(|expected| !consumed_sources.contains(&expected))
    {
        return Err(AnalyticalCostError::InvalidPhysicalDag(
            "physical scans omit a comparison-scope source",
        ));
    }

    let mut remaining_consumers: HashMap<&str, usize> = HashMap::new();
    for id in &order {
        for child in &by_id[id].children {
            *remaining_consumers.entry(child).or_default() += 1;
        }
    }
    let mut cpu_ops = 0.0;
    let mut scan_bytes = 0_u64;
    let mut live_bytes = 0_u64;
    let mut peak_memory_bytes = 0_u64;
    let mut live_outputs: HashMap<&str, u64> = HashMap::new();
    for id in order {
        let node = by_id[id];
        let local = estimate_operator(node.operator, resolved_statistics[id].clone())?;
        let executions = match node.execution {
            ExecutionMultiplicity::Once => 1,
            ExecutionMultiplicity::PerEvaluation => evaluation_count,
        };
        cpu_ops += local.cpu_ops * executions as f64;
        scan_bytes = scan_bytes
            .checked_add(
                local
                    .scan_bytes
                    .checked_mul(executions)
                    .ok_or(AnalyticalCostError::Overflow)?,
            )
            .ok_or(AnalyticalCostError::Overflow)?;
        peak_memory_bytes = peak_memory_bytes.max(
            live_bytes
                .checked_add(local.peak_memory_bytes)
                .and_then(|bytes| bytes.checked_add(node.output_buffer_bytes))
                .ok_or(AnalyticalCostError::Overflow)?,
        );
        if node.retained_bytes > 0 {
            live_bytes = live_bytes
                .checked_add(node.retained_bytes)
                .ok_or(AnalyticalCostError::Overflow)?;
            peak_memory_bytes = peak_memory_bytes.max(live_bytes);
        }
        let output = node.output_buffer_bytes;
        if remaining_consumers.get(id).copied().unwrap_or(0) > 0 || id == root {
            live_bytes = live_bytes
                .checked_add(output)
                .ok_or(AnalyticalCostError::Overflow)?;
            live_outputs.insert(id, output);
            peak_memory_bytes = peak_memory_bytes.max(live_bytes);
        }
        for child in &node.children {
            let remaining = remaining_consumers.get_mut(child.as_str()).ok_or(
                AnalyticalCostError::InvalidPhysicalDag("invalid consumer count"),
            )?;
            *remaining -= 1;
            if *remaining == 0 {
                if let Some(bytes) = live_outputs.remove(child.as_str()) {
                    live_bytes = live_bytes
                        .checked_sub(bytes)
                        .ok_or(AnalyticalCostError::Overflow)?;
                }
            }
        }
    }
    if !cpu_ops.is_finite() {
        return Err(AnalyticalCostError::Overflow);
    }
    Ok(ResourceEstimate {
        cpu_ops,
        peak_memory_bytes,
        scan_bytes,
    })
}

fn validate_operator_statistics(
    node: &PhysicalDagNode,
    node_statistics: &OperatorStatistics,
    nodes: &HashMap<&str, &PhysicalDagNode>,
    statistics: &HashMap<&str, OperatorStatistics>,
) -> Result<(), AnalyticalCostError> {
    let arity = expected_input_arity(node.operator, node.children.len());
    if node_statistics.input_count() != arity.statistics_inputs {
        return Err(AnalyticalCostError::InvalidOperatorStatistics {
            node: node.id.clone(),
            reason: "operator-statistics input count does not match physical arity",
        });
    }
    if node.children.len() != arity.dag_children {
        return Err(AnalyticalCostError::InvalidPhysicalDag(
            "operator child count does not match physical arity",
        ));
    }
    if !statistics_match_operator(node.operator, node_statistics) {
        return Err(AnalyticalCostError::InvalidOperatorStatistics {
            node: node.id.clone(),
            reason: "statistics variant does not match physical operator",
        });
    }
    for edge in (0..node_statistics.input_count())
        .filter_map(|index| node_statistics.input(index))
        .chain(std::iter::once(node_statistics.output()))
    {
        if !edge.is_consistent() {
            return Err(AnalyticalCostError::InvalidOperatorStatistics {
                node: node.id.clone(),
                reason: "edge rows and logical bytes are inconsistent",
            });
        }
    }
    for (input_index, child_id) in node.children.iter().enumerate() {
        let child = nodes
            .get(child_id.as_str())
            .ok_or(AnalyticalCostError::InvalidPhysicalDag("missing node"))?;
        let child_statistics = &statistics[child.id.as_str()];
        if node_statistics.input(input_index) != Some(child_statistics.output()) {
            return Err(AnalyticalCostError::ConflictingEdgeStatistics {
                parent: node.id.clone(),
                child: child.id.clone(),
                input_index,
            });
        }
    }
    validate_operator_semantics(node.operator, node_statistics)?;
    if let Some(promql) = node_statistics.promql.as_ref() {
        if promql.input_series.len() != expected_inputs {
            return Err(AnalyticalCostError::InvalidOperatorStatistics {
                node: node.id.clone(),
                reason: "PromQL input-series arity does not match physical inputs",
            });
        }
        for (index, child_id) in node.children.iter().enumerate() {
            let child = &statistics[child_id.as_str()];
            let child_promql = child.promql.as_ref().ok_or_else(|| {
                AnalyticalCostError::InvalidOperatorStatistics {
                    node: child_id.clone(),
                    reason: "PromQL child is missing series statistics",
                }
            })?;
            if promql.input_series[index] != child_promql.output_series
                || (!matches!(node.operator, PhysicalOperator::PromqlSubquery)
                    && promql.evaluation_steps != child_promql.evaluation_steps)
            {
                return Err(AnalyticalCostError::InvalidOperatorStatistics {
                    node: node.id.clone(),
                    reason: "PromQL edge series or evaluation steps conflict",
                });
            }
        }
        if matches!(node.operator, PhysicalOperator::PromqlSubquery) {
            let subquery_steps = promql
                .subquery_steps
                .filter(|steps| *steps > 0)
                .ok_or(AnalyticalCostError::MissingOrZero("subquery_steps"))?;
            let child = &statistics[node.children[0].as_str()];
            let child_steps = child
                .promql
                .as_ref()
                .ok_or(AnalyticalCostError::MissingOrStale(
                    "child_promql_operator_statistics",
                ))?
                .evaluation_steps;
            let expected = promql
                .evaluation_steps
                .checked_mul(subquery_steps)
                .ok_or(AnalyticalCostError::Overflow)?;
            if child_steps != expected {
                return Err(AnalyticalCostError::InvalidOperatorStatistics {
                    node: node.id.clone(),
                    reason: "subquery child steps do not equal parent steps times subquery steps",
                });
            }
        }
    }
    Ok(())
}

pub(crate) fn validate_operator_semantics(
    operator: PhysicalOperator,
    statistics: &OperatorStatistics,
) -> Result<(), AnalyticalCostError> {
    let invalid = |reason| Err(AnalyticalCostError::InconsistentOperatorStatistics(reason));
    if !matches!(operator, PhysicalOperator::Scan) && statistics.source_scan_bytes != 0 {
        return invalid("only Scan may charge source bytes");
    }
    if matches!(operator, PhysicalOperator::PromqlScalarLeaf) {
        let promql = require_promql_statistics(statistics, 0)?;
        if promql.output_series != 0 || statistics.output.rows != promql.evaluation_steps {
            return invalid("PromQL scalar leaf must emit one scalar row per evaluation step");
        }
        return Ok(());
    }
    let input = statistics.inputs.first().copied().ok_or(
        AnalyticalCostError::InconsistentOperatorStatistics("operator input is missing"),
    )?;
    let output = statistics.output;
    match operator {
        PhysicalOperator::Scan => {
            if input != output {
                return invalid("Scan external input edge does not match its output edge");
            }
        }
        PhysicalOperator::Filter => {
            if output.rows > input.rows || output.bytes > input.bytes {
                return invalid("filter output expands its input");
            }
        }
        PhysicalOperator::Project => {
            if output.rows != input.rows {
                return invalid("projection changes row cardinality");
            }
        }
        PhysicalOperator::HashAggregate => {
            let groups = statistics
                .group_count
                .ok_or(AnalyticalCostError::MissingOrZero("group_count"))?;
            if groups == 0 && (input.rows != 0 || output.rows != 0) {
                return invalid("zero groups require an empty grouped input and output");
            }
            if output.rows != groups {
                return invalid("aggregate output does not equal group cardinality");
            }
        }
        PhysicalOperator::Deduplicate => {
            let groups = statistics
                .group_count
                .ok_or(AnalyticalCostError::MissingOrZero("group_count"))?;
            if output.rows != groups || output.rows > input.rows {
                return invalid("deduplicate output does not equal distinct cardinality");
            }
        }
        PhysicalOperator::Sort => {
            if output != input {
                return invalid("sort changes its input cardinality or width");
            }
        }
        PhysicalOperator::TopK => {
            let k = statistics
                .k
                .filter(|k| *k > 0)
                .ok_or(AnalyticalCostError::MissingOrZero("k"))?;
            let offset = statistics
                .topk_output_offset
                .ok_or(AnalyticalCostError::MissingOrZero("topk_output_offset"))?;
            if offset > k || output.rows != input.rows.min(k).saturating_sub(offset) {
                return invalid("top-k output does not equal its cardinality bound");
            }
        }
        PhysicalOperator::Limit => {
            if output.rows > input.rows {
                return invalid("bounded output exceeds its input cardinality");
            }
        }
        PhysicalOperator::Window => {
            if output.rows != input.rows {
                return invalid("SQL window changes row cardinality");
            }
        }
        PhysicalOperator::PassThrough => {
            if output != input {
                return invalid("pass-through wrapper changes its edge statistics");
            }
        }
        PhysicalOperator::Concat => {
            let totals = statistics.inputs.iter().try_fold(
                EdgeStatistics { rows: 0, bytes: 0 },
                |total, edge| {
                    Ok::<_, AnalyticalCostError>(EdgeStatistics {
                        rows: total
                            .rows
                            .checked_add(edge.rows)
                            .ok_or(AnalyticalCostError::Overflow)?,
                        bytes: total
                            .bytes
                            .checked_add(edge.bytes)
                            .ok_or(AnalyticalCostError::Overflow)?,
                    })
                },
            )?;
            if output != totals {
                return invalid("concat output does not equal the sum of its inputs");
            }
        }
        PhysicalOperator::PromqlRange => {
            let promql = require_promql_statistics(statistics, 1)?;
            if !matches!(promql.window_samples_per_series, Some(value) if value > 0) {
                return Err(AnalyticalCostError::MissingOrZero(
                    "window_samples_per_series",
                ));
            }
        }
        PhysicalOperator::PromqlSubquery => {
            require_promql_statistics(statistics, 1)?;
        }
        PhysicalOperator::PromqlScalarToVector => {
            validate_promql_bridge(operator, statistics)?;
        }
        PhysicalOperator::PromqlVectorToScalar => {
            validate_promql_bridge(operator, statistics)?;
        }
        PhysicalOperator::PromqlVectorBinary => {
            validate_promql_binary(statistics)?;
        }
        PhysicalOperator::PromqlInfoEnrich => {
            let promql = require_promql_statistics(statistics, 2)?;
            if output.rows != input.rows || promql.output_series != promql.input_series[0] {
                return invalid("info enrichment changes left sample or series cardinality");
            }
            if statistics.hash_join_build_side != Some(HashJoinBuildSide::Right) {
                return invalid("info enrichment must build its label index from the right side");
            }
        }
        PhysicalOperator::PromqlRelabel => {
            let promql = require_promql_statistics(statistics, 1)?;
            if output.rows != input.rows || promql.output_series != promql.input_series[0] {
                return invalid("relabel changes row or series cardinality");
            }
        }
        PhysicalOperator::PromqlSeriesSample => {
            let promql = require_promql_statistics(statistics, 1)?;
            if output.rows > input.rows || promql.output_series > promql.input_series[0] {
                return invalid("series sampling expands its input");
            }
        }
        PhysicalOperator::PromqlPerSeries => {
            let promql = require_promql_statistics(statistics, 1)?;
            if promql.output_series > promql.input_series[0] {
                return invalid("per-series operator expands series cardinality");
            }
        }
        PhysicalOperator::PromqlPresence => {
            let promql = require_promql_statistics(statistics, 1)?;
            if promql.output_series > 1 {
                return invalid("PromQL absence operator emits more than one series");
            }
        }
        PhysicalOperator::HashJoin => {}
        PhysicalOperator::PromqlScalarLeaf => unreachable!(),
    }
    Ok(())
}

fn statistics_match_operator(operator: PhysicalOperator, statistics: &OperatorStatistics) -> bool {
    match operator {
        PhysicalOperator::Scan => matches!(statistics, OperatorStatistics::Scan { .. }),
        PhysicalOperator::Filter { .. } => matches!(statistics, OperatorStatistics::Filter { .. }),
        PhysicalOperator::Project { .. } => {
            matches!(statistics, OperatorStatistics::Project { .. })
        }
        PhysicalOperator::HashAggregate { .. } => {
            matches!(statistics, OperatorStatistics::HashAggregate { .. })
        }
        PhysicalOperator::InMemoryComparisonSort { .. } => {
            matches!(
                statistics,
                OperatorStatistics::InMemoryComparisonSort { .. }
            )
        }
        PhysicalOperator::TopK { .. } => matches!(statistics, OperatorStatistics::TopK { .. }),
        PhysicalOperator::HashJoin { .. } => {
            matches!(statistics, OperatorStatistics::HashJoin { .. })
        }
        PhysicalOperator::HashDeduplicate { .. } => {
            matches!(statistics, OperatorStatistics::HashDeduplicate { .. })
        }
        PhysicalOperator::Concat => matches!(statistics, OperatorStatistics::Concat { .. }),
        PhysicalOperator::InMemoryAnalyticWindow { .. } => {
            matches!(
                statistics,
                OperatorStatistics::InMemoryAnalyticWindow { .. }
            )
        }
        PhysicalOperator::Limit { .. } => matches!(statistics, OperatorStatistics::Limit { .. }),
        PhysicalOperator::PassThrough => {
            matches!(statistics, OperatorStatistics::PassThrough { .. })
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct OperatorInputArity {
    /// Number of logical input-edge records required in `OperatorStatistics`.
    statistics_inputs: usize,
    /// Number of upstream physical nodes required in the DAG.
    dag_children: usize,
}

/// Declare both notions of operator input explicitly. A scan has one external
/// source-input statistics record but no upstream DAG node. Every other
/// operator's statistics inputs correspond one-to-one with its DAG children.
///
/// Keep this match exhaustive: adding a physical operator must also define its
/// statistics and DAG arity instead of silently inheriting unary behavior.
fn expected_input_arity(
    operator: PhysicalOperator,
    variadic_child_count: usize,
) -> OperatorInputArity {
    let unary = OperatorInputArity {
        statistics_inputs: 1,
        dag_children: 1,
    };
    match operator {
        PhysicalOperator::Scan => OperatorInputArity {
            statistics_inputs: 1,
            dag_children: 0,
        },
        PhysicalOperator::Filter { .. }
        | PhysicalOperator::Project { .. }
        | PhysicalOperator::HashAggregate { .. }
        | PhysicalOperator::InMemoryComparisonSort { .. }
        | PhysicalOperator::TopK { .. }
        | PhysicalOperator::HashDeduplicate { .. }
        | PhysicalOperator::InMemoryAnalyticWindow { .. }
        | PhysicalOperator::Limit { .. }
        | PhysicalOperator::PassThrough => unary,
        PhysicalOperator::HashJoin { .. } => OperatorInputArity {
            statistics_inputs: 2,
            dag_children: 2,
        },
        PhysicalOperator::Concat => OperatorInputArity {
            statistics_inputs: variadic_child_count,
            dag_children: variadic_child_count,
        },
    }
}

fn checked_cpu_product(rows: u64, operations_per_row: u64) -> Result<f64, AnalyticalCostError> {
    Ok(rows
        .checked_mul(operations_per_row)
        .ok_or(AnalyticalCostError::Overflow)? as f64)
}

fn partitioned_order_estimate(
    partitioning: &crate::analytical_statistics::PartitionStatistics,
    comparison_operations: u64,
    row_operations: u64,
) -> Result<ResourceEstimate, AnalyticalCostError> {
    let mut cpu_ops = 0.0;
    let mut peak_memory_bytes = 0;
    for partition in &partitioning.partitions {
        let comparisons = partition.rows as f64
            * (partition.rows.max(2) as f64).log2().ceil()
            * comparison_operations as f64;
        cpu_ops += comparisons + checked_cpu_product(partition.rows, row_operations)?;
        peak_memory_bytes = peak_memory_bytes.max(partition.bytes);
    }
    if !cpu_ops.is_finite() {
        return Err(AnalyticalCostError::Overflow);
    }
    Ok(ResourceEstimate {
        cpu_ops,
        peak_memory_bytes,
        scan_bytes: 0,
    })
}

fn validate_partitioning(
    input: EdgeStatistics,
    partitioning: &crate::analytical_statistics::PartitionStatistics,
    partitioned: bool,
) -> Result<(), AnalyticalCostError> {
    let inconsistent = |reason| Err(AnalyticalCostError::InconsistentOperatorStatistics(reason));
    if input.rows == 0 {
        return if partitioning.partitions.is_empty() {
            Ok(())
        } else {
            inconsistent("empty input has non-empty partition evidence")
        };
    }
    if partitioning.partitions.is_empty() {
        return inconsistent("non-empty ordered input has no partition evidence");
    }
    if !partitioned && partitioning.partitions.len() != 1 {
        return inconsistent("global ordering must have exactly one partition");
    }
    let total = partitioning.partitions.iter().try_fold(
        EdgeStatistics { rows: 0, bytes: 0 },
        |total, partition| {
            if !partition.is_consistent() || partition.rows == 0 {
                return Err(AnalyticalCostError::InconsistentOperatorStatistics(
                    "ordered partition evidence is invalid",
                ));
            }
            Ok(EdgeStatistics {
                rows: total
                    .rows
                    .checked_add(partition.rows)
                    .ok_or(AnalyticalCostError::Overflow)?,
                bytes: total
                    .bytes
                    .checked_add(partition.bytes)
                    .ok_or(AnalyticalCostError::Overflow)?,
            })
        },
    )?;
    if total != input {
        return inconsistent("ordered partitions do not sum to the input edge");
    }
    Ok(())
}

/// Estimate one physical operator. Child costs are deliberately excluded;
/// a DAG walker sums CPU/disk once per node and combines simultaneously
/// retained state separately.
pub fn estimate_operator(
    operator: PhysicalOperator,
    statistics: OperatorStatistics,
) -> Result<ResourceEstimate, AnalyticalCostError> {
    validate_operator_semantics(operator, &statistics)?;
    let per_row_width = |rows: u64, bytes: u64| -> Result<u64, AnalyticalCostError> {
        match (rows, bytes) {
            (0, 0) => return Ok(0),
            (0, _) | (_, 0) => {
                return Err(AnalyticalCostError::MissingOrZero("operator rows/bytes"));
            }
            _ => {}
        }
        Ok(bytes.div_ceil(rows))
    };
    let input = |index: usize| {
        statistics
            .input(index)
            .ok_or(AnalyticalCostError::MissingOrZero("operator input edge"))
    };
    let left = input(0)?;
    let output = statistics.output();
    let estimate = match (operator, &statistics) {
        (
            PhysicalOperator::Scan,
            OperatorStatistics::Scan {
                source_read_bytes, ..
            },
        ) => ResourceEstimate {
            cpu_ops: left.rows as f64,
            peak_memory_bytes: per_row_width(left.rows, left.bytes)?,
            scan_bytes: *source_read_bytes,
        },
        (
            PhysicalOperator::Filter {
                predicate_operations_per_row,
            },
            _,
        ) => ResourceEstimate {
            cpu_ops: checked_cpu_product(left.rows, predicate_operations_per_row)?,
            peak_memory_bytes: per_row_width(output.rows, output.bytes)?,
            scan_bytes: 0,
        },
        (
            PhysicalOperator::Project {
                expression_operations_per_row,
            },
            _,
        ) => ResourceEstimate {
            cpu_ops: checked_cpu_product(left.rows, expression_operations_per_row)?,
            peak_memory_bytes: per_row_width(output.rows, output.bytes)?,
            scan_bytes: 0,
        },
        (PhysicalOperator::PassThrough, _) => ResourceEstimate {
            cpu_ops: left.rows as f64,
            peak_memory_bytes: per_row_width(output.rows, output.bytes)?,
            scan_bytes: 0,
        },
        (
            PhysicalOperator::HashAggregate {
                grouping_key_count,
                accumulator_count,
            },
            OperatorStatistics::HashAggregate {
                group_count,
                key_bytes,
                accumulator_bytes_per_group,
                ..
            },
        ) => ResourceEstimate {
            cpu_ops: checked_cpu_product(
                left.rows,
                grouping_key_count
                    .checked_add(accumulator_count)
                    .ok_or(AnalyticalCostError::Overflow)?,
            )?,
            peak_memory_bytes: checked_bytes(&[
                *group_count,
                key_bytes
                    .checked_add(*accumulator_bytes_per_group)
                    .and_then(|bytes| bytes.checked_add(16))
                    .ok_or(AnalyticalCostError::Overflow)?,
            ])?,
            scan_bytes: 0,
        },
        (
            PhysicalOperator::HashDeduplicate { key_count },
            OperatorStatistics::HashDeduplicate {
                distinct_key_count,
                key_bytes,
                ..
            },
        ) => ResourceEstimate {
            cpu_ops: checked_cpu_product(left.rows, key_count)?,
            peak_memory_bytes: checked_bytes(&[
                *distinct_key_count,
                key_bytes
                    .checked_add(16)
                    .ok_or(AnalyticalCostError::Overflow)?,
            ])?,
            scan_bytes: 0,
        },
        (
            PhysicalOperator::InMemoryComparisonSort {
                ordering_key_count, ..
            },
            OperatorStatistics::InMemoryComparisonSort {
                input_partitioning, ..
            },
        ) => partitioned_order_estimate(input_partitioning, ordering_key_count, 0)?,
        (
            PhysicalOperator::InMemoryAnalyticWindow {
                partition_key_count,
                ordering_key_count,
                function_operations_per_row,
            },
            OperatorStatistics::InMemoryAnalyticWindow {
                input_partitioning, ..
            },
        ) => partitioned_order_estimate(
            input_partitioning,
            partition_key_count
                .checked_add(ordering_key_count)
                .ok_or(AnalyticalCostError::Overflow)?,
            function_operations_per_row,
        )?,
        (
            PhysicalOperator::TopK {
                limit,
                offset,
                ordering_key_count,
            },
            _,
        ) => {
            let heap_capacity = limit
                .checked_add(offset)
                .ok_or(AnalyticalCostError::Overflow)?;
            let heap_rows = heap_capacity.min(left.rows);
            ResourceEstimate {
                cpu_ops: left.rows as f64
                    * (heap_rows.max(2) as f64).log2().ceil()
                    * ordering_key_count as f64,
                peak_memory_bytes: checked_bytes(&[
                    heap_rows,
                    per_row_width(left.rows, left.bytes)?,
                ])?,
                scan_bytes: 0,
            }
        }
        (
            PhysicalOperator::HashJoin {
                build_side,
                equality_key_count,
            },
            _,
        ) => {
            let right = input(1)?;
            ResourceEstimate {
                cpu_ops: checked_cpu_product(
                    left.rows
                        .checked_add(right.rows)
                        .ok_or(AnalyticalCostError::Overflow)?,
                    equality_key_count,
                )? + output.rows as f64,
                peak_memory_bytes: match build_side {
                    HashJoinBuildSide::Left => left
                        .rows
                        .checked_mul(16)
                        .and_then(|metadata| left.bytes.checked_add(metadata)),
                    HashJoinBuildSide::Right => right
                        .rows
                        .checked_mul(16)
                        .and_then(|metadata| right.bytes.checked_add(metadata)),
                }
                .ok_or(AnalyticalCostError::Overflow)?,
                scan_bytes: 0,
            }
        }
        (PhysicalOperator::Concat, _) => ResourceEstimate {
            cpu_ops: output.rows as f64,
            peak_memory_bytes: per_row_width(output.rows, output.bytes)?,
            scan_bytes: 0,
        },
        (PhysicalOperator::Limit { limit, offset }, _) => {
            let consumed = if limit == 0 {
                0
            } else {
                left.rows.min(
                    offset
                        .checked_add(limit)
                        .ok_or(AnalyticalCostError::Overflow)?,
                )
            };
            ResourceEstimate {
                cpu_ops: consumed as f64,
                peak_memory_bytes: per_row_width(output.rows, output.bytes)?,
                scan_bytes: 0,
            }
        }
        _ => {
            return Err(AnalyticalCostError::InconsistentOperatorStatistics(
                "statistics variant does not match physical operator",
            ));
        }
    };
    if estimate.cpu_ops.is_finite() {
        Ok(estimate)
    } else {
        Err(AnalyticalCostError::Overflow)
    }
}

pub(crate) fn validate_operator_semantics(
    operator: PhysicalOperator,
    statistics: &OperatorStatistics,
) -> Result<(), AnalyticalCostError> {
    let inconsistent = |reason| Err(AnalyticalCostError::InconsistentOperatorStatistics(reason));
    if !statistics_match_operator(operator, statistics) {
        return inconsistent("statistics variant does not match physical operator");
    }
    if matches!(
        (operator, statistics),
        (
            PhysicalOperator::Concat,
            OperatorStatistics::Concat { inputs, .. }
        ) if inputs.is_empty()
    ) {
        return inconsistent("Concat must have at least one input");
    }
    let input = statistics
        .input(0)
        .ok_or(AnalyticalCostError::InconsistentOperatorStatistics(
            "operator input is missing",
        ))?;
    let output = statistics.output();
    match (operator, statistics) {
        (
            PhysicalOperator::Scan,
            OperatorStatistics::Scan {
                source_read_bytes, ..
            },
        ) => {
            if input != output {
                return inconsistent("Scan input and output edges differ");
            }
            if input.rows > 0 && *source_read_bytes == 0 {
                return inconsistent("non-empty Scan has zero source-read bytes");
            }
        }
        (
            PhysicalOperator::Filter {
                predicate_operations_per_row,
            },
            _,
        ) => {
            if predicate_operations_per_row == 0 {
                return inconsistent("Filter has no predicate work");
            }
            if output.rows > input.rows || output.bytes > input.bytes {
                return inconsistent("Filter output expands its input");
            }
        }
        (
            PhysicalOperator::Project {
                expression_operations_per_row,
            },
            _,
        ) => {
            if expression_operations_per_row == 0 {
                return inconsistent("Project has no expression work");
            }
            if output.rows != input.rows {
                return inconsistent("Project changes row cardinality");
            }
        }
        (
            PhysicalOperator::HashAggregate {
                grouping_key_count,
                accumulator_count,
            },
            OperatorStatistics::HashAggregate {
                group_count,
                key_bytes,
                accumulator_bytes_per_group,
                ..
            },
        ) => {
            if accumulator_count == 0 || *accumulator_bytes_per_group == 0 {
                return inconsistent("HashAggregate accumulator work and width must be positive");
            }
            if grouping_key_count == 0 {
                if *key_bytes != 0 || *group_count != 1 {
                    return inconsistent(
                        "ungrouped HashAggregate must have one zero-key-width group",
                    );
                }
            } else if *key_bytes == 0 || *group_count > input.rows {
                return inconsistent("grouped HashAggregate has invalid group evidence");
            }
            if output.rows != *group_count {
                return inconsistent("grouped output differs from distinct group cardinality");
            }
        }
        (
            PhysicalOperator::HashDeduplicate { key_count },
            OperatorStatistics::HashDeduplicate {
                distinct_key_count,
                key_bytes,
                ..
            },
        ) => {
            if key_count == 0 || *key_bytes == 0 {
                return inconsistent("Deduplicate key count and width must be positive");
            }
            if *distinct_key_count > input.rows || output.rows != *distinct_key_count {
                return inconsistent("deduplicated output differs from distinct key cardinality");
            }
        }
        (
            PhysicalOperator::InMemoryComparisonSort {
                ordering_key_count,
                partitioned,
            },
            OperatorStatistics::InMemoryComparisonSort {
                input_partitioning, ..
            },
        ) => {
            if ordering_key_count == 0 {
                return inconsistent("Sort has no ordering keys");
            }
            if input != output {
                return inconsistent("cardinality-preserving operator changes its edge");
            }
            validate_partitioning(input, input_partitioning, partitioned)?;
        }
        (PhysicalOperator::PassThrough, _) => {
            if input != output {
                return inconsistent("cardinality-preserving operator changes its edge");
            }
        }
        (
            PhysicalOperator::InMemoryAnalyticWindow {
                partition_key_count,
                ordering_key_count,
                function_operations_per_row,
            },
            OperatorStatistics::InMemoryAnalyticWindow {
                input_partitioning, ..
            },
        ) => {
            if ordering_key_count == 0 || function_operations_per_row == 0 {
                return inconsistent("analytic Window work must be positive");
            }
            if input.rows != output.rows {
                return inconsistent("Window changes row cardinality");
            }
            validate_partitioning(input, input_partitioning, partition_key_count > 0)?;
        }
        (PhysicalOperator::Concat, OperatorStatistics::Concat { inputs, .. }) => {
            if inputs.is_empty() {
                return inconsistent("Concat must have at least one input");
            }
            let total =
                inputs
                    .iter()
                    .try_fold(EdgeStatistics { rows: 0, bytes: 0 }, |total, edge| {
                        Ok::<_, AnalyticalCostError>(EdgeStatistics {
                            rows: total
                                .rows
                                .checked_add(edge.rows)
                                .ok_or(AnalyticalCostError::Overflow)?,
                            bytes: total
                                .bytes
                                .checked_add(edge.bytes)
                                .ok_or(AnalyticalCostError::Overflow)?,
                        })
                    })?;
            if output != total {
                return inconsistent("Concat output differs from the sum of its inputs");
            }
        }
        (
            PhysicalOperator::TopK {
                limit,
                offset,
                ordering_key_count,
            },
            _,
        ) => {
            if limit == 0 || ordering_key_count == 0 {
                return inconsistent("Top-K limit and ordering-key count must be positive");
            }
            let expected = input.rows.saturating_sub(offset).min(limit);
            if output.rows != expected {
                return inconsistent("Top-K output differs from its cardinality bound");
            }
        }
        (PhysicalOperator::Limit { limit, offset }, _) => {
            let expected = input.rows.saturating_sub(offset).min(limit);
            if output.rows != expected {
                return inconsistent("Limit output differs from limit and offset");
            }
        }
        (
            PhysicalOperator::HashJoin {
                equality_key_count, ..
            },
            _,
        ) => {
            if equality_key_count == 0 {
                return inconsistent("HashJoin has no equality keys");
            }
        }
        _ => return inconsistent("statistics variant does not match physical operator"),
    }
    Ok(())
}

fn validate_promql_binary(
    statistics: &OperatorStatistics,
) -> Result<PromqlBinaryOperandMode, AnalyticalCostError> {
    let promql = require_promql_statistics(statistics, 2)?;
    let mode = promql
        .binary_operand_mode
        .ok_or(AnalyticalCostError::MissingOrStale(
            "promql_binary_operand_mode",
        ))?;
    match mode {
        PromqlBinaryOperandMode::VectorVector => {
            if statistics.hash_join_build_side.is_none() || statistics.key_bytes.is_none() {
                return Err(AnalyticalCostError::MissingOrStale(
                    "vector_binary_label_match_statistics",
                ));
            }
        }
        PromqlBinaryOperandMode::VectorScalar => {
            if promql.input_series[1] != 0
                || statistics.inputs[1].rows != promql.evaluation_steps
                || statistics.hash_join_build_side.is_some()
                || statistics.key_bytes.is_some()
            {
                return Err(AnalyticalCostError::InconsistentOperatorStatistics(
                    "vector/scalar binary evidence has an invalid scalar edge or label-match state",
                ));
            }
        }
        PromqlBinaryOperandMode::ScalarVector => {
            if promql.input_series[0] != 0
                || statistics.inputs[0].rows != promql.evaluation_steps
                || statistics.hash_join_build_side.is_some()
                || statistics.key_bytes.is_some()
            {
                return Err(AnalyticalCostError::InconsistentOperatorStatistics(
                    "scalar/vector binary evidence has an invalid scalar edge or label-match state",
                ));
            }
        }
    }
    Ok(mode)
}

fn require_promql_statistics(
    statistics: &OperatorStatistics,
    inputs: usize,
) -> Result<&crate::analytical_statistics::PromqlOperatorStatistics, AnalyticalCostError> {
    let promql = statistics
        .promql
        .as_ref()
        .ok_or(AnalyticalCostError::MissingOrStale(
            "promql_operator_statistics",
        ))?;
    if promql.evaluation_steps == 0 {
        return Err(AnalyticalCostError::MissingOrZero("evaluation_steps"));
    }
    if promql.input_series.len() != inputs {
        return Err(AnalyticalCostError::InconsistentOperatorStatistics(
            "PromQL input-series arity does not match physical inputs",
        ));
    }
    Ok(promql)
}

impl ResourceEstimate {
    pub fn calibrated_cost(
        self,
        calibration: &ResourceCalibration,
    ) -> Result<f64, AnalyticalCostError> {
        calibration.validate()?;
        let value = self.cpu_ops * calibration.cost_per_cpu_op
            + self.scan_bytes as f64 * calibration.cost_per_scan_byte
            + self.peak_memory_bytes as f64 * calibration.cost_per_retained_byte;
        if value.is_finite() {
            Ok(value)
        } else {
            Err(AnalyticalCostError::Overflow)
        }
    }
}

#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum AnalyticalCostError {
    #[error("analytical resource model v1 supports only DataArrival::AtRest, got {0:?}")]
    UnsupportedDataArrival(DataArrival),
    #[error("required analytical input {0} is missing or zero")]
    MissingOrZero(&'static str),
    #[error("required analytical evidence {0} is missing or stale")]
    MissingOrStale(&'static str),
    #[error("query recurrence cannot be resolved over the planning horizon")]
    InvalidRecurrence,
    #[error("query has no evaluations in the planning horizon")]
    NoEvaluationsInHorizon,
    #[error("calibration {0} must be finite and non-negative, got {1}")]
    InvalidCalibration(&'static str, f64),
    #[error("at least one calibration coefficient must be positive")]
    ZeroCalibration,
    #[error("algorithm {0:?} does not match parameters {1:?}")]
    ParameterMismatch(SketchAlgorithm, SketchParams),
    #[error("{0} needs a value-range/bin-count model before it can be estimated")]
    UnsupportedWithoutDistribution(&'static str),
    #[error("analytical arithmetic overflowed")]
    Overflow,
    #[error("candidate has no supported exact or sketch state")]
    UnsupportedCandidate,
    #[error("query operator has no physical implementation in the analytical model")]
    UnsupportedQueryOperator,
    #[error("inconsistent operator statistics: {0}")]
    InconsistentOperatorStatistics(&'static str),
    #[error("summary operation {0} has no lifecycle-aware cost formula")]
    UnsupportedSummaryOperation(&'static str),
    #[error("required comparison-scope field {0} is missing")]
    MissingComparisonScope(&'static str),
    #[error("scan node {0} does not declare source coverage")]
    MissingScanSourceCoverage(String),
    #[error("scan node {0} reads source coverage outside the comparison scope")]
    ScanOutsideComparisonScope(String),
    #[error("raw and candidate comparison scopes differ in {0}")]
    ComparisonScopeMismatch(&'static str),
    #[error("operator statistics are unavailable for physical node {0}")]
    MissingOperatorStatistics(String),
    #[error("invalid operator statistics for {node}: {reason}")]
    InvalidOperatorStatistics { node: String, reason: &'static str },
    #[error(
        "operator statistics conflict: parent {parent} input {input_index} does not match child {child} output"
    )]
    ConflictingEdgeStatistics {
        parent: String,
        child: String,
        input_index: usize,
    },
    #[error("invalid physical DAG: {0}")]
    InvalidPhysicalDag(&'static str),
}

fn checked_bytes(parts: &[u64]) -> Result<u64, AnalyticalCostError> {
    parts
        .iter()
        .try_fold(1_u64, |acc, value| acc.checked_mul(*value))
        .ok_or(AnalyticalCostError::Overflow)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::analytical_statistics::{
        validate_comparison_scopes, BinaryEdgeStatistics, ComparisonScope, EdgeStatistics,
        OperatorStatistics, PartitionStatistics, SourceCoverage, UnaryEdgeStatistics,
    };

    fn filter_operator() -> PhysicalOperator {
        PhysicalOperator::Filter {
            predicate_operations_per_row: 1,
        }
    }

    fn project_operator() -> PhysicalOperator {
        PhysicalOperator::Project {
            expression_operations_per_row: 1,
        }
    }

    fn aggregate_operator() -> PhysicalOperator {
        PhysicalOperator::HashAggregate {
            grouping_key_count: 1,
            accumulator_count: 1,
        }
    }

    fn unary_row_edges(input_rows: u64, output_rows: u64) -> UnaryEdgeStatistics {
        UnaryEdgeStatistics {
            input: EdgeStatistics {
                rows: input_rows,
                bytes: input_rows.saturating_mul(8),
            },
            output: EdgeStatistics {
                rows: output_rows,
                bytes: output_rows.saturating_mul(8),
            },
        }
    }

    #[test]
    fn operator_arity_distinguishes_source_statistics_from_dag_children() {
        assert_eq!(
            expected_input_arity(PhysicalOperator::Scan, 0),
            OperatorInputArity {
                statistics_inputs: 1,
                dag_children: 0,
            }
        );
        assert_eq!(
            expected_input_arity(filter_operator(), 1),
            OperatorInputArity {
                statistics_inputs: 1,
                dag_children: 1,
            }
        );
        assert_eq!(
            expected_input_arity(
                PhysicalOperator::HashJoin {
                    build_side: HashJoinBuildSide::Left,
                    equality_key_count: 1,
                },
                2,
            ),
            OperatorInputArity {
                statistics_inputs: 2,
                dag_children: 2,
            }
        );
        assert_eq!(
            expected_input_arity(PhysicalOperator::Concat, 3),
            OperatorInputArity {
                statistics_inputs: 3,
                dag_children: 3,
            }
        );
    }

    #[test]
    fn operator_estimator_rejects_contradictory_cardinality_evidence() {
        assert!(matches!(
            estimate_operator(
                filter_operator(),
                OperatorStatistics::Filter {
                    edges: unary_row_edges(10, 11),
                },
            ),
            Err(AnalyticalCostError::InconsistentOperatorStatistics(_))
        ));
        assert!(matches!(
            estimate_operator(
                project_operator(),
                OperatorStatistics::Project {
                    edges: unary_row_edges(10, 9),
                },
            ),
            Err(AnalyticalCostError::InconsistentOperatorStatistics(_))
        ));

        assert!(matches!(
            estimate_operator(
                aggregate_operator(),
                OperatorStatistics::HashAggregate {
                    edges: unary_row_edges(10, 3),
                    group_count: 2,
                    key_bytes: 8,
                    accumulator_bytes_per_group: 8,
                },
            ),
            Err(AnalyticalCostError::InconsistentOperatorStatistics(_))
        ));

        assert!(matches!(
            estimate_operator(
                PhysicalOperator::TopK {
                    limit: 3,
                    offset: 0,
                    ordering_key_count: 1,
                },
                OperatorStatistics::TopK {
                    edges: unary_row_edges(10, 4),
                },
            ),
            Err(AnalyticalCostError::InconsistentOperatorStatistics(_))
        ));
    }

    #[test]
    fn physical_operator_formulas_keep_disk_at_scan_and_require_join_stats() {
        let scan = estimate_operator(
            PhysicalOperator::Scan,
            OperatorStatistics::Scan {
                source_read_bytes: 64_000,
                edges: UnaryEdgeStatistics {
                    input: EdgeStatistics {
                        rows: 1_000,
                        bytes: 64_000,
                    },
                    output: EdgeStatistics {
                        rows: 1_000,
                        bytes: 64_000,
                    },
                },
            },
        )
        .unwrap();
        assert_eq!(scan.scan_bytes, 64_000);

        let topk = estimate_operator(
            PhysicalOperator::TopK {
                limit: 10,
                offset: 0,
                ordering_key_count: 1,
            },
            OperatorStatistics::TopK {
                edges: UnaryEdgeStatistics {
                    input: EdgeStatistics {
                        rows: 1_000,
                        bytes: 40_000,
                    },
                    output: EdgeStatistics {
                        rows: 10,
                        bytes: 400,
                    },
                },
            },
        )
        .unwrap();
        assert_eq!(topk.scan_bytes, 0);
        assert_eq!(topk.cpu_ops, 4_000.0);
        assert_eq!(topk.peak_memory_bytes, 400);

        let mismatched_join_statistics = estimate_operator(
            PhysicalOperator::HashJoin {
                build_side: HashJoinBuildSide::Left,
                equality_key_count: 1,
            },
            OperatorStatistics::Filter {
                edges: unary_row_edges(1_000, 100),
            },
        );
        assert_eq!(
            mismatched_join_statistics,
            Err(AnalyticalCostError::InconsistentOperatorStatistics(
                "statistics variant does not match physical operator"
            ))
        );

        let build_left = estimate_operator(
            PhysicalOperator::HashJoin {
                build_side: HashJoinBuildSide::Left,
                equality_key_count: 1,
            },
            OperatorStatistics::HashJoin {
                edges: BinaryEdgeStatistics {
                    inputs: [
                        EdgeStatistics {
                            rows: 1_000,
                            bytes: 64_000,
                        },
                        EdgeStatistics {
                            rows: 10,
                            bytes: 1_280,
                        },
                    ],
                    output: EdgeStatistics {
                        rows: 100,
                        bytes: 12_800,
                    },
                },
            },
        )
        .unwrap();
        assert_eq!(build_left.peak_memory_bytes, 80_000);

        let aggregate = estimate_operator(
            aggregate_operator(),
            OperatorStatistics::HashAggregate {
                group_count: 100,
                key_bytes: 16,
                accumulator_bytes_per_group: 24,
                edges: UnaryEdgeStatistics {
                    input: EdgeStatistics {
                        rows: 1_000,
                        bytes: 64_000,
                    },
                    output: EdgeStatistics {
                        rows: 100,
                        bytes: 4_000,
                    },
                },
            },
        )
        .unwrap();
        assert_eq!(aggregate.peak_memory_bytes, 5_600);

        let oversized_topk = estimate_operator(
            PhysicalOperator::TopK {
                limit: 1_000,
                offset: 0,
                ordering_key_count: 1,
            },
            OperatorStatistics::TopK {
                edges: UnaryEdgeStatistics {
                    input: EdgeStatistics {
                        rows: 4,
                        bytes: 160,
                    },
                    output: EdgeStatistics {
                        rows: 4,
                        bytes: 160,
                    },
                },
            },
        )
        .unwrap();
        assert_eq!(oversized_topk.cpu_ops, 8.0);

        let offset_limit = estimate_operator(
            PhysicalOperator::Limit {
                limit: 10,
                offset: 900_000,
            },
            OperatorStatistics::Limit {
                edges: UnaryEdgeStatistics {
                    input: EdgeStatistics {
                        rows: 1_000_000,
                        bytes: 40_000_000,
                    },
                    output: EdgeStatistics {
                        rows: 10,
                        bytes: 400,
                    },
                },
            },
        )
        .unwrap();
        assert_eq!(offset_limit.cpu_ops, 900_010.0);
    }

    #[test]
    fn physical_dag_counts_shared_scan_once_and_uses_live_memory() {
        let coverage = comparison_scope().sources[0].clone();
        let nodes = vec![
            PhysicalDagNode {
                id: "scan".into(),
                operator: PhysicalOperator::Scan,
                children: vec![],
                source_coverage: Some(coverage),
                output_buffer_bytes: 10,
                retained_bytes: 0,
                execution: ExecutionMultiplicity::PerEvaluation,
            },
            PhysicalDagNode {
                id: "left".into(),
                operator: filter_operator(),
                children: vec!["scan".into()],
                source_coverage: None,
                output_buffer_bytes: 4,
                retained_bytes: 0,
                execution: ExecutionMultiplicity::PerEvaluation,
            },
            PhysicalDagNode {
                id: "right".into(),
                operator: filter_operator(),
                children: vec!["scan".into()],
                source_coverage: None,
                output_buffer_bytes: 4,
                retained_bytes: 0,
                execution: ExecutionMultiplicity::PerEvaluation,
            },
            PhysicalDagNode {
                id: "root".into(),
                operator: PhysicalOperator::Concat,
                children: vec!["left".into(), "right".into()],
                source_coverage: None,
                output_buffer_bytes: 8,
                retained_bytes: 0,
                execution: ExecutionMultiplicity::PerEvaluation,
            },
        ];
        let scan_edge = EdgeStatistics {
            rows: 100,
            bytes: 1_000,
        };
        let branch_edge = EdgeStatistics {
            rows: 40,
            bytes: 400,
        };
        let provided = HashMap::from([
            (
                "scan".into(),
                OperatorStatistics::Scan {
                    edges: unary_edges(scan_edge, scan_edge),
                    source_read_bytes: 1_000,
                },
            ),
            (
                "left".into(),
                OperatorStatistics::Filter {
                    edges: unary_edges(scan_edge, branch_edge),
                },
            ),
            (
                "right".into(),
                OperatorStatistics::Filter {
                    edges: unary_edges(scan_edge, branch_edge),
                },
            ),
            (
                "root".into(),
                OperatorStatistics::Concat {
                    inputs: vec![branch_edge, branch_edge],
                    output: EdgeStatistics {
                        rows: 80,
                        bytes: 800,
                    },
                },
            ),
        ]);
        let mut scope = comparison_scope();
        scope.horizon.0 = 20_000;
        let estimate = estimate_physical_dag(&nodes, "root", &scope, &provided).unwrap();
        assert_eq!(estimate.cpu_ops, 760.0);
        assert_eq!(estimate.scan_bytes, 2_000);
        // This is neither the sum of every node's memory nor just the largest
        // node: it is the maximum state simultaneously live at the fan-out.
        assert_eq!(estimate.peak_memory_bytes, 28);
    }

    #[test]
    fn physical_dag_separates_build_once_from_per_evaluation_work() {
        let coverage = comparison_scope().sources[0].clone();
        let nodes = vec![
            PhysicalDagNode {
                id: "scan".into(),
                operator: PhysicalOperator::Scan,
                children: vec![],
                source_coverage: Some(coverage),
                output_buffer_bytes: 10,
                retained_bytes: 0,
                execution: ExecutionMultiplicity::Once,
            },
            PhysicalDagNode {
                id: "state".into(),
                operator: aggregate_operator(),
                children: vec!["scan".into()],
                source_coverage: None,
                output_buffer_bytes: 16,
                retained_bytes: 32,
                execution: ExecutionMultiplicity::Once,
            },
            PhysicalDagNode {
                id: "read".into(),
                operator: PhysicalOperator::Limit {
                    limit: 1,
                    offset: 0,
                },
                children: vec!["state".into()],
                source_coverage: None,
                output_buffer_bytes: 16,
                retained_bytes: 0,
                execution: ExecutionMultiplicity::PerEvaluation,
            },
        ];
        let scan_edge = EdgeStatistics {
            rows: 100,
            bytes: 1_000,
        };
        let state_edge = EdgeStatistics { rows: 1, bytes: 16 };
        let provided = HashMap::from([
            (
                "scan".into(),
                OperatorStatistics::Scan {
                    edges: unary_edges(scan_edge, scan_edge),
                    source_read_bytes: 1_000,
                },
            ),
            (
                "state".into(),
                OperatorStatistics::HashAggregate {
                    edges: unary_edges(scan_edge, state_edge),
                    group_count: 1,
                    key_bytes: 8,
                    accumulator_bytes_per_group: 8,
                },
            ),
            (
                "read".into(),
                OperatorStatistics::Limit {
                    edges: unary_edges(state_edge, state_edge),
                },
            ),
        ]);
        let mut scope = comparison_scope();
        scope.horizon.0 = 100_000;
        let estimate = estimate_physical_dag(&nodes, "read", &scope, &provided).unwrap();
        assert_eq!(estimate.cpu_ops, 310.0);
        assert_eq!(estimate.scan_bytes, 1_000);
    }

    fn comparison_scope() -> ComparisonScope {
        use asap_types::pre_asap::query_expr::Source;
        use asap_types::workload::{
            DurationMs, QueryRecurrence, QueryTimeScope, RepeatedDemand, RepetitionInterval,
            TimeSelection, TimestampMs,
        };

        ComparisonScope {
            data_arrival: DataArrival::AtRest,
            planning_time: TimestampMs(1_000),
            horizon: DurationMs(60_000),
            recurrence: QueryRecurrence::Repeated(RepeatedDemand::FixedInterval(
                RepetitionInterval(10_000),
            )),
            time_selection: TimeSelection {
                scope: QueryTimeScope::Longitudinal,
                lookback: Some(DurationMs(300_000)),
                as_of: Some(TimestampMs(1_000)),
            },
            sources: vec![SourceCoverage {
                source: Source::Table {
                    table_ref: "metrics".into(),
                },
                source_snapshot_id: "catalog-version-42".into(),
                predicates: vec![],
            }],
        }
    }

    fn unary_edges(input: EdgeStatistics, output: EdgeStatistics) -> UnaryEdgeStatistics {
        UnaryEdgeStatistics { input, output }
    }

    fn manual_unary_plan(
        operator: PhysicalOperator,
        operator_statistics: OperatorStatistics,
    ) -> (Vec<PhysicalDagNode>, HashMap<String, OperatorStatistics>) {
        let coverage = comparison_scope().sources[0].clone();
        let scan_edge = operator_statistics.inputs[0];
        let nodes = vec![
            PhysicalDagNode {
                id: "scan".into(),
                operator: PhysicalOperator::Scan,
                children: vec![],
                source_coverage: Some(coverage),
                output_buffer_bytes: 10,
                retained_bytes: 0,
                execution: ExecutionMultiplicity::PerEvaluation,
            },
            PhysicalDagNode {
                id: "operator".into(),
                operator,
                children: vec!["scan".into()],
                source_coverage: None,
                output_buffer_bytes: 10,
                retained_bytes: 0,
                execution: ExecutionMultiplicity::PerEvaluation,
            },
        ];
        let provided = HashMap::from([
            (
                "scan".into(),
                OperatorStatistics {
                    source_scan_bytes: scan_edge.bytes,
                    ..statistics(vec![scan_edge], scan_edge)
                },
            ),
            ("operator".into(), operator_statistics),
        ]);
        (nodes, provided)
    }

    #[test]
    fn estimator_requires_reachable_scans_to_cover_the_exact_scope_set() {
        use asap_types::pre_asap::Source;

        let edge = EdgeStatistics {
            rows: 100,
            bytes: 1_000,
        };
        let (nodes, provided) =
            manual_unary_plan(PhysicalOperator::Filter, statistics(vec![edge], edge));
        let mut scope = comparison_scope();
        scope.sources.push(SourceCoverage {
            source: Source::Table {
                table_ref: "unread_metrics".into(),
            },
            snapshot_id: "catalog-version-42".into(),
            predicates: vec![],
        });

        assert_eq!(
            estimate_physical_dag(&nodes, "operator", &scope, &provided),
            Err(AnalyticalCostError::InvalidPhysicalDag(
                "physical scans omit a comparison-scope source"
            ))
        );
    }

    #[test]
    fn estimator_rejects_semantically_impossible_manual_operator_statistics() {
        let input = EdgeStatistics {
            rows: 100,
            bytes: 1_000,
        };
        let invalid_cases = [
            (
                PhysicalOperator::Filter,
                statistics(
                    vec![input],
                    EdgeStatistics {
                        rows: 101,
                        bytes: 1_010,
                    },
                ),
            ),
            (
                PhysicalOperator::Sort,
                statistics(
                    vec![input],
                    EdgeStatistics {
                        rows: 99,
                        bytes: 990,
                    },
                ),
            ),
            (
                PhysicalOperator::HashAggregate,
                OperatorStatistics {
                    group_count: Some(10),
                    key_bytes: Some(8),
                    aggregate_value_bytes: Some(8),
                    ..statistics(
                        vec![input],
                        EdgeStatistics {
                            rows: 9,
                            bytes: 144,
                        },
                    )
                },
            ),
        ];

        for (operator, invalid_statistics) in invalid_cases {
            let (nodes, provided) = manual_unary_plan(operator, invalid_statistics);
            assert!(matches!(
                estimate_physical_dag(&nodes, "operator", &comparison_scope(), &provided),
                Err(AnalyticalCostError::InconsistentOperatorStatistics(_))
            ));
        }
    }

    #[test]
    fn estimator_validates_concat_totals_but_allows_duplicate_source_coverage() {
        let coverage = comparison_scope().sources[0].clone();
        let edge = EdgeStatistics {
            rows: 50,
            bytes: 500,
        };
        let nodes = vec![
            PhysicalDagNode {
                id: "left".into(),
                operator: PhysicalOperator::Scan,
                children: vec![],
                source_coverage: Some(coverage.clone()),
                output_buffer_bytes: 10,
                retained_bytes: 0,
                execution: ExecutionMultiplicity::PerEvaluation,
            },
            PhysicalDagNode {
                id: "right".into(),
                operator: PhysicalOperator::Scan,
                children: vec![],
                source_coverage: Some(coverage),
                output_buffer_bytes: 10,
                retained_bytes: 0,
                execution: ExecutionMultiplicity::PerEvaluation,
            },
            PhysicalDagNode {
                id: "concat".into(),
                operator: PhysicalOperator::Concat,
                children: vec!["left".into(), "right".into()],
                source_coverage: None,
                output_buffer_bytes: 10,
                retained_bytes: 0,
                execution: ExecutionMultiplicity::PerEvaluation,
            },
        ];
        let mut provided = HashMap::from([
            (
                "left".into(),
                OperatorStatistics {
                    source_scan_bytes: 500,
                    ..statistics(vec![edge], edge)
                },
            ),
            (
                "right".into(),
                OperatorStatistics {
                    source_scan_bytes: 500,
                    ..statistics(vec![edge], edge)
                },
            ),
            (
                "concat".into(),
                statistics(
                    vec![edge, edge],
                    EdgeStatistics {
                        rows: 99,
                        bytes: 990,
                    },
                ),
            ),
        ]);

        assert!(matches!(
            estimate_physical_dag(&nodes, "concat", &comparison_scope(), &provided),
            Err(AnalyticalCostError::InconsistentOperatorStatistics(_))
        ));

        provided.insert(
            "concat".into(),
            statistics(
                vec![edge, edge],
                EdgeStatistics {
                    rows: 100,
                    bytes: 1_000,
                },
            ),
        );
        assert!(estimate_physical_dag(&nodes, "concat", &comparison_scope(), &provided).is_ok());
    }

    #[test]
    fn directional_promql_bridges_fail_closed_on_wrong_cardinality() {
        use crate::analytical_statistics::PromqlOperatorStatistics;

        let bridge_statistics = |input_series, output_series, input_rows, output_rows| {
            let mut statistics = statistics(
                vec![EdgeStatistics {
                    rows: input_rows,
                    bytes: input_rows * 8,
                }],
                EdgeStatistics {
                    rows: output_rows,
                    bytes: output_rows * 8,
                },
            );
            statistics.promql = Some(PromqlOperatorStatistics {
                input_series: vec![input_series],
                output_series,
                evaluation_steps: 10,
                window_samples_per_series: None,
                subquery_steps: None,
                scalar_ops_per_row: None,
                binary_operand_mode: None,
            });
            statistics
        };

        assert!(estimate_operator(
            PhysicalOperator::PromqlScalarToVector,
            bridge_statistics(0, 1, 10, 10),
        )
        .is_ok());
        assert!(estimate_operator(
            PhysicalOperator::PromqlVectorToScalar,
            bridge_statistics(3, 0, 30, 10),
        )
        .is_ok());
        assert!(matches!(
            estimate_operator(
                PhysicalOperator::PromqlScalarToVector,
                bridge_statistics(0, 0, 10, 10),
            ),
            Err(AnalyticalCostError::InconsistentOperatorStatistics(_))
        ));
        assert!(matches!(
            estimate_operator(
                PhysicalOperator::PromqlVectorToScalar,
                bridge_statistics(3, 1, 30, 10),
            ),
            Err(AnalyticalCostError::InconsistentOperatorStatistics(_))
        ));
        assert!(matches!(
            estimate_operator(
                PhysicalOperator::PromqlScalarToVector,
                bridge_statistics(0, 1, 9, 10),
            ),
            Err(AnalyticalCostError::InconsistentOperatorStatistics(_))
        ));
        assert!(matches!(
            estimate_operator(
                PhysicalOperator::PromqlVectorToScalar,
                bridge_statistics(3, 0, 30, 9),
            ),
            Err(AnalyticalCostError::InconsistentOperatorStatistics(_))
        ));
    }

    #[test]
    fn promql_binary_mode_distinguishes_empty_vectors_from_scalars() {
        use crate::analytical_statistics::{PromqlBinaryOperandMode, PromqlOperatorStatistics};

        let empty = EdgeStatistics { rows: 0, bytes: 0 };
        let mut vector_vector = OperatorStatistics {
            key_bytes: Some(16),
            hash_join_build_side: Some(HashJoinBuildSide::Right),
            promql: Some(PromqlOperatorStatistics {
                input_series: vec![0, 0],
                output_series: 0,
                evaluation_steps: 10,
                window_samples_per_series: None,
                subquery_steps: None,
                scalar_ops_per_row: None,
                binary_operand_mode: Some(PromqlBinaryOperandMode::VectorVector),
            }),
            ..statistics(vec![empty, empty], empty)
        };
        assert!(
            estimate_operator(PhysicalOperator::PromqlVectorBinary, vector_vector.clone()).is_ok()
        );

        vector_vector.hash_join_build_side = None;
        assert_eq!(
            estimate_operator(PhysicalOperator::PromqlVectorBinary, vector_vector),
            Err(AnalyticalCostError::MissingOrStale(
                "vector_binary_label_match_statistics"
            ))
        );
    }

    #[test]
    fn promql_scalar_leaf_requires_one_row_per_evaluation_step() {
        use crate::analytical_statistics::PromqlOperatorStatistics;

        let mut scalar = statistics(vec![], EdgeStatistics { rows: 9, bytes: 72 });
        scalar.promql = Some(PromqlOperatorStatistics {
            input_series: vec![],
            output_series: 0,
            evaluation_steps: 10,
            window_samples_per_series: None,
            subquery_steps: None,
            scalar_ops_per_row: None,
            binary_operand_mode: None,
        });
        assert!(matches!(
            estimate_operator(PhysicalOperator::PromqlScalarLeaf, scalar),
            Err(AnalyticalCostError::InconsistentOperatorStatistics(_))
        ));
    }

    #[test]
    fn comparison_rejects_different_snapshot_predicate_time_or_horizon() {
        use std::rc::Rc;

        use asap_types::pre_asap::query_expr::{Predicate, QueryExpr};
        use asap_types::workload::{DurationMs, TimestampMs};

        let raw = comparison_scope();
        assert_eq!(validate_comparison_scopes(&raw, &raw).unwrap(), 6);

        let mut candidate = raw.clone();
        candidate.sources[0].source_snapshot_id = "catalog-version-43".into();
        assert_eq!(
            validate_comparison_scopes(&raw, &candidate),
            Err(AnalyticalCostError::ComparisonScopeMismatch("sources"))
        );

        candidate = raw.clone();
        candidate.sources[0]
            .predicates
            .push(Predicate(Rc::new(QueryExpr::promql_scalar(1.0))));
        assert_eq!(
            validate_comparison_scopes(&raw, &candidate),
            Err(AnalyticalCostError::ComparisonScopeMismatch("sources"))
        );

        candidate = raw.clone();
        candidate.time_selection.as_of = Some(TimestampMs(2_000));
        assert_eq!(
            validate_comparison_scopes(&raw, &candidate),
            Err(AnalyticalCostError::ComparisonScopeMismatch(
                "time_selection"
            ))
        );

        candidate = raw.clone();
        candidate.horizon = DurationMs(120_000);
        assert_eq!(
            validate_comparison_scopes(&raw, &candidate),
            Err(AnalyticalCostError::ComparisonScopeMismatch("horizon"))
        );
    }

    #[test]
    fn physical_dag_fails_closed_on_missing_or_conflicting_edge_statistics() {
        use std::collections::HashMap;

        let coverage = comparison_scope().sources[0].clone();
        let nodes = vec![
            PhysicalDagNode {
                id: "scan".into(),
                operator: PhysicalOperator::Scan,
                children: vec![],
                source_coverage: Some(coverage),
                output_buffer_bytes: 10,
                retained_bytes: 0,
                execution: ExecutionMultiplicity::PerEvaluation,
            },
            PhysicalDagNode {
                id: "filter".into(),
                operator: filter_operator(),
                children: vec!["scan".into()],
                source_coverage: None,
                output_buffer_bytes: 4,
                retained_bytes: 0,
                execution: ExecutionMultiplicity::PerEvaluation,
            },
        ];
        let scope = comparison_scope();
        let mut provided = HashMap::from([(
            "scan".to_string(),
            OperatorStatistics::Scan {
                edges: unary_edges(
                    EdgeStatistics {
                        rows: 100,
                        bytes: 1_000,
                    },
                    EdgeStatistics {
                        rows: 100,
                        bytes: 1_000,
                    },
                ),
                source_read_bytes: 1_000,
            },
        )]);

        assert_eq!(
            estimate_physical_dag(&nodes, "filter", &scope, &provided),
            Err(AnalyticalCostError::MissingOperatorStatistics(
                "filter".into()
            ))
        );

        provided.insert(
            "filter".into(),
            OperatorStatistics::Filter {
                edges: unary_edges(
                    EdgeStatistics {
                        rows: 99,
                        bytes: 990,
                    },
                    EdgeStatistics {
                        rows: 40,
                        bytes: 400,
                    },
                ),
            },
        );
        assert_eq!(
            estimate_physical_dag(&nodes, "filter", &scope, &provided),
            Err(AnalyticalCostError::ConflictingEdgeStatistics {
                parent: "filter".into(),
                child: "scan".into(),
                input_index: 0,
            })
        );
    }

    #[test]
    fn provider_statistics_drive_a_consistent_physical_dag_estimate() {
        use std::collections::HashMap;

        let coverage = comparison_scope().sources[0].clone();
        let nodes = vec![
            PhysicalDagNode {
                id: "scan".into(),
                operator: PhysicalOperator::Scan,
                children: vec![],
                source_coverage: Some(coverage),
                output_buffer_bytes: 10,
                retained_bytes: 0,
                execution: ExecutionMultiplicity::PerEvaluation,
            },
            PhysicalDagNode {
                id: "filter".into(),
                operator: filter_operator(),
                children: vec!["scan".into()],
                source_coverage: None,
                output_buffer_bytes: 4,
                retained_bytes: 0,
                execution: ExecutionMultiplicity::PerEvaluation,
            },
        ];
        let provided = HashMap::from([
            (
                "scan".to_string(),
                OperatorStatistics::Scan {
                    edges: unary_edges(
                        EdgeStatistics {
                            rows: 100,
                            bytes: 1_000,
                        },
                        EdgeStatistics {
                            rows: 100,
                            bytes: 1_000,
                        },
                    ),
                    source_read_bytes: 1_000,
                },
            ),
            (
                "filter".to_string(),
                OperatorStatistics::Filter {
                    edges: unary_edges(
                        EdgeStatistics {
                            rows: 100,
                            bytes: 1_000,
                        },
                        EdgeStatistics {
                            rows: 40,
                            bytes: 400,
                        },
                    ),
                },
            ),
        ]);

        let estimate =
            estimate_physical_dag(&nodes, "filter", &comparison_scope(), &provided).unwrap();
        assert_eq!(estimate.cpu_ops, 1_200.0);
        assert_eq!(estimate.scan_bytes, 6_000);
    }

    #[test]
    fn physical_dag_accepts_an_empty_operator_output() {
        let coverage = comparison_scope().sources[0].clone();
        let nodes = vec![
            PhysicalDagNode {
                id: "scan".into(),
                operator: PhysicalOperator::Scan,
                children: vec![],
                source_coverage: Some(coverage),
                output_buffer_bytes: 10,
                retained_bytes: 0,
                execution: ExecutionMultiplicity::PerEvaluation,
            },
            PhysicalDagNode {
                id: "filter".into(),
                operator: filter_operator(),
                children: vec!["scan".into()],
                source_coverage: None,
                output_buffer_bytes: 0,
                retained_bytes: 0,
                execution: ExecutionMultiplicity::PerEvaluation,
            },
        ];
        let input = EdgeStatistics {
            rows: 100,
            bytes: 1_000,
        };
        let provided = HashMap::from([
            (
                "scan".into(),
                OperatorStatistics::Scan {
                    edges: unary_edges(input, input),
                    source_read_bytes: 1_000,
                },
            ),
            (
                "filter".into(),
                OperatorStatistics::Filter {
                    edges: unary_edges(input, EdgeStatistics { rows: 0, bytes: 0 }),
                },
            ),
        ]);

        let estimate =
            estimate_physical_dag(&nodes, "filter", &comparison_scope(), &provided).unwrap();
        assert_eq!(estimate.cpu_ops, 1_200.0);
        assert_eq!(estimate.peak_memory_bytes, 20);
        assert_eq!(estimate.scan_bytes, 6_000);
    }

    #[test]
    fn physical_dag_rejects_a_scan_not_covered_by_its_scope() {
        let nodes = vec![PhysicalDagNode {
            id: "scan".into(),
            operator: PhysicalOperator::Scan,
            children: vec![],
            source_coverage: Some(SourceCoverage {
                source: asap_types::pre_asap::query_expr::Source::Table {
                    table_ref: "other_metrics".into(),
                },
                source_snapshot_id: "catalog-version-42".into(),
                predicates: vec![],
            }),
            output_buffer_bytes: 10,
            retained_bytes: 0,
            execution: ExecutionMultiplicity::PerEvaluation,
        }];
        let edge = EdgeStatistics {
            rows: 100,
            bytes: 1_000,
        };
        let provided = HashMap::from([(
            "scan".into(),
            OperatorStatistics::Scan {
                edges: unary_edges(edge, edge),
                source_read_bytes: 250,
            },
        )]);

        assert_eq!(
            estimate_physical_dag(&nodes, "scan", &comparison_scope(), &provided),
            Err(AnalyticalCostError::ScanOutsideComparisonScope(
                "scan".into()
            ))
        );
    }

    #[test]
    fn physical_dag_rejects_a_scan_without_explicit_coverage() {
        let nodes = vec![PhysicalDagNode {
            id: "scan".into(),
            operator: PhysicalOperator::Scan,
            children: vec![],
            source_coverage: None,
            output_buffer_bytes: 10,
            retained_bytes: 0,
            execution: ExecutionMultiplicity::PerEvaluation,
        }];
        let edge = EdgeStatistics {
            rows: 100,
            bytes: 1_000,
        };
        let provided = HashMap::from([(
            "scan".into(),
            OperatorStatistics::Scan {
                edges: unary_edges(edge, edge),
                source_read_bytes: 250,
            },
        )]);

        assert_eq!(
            estimate_physical_dag(&nodes, "scan", &comparison_scope(), &provided),
            Err(AnalyticalCostError::MissingScanSourceCoverage(
                "scan".into()
            ))
        );
    }

    #[test]
    fn physical_dag_rejects_an_unconsumed_scope_source() {
        let mut scope = comparison_scope();
        let coverage = scope.sources[0].clone();
        scope.sources.push(SourceCoverage {
            source: asap_types::pre_asap::query_expr::Source::Table {
                table_ref: "auxiliary".into(),
            },
            source_snapshot_id: "catalog-version-42".into(),
            predicates: vec![],
        });
        let nodes = vec![PhysicalDagNode {
            id: "scan".into(),
            operator: PhysicalOperator::Scan,
            children: vec![],
            source_coverage: Some(coverage),
            output_buffer_bytes: 10,
            retained_bytes: 0,
            execution: ExecutionMultiplicity::PerEvaluation,
        }];
        let edge = EdgeStatistics {
            rows: 100,
            bytes: 1_000,
        };
        let provided = HashMap::from([(
            "scan".into(),
            OperatorStatistics::Scan {
                edges: unary_edges(edge, edge),
                source_read_bytes: 250,
            },
        )]);

        assert_eq!(
            estimate_physical_dag(&nodes, "scan", &scope, &provided),
            Err(AnalyticalCostError::InvalidPhysicalDag(
                "physical scans omit a comparison-scope source"
            ))
        );
    }

    #[test]
    fn build_once_parent_cannot_consume_a_per_evaluation_child() {
        let coverage = comparison_scope().sources[0].clone();
        let nodes = vec![
            PhysicalDagNode {
                id: "scan".into(),
                operator: PhysicalOperator::Scan,
                children: vec![],
                source_coverage: Some(coverage),
                output_buffer_bytes: 10,
                retained_bytes: 0,
                execution: ExecutionMultiplicity::PerEvaluation,
            },
            PhysicalDagNode {
                id: "aggregate".into(),
                operator: aggregate_operator(),
                children: vec!["scan".into()],
                source_coverage: None,
                output_buffer_bytes: 16,
                retained_bytes: 32,
                execution: ExecutionMultiplicity::Once,
            },
        ];
        let input = EdgeStatistics {
            rows: 100,
            bytes: 1_000,
        };
        let output = EdgeStatistics { rows: 1, bytes: 16 };
        let provided = HashMap::from([
            (
                "scan".into(),
                OperatorStatistics::Scan {
                    edges: unary_edges(input, input),
                    source_read_bytes: 250,
                },
            ),
            (
                "aggregate".into(),
                OperatorStatistics::HashAggregate {
                    edges: unary_edges(input, output),
                    group_count: 1,
                    key_bytes: 8,
                    accumulator_bytes_per_group: 8,
                },
            ),
        ]);

        assert_eq!(
            estimate_physical_dag(&nodes, "aggregate", &comparison_scope(), &provided),
            Err(AnalyticalCostError::InvalidPhysicalDag(
                "build-once node cannot consume a per-evaluation child"
            ))
        );
    }

    #[test]
    fn scoped_comparison_rejects_different_source_snapshots() {
        let coverage = comparison_scope().sources[0].clone();
        let nodes = vec![PhysicalDagNode {
            id: "scan".into(),
            operator: PhysicalOperator::Scan,
            children: vec![],
            source_coverage: Some(coverage),
            output_buffer_bytes: 10,
            retained_bytes: 0,
            execution: ExecutionMultiplicity::PerEvaluation,
        }];
        let edge = EdgeStatistics {
            rows: 100,
            bytes: 1_000,
        };
        let provided = HashMap::from([(
            "scan".into(),
            OperatorStatistics::Scan {
                edges: unary_edges(edge, edge),
                source_read_bytes: 250,
            },
        )]);
        let raw_scope = comparison_scope();
        let mut candidate_scope = raw_scope.clone();
        candidate_scope.sources[0].source_snapshot_id = "catalog-version-43".into();

        assert_eq!(
            estimate_physical_dag_comparison(
                PhysicalDagEstimateRequest {
                    nodes: &nodes,
                    root: "scan",
                    scope: &raw_scope,
                    statistics: &provided,
                },
                PhysicalDagEstimateRequest {
                    nodes: &nodes,
                    root: "scan",
                    scope: &candidate_scope,
                    statistics: &provided,
                },
            ),
            Err(AnalyticalCostError::ComparisonScopeMismatch("sources"))
        );
    }

    #[test]
    fn scan_uses_physical_source_bytes_not_decoded_logical_bytes() {
        let logical = EdgeStatistics {
            rows: 100,
            bytes: 10_000,
        };
        let estimate = estimate_operator(
            PhysicalOperator::Scan,
            OperatorStatistics::Scan {
                edges: unary_edges(logical, logical),
                source_read_bytes: 2_500,
            },
        )
        .unwrap();

        assert_eq!(estimate.scan_bytes, 2_500);
        assert_eq!(estimate.peak_memory_bytes, 100);
    }

    #[test]
    fn non_scan_operator_estimates_cannot_charge_source_reads() {
        let edge = EdgeStatistics {
            rows: 100,
            bytes: 1_000,
        };
        let filter = OperatorStatistics::Filter {
            edges: unary_edges(edge, edge),
        };

        assert_eq!(
            estimate_operator(filter_operator(), filter)
                .unwrap()
                .scan_bytes,
            0
        );
    }

    #[test]
    fn non_empty_scan_cannot_claim_zero_source_reads() {
        let logical = EdgeStatistics {
            rows: 10,
            bytes: 80,
        };
        assert_eq!(
            estimate_operator(
                PhysicalOperator::Scan,
                OperatorStatistics::Scan {
                    edges: unary_edges(logical, logical),
                    source_read_bytes: 0,
                },
            ),
            Err(AnalyticalCostError::InconsistentOperatorStatistics(
                "non-empty Scan has zero source-read bytes"
            ))
        );
    }

    #[test]
    fn hash_aggregate_handles_empty_grouped_and_ungrouped_inputs() {
        let empty = EdgeStatistics { rows: 0, bytes: 0 };
        let ungrouped = estimate_operator(
            PhysicalOperator::HashAggregate {
                grouping_key_count: 0,
                accumulator_count: 1,
            },
            OperatorStatistics::HashAggregate {
                edges: unary_edges(empty, EdgeStatistics { rows: 1, bytes: 8 }),
                group_count: 1,
                key_bytes: 0,
                accumulator_bytes_per_group: 8,
            },
        )
        .unwrap();
        assert_eq!(ungrouped.cpu_ops, 0.0);
        assert_eq!(ungrouped.peak_memory_bytes, 24);

        let grouped = estimate_operator(
            PhysicalOperator::HashAggregate {
                grouping_key_count: 1,
                accumulator_count: 1,
            },
            OperatorStatistics::HashAggregate {
                edges: unary_edges(empty, empty),
                group_count: 0,
                key_bytes: 8,
                accumulator_bytes_per_group: 8,
            },
        )
        .unwrap();
        assert_eq!(grouped.peak_memory_bytes, 0);
    }

    #[test]
    fn operator_local_work_and_partition_distribution_change_cpu_and_memory() {
        let edge = EdgeStatistics {
            rows: 100,
            bytes: 800,
        };
        let filter = OperatorStatistics::Filter {
            edges: unary_edges(edge, edge),
        };
        assert_eq!(
            estimate_operator(
                PhysicalOperator::Filter {
                    predicate_operations_per_row: 3,
                },
                filter,
            )
            .unwrap()
            .cpu_ops,
            300.0
        );

        let sort = estimate_operator(
            PhysicalOperator::InMemoryComparisonSort {
                ordering_key_count: 1,
                partitioned: true,
            },
            OperatorStatistics::InMemoryComparisonSort {
                edges: unary_edges(edge, edge),
                input_partitioning: PartitionStatistics {
                    partitions: vec![
                        EdgeStatistics {
                            rows: 50,
                            bytes: 400,
                        },
                        EdgeStatistics {
                            rows: 50,
                            bytes: 400,
                        },
                    ],
                },
            },
        )
        .unwrap();
        assert_eq!(sort.cpu_ops, 600.0);
        assert_eq!(sort.peak_memory_bytes, 400);
    }

    #[test]
    fn concat_requires_at_least_one_physical_input() {
        assert_eq!(
            estimate_operator(
                PhysicalOperator::Concat,
                OperatorStatistics::Concat {
                    inputs: vec![],
                    output: EdgeStatistics { rows: 0, bytes: 0 },
                },
            ),
            Err(AnalyticalCostError::InconsistentOperatorStatistics(
                "Concat must have at least one input"
            ))
        );
    }

    #[test]
    fn serialized_operator_statistics_reject_unrelated_fields() {
        let edge = serde_json::json!({ "rows": 100, "bytes": 1_000 });
        let edges = serde_json::json!({ "input": edge, "output": edge });

        assert!(
            serde_json::from_value::<OperatorStatistics>(serde_json::json!({
                "operator": "filter",
                "edges": edges.clone(),
                "source_read_bytes": 1_000
            }))
            .is_err()
        );
        assert!(
            serde_json::from_value::<OperatorStatistics>(serde_json::json!({
                "operator": "top_k",
                "edges": edges,
                "k": 10
            }))
            .is_err()
        );
    }
}

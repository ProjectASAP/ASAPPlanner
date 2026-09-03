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
    OperatorStatisticsProvider, PromqlEdgeStatistics, PromqlValueKind, SourceCoverage,
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
    PromqlRange {
        range_millis: u64,
    },
    PromqlSubquery {
        range_millis: u64,
        resolution_millis: Option<u64>,
    },
    PromqlBinary {
        operation: PromqlBinaryOperation,
        operand_mode: PromqlBinaryOperandMode,
        cardinality: PromqlVectorCardinality,
        build_side: Option<HashJoinBuildSide>,
    },
    PromqlRelabel {
        expression_operations_per_row: u64,
    },
    PromqlInfoEnrich {
        matcher_operations_per_info_row: u64,
    },
    PromqlSeriesSample {
        kind: PromqlSeriesSampleKind,
        grouping_key_count: u64,
    },
    PromqlScalarToVector,
    PromqlVectorToScalar,
    PromqlScalarLeaf,
    PromqlPerSeries {
        operations_per_row: u64,
        accumulator_count: u64,
    },
    PromqlPresence {
        kind: PromqlPresenceKind,
        operations_per_row: u64,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum PromqlBinaryOperandMode {
    VectorVector,
    VectorScalar,
    ScalarVector,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum PromqlBinaryOperation {
    ArithmeticOrComparison,
    And,
    Or,
    Unless,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum PromqlVectorCardinality {
    OneToOne,
    ManyToOne,
    OneToMany,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum PromqlSeriesSampleKind {
    LimitK { k: u64 },
    LimitRatio { ratio_bits: u64 },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum PromqlPresenceKind {
    /// `absent` and `absent_over_time`: synthesize at most one output series.
    Absent,
    /// `present_over_time`: emit independently for each input series.
    PresentPerSeries,
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
    validate_promql_child_edges(node, node_statistics, statistics)?;
    Ok(())
}

fn validate_promql_child_edges(
    node: &PhysicalDagNode,
    node_statistics: &OperatorStatistics,
    statistics: &HashMap<&str, OperatorStatistics>,
) -> Result<(), AnalyticalCostError> {
    let output = node_statistics.promql_output();
    let child_has_promql = node
        .children
        .iter()
        .any(|child_id| statistics[child_id.as_str()].promql_output().is_some());
    if output.is_none() && child_has_promql {
        return Err(AnalyticalCostError::InvalidOperatorStatistics {
            node: node.id.clone(),
            reason: "operator drops child PromQL edge statistics",
        });
    }
    let Some(output) = output else { return Ok(()) };
    validate_promql_edge(output)?;
    for (index, child_id) in node.children.iter().enumerate() {
        let input =
            node_statistics
                .promql_input(index)
                .ok_or(AnalyticalCostError::MissingOrStale(
                    "promql_input_edge_statistics",
                ))?;
        let child = statistics[child_id.as_str()].promql_output().ok_or(
            AnalyticalCostError::MissingOrStale("promql_child_output_statistics"),
        )?;
        validate_promql_edge(input)?;
        if input != child {
            return Err(AnalyticalCostError::InvalidOperatorStatistics {
                node: node.id.clone(),
                reason: "PromQL parent input does not match child output",
            });
        }
    }
    Ok(())
}

fn validate_promql_edge(edge: PromqlEdgeStatistics) -> Result<(), AnalyticalCostError> {
    if edge.evaluation_steps == 0 {
        return Err(AnalyticalCostError::MissingOrZero("evaluation_steps"));
    }
    if matches!(edge.value_kind, PromqlValueKind::Scalar) && edge.series != 0 {
        return Err(AnalyticalCostError::InconsistentOperatorStatistics(
            "PromQL scalar edge cannot carry series",
        ));
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
        PhysicalOperator::PromqlRange { .. } => {
            matches!(statistics, OperatorStatistics::PromqlRange { .. })
        }
        PhysicalOperator::PromqlSubquery { .. } => {
            matches!(statistics, OperatorStatistics::PromqlSubquery { .. })
        }
        PhysicalOperator::PromqlBinary { .. } => {
            matches!(statistics, OperatorStatistics::PromqlBinary { .. })
        }
        PhysicalOperator::PromqlRelabel { .. } => {
            matches!(statistics, OperatorStatistics::PromqlRelabel { .. })
        }
        PhysicalOperator::PromqlInfoEnrich { .. } => {
            matches!(statistics, OperatorStatistics::PromqlInfoEnrich { .. })
        }
        PhysicalOperator::PromqlSeriesSample { .. } => {
            matches!(statistics, OperatorStatistics::PromqlSeriesSample { .. })
        }
        PhysicalOperator::PromqlScalarToVector => {
            matches!(statistics, OperatorStatistics::PromqlScalarToVector { .. })
        }
        PhysicalOperator::PromqlVectorToScalar => {
            matches!(statistics, OperatorStatistics::PromqlVectorToScalar { .. })
        }
        PhysicalOperator::PromqlScalarLeaf => {
            matches!(statistics, OperatorStatistics::PromqlScalarLeaf { .. })
        }
        PhysicalOperator::PromqlPerSeries { .. } => {
            matches!(statistics, OperatorStatistics::PromqlPerSeries { .. })
        }
        PhysicalOperator::PromqlPresence { .. } => {
            matches!(statistics, OperatorStatistics::PromqlPresence { .. })
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
        | PhysicalOperator::PassThrough
        | PhysicalOperator::PromqlRange { .. }
        | PhysicalOperator::PromqlSubquery { .. }
        | PhysicalOperator::PromqlRelabel { .. }
        | PhysicalOperator::PromqlSeriesSample { .. }
        | PhysicalOperator::PromqlScalarToVector
        | PhysicalOperator::PromqlVectorToScalar
        | PhysicalOperator::PromqlPerSeries { .. }
        | PhysicalOperator::PromqlPresence { .. } => unary,
        PhysicalOperator::HashJoin { .. }
        | PhysicalOperator::PromqlBinary { .. }
        | PhysicalOperator::PromqlInfoEnrich { .. } => OperatorInputArity {
            statistics_inputs: 2,
            dag_children: 2,
        },
        PhysicalOperator::PromqlScalarLeaf => OperatorInputArity {
            statistics_inputs: 0,
            dag_children: 0,
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
    let output = statistics.output();
    if let (
        PhysicalOperator::PromqlScalarLeaf,
        OperatorStatistics::PromqlScalarLeaf { promql_output, .. },
    ) = (operator, &statistics)
    {
        validate_promql_edge(*promql_output)?;
        if promql_output.value_kind != PromqlValueKind::Scalar
            || output.rows != promql_output.evaluation_steps
        {
            return Err(AnalyticalCostError::InconsistentOperatorStatistics(
                "PromQL scalar leaf must emit one scalar row per evaluation step",
            ));
        }
        return Ok(ResourceEstimate {
            cpu_ops: output.rows as f64,
            peak_memory_bytes: per_row_width(output.rows, output.bytes)?,
            scan_bytes: 0,
        });
    }
    let left = input(0)?;
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
        (
            PhysicalOperator::PromqlRange { .. },
            OperatorStatistics::PromqlRange {
                edges,
                max_window_samples_per_series,
            },
        ) => {
            let promql = require_promql_unary(edges)?;
            ResourceEstimate {
                cpu_ops: left.rows as f64,
                peak_memory_bytes: checked_bytes(&[
                    promql.input.series,
                    *max_window_samples_per_series,
                    per_row_width(left.rows, left.bytes)?,
                ])?,
                scan_bytes: 0,
            }
        }
        (
            PhysicalOperator::PromqlSubquery { .. },
            OperatorStatistics::PromqlSubquery { edges, .. },
        ) => ResourceEstimate {
            cpu_ops: left.rows as f64 + output.rows as f64,
            peak_memory_bytes: edges.input.bytes,
            scan_bytes: 0,
        },
        (
            PhysicalOperator::PromqlBinary {
                operand_mode,
                build_side,
                ..
            },
            OperatorStatistics::PromqlBinary {
                edges,
                matching_key_bytes,
            },
        ) => {
            let promql = require_promql_binary(edges)?;
            let right = input(1)?;
            let matching_bytes = match operand_mode {
                PromqlBinaryOperandMode::VectorScalar | PromqlBinaryOperandMode::ScalarVector => {
                    per_row_width(output.rows, output.bytes)?
                }
                PromqlBinaryOperandMode::VectorVector => {
                    let build_series = match build_side.ok_or(
                        AnalyticalCostError::MissingOrStale("vector_match_build_side"),
                    )? {
                        HashJoinBuildSide::Left => promql.inputs[0].series,
                        HashJoinBuildSide::Right => promql.inputs[1].series,
                    };
                    checked_bytes(&[
                        build_series,
                        matching_key_bytes
                            .checked_add(16)
                            .ok_or(AnalyticalCostError::Overflow)?,
                    ])?
                }
            };
            ResourceEstimate {
                cpu_ops: left.rows as f64 + right.rows as f64 + output.rows as f64,
                peak_memory_bytes: matching_bytes,
                scan_bytes: 0,
            }
        }
        (
            PhysicalOperator::PromqlRelabel {
                expression_operations_per_row,
            },
            OperatorStatistics::PromqlRelabel { .. },
        ) => ResourceEstimate {
            cpu_ops: checked_cpu_product(left.rows, expression_operations_per_row)?,
            peak_memory_bytes: per_row_width(output.rows, output.bytes)?,
            scan_bytes: 0,
        },
        (
            PhysicalOperator::PromqlInfoEnrich {
                matcher_operations_per_info_row,
            },
            OperatorStatistics::PromqlInfoEnrich {
                edges,
                matching_key_bytes,
            },
        ) => {
            let promql = require_promql_binary(edges)?;
            let right = input(1)?;
            ResourceEstimate {
                cpu_ops: left.rows as f64
                    + right.rows as f64
                    + output.rows as f64
                    + checked_cpu_product(right.rows, matcher_operations_per_info_row)?,
                peak_memory_bytes: checked_bytes(&[
                    promql.inputs[1].series,
                    matching_key_bytes
                        .checked_add(16)
                        .ok_or(AnalyticalCostError::Overflow)?,
                ])?,
                scan_bytes: 0,
            }
        }
        (
            PhysicalOperator::PromqlSeriesSample { .. },
            OperatorStatistics::PromqlSeriesSample {
                edges, key_bytes, ..
            },
        ) => {
            let promql = require_promql_unary(edges)?;
            ResourceEstimate {
                cpu_ops: left.rows as f64 + promql.input.series as f64,
                peak_memory_bytes: checked_bytes(&[
                    promql.output.series,
                    key_bytes
                        .checked_add(16)
                        .ok_or(AnalyticalCostError::Overflow)?,
                ])?,
                scan_bytes: 0,
            }
        }
        (PhysicalOperator::PromqlScalarToVector | PhysicalOperator::PromqlVectorToScalar, _) => {
            ResourceEstimate {
                cpu_ops: left.rows as f64 + output.rows as f64,
                peak_memory_bytes: per_row_width(output.rows, output.bytes)?,
                scan_bytes: 0,
            }
        }
        (
            PhysicalOperator::PromqlPerSeries {
                operations_per_row, ..
            },
            OperatorStatistics::PromqlPerSeries {
                edges,
                accumulator_bytes_per_series,
            },
        ) => {
            let promql = require_promql_unary(edges)?;
            ResourceEstimate {
                cpu_ops: checked_cpu_product(left.rows, operations_per_row)?,
                peak_memory_bytes: checked_bytes(&[
                    promql.input.series,
                    *accumulator_bytes_per_series,
                ])?,
                scan_bytes: 0,
            }
        }
        (
            PhysicalOperator::PromqlPresence {
                operations_per_row, ..
            },
            OperatorStatistics::PromqlPresence { .. },
        ) => ResourceEstimate {
            cpu_ops: checked_cpu_product(left.rows, operations_per_row)? + output.rows as f64,
            peak_memory_bytes: per_row_width(output.rows, output.bytes)?,
            scan_bytes: 0,
        },
        (PhysicalOperator::PromqlScalarLeaf, _) => unreachable!(),
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
    if let (
        PhysicalOperator::PromqlScalarLeaf,
        OperatorStatistics::PromqlScalarLeaf {
            output,
            promql_output,
        },
    ) = (operator, statistics)
    {
        validate_promql_edge_shape(*output, *promql_output)?;
        return if promql_output.value_kind == PromqlValueKind::Scalar {
            Ok(())
        } else {
            inconsistent("PromQL scalar leaf output is not scalar")
        };
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
                edges,
                source_read_bytes,
            },
        ) => {
            if input != output {
                return inconsistent("Scan input and output edges differ");
            }
            if input.rows > 0 && *source_read_bytes == 0 {
                return inconsistent("non-empty Scan has zero source-read bytes");
            }
            if let Some(promql) = edges.promql {
                validate_promql_edge_shape(edges.input, promql.input)?;
                validate_promql_edge_shape(edges.output, promql.output)?;
                if promql.input != promql.output {
                    return inconsistent("Scan changes its PromQL edge shape");
                }
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
            if let Some(promql) = statistics.unary_promql() {
                validate_promql_filter_shape(promql)?;
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
            if let Some(promql) = statistics.unary_promql() {
                validate_promql_cardinality_preserving_shape(promql)?;
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
            if statistics.promql_output().is_some() {
                return inconsistent(
                    "PromQL cross-series aggregation needs an explicit physical operator",
                );
            }
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
            if let Some(promql) = statistics.unary_promql() {
                validate_promql_cardinality_preserving_shape(promql)?;
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
            if let OperatorStatistics::Concat {
                promql: Some(promql),
                ..
            } = statistics
            {
                if promql.inputs.len() != inputs.len() || promql.inputs.is_empty() {
                    return inconsistent("PromQL Concat edge arity is invalid");
                }
                let first = promql.inputs[0];
                let series_bound = promql.inputs.iter().try_fold(0_u64, |total, edge| {
                    if edge.evaluation_steps != first.evaluation_steps
                        || edge.value_kind != first.value_kind
                    {
                        return Err(AnalyticalCostError::InconsistentOperatorStatistics(
                            "PromQL Concat inputs have different shapes",
                        ));
                    }
                    total
                        .checked_add(edge.series)
                        .ok_or(AnalyticalCostError::Overflow)
                })?;
                if promql.output.evaluation_steps != first.evaluation_steps
                    || promql.output.value_kind != first.value_kind
                    || promql.output.series > series_bound
                {
                    return inconsistent("PromQL Concat output exceeds its input-series bound");
                }
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
        (
            PhysicalOperator::PromqlRange { range_millis },
            OperatorStatistics::PromqlRange {
                edges,
                max_window_samples_per_series,
            },
        ) => {
            let promql = require_promql_unary(edges)?;
            if range_millis == 0 || *max_window_samples_per_series == 0 {
                return inconsistent("PromQL range needs a positive range and window bound");
            }
            if promql.input.value_kind != PromqlValueKind::Vector
                || promql.output.value_kind != PromqlValueKind::RangeVector
                || promql.input.series != promql.output.series
                || promql.input.evaluation_steps != promql.output.evaluation_steps
            {
                return inconsistent("PromQL range edge shape is invalid");
            }
        }
        (
            PhysicalOperator::PromqlSubquery {
                range_millis,
                resolution_millis,
            },
            OperatorStatistics::PromqlSubquery {
                edges,
                subquery_steps,
            },
        ) => {
            let promql = require_promql_unary(edges)?;
            if range_millis == 0
                || resolution_millis == Some(0)
                || *subquery_steps == 0
                || promql.input.value_kind != PromqlValueKind::RangeVector
                || promql.output.value_kind != PromqlValueKind::RangeVector
                || promql.input.series != promql.output.series
            {
                return inconsistent("PromQL subquery configuration or edge shape is invalid");
            }
            let expected = promql
                .output
                .evaluation_steps
                .checked_mul(*subquery_steps)
                .ok_or(AnalyticalCostError::Overflow)?;
            if promql.input.evaluation_steps != expected {
                return inconsistent(
                    "subquery child steps do not equal output steps times subquery steps",
                );
            }
        }
        (
            PhysicalOperator::PromqlBinary {
                operation,
                operand_mode,
                cardinality,
                build_side,
            },
            OperatorStatistics::PromqlBinary {
                edges,
                matching_key_bytes,
            },
        ) => validate_promql_binary(
            operation,
            operand_mode,
            cardinality,
            build_side,
            *matching_key_bytes,
            edges,
        )?,
        (
            PhysicalOperator::PromqlRelabel {
                expression_operations_per_row,
            },
            OperatorStatistics::PromqlRelabel { edges },
        ) => {
            let promql = require_promql_unary(edges)?;
            validate_instant_vector_rows(output, promql.output)?;
            if expression_operations_per_row == 0
                || input.rows != output.rows
                || promql.input.series != promql.output.series
                || promql.input.evaluation_steps != promql.output.evaluation_steps
                || promql.input.value_kind != PromqlValueKind::Vector
                || promql.output.value_kind != PromqlValueKind::Vector
            {
                return inconsistent("PromQL relabel configuration or edge shape is invalid");
            }
        }
        (
            PhysicalOperator::PromqlInfoEnrich {
                matcher_operations_per_info_row,
            },
            OperatorStatistics::PromqlInfoEnrich {
                edges,
                matching_key_bytes,
            },
        ) => {
            let promql = require_promql_binary(edges)?;
            validate_instant_vector_rows(output, promql.output)?;
            if matcher_operations_per_info_row == 0
                || *matching_key_bytes == 0
                || input.rows != output.rows
                || promql.inputs[0].series != promql.output.series
                || promql.inputs[0].evaluation_steps != promql.output.evaluation_steps
                || promql.inputs[1].evaluation_steps != promql.output.evaluation_steps
                || promql
                    .inputs
                    .iter()
                    .any(|edge| edge.value_kind != PromqlValueKind::Vector)
                || promql.output.value_kind != PromqlValueKind::Vector
            {
                return inconsistent(
                    "PromQL info enrichment configuration or edge shape is invalid",
                );
            }
        }
        (
            PhysicalOperator::PromqlSeriesSample {
                kind,
                grouping_key_count,
            },
            OperatorStatistics::PromqlSeriesSample {
                edges,
                group_count,
                key_bytes,
            },
        ) => validate_promql_series_sample(
            kind,
            grouping_key_count,
            *group_count,
            *key_bytes,
            edges,
        )?,
        (
            PhysicalOperator::PromqlScalarToVector,
            OperatorStatistics::PromqlScalarToVector { edges },
        )
        | (
            PhysicalOperator::PromqlVectorToScalar,
            OperatorStatistics::PromqlVectorToScalar { edges },
        ) => {
            validate_promql_bridge(operator, edges)?;
        }
        (PhysicalOperator::PromqlScalarLeaf, _) => unreachable!(),
        (
            PhysicalOperator::PromqlPerSeries {
                operations_per_row,
                accumulator_count,
            },
            OperatorStatistics::PromqlPerSeries {
                edges,
                accumulator_bytes_per_series,
            },
        ) => {
            let promql = require_promql_unary(edges)?;
            validate_instant_vector_rows(output, promql.output)?;
            if operations_per_row == 0
                || accumulator_count == 0
                || *accumulator_bytes_per_series == 0
                || promql.output.value_kind != PromqlValueKind::Vector
                || promql.output.series > promql.input.series
                || promql.output.evaluation_steps != promql.input.evaluation_steps
            {
                return inconsistent("PromQL per-series configuration or edge shape is invalid");
            }
        }
        (
            PhysicalOperator::PromqlPresence {
                kind,
                operations_per_row,
            },
            OperatorStatistics::PromqlPresence { edges },
        ) => {
            let promql = require_promql_unary(edges)?;
            validate_instant_vector_rows(output, promql.output)?;
            if operations_per_row == 0 || promql.output.value_kind != PromqlValueKind::Vector {
                return inconsistent("PromQL presence output violates its per-step bound");
            }
            match kind {
                PromqlPresenceKind::Absent
                    if promql.output.series > 1
                        || output.rows > promql.output.evaluation_steps
                        || (input.rows == 0 && output.rows != promql.output.evaluation_steps)
                        || (output.rows == 0 && promql.output.series != 0)
                        || (output.rows > 0 && promql.output.series != 1) =>
                {
                    return inconsistent("PromQL absence output violates its per-step bound");
                }
                PromqlPresenceKind::PresentPerSeries
                    if output.rows > input.rows
                        || promql.output.series > promql.input.series
                        || promql.output.evaluation_steps != promql.input.evaluation_steps
                        || (input.rows == 0 && (output.rows != 0 || promql.output.series != 0)) =>
                {
                    return inconsistent(
                        "PromQL present-over-time output exceeds its per-series bound",
                    );
                }
                _ => {}
            }
        }
        _ => return inconsistent("statistics variant does not match physical operator"),
    }
    Ok(())
}

fn require_promql_unary(
    edges: &crate::analytical_statistics::UnaryEdgeStatistics,
) -> Result<crate::analytical_statistics::PromqlUnaryEdgeStatistics, AnalyticalCostError> {
    let promql = edges.promql.ok_or(AnalyticalCostError::MissingOrStale(
        "promql_edge_statistics",
    ))?;
    validate_promql_edge_shape(edges.input, promql.input)?;
    validate_promql_edge_shape(edges.output, promql.output)?;
    Ok(promql)
}

fn require_promql_binary(
    edges: &crate::analytical_statistics::BinaryEdgeStatistics,
) -> Result<crate::analytical_statistics::PromqlBinaryEdgeStatistics, AnalyticalCostError> {
    let promql = edges.promql.ok_or(AnalyticalCostError::MissingOrStale(
        "promql_edge_statistics",
    ))?;
    validate_promql_edge_shape(edges.inputs[0], promql.inputs[0])?;
    validate_promql_edge_shape(edges.inputs[1], promql.inputs[1])?;
    validate_promql_edge_shape(edges.output, promql.output)?;
    Ok(promql)
}

fn validate_promql_cardinality_preserving_shape(
    promql: crate::analytical_statistics::PromqlUnaryEdgeStatistics,
) -> Result<(), AnalyticalCostError> {
    validate_promql_edge(promql.input)?;
    validate_promql_edge(promql.output)?;
    if promql.input == promql.output {
        Ok(())
    } else {
        Err(AnalyticalCostError::InconsistentOperatorStatistics(
            "cardinality-preserving operator changes its PromQL edge shape",
        ))
    }
}

fn validate_promql_filter_shape(
    promql: crate::analytical_statistics::PromqlUnaryEdgeStatistics,
) -> Result<(), AnalyticalCostError> {
    validate_promql_edge(promql.input)?;
    validate_promql_edge(promql.output)?;
    if promql.input.evaluation_steps == promql.output.evaluation_steps
        && promql.input.value_kind == promql.output.value_kind
        && promql.output.series <= promql.input.series
    {
        Ok(())
    } else {
        Err(AnalyticalCostError::InconsistentOperatorStatistics(
            "Filter changes PromQL steps/kind or expands its series",
        ))
    }
}

fn validate_instant_vector_rows(
    logical: EdgeStatistics,
    promql: PromqlEdgeStatistics,
) -> Result<(), AnalyticalCostError> {
    if promql.value_kind != PromqlValueKind::Vector {
        return Err(AnalyticalCostError::InconsistentOperatorStatistics(
            "instant-vector result has a non-vector value kind",
        ));
    }
    let bound = promql
        .series
        .checked_mul(promql.evaluation_steps)
        .ok_or(AnalyticalCostError::Overflow)?;
    if logical.rows <= bound {
        Ok(())
    } else {
        Err(AnalyticalCostError::InconsistentOperatorStatistics(
            "instant-vector rows exceed series times evaluation steps",
        ))
    }
}

fn validate_promql_edge_shape(
    logical: EdgeStatistics,
    promql: PromqlEdgeStatistics,
) -> Result<(), AnalyticalCostError> {
    validate_promql_edge(promql)?;
    let max_rows = promql.series.checked_mul(promql.evaluation_steps);
    match promql.value_kind {
        PromqlValueKind::Scalar if logical.rows != promql.evaluation_steps => {
            Err(AnalyticalCostError::InconsistentOperatorStatistics(
                "PromQL scalar rows do not equal evaluation steps",
            ))
        }
        PromqlValueKind::RangeVector if max_rows.is_none_or(|bound| logical.rows > bound) => {
            Err(AnalyticalCostError::InconsistentOperatorStatistics(
                "PromQL vector rows exceed series times evaluation steps",
            ))
        }
        PromqlValueKind::Vector if logical.rows > 0 && promql.series == 0 => {
            Err(AnalyticalCostError::InconsistentOperatorStatistics(
                "PromQL samples have no source series",
            ))
        }
        _ => Ok(()),
    }
}

fn validate_promql_bridge(
    operator: PhysicalOperator,
    edges: &crate::analytical_statistics::UnaryEdgeStatistics,
) -> Result<(), AnalyticalCostError> {
    let promql = require_promql_unary(edges)?;
    let valid = match operator {
        PhysicalOperator::PromqlScalarToVector => {
            validate_instant_vector_rows(edges.output, promql.output)?;
            promql.input.value_kind == PromqlValueKind::Scalar
                && promql.output.value_kind == PromqlValueKind::Vector
                && promql.output.series == 1
                && promql.input.evaluation_steps == promql.output.evaluation_steps
                && edges.input.rows == edges.output.rows
        }
        PhysicalOperator::PromqlVectorToScalar => {
            promql.input.value_kind == PromqlValueKind::Vector
                && promql.output.value_kind == PromqlValueKind::Scalar
                && promql.input.evaluation_steps == promql.output.evaluation_steps
        }
        _ => false,
    };
    if valid {
        Ok(())
    } else {
        Err(AnalyticalCostError::InconsistentOperatorStatistics(
            "PromQL scalar/vector bridge edge shape is invalid",
        ))
    }
}

fn validate_promql_binary(
    operation: PromqlBinaryOperation,
    operand_mode: PromqlBinaryOperandMode,
    cardinality: PromqlVectorCardinality,
    build_side: Option<HashJoinBuildSide>,
    matching_key_bytes: u64,
    edges: &crate::analytical_statistics::BinaryEdgeStatistics,
) -> Result<(), AnalyticalCostError> {
    let promql = require_promql_binary(edges)?;
    validate_instant_vector_rows(edges.output, promql.output)?;
    let invalid = |reason| Err(AnalyticalCostError::InconsistentOperatorStatistics(reason));
    if matches!(
        operation,
        PromqlBinaryOperation::And | PromqlBinaryOperation::Or | PromqlBinaryOperation::Unless
    ) && cardinality != PromqlVectorCardinality::OneToOne
    {
        return invalid("PromQL set operations cannot use group_left or group_right");
    }
    if promql.output.value_kind != PromqlValueKind::Vector
        || promql.inputs[0].evaluation_steps != promql.output.evaluation_steps
        || promql.inputs[1].evaluation_steps != promql.output.evaluation_steps
    {
        return invalid("PromQL binary evaluation steps or output kind are invalid");
    }
    let (row_bound, series_bound) = match operand_mode {
        PromqlBinaryOperandMode::VectorScalar => {
            if promql.inputs[0].value_kind != PromqlValueKind::Vector
                || promql.inputs[1].value_kind != PromqlValueKind::Scalar
                || cardinality != PromqlVectorCardinality::OneToOne
                || build_side.is_some()
                || matching_key_bytes != 0
            {
                return invalid("vector/scalar binary configuration is invalid");
            }
            (edges.inputs[0].rows, promql.inputs[0].series)
        }
        PromqlBinaryOperandMode::ScalarVector => {
            if promql.inputs[0].value_kind != PromqlValueKind::Scalar
                || promql.inputs[1].value_kind != PromqlValueKind::Vector
                || cardinality != PromqlVectorCardinality::OneToOne
                || build_side.is_some()
                || matching_key_bytes != 0
            {
                return invalid("scalar/vector binary configuration is invalid");
            }
            (edges.inputs[1].rows, promql.inputs[1].series)
        }
        PromqlBinaryOperandMode::VectorVector => {
            if promql
                .inputs
                .iter()
                .any(|input| input.value_kind != PromqlValueKind::Vector)
                || build_side.is_none()
                || matching_key_bytes == 0
            {
                return invalid("vector/vector binary label-match configuration is invalid");
            }
            match operation {
                PromqlBinaryOperation::And | PromqlBinaryOperation::Unless => {
                    (edges.inputs[0].rows, promql.inputs[0].series)
                }
                PromqlBinaryOperation::Or => (
                    edges.inputs[0]
                        .rows
                        .checked_add(edges.inputs[1].rows)
                        .ok_or(AnalyticalCostError::Overflow)?,
                    promql.inputs[0]
                        .series
                        .checked_add(promql.inputs[1].series)
                        .ok_or(AnalyticalCostError::Overflow)?,
                ),
                PromqlBinaryOperation::ArithmeticOrComparison => match cardinality {
                    PromqlVectorCardinality::OneToOne => (
                        edges.inputs[0].rows.min(edges.inputs[1].rows),
                        promql.inputs[0].series.min(promql.inputs[1].series),
                    ),
                    PromqlVectorCardinality::ManyToOne => {
                        (edges.inputs[0].rows, promql.inputs[0].series)
                    }
                    PromqlVectorCardinality::OneToMany => {
                        (edges.inputs[1].rows, promql.inputs[1].series)
                    }
                },
            }
        }
    };
    if edges.output.rows > row_bound || promql.output.series > series_bound {
        return invalid("PromQL binary output exceeds its semantic cardinality bound");
    }
    Ok(())
}

fn validate_promql_series_sample(
    kind: PromqlSeriesSampleKind,
    grouping_key_count: u64,
    group_count: u64,
    key_bytes: u64,
    edges: &crate::analytical_statistics::UnaryEdgeStatistics,
) -> Result<(), AnalyticalCostError> {
    let promql = require_promql_unary(edges)?;
    validate_instant_vector_rows(edges.output, promql.output)?;
    let invalid = |reason| Err(AnalyticalCostError::InconsistentOperatorStatistics(reason));
    if key_bytes == 0
        || promql.input.value_kind != PromqlValueKind::Vector
        || promql.output.value_kind != PromqlValueKind::Vector
        || promql.input.evaluation_steps != promql.output.evaluation_steps
        || edges.output.rows > edges.input.rows
        || promql.output.series > promql.input.series
        || (grouping_key_count == 0 && group_count != 1)
        || (grouping_key_count > 0 && group_count == 0 && promql.input.series > 0)
    {
        return invalid("PromQL series-sample configuration or edge shape is invalid");
    }
    if let PromqlSeriesSampleKind::LimitK { k } = kind {
        if k == 0 {
            return invalid("PromQL limitk must be positive");
        }
        let bound = group_count
            .checked_mul(k)
            .ok_or(AnalyticalCostError::Overflow)?;
        if promql.output.series > promql.input.series.min(bound) {
            return invalid("PromQL limitk output exceeds its group bound");
        }
    } else if let PromqlSeriesSampleKind::LimitRatio { ratio_bits } = kind {
        let ratio = f64::from_bits(ratio_bits);
        if !ratio.is_finite() || !(-1.0..=1.0).contains(&ratio) {
            return invalid("PromQL limit_ratio is outside [-1, 1]");
        }
    }
    Ok(())
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
        OperatorStatistics, PartitionStatistics, PromqlBinaryEdgeStatistics, PromqlEdgeStatistics,
        PromqlUnaryEdgeStatistics, PromqlValueKind, SourceCoverage, UnaryEdgeStatistics,
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
            promql: None,
        }
    }

    fn promql_edge(
        series: u64,
        evaluation_steps: u64,
        value_kind: PromqlValueKind,
    ) -> PromqlEdgeStatistics {
        PromqlEdgeStatistics {
            series,
            evaluation_steps,
            value_kind,
        }
    }

    fn promql_binary_statistics(output_rows: u64, output_series: u64) -> OperatorStatistics {
        let left = EdgeStatistics {
            rows: 100,
            bytes: 1_600,
        };
        let right = EdgeStatistics {
            rows: 50,
            bytes: 800,
        };
        let output = EdgeStatistics {
            rows: output_rows,
            bytes: output_rows.saturating_mul(16),
        };
        OperatorStatistics::PromqlBinary {
            edges: BinaryEdgeStatistics {
                inputs: [left, right],
                output,
                promql: Some(PromqlBinaryEdgeStatistics {
                    inputs: [
                        promql_edge(10, 10, PromqlValueKind::Vector),
                        promql_edge(5, 10, PromqlValueKind::Vector),
                    ],
                    output: promql_edge(output_series, 10, PromqlValueKind::Vector),
                }),
            },
            matching_key_bytes: 8,
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
    fn promql_binary_operation_controls_cardinality_bounds() {
        let operator = |operation| PhysicalOperator::PromqlBinary {
            operation,
            operand_mode: PromqlBinaryOperandMode::VectorVector,
            cardinality: PromqlVectorCardinality::OneToOne,
            build_side: Some(HashJoinBuildSide::Right),
        };

        assert!(estimate_operator(
            operator(PromqlBinaryOperation::Or),
            promql_binary_statistics(150, 15),
        )
        .is_ok());
        assert!(matches!(
            estimate_operator(
                operator(PromqlBinaryOperation::ArithmeticOrComparison),
                promql_binary_statistics(60, 5),
            ),
            Err(AnalyticalCostError::InconsistentOperatorStatistics(_))
        ));
        assert!(matches!(
            estimate_operator(
                operator(PromqlBinaryOperation::And),
                promql_binary_statistics(101, 10),
            ),
            Err(AnalyticalCostError::InconsistentOperatorStatistics(_))
        ));
    }

    #[test]
    fn promql_bridges_require_directional_edge_kinds() {
        let scalar = promql_edge(0, 10, PromqlValueKind::Scalar);
        let vector = promql_edge(1, 10, PromqlValueKind::Vector);
        let edges = |input, output| UnaryEdgeStatistics {
            input: EdgeStatistics {
                rows: 10,
                bytes: 80,
            },
            output: EdgeStatistics {
                rows: 10,
                bytes: 80,
            },
            promql: Some(PromqlUnaryEdgeStatistics { input, output }),
        };
        assert!(estimate_operator(
            PhysicalOperator::PromqlScalarToVector,
            OperatorStatistics::PromqlScalarToVector {
                edges: edges(scalar, vector),
            },
        )
        .is_ok());
        assert!(matches!(
            estimate_operator(
                PhysicalOperator::PromqlScalarToVector,
                OperatorStatistics::PromqlScalarToVector {
                    edges: edges(vector, scalar),
                },
            ),
            Err(AnalyticalCostError::InconsistentOperatorStatistics(_))
        ));
    }

    #[test]
    fn promql_scalar_leaf_has_no_input_and_one_row_per_step() {
        let statistics = OperatorStatistics::PromqlScalarLeaf {
            output: EdgeStatistics {
                rows: 10,
                bytes: 80,
            },
            promql_output: promql_edge(0, 10, PromqlValueKind::Scalar),
        };
        let estimate = estimate_operator(PhysicalOperator::PromqlScalarLeaf, statistics).unwrap();

        assert_eq!(estimate.cpu_ops, 10.0);
        assert_eq!(estimate.peak_memory_bytes, 8);
    }

    #[test]
    fn physical_dag_cannot_drop_promql_edge_evidence() {
        let edge = EdgeStatistics {
            rows: 10,
            bytes: 80,
        };
        let promql = promql_edge(1, 10, PromqlValueKind::Vector);
        let nodes = vec![
            PhysicalDagNode {
                id: "scan".into(),
                operator: PhysicalOperator::Scan,
                children: vec![],
                source_coverage: Some(comparison_scope().sources[0].clone()),
                output_buffer_bytes: 8,
                retained_bytes: 0,
                execution: ExecutionMultiplicity::PerEvaluation,
            },
            PhysicalDagNode {
                id: "filter".into(),
                operator: filter_operator(),
                children: vec!["scan".into()],
                source_coverage: None,
                output_buffer_bytes: 8,
                retained_bytes: 0,
                execution: ExecutionMultiplicity::PerEvaluation,
            },
        ];
        let provided = HashMap::from([
            (
                "scan".into(),
                OperatorStatistics::Scan {
                    edges: UnaryEdgeStatistics {
                        input: edge,
                        output: edge,
                        promql: Some(PromqlUnaryEdgeStatistics {
                            input: promql,
                            output: promql,
                        }),
                    },
                    source_read_bytes: 80,
                },
            ),
            (
                "filter".into(),
                OperatorStatistics::Filter {
                    edges: unary_edges(edge, edge),
                },
            ),
        ]);

        assert!(matches!(
            estimate_physical_dag(&nodes, "filter", &comparison_scope(), &provided),
            Err(AnalyticalCostError::InvalidOperatorStatistics {
                reason: "operator drops child PromQL edge statistics",
                ..
            })
        ));
    }

    #[test]
    fn generic_hash_aggregate_cannot_masquerade_as_promql_aggregation() {
        let input = EdgeStatistics {
            rows: 10,
            bytes: 80,
        };
        let output = EdgeStatistics { rows: 1, bytes: 8 };
        let statistics = OperatorStatistics::HashAggregate {
            edges: UnaryEdgeStatistics {
                input,
                output,
                promql: Some(PromqlUnaryEdgeStatistics {
                    input: promql_edge(1, 10, PromqlValueKind::Vector),
                    output: promql_edge(1, 10, PromqlValueKind::Vector),
                }),
            },
            group_count: 1,
            key_bytes: 0,
            accumulator_bytes_per_group: 8,
        };

        assert_eq!(
            estimate_operator(
                PhysicalOperator::HashAggregate {
                    grouping_key_count: 0,
                    accumulator_count: 1,
                },
                statistics,
            ),
            Err(AnalyticalCostError::InconsistentOperatorStatistics(
                "PromQL cross-series aggregation needs an explicit physical operator"
            ))
        );
    }

    #[test]
    fn present_over_time_keeps_per_series_cardinality() {
        let input = EdgeStatistics {
            rows: 100,
            bytes: 1_600,
        };
        let output = EdgeStatistics {
            rows: 80,
            bytes: 1_280,
        };
        let valid = OperatorStatistics::PromqlPresence {
            edges: UnaryEdgeStatistics {
                input,
                output,
                promql: Some(PromqlUnaryEdgeStatistics {
                    input: promql_edge(10, 10, PromqlValueKind::RangeVector),
                    output: promql_edge(8, 10, PromqlValueKind::Vector),
                }),
            },
        };

        assert!(estimate_operator(
            PhysicalOperator::PromqlPresence {
                kind: PromqlPresenceKind::PresentPerSeries,
                operations_per_row: 1,
            },
            valid.clone(),
        )
        .is_ok());
        assert!(matches!(
            estimate_operator(
                PhysicalOperator::PromqlPresence {
                    kind: PromqlPresenceKind::Absent,
                    operations_per_row: 1,
                },
                valid,
            ),
            Err(AnalyticalCostError::InconsistentOperatorStatistics(_))
        ));
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
                    promql: None,
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
                    promql: None,
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
                    promql: None,
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
                    promql: None,
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
                    promql: None,
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
                    promql: None,
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
                    promql: None,
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
                info_matchers: vec![],
            }],
        }
    }

    fn unary_edges(input: EdgeStatistics, output: EdgeStatistics) -> UnaryEdgeStatistics {
        UnaryEdgeStatistics {
            input,
            output,
            promql: None,
        }
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
                info_matchers: vec![],
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
            info_matchers: vec![],
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
                    promql: None,
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

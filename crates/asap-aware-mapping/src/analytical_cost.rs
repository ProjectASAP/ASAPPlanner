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
    validate_comparison_scopes, ComparisonScope, OperatorStatistics, OperatorStatisticsProvider,
    SourceCoverage,
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
    Filter,
    Project,
    HashAggregate,
    Sort,
    TopK,
    HashJoin,
    Deduplicate,
    Concat,
    Window,
    Limit,
    PassThrough,
    PromqlRange,
    PromqlSubquery,
    PromqlVectorBinary,
    PromqlRelabel,
    PromqlInfoEnrich,
    PromqlSeriesSample,
    PromqlBridge,
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
    let expected_inputs = match node.operator {
        PhysicalOperator::Scan => 1,
        PhysicalOperator::HashJoin
        | PhysicalOperator::PromqlVectorBinary
        | PhysicalOperator::PromqlInfoEnrich => 2,
        PhysicalOperator::Concat => node.children.len(),
        PhysicalOperator::PromqlScalarLeaf => 0,
        _ => 1,
    };
    if node_statistics.inputs.len() != expected_inputs {
        return Err(AnalyticalCostError::InvalidOperatorStatistics {
            node: node.id.clone(),
            reason: "wrong input-edge count",
        });
    }
    let expected_children = match node.operator {
        PhysicalOperator::Scan | PhysicalOperator::PromqlScalarLeaf => 0,
        _ => expected_inputs,
    };
    if node.children.len() != expected_children {
        return Err(AnalyticalCostError::InvalidPhysicalDag(
            "operator child count does not match physical arity",
        ));
    }
    match node.operator {
        PhysicalOperator::Scan => {}
        _ if node_statistics.source_scan_bytes != 0 => {
            return Err(AnalyticalCostError::InvalidOperatorStatistics {
                node: node.id.clone(),
                reason: "only scan operators may read source bytes",
            });
        }
        _ => {}
    }
    for edge in node_statistics
        .inputs
        .iter()
        .chain(std::iter::once(&node_statistics.output))
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
        if node_statistics.inputs[input_index] != child_statistics.output {
            return Err(AnalyticalCostError::ConflictingEdgeStatistics {
                parent: node.id.clone(),
                child: child.id.clone(),
                input_index,
            });
        }
    }
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

/// Estimate one physical operator. Child costs are deliberately excluded;
/// a DAG walker sums CPU/disk once per node and combines simultaneously
/// retained state separately.
pub fn estimate_operator(
    operator: PhysicalOperator,
    statistics: OperatorStatistics,
) -> Result<ResourceEstimate, AnalyticalCostError> {
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
            .inputs
            .get(index)
            .copied()
            .ok_or(AnalyticalCostError::MissingOrZero("operator input edge"))
    };
    let output = statistics.output;
    if matches!(operator, PhysicalOperator::PromqlScalarLeaf) {
        let promql = require_promql_statistics(&statistics, 0)?;
        if promql.output_series > output.rows && output.rows > 0 {
            return Err(AnalyticalCostError::InconsistentOperatorStatistics(
                "output series exceed output rows",
            ));
        }
        return Ok(ResourceEstimate {
            cpu_ops: output.rows as f64,
            peak_memory_bytes: per_row_width(output.rows, output.bytes)?,
            scan_bytes: 0,
        });
    }
    let left = input(0)?;
    let estimate = match operator {
        PhysicalOperator::Scan => ResourceEstimate {
            cpu_ops: left.rows as f64,
            peak_memory_bytes: per_row_width(left.rows, left.bytes)?,
            scan_bytes: statistics.source_scan_bytes,
        },
        PhysicalOperator::Filter | PhysicalOperator::Project | PhysicalOperator::PassThrough => {
            ResourceEstimate {
                cpu_ops: left.rows as f64,
                peak_memory_bytes: per_row_width(output.rows, output.bytes)?,
                scan_bytes: 0,
            }
        }
        PhysicalOperator::HashAggregate => {
            let groups = statistics
                .group_count
                .ok_or(AnalyticalCostError::MissingOrZero("group_count"))?;
            let key = statistics
                .key_bytes
                .ok_or(AnalyticalCostError::MissingOrZero("key_bytes"))?;
            let value = statistics
                .aggregate_value_bytes
                .ok_or(AnalyticalCostError::MissingOrZero("aggregate_value_bytes"))?;
            ResourceEstimate {
                cpu_ops: left.rows as f64,
                peak_memory_bytes: checked_bytes(&[
                    groups,
                    key.checked_add(value)
                        .and_then(|bytes| bytes.checked_add(16))
                        .ok_or(AnalyticalCostError::Overflow)?,
                ])?,
                scan_bytes: 0,
            }
        }
        PhysicalOperator::Deduplicate => {
            let groups = statistics
                .group_count
                .ok_or(AnalyticalCostError::MissingOrZero("group_count"))?;
            let key = statistics
                .key_bytes
                .ok_or(AnalyticalCostError::MissingOrZero("key_bytes"))?;
            ResourceEstimate {
                cpu_ops: left.rows as f64,
                peak_memory_bytes: checked_bytes(&[
                    groups,
                    key.checked_add(16).ok_or(AnalyticalCostError::Overflow)?,
                ])?,
                scan_bytes: 0,
            }
        }
        PhysicalOperator::Sort | PhysicalOperator::Window => ResourceEstimate {
            cpu_ops: left.rows as f64 * (left.rows.max(2) as f64).log2().ceil(),
            peak_memory_bytes: left.bytes,
            scan_bytes: 0,
        },
        PhysicalOperator::TopK => {
            let k = statistics
                .k
                .ok_or(AnalyticalCostError::MissingOrZero("k"))?;
            let heap_rows = k.min(left.rows);
            ResourceEstimate {
                cpu_ops: left.rows as f64 * (heap_rows.max(2) as f64).log2().ceil(),
                peak_memory_bytes: checked_bytes(&[
                    heap_rows,
                    per_row_width(left.rows, left.bytes)?,
                ])?,
                scan_bytes: 0,
            }
        }
        PhysicalOperator::HashJoin => {
            let right = input(1)?;
            let build_side = statistics
                .hash_join_build_side
                .ok_or(AnalyticalCostError::MissingOrZero("hash_join_build_side"))?;
            ResourceEstimate {
                cpu_ops: left.rows as f64 + right.rows as f64 + output.rows as f64,
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
        PhysicalOperator::Concat => ResourceEstimate {
            cpu_ops: output.rows as f64,
            peak_memory_bytes: per_row_width(output.rows, output.bytes)?,
            scan_bytes: 0,
        },
        PhysicalOperator::Limit => ResourceEstimate {
            cpu_ops: output.rows as f64,
            peak_memory_bytes: per_row_width(output.rows, output.bytes)?,
            scan_bytes: 0,
        },
        PhysicalOperator::PromqlRange => {
            let promql = require_promql_statistics(&statistics, 1)?;
            let samples = promql
                .window_samples_per_series
                .filter(|value| *value > 0)
                .ok_or(AnalyticalCostError::MissingOrZero(
                    "window_samples_per_series",
                ))?;
            ResourceEstimate {
                cpu_ops: left.rows as f64,
                peak_memory_bytes: checked_bytes(&[
                    promql.input_series[0],
                    samples,
                    per_row_width(left.rows, left.bytes)?,
                ])?,
                scan_bytes: 0,
            }
        }
        PhysicalOperator::PromqlSubquery => {
            let promql = require_promql_statistics(&statistics, 1)?;
            if !matches!(promql.subquery_steps, Some(value) if value > 0) {
                return Err(AnalyticalCostError::MissingOrZero("subquery_steps"));
            }
            ResourceEstimate {
                cpu_ops: left.rows as f64 + output.rows as f64,
                peak_memory_bytes: left.bytes,
                scan_bytes: 0,
            }
        }
        PhysicalOperator::PromqlVectorBinary => {
            let promql = require_promql_statistics(&statistics, 2)?;
            let right = input(1)?;
            let scalar_vector = matches!(operator, PhysicalOperator::PromqlVectorBinary)
                && promql.input_series.contains(&0);
            let matching_bytes = if scalar_vector {
                per_row_width(output.rows, output.bytes)?
            } else {
                let build_side = statistics
                    .hash_join_build_side
                    .ok_or(AnalyticalCostError::MissingOrZero("hash_join_build_side"))?;
                let key_bytes = statistics
                    .key_bytes
                    .ok_or(AnalyticalCostError::MissingOrZero("key_bytes"))?;
                let build_series = match build_side {
                    HashJoinBuildSide::Left => promql.input_series[0],
                    HashJoinBuildSide::Right => promql.input_series[1],
                };
                checked_bytes(&[
                    build_series,
                    key_bytes
                        .checked_add(16)
                        .ok_or(AnalyticalCostError::Overflow)?,
                ])?
            };
            ResourceEstimate {
                cpu_ops: left.rows as f64 + right.rows as f64 + output.rows as f64,
                peak_memory_bytes: matching_bytes,
                scan_bytes: 0,
            }
        }
        PhysicalOperator::PromqlInfoEnrich => {
            let promql = require_promql_statistics(&statistics, 2)?;
            let right = input(1)?;
            if statistics.hash_join_build_side != Some(HashJoinBuildSide::Right) {
                return Err(AnalyticalCostError::InconsistentOperatorStatistics(
                    "info enrichment must build its label index from the right side",
                ));
            }
            let key_bytes = statistics
                .key_bytes
                .ok_or(AnalyticalCostError::MissingOrZero("key_bytes"))?;
            let predicate_ops = promql.scalar_ops_per_row.unwrap_or(0);
            ResourceEstimate {
                cpu_ops: left.rows as f64
                    + right.rows as f64
                    + output.rows as f64
                    + right.rows as f64 * predicate_ops as f64,
                peak_memory_bytes: checked_bytes(&[
                    promql.input_series[1],
                    key_bytes
                        .checked_add(16)
                        .ok_or(AnalyticalCostError::Overflow)?,
                ])?,
                scan_bytes: 0,
            }
        }
        PhysicalOperator::PromqlRelabel => {
            let promql = require_promql_statistics(&statistics, 1)?;
            let operations = promql
                .scalar_ops_per_row
                .filter(|value| *value > 0)
                .ok_or(AnalyticalCostError::MissingOrZero("scalar_ops_per_row"))?;
            ResourceEstimate {
                cpu_ops: left.rows as f64 * operations as f64,
                peak_memory_bytes: per_row_width(output.rows, output.bytes)?,
                scan_bytes: 0,
            }
        }
        PhysicalOperator::PromqlSeriesSample => {
            let promql = require_promql_statistics(&statistics, 1)?;
            let key_bytes = statistics
                .key_bytes
                .ok_or(AnalyticalCostError::MissingOrZero("key_bytes"))?;
            ResourceEstimate {
                cpu_ops: left.rows as f64 + promql.input_series[0] as f64,
                peak_memory_bytes: checked_bytes(&[
                    promql.output_series,
                    key_bytes
                        .checked_add(16)
                        .ok_or(AnalyticalCostError::Overflow)?,
                ])?,
                scan_bytes: 0,
            }
        }
        PhysicalOperator::PromqlBridge => {
            require_promql_statistics(&statistics, 1)?;
            ResourceEstimate {
                cpu_ops: left.rows as f64 + output.rows as f64,
                peak_memory_bytes: per_row_width(output.rows, output.bytes)?,
                scan_bytes: 0,
            }
        }
        PhysicalOperator::PromqlPerSeries => {
            let promql = require_promql_statistics(&statistics, 1)?;
            let operations = promql
                .scalar_ops_per_row
                .filter(|value| *value > 0)
                .ok_or(AnalyticalCostError::MissingOrZero("scalar_ops_per_row"))?;
            let accumulator = statistics
                .aggregate_value_bytes
                .ok_or(AnalyticalCostError::MissingOrZero("aggregate_value_bytes"))?;
            ResourceEstimate {
                cpu_ops: left.rows as f64 * operations as f64,
                peak_memory_bytes: checked_bytes(&[promql.input_series[0], accumulator])?,
                scan_bytes: 0,
            }
        }
        PhysicalOperator::PromqlPresence => {
            let promql = require_promql_statistics(&statistics, 1)?;
            let operations = promql
                .scalar_ops_per_row
                .filter(|value| *value > 0)
                .ok_or(AnalyticalCostError::MissingOrZero("scalar_ops_per_row"))?;
            ResourceEstimate {
                cpu_ops: left.rows as f64 * operations as f64 + output.rows as f64,
                peak_memory_bytes: per_row_width(output.rows, output.bytes)?,
                scan_bytes: 0,
            }
        }
        PhysicalOperator::PromqlScalarLeaf => unreachable!(),
    };
    if estimate.cpu_ops.is_finite() {
        Ok(estimate)
    } else {
        Err(AnalyticalCostError::Overflow)
    }
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
        validate_comparison_scopes, ComparisonScope, EdgeStatistics, OperatorStatistics,
        SourceCoverage,
    };

    #[test]
    fn physical_operator_formulas_keep_disk_at_scan_and_require_join_stats() {
        let scan = estimate_operator(
            PhysicalOperator::Scan,
            OperatorStatistics {
                source_scan_bytes: 64_000,
                ..statistics(
                    vec![EdgeStatistics {
                        rows: 1_000,
                        bytes: 64_000,
                    }],
                    EdgeStatistics {
                        rows: 1_000,
                        bytes: 64_000,
                    },
                )
            },
        )
        .unwrap();
        assert_eq!(scan.scan_bytes, 64_000);

        let topk = estimate_operator(
            PhysicalOperator::TopK,
            OperatorStatistics {
                k: Some(10),
                ..statistics(
                    vec![EdgeStatistics {
                        rows: 1_000,
                        bytes: 40_000,
                    }],
                    EdgeStatistics {
                        rows: 10,
                        bytes: 400,
                    },
                )
            },
        )
        .unwrap();
        assert_eq!(topk.scan_bytes, 0);
        assert_eq!(topk.cpu_ops, 4_000.0);
        assert_eq!(topk.peak_memory_bytes, 400);

        let missing_join_stats = estimate_operator(
            PhysicalOperator::HashJoin,
            statistics(
                vec![EdgeStatistics {
                    rows: 1_000,
                    bytes: 64_000,
                }],
                EdgeStatistics {
                    rows: 100,
                    bytes: 12_800,
                },
            ),
        );
        assert_eq!(
            missing_join_stats,
            Err(AnalyticalCostError::MissingOrZero("operator input edge"))
        );

        let missing_build_side = estimate_operator(
            PhysicalOperator::HashJoin,
            statistics(
                vec![
                    EdgeStatistics {
                        rows: 1_000,
                        bytes: 64_000,
                    },
                    EdgeStatistics {
                        rows: 10,
                        bytes: 1_280,
                    },
                ],
                EdgeStatistics {
                    rows: 100,
                    bytes: 12_800,
                },
            ),
        );
        assert_eq!(
            missing_build_side,
            Err(AnalyticalCostError::MissingOrZero("hash_join_build_side"))
        );

        let build_left = estimate_operator(
            PhysicalOperator::HashJoin,
            OperatorStatistics {
                hash_join_build_side: Some(HashJoinBuildSide::Left),
                ..statistics(
                    vec![
                        EdgeStatistics {
                            rows: 1_000,
                            bytes: 64_000,
                        },
                        EdgeStatistics {
                            rows: 10,
                            bytes: 1_280,
                        },
                    ],
                    EdgeStatistics {
                        rows: 100,
                        bytes: 12_800,
                    },
                )
            },
        )
        .unwrap();
        assert_eq!(build_left.peak_memory_bytes, 80_000);

        let aggregate = estimate_operator(
            PhysicalOperator::HashAggregate,
            OperatorStatistics {
                group_count: Some(100),
                key_bytes: Some(16),
                aggregate_value_bytes: Some(24),
                ..statistics(
                    vec![EdgeStatistics {
                        rows: 1_000,
                        bytes: 64_000,
                    }],
                    EdgeStatistics {
                        rows: 100,
                        bytes: 4_000,
                    },
                )
            },
        )
        .unwrap();
        assert_eq!(aggregate.peak_memory_bytes, 5_600);

        let oversized_topk = estimate_operator(
            PhysicalOperator::TopK,
            OperatorStatistics {
                k: Some(1_000),
                ..statistics(
                    vec![EdgeStatistics {
                        rows: 4,
                        bytes: 160,
                    }],
                    EdgeStatistics {
                        rows: 4,
                        bytes: 160,
                    },
                )
            },
        )
        .unwrap();
        assert_eq!(oversized_topk.cpu_ops, 8.0);

        let offset_limit = estimate_operator(
            PhysicalOperator::Limit,
            OperatorStatistics {
                limit_rows_consumed: Some(900_010),
                ..statistics(
                    vec![EdgeStatistics {
                        rows: 1_000_000,
                        bytes: 40_000_000,
                    }],
                    EdgeStatistics {
                        rows: 10,
                        bytes: 400,
                    },
                )
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
                operator: PhysicalOperator::Filter,
                children: vec!["scan".into()],
                source_coverage: None,
                output_buffer_bytes: 4,
                retained_bytes: 0,
                execution: ExecutionMultiplicity::PerEvaluation,
            },
            PhysicalDagNode {
                id: "right".into(),
                operator: PhysicalOperator::Filter,
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
                OperatorStatistics {
                    source_scan_bytes: 1_000,
                    ..statistics(vec![scan_edge], scan_edge)
                },
            ),
            ("left".into(), statistics(vec![scan_edge], branch_edge)),
            ("right".into(), statistics(vec![scan_edge], branch_edge)),
            (
                "root".into(),
                statistics(
                    vec![branch_edge, branch_edge],
                    EdgeStatistics {
                        rows: 80,
                        bytes: 800,
                    },
                ),
            ),
        ]);
        let mut scope = comparison_scope();
        scope.horizon.0 = 20_000;
        let estimate = estimate_physical_dag(&nodes, "root", &scope, &provided).unwrap();
        assert_eq!(estimate.cpu_ops, 760.0);
        assert_eq!(estimate.scan_bytes, 2_000);
        // This is neither the sum of every node's memory nor just the largest
        // node: it is the maximum state simultaneously live at the fan-out.
        assert_eq!(estimate.peak_memory_bytes, 24);
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
                operator: PhysicalOperator::HashAggregate,
                children: vec!["scan".into()],
                source_coverage: None,
                output_buffer_bytes: 16,
                retained_bytes: 32,
                execution: ExecutionMultiplicity::Once,
            },
            PhysicalDagNode {
                id: "read".into(),
                operator: PhysicalOperator::Limit,
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
                OperatorStatistics {
                    source_scan_bytes: 1_000,
                    ..statistics(vec![scan_edge], scan_edge)
                },
            ),
            (
                "state".into(),
                OperatorStatistics {
                    group_count: Some(1),
                    key_bytes: Some(8),
                    aggregate_value_bytes: Some(8),
                    ..statistics(vec![scan_edge], state_edge)
                },
            ),
            (
                "read".into(),
                OperatorStatistics {
                    limit_rows_consumed: Some(1),
                    ..statistics(vec![state_edge], state_edge)
                },
            ),
        ]);
        let mut scope = comparison_scope();
        scope.horizon.0 = 100_000;
        let estimate = estimate_physical_dag(&nodes, "read", &scope, &provided).unwrap();
        assert_eq!(estimate.cpu_ops, 210.0);
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
                snapshot_id: "catalog-version-42".into(),
                predicates: vec![],
            }],
        }
    }

    fn statistics(inputs: Vec<EdgeStatistics>, output: EdgeStatistics) -> OperatorStatistics {
        OperatorStatistics {
            source_scan_bytes: 0,
            inputs,
            output,
            group_count: None,
            key_bytes: None,
            aggregate_value_bytes: None,
            k: None,
            limit_rows_consumed: None,
            hash_join_build_side: None,
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
        candidate.sources[0].snapshot_id = "catalog-version-43".into();
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
                operator: PhysicalOperator::Filter,
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
            OperatorStatistics {
                source_scan_bytes: 1_000,
                ..statistics(
                    vec![EdgeStatistics {
                        rows: 100,
                        bytes: 1_000,
                    }],
                    EdgeStatistics {
                        rows: 100,
                        bytes: 1_000,
                    },
                )
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
            statistics(
                vec![EdgeStatistics {
                    rows: 99,
                    bytes: 990,
                }],
                EdgeStatistics {
                    rows: 40,
                    bytes: 400,
                },
            ),
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
                operator: PhysicalOperator::Filter,
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
                OperatorStatistics {
                    source_scan_bytes: 1_000,
                    ..statistics(
                        vec![EdgeStatistics {
                            rows: 100,
                            bytes: 1_000,
                        }],
                        EdgeStatistics {
                            rows: 100,
                            bytes: 1_000,
                        },
                    )
                },
            ),
            (
                "filter".to_string(),
                statistics(
                    vec![EdgeStatistics {
                        rows: 100,
                        bytes: 1_000,
                    }],
                    EdgeStatistics {
                        rows: 40,
                        bytes: 400,
                    },
                ),
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
                operator: PhysicalOperator::Filter,
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
                OperatorStatistics {
                    source_scan_bytes: 1_000,
                    ..statistics(vec![input], input)
                },
            ),
            (
                "filter".into(),
                statistics(vec![input], EdgeStatistics { rows: 0, bytes: 0 }),
            ),
        ]);

        let estimate =
            estimate_physical_dag(&nodes, "filter", &comparison_scope(), &provided).unwrap();
        assert_eq!(estimate.cpu_ops, 1_200.0);
        assert_eq!(estimate.peak_memory_bytes, 10);
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
                snapshot_id: "catalog-version-42".into(),
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
            OperatorStatistics {
                source_scan_bytes: 250,
                ..statistics(vec![edge], edge)
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
            OperatorStatistics {
                source_scan_bytes: 250,
                ..statistics(vec![edge], edge)
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
            snapshot_id: "catalog-version-42".into(),
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
            OperatorStatistics {
                source_scan_bytes: 250,
                ..statistics(vec![edge], edge)
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
                operator: PhysicalOperator::HashAggregate,
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
                OperatorStatistics {
                    source_scan_bytes: 250,
                    ..statistics(vec![input], input)
                },
            ),
            (
                "aggregate".into(),
                OperatorStatistics {
                    group_count: Some(1),
                    key_bytes: Some(8),
                    aggregate_value_bytes: Some(8),
                    ..statistics(vec![input], output)
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
            OperatorStatistics {
                source_scan_bytes: 250,
                ..statistics(vec![edge], edge)
            },
        )]);
        let raw_scope = comparison_scope();
        let mut candidate_scope = raw_scope.clone();
        candidate_scope.sources[0].snapshot_id = "catalog-version-43".into();

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
            OperatorStatistics {
                source_scan_bytes: 2_500,
                ..statistics(vec![logical], logical)
            },
        )
        .unwrap();

        assert_eq!(estimate.scan_bytes, 2_500);
        assert_eq!(estimate.peak_memory_bytes, 100);
    }

    #[test]
    fn non_scan_operator_cannot_charge_source_bytes() {
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
                operator: PhysicalOperator::Filter,
                children: vec!["scan".into()],
                source_coverage: None,
                output_buffer_bytes: 10,
                retained_bytes: 0,
                execution: ExecutionMultiplicity::PerEvaluation,
            },
        ];
        let edge = EdgeStatistics {
            rows: 100,
            bytes: 1_000,
        };
        let provided = HashMap::from([
            (
                "scan".into(),
                OperatorStatistics {
                    source_scan_bytes: 250,
                    ..statistics(vec![edge], edge)
                },
            ),
            (
                "filter".into(),
                OperatorStatistics {
                    source_scan_bytes: 250,
                    ..statistics(vec![edge], edge)
                },
            ),
        ]);

        assert_eq!(
            estimate_physical_dag(&nodes, "filter", &comparison_scope(), &provided),
            Err(AnalyticalCostError::InvalidOperatorStatistics {
                node: "filter".into(),
                reason: "only scan operators may read source bytes",
            })
        );
    }
}

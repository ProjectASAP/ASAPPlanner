//! Analytical, resource-dimensional cost estimates (issue #323).
//!
//! This module deliberately keeps CPU work, retained state, and scan I/O
//! separate.  They become one planner objective only through an explicit
//! [`ResourceCalibration`]; without that calibration the dimensional
//! estimate is still useful for explanations, but is not silently comparable.

use std::collections::HashSet;

use asap_types::post_asap::{
    GroupingStrategy, SketchAlgorithm, SketchParams, SummaryFamilyType, SummaryNode,
};
use asap_types::pre_asap::agg_intent::AggIntent;
use asap_types::workload::{
    DataArrival, DataWorkload, QueryRecurrence, QueryWorkloadEntry, RepeatedDemand,
};
use serde::{Deserialize, Serialize};

use crate::cost_model::{Cost, CostModel, DefaultCostModel};
use crate::replacement::{
    accuracy_budget, accuracy_target, Replacement, ReplacementSubDAG, TargetSubDAG,
};

pub const ANALYTICAL_MODEL_VERSION: &str = "analytical-resource-at-rest-v1";

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct AnalyticalInputs {
    /// Source-data arrival mode for this comparison. Version 1 deliberately
    /// supports only a fixed snapshot (`AtRest`); it does not silently treat
    /// incremental maintenance as a one-time build.
    pub data_arrival: DataArrival,
    pub input_rows: u64,
    /// Logical bytes consumed by the operator. Intermediate input may have
    /// bytes here while reading zero source/disk bytes.
    pub input_bytes: u64,
    /// Source/disk bytes read once to produce this operator's input.
    pub source_scan_bytes: u64,
    pub group_count: u64,
    /// Average encoded bytes of one distinct grouping-key tuple.
    pub group_key_bytes: u64,
    /// `Some(k)` when the aggregation feeds an `ORDER BY value DESC LIMIT k`.
    pub topk_k: Option<u64>,
    /// Number of effective reads/recomputations in the comparison scope.
    pub evaluation_count: u64,
}

impl AnalyticalInputs {
    pub fn validate(self) -> Result<Self, AnalyticalCostError> {
        if self.data_arrival != DataArrival::AtRest {
            return Err(AnalyticalCostError::UnsupportedDataArrival(
                self.data_arrival,
            ));
        }
        if self.input_rows == 0 {
            return Err(AnalyticalCostError::MissingOrZero("input_rows"));
        }
        if self.input_bytes == 0 {
            return Err(AnalyticalCostError::MissingOrZero("input_bytes"));
        }
        if self.group_count == 0 {
            return Err(AnalyticalCostError::MissingOrZero("group_count"));
        }
        if self.group_key_bytes == 0 {
            return Err(AnalyticalCostError::MissingOrZero("group_key_bytes"));
        }
        if matches!(self.topk_k, Some(0)) {
            return Err(AnalyticalCostError::MissingOrZero("topk_k"));
        }
        if self.evaluation_count == 0 {
            return Err(AnalyticalCostError::MissingOrZero("evaluation_count"));
        }
        Ok(self)
    }

    /// Resolve the workload-owned axes of an analytical estimate. Physical
    /// widths and cardinalities that are not represented by `DataWorkload`
    /// remain explicit arguments instead of being guessed.
    pub fn from_workload(
        physical: PhysicalInputEvidence,
        data: &DataWorkload,
        query: &QueryWorkloadEntry,
        planning_time_ms: u64,
        horizon_ms: u64,
    ) -> Result<Self, AnalyticalCostError> {
        let input_rows = data
            .input_cardinality
            .value_at(planning_time_ms)
            .copied()
            .ok_or(AnalyticalCostError::MissingOrStale("input_cardinality"))?;
        let evaluation_count =
            evaluations_in_horizon(&query.recurrence, planning_time_ms, horizon_ms)?;
        Self {
            data_arrival: data.arrival,
            input_rows,
            input_bytes: physical.input_bytes,
            source_scan_bytes: physical.source_scan_bytes,
            group_count: physical.group_count,
            group_key_bytes: physical.group_key_bytes,
            // Query-shape constants are read from the lowered IR by
            // `inputs_for_target`, never duplicated in workload evidence.
            topk_k: None,
            evaluation_count,
        }
        .validate()
    }
}

/// Catalog/operator statistics absent from the canonical workload schema.
/// Provenance belongs to the provider that resolves this adapter.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct PhysicalInputEvidence {
    pub input_bytes: u64,
    pub source_scan_bytes: u64,
    pub group_count: u64,
    pub group_key_bytes: u64,
}

fn evaluations_in_horizon(
    recurrence: &QueryRecurrence,
    planning_time_ms: u64,
    horizon_ms: u64,
) -> Result<u64, AnalyticalCostError> {
    if horizon_ms == 0 {
        return Err(AnalyticalCostError::MissingOrZero("horizon_ms"));
    }
    let end = planning_time_ms.saturating_add(horizon_ms);
    let count = match recurrence {
        QueryRecurrence::OneTime {
            invocations,
            execute_at,
        } => {
            if execute_at.is_none_or(|at| at.0 >= planning_time_ms && at.0 <= end) {
                *invocations
            } else {
                0
            }
        }
        QueryRecurrence::Repeated(RepeatedDemand::FixedInterval(interval)) => {
            if interval.0 == 0 {
                return Err(AnalyticalCostError::InvalidRecurrence);
            }
            horizon_ms / u64::from(interval.0)
        }
        QueryRecurrence::Repeated(RepeatedDemand::Scheduled(schedule)) => schedule
            .iter()
            .filter(|at| at.0 >= planning_time_ms && at.0 <= end)
            .count()
            as u64,
        QueryRecurrence::Repeated(RepeatedDemand::EstimatedRate(estimate)) => {
            if !estimate.is_fresh_at(planning_time_ms)
                || !estimate.expected_rate.0.is_finite()
                || estimate.expected_rate.0 < 0.0
            {
                return Err(AnalyticalCostError::InvalidRecurrence);
            }
            let expected = estimate.expected_rate.0 * horizon_ms as f64 / 1000.0;
            if expected > u64::MAX as f64 {
                return Err(AnalyticalCostError::Overflow);
            }
            // The integer adapter is deliberately conservative: a positive
            // fractional expected demand still requires one provisioned read.
            expected.ceil() as u64
        }
        QueryRecurrence::Unknown => return Err(AnalyticalCostError::InvalidRecurrence),
    };
    if count == 0 {
        return Err(AnalyticalCostError::NoEvaluationsInHorizon);
    }
    Ok(count)
}

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
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct OperatorInputs {
    pub input_rows: u64,
    pub input_bytes: u64,
    pub output_rows: u64,
    pub output_bytes: u64,
    pub group_count: Option<u64>,
    pub key_bytes: Option<u64>,
    /// Bytes of aggregate accumulator state retained per group. Required for
    /// hash aggregation because one 8-byte value is not universal.
    pub aggregate_value_bytes: Option<u64>,
    pub k: Option<u64>,
    pub right_rows: Option<u64>,
    pub right_bytes: Option<u64>,
    pub hash_join_build_side: Option<HashJoinBuildSide>,
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
    pub inputs: OperatorInputs,
    pub children: Vec<String>,
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

/// Compose local operator estimates once per physical identity. CPU and disk
/// are additive; peak memory is simulated over a child-before-parent schedule
/// and releases transient child outputs after their last consumer.
pub fn estimate_physical_dag(
    nodes: &[PhysicalDagNode],
    root: &str,
    evaluation_count: u64,
) -> Result<ResourceEstimate, AnalyticalCostError> {
    use std::collections::HashMap;

    if evaluation_count == 0 {
        return Err(AnalyticalCostError::MissingOrZero("evaluation_count"));
    }
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

    for id in &order {
        let node = by_id[id];
        if node.retained_bytes > 0 && matches!(node.execution, ExecutionMultiplicity::PerEvaluation)
        {
            return Err(AnalyticalCostError::InvalidPhysicalDag(
                "per-evaluation node cannot retain state across the horizon",
            ));
        }
        for child in &node.children {
            let child = by_id[child.as_str()];
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
        let local = estimate_operator(node.operator, node.inputs)?;
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

/// Estimate one physical operator. Child costs are deliberately excluded;
/// a DAG walker sums CPU/disk once per node and combines simultaneously
/// retained state separately.
pub fn estimate_operator(
    operator: PhysicalOperator,
    input: OperatorInputs,
) -> Result<ResourceEstimate, AnalyticalCostError> {
    let per_row_width = |rows: u64, bytes: u64| -> Result<u64, AnalyticalCostError> {
        if rows == 0 || bytes == 0 {
            return Err(AnalyticalCostError::MissingOrZero("operator rows/bytes"));
        }
        Ok(bytes.div_ceil(rows))
    };
    let estimate = match operator {
        PhysicalOperator::Scan => ResourceEstimate {
            cpu_ops: input.input_rows as f64,
            peak_memory_bytes: per_row_width(input.input_rows, input.input_bytes)?,
            scan_bytes: input.input_bytes,
        },
        PhysicalOperator::Filter | PhysicalOperator::Project | PhysicalOperator::PassThrough => {
            ResourceEstimate {
                cpu_ops: input.input_rows as f64,
                peak_memory_bytes: per_row_width(input.output_rows, input.output_bytes)?,
                scan_bytes: 0,
            }
        }
        PhysicalOperator::HashAggregate => {
            let groups = input
                .group_count
                .ok_or(AnalyticalCostError::MissingOrZero("group_count"))?;
            let key = input
                .key_bytes
                .ok_or(AnalyticalCostError::MissingOrZero("key_bytes"))?;
            let value = input
                .aggregate_value_bytes
                .ok_or(AnalyticalCostError::MissingOrZero("aggregate_value_bytes"))?;
            ResourceEstimate {
                cpu_ops: input.input_rows as f64,
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
            let groups = input
                .group_count
                .ok_or(AnalyticalCostError::MissingOrZero("group_count"))?;
            let key = input
                .key_bytes
                .ok_or(AnalyticalCostError::MissingOrZero("key_bytes"))?;
            ResourceEstimate {
                cpu_ops: input.input_rows as f64,
                peak_memory_bytes: checked_bytes(&[
                    groups,
                    key.checked_add(16).ok_or(AnalyticalCostError::Overflow)?,
                ])?,
                scan_bytes: 0,
            }
        }
        PhysicalOperator::Sort | PhysicalOperator::Window => ResourceEstimate {
            cpu_ops: input.input_rows as f64 * (input.input_rows.max(2) as f64).log2().ceil(),
            peak_memory_bytes: input.input_bytes,
            scan_bytes: 0,
        },
        PhysicalOperator::TopK => {
            let k = input.k.ok_or(AnalyticalCostError::MissingOrZero("k"))?;
            ResourceEstimate {
                cpu_ops: input.input_rows as f64 * (k.max(2) as f64).log2().ceil(),
                peak_memory_bytes: checked_bytes(&[
                    k.min(input.input_rows),
                    per_row_width(input.input_rows, input.input_bytes)?,
                ])?,
                scan_bytes: 0,
            }
        }
        PhysicalOperator::HashJoin => {
            let right_rows = input
                .right_rows
                .ok_or(AnalyticalCostError::MissingOrZero("right_rows"))?;
            let right_bytes = input
                .right_bytes
                .ok_or(AnalyticalCostError::MissingOrZero("right_bytes"))?;
            let build_side = input
                .hash_join_build_side
                .ok_or(AnalyticalCostError::MissingOrZero("hash_join_build_side"))?;
            ResourceEstimate {
                cpu_ops: input.input_rows as f64 + right_rows as f64 + input.output_rows as f64,
                peak_memory_bytes: match build_side {
                    HashJoinBuildSide::Left => input.input_bytes,
                    HashJoinBuildSide::Right => right_bytes,
                },
                scan_bytes: 0,
            }
        }
        PhysicalOperator::Concat => ResourceEstimate {
            cpu_ops: input.output_rows as f64,
            peak_memory_bytes: per_row_width(input.output_rows, input.output_bytes)?,
            scan_bytes: 0,
        },
        PhysicalOperator::Limit => ResourceEstimate {
            cpu_ops: input.output_rows as f64,
            peak_memory_bytes: per_row_width(input.output_rows, input.output_bytes)?,
            scan_bytes: 0,
        },
    };
    if estimate.cpu_ops.is_finite() {
        Ok(estimate)
    } else {
        Err(AnalyticalCostError::Overflow)
    }
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
    #[error("summary operation {0} has no lifecycle-aware cost formula")]
    UnsupportedSummaryOperation(&'static str),
    #[error("invalid physical DAG: {0}")]
    InvalidPhysicalDag(&'static str),
}

fn checked_bytes(parts: &[u64]) -> Result<u64, AnalyticalCostError> {
    parts
        .iter()
        .try_fold(1_u64, |acc, value| acc.checked_mul(*value))
        .ok_or(AnalyticalCostError::Overflow)
}

fn log2_at_least_one(value: u32) -> f64 {
    f64::from(value.max(2)).log2().ceil()
}

/// Raw exact grouped aggregation: every evaluation scans and processes the
/// whole input; one 16-byte accumulator/key slot is retained per group.
pub fn estimate_raw_aggregation(
    inputs: AnalyticalInputs,
) -> Result<ResourceEstimate, AnalyticalCostError> {
    let inputs = inputs.validate()?;
    let group_entry_bytes = inputs
        .group_key_bytes
        .checked_add(8 + 16) // exact value plus hash-table metadata
        .ok_or(AnalyticalCostError::Overflow)?;
    let topk_memory = topk_heap_bytes(inputs)?;
    let scan_cpu = inputs.input_rows as f64 * inputs.evaluation_count as f64;
    let scan_buffer = inputs.input_bytes.div_ceil(inputs.input_rows);
    Ok(ResourceEstimate {
        cpu_ops: scan_cpu
            + inputs.input_rows as f64 * inputs.evaluation_count as f64
            + topk_cpu_ops(inputs),
        peak_memory_bytes: scan_buffer.max(
            checked_bytes(&[inputs.group_count, group_entry_bytes])?
                .checked_add(topk_memory)
                .ok_or(AnalyticalCostError::Overflow)?,
        ),
        scan_bytes: inputs
            .source_scan_bytes
            .checked_mul(inputs.evaluation_count)
            .ok_or(AnalyticalCostError::Overflow)?,
    })
}

/// Build one sketch once and serve every evaluation from its retained state.
/// The formula exposes algorithm-specific update/read complexity and concrete
/// parameter-derived state size; it never substitutes a node count.
pub fn estimate_sketch_aggregation(
    algorithm: SketchAlgorithm,
    params: &SketchParams,
    inputs: AnalyticalInputs,
) -> Result<ResourceEstimate, AnalyticalCostError> {
    let instance_count = inputs.group_count;
    estimate_sketch_aggregation_with_instances(algorithm, params, inputs, instance_count)
}

fn estimate_sketch_aggregation_with_instances(
    algorithm: SketchAlgorithm,
    params: &SketchParams,
    inputs: AnalyticalInputs,
    physical_sketch_count: u64,
) -> Result<ResourceEstimate, AnalyticalCostError> {
    let inputs = inputs.validate()?;
    let (update_ops, read_ops, bytes_per_group) = match (&algorithm, params) {
        (SketchAlgorithm::Cms, SketchParams::Cms { width, depth })
        | (SketchAlgorithm::CountSketch, SketchParams::CountSketch { width, depth }) => (
            f64::from(*depth),
            f64::from(*depth),
            checked_bytes(&[u64::from(*width), u64::from(*depth), 8])?,
        ),
        (
            SketchAlgorithm::CmsWithHeap,
            SketchParams::CmsWithHeap {
                width,
                depth,
                heap_size,
            },
        )
        | (
            SketchAlgorithm::CountSketchWithHeap,
            SketchParams::CountSketchWithHeap {
                width,
                depth,
                heap_size,
            },
        ) => (
            f64::from(*depth) + log2_at_least_one(*heap_size),
            f64::from(*depth) + f64::from(*heap_size),
            checked_bytes(&[u64::from(*width), u64::from(*depth), 8])?
                .checked_add(checked_bytes(&[u64::from(*heap_size), 16])?)
                .ok_or(AnalyticalCostError::Overflow)?,
        ),
        (SketchAlgorithm::Kll, SketchParams::Kll { k }) => (
            1.0 + log2_at_least_one(*k),
            log2_at_least_one(*k),
            checked_bytes(&[u64::from(*k), 8])?,
        ),
        (SketchAlgorithm::Hll, SketchParams::Hll { precision }) => {
            let registers = 1_u64
                .checked_shl(u32::from(*precision))
                .ok_or(AnalyticalCostError::Overflow)?;
            (1.0, registers as f64, registers)
        }
        (SketchAlgorithm::Kmv, SketchParams::Kmv { k })
        | (SketchAlgorithm::Theta, SketchParams::Theta { k }) => (
            log2_at_least_one(*k),
            f64::from(*k),
            checked_bytes(&[u64::from(*k), 8])?,
        ),
        (SketchAlgorithm::DDSketch, SketchParams::DDSketch { .. }) => {
            return Err(AnalyticalCostError::UnsupportedWithoutDistribution(
                "DDSketch",
            ));
        }
        _ => {
            return Err(AnalyticalCostError::ParameterMismatch(
                algorithm,
                params.clone(),
            ))
        }
    };

    let keyed_state = inputs
        .group_key_bytes
        .checked_add(16) // hash-table metadata
        .and_then(|bytes| bytes.checked_mul(inputs.group_count))
        .ok_or(AnalyticalCostError::Overflow)?;
    Ok(ResourceEstimate {
        cpu_ops: inputs.input_rows as f64
            + inputs.input_rows as f64 * update_ops
            + inputs.evaluation_count as f64 * read_ops * physical_sketch_count as f64
            + topk_cpu_ops(inputs),
        peak_memory_bytes: inputs.input_bytes.div_ceil(inputs.input_rows).max(
            bytes_per_group
                .checked_mul(physical_sketch_count)
                .and_then(|bytes| bytes.checked_add(keyed_state))
                .and_then(|bytes| bytes.checked_add(topk_heap_bytes(inputs).ok()?))
                .ok_or(AnalyticalCostError::Overflow)?,
        ),
        // One initial build scan; subsequent evaluations read the sketch.
        scan_bytes: inputs.source_scan_bytes,
    })
}

fn topk_cpu_ops(inputs: AnalyticalInputs) -> f64 {
    inputs.topk_k.map_or(0.0, |k| {
        inputs.evaluation_count as f64 * inputs.group_count as f64 * (k.max(2) as f64).log2().ceil()
    })
}

fn topk_heap_bytes(inputs: AnalyticalInputs) -> Result<u64, AnalyticalCostError> {
    inputs.topk_k.map_or(Ok(0), |k| {
        let row_bytes = inputs
            .group_key_bytes
            .checked_add(8)
            .ok_or(AnalyticalCostError::Overflow)?;
        checked_bytes(&[k.min(inputs.group_count), row_bytes])
    })
}

/// Public cost-model adapter. It preserves candidate legality, sizes every
/// candidate against the declared accuracy target, then ranks supported
/// algorithms by their calibrated resource estimate.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AnalyticalCostModel {
    pub inputs: AnalyticalInputs,
    pub calibration: ResourceCalibration,
}

impl AnalyticalCostModel {
    pub fn from_workload(
        physical: PhysicalInputEvidence,
        data: &DataWorkload,
        query: &QueryWorkloadEntry,
        planning_time_ms: u64,
        horizon_ms: u64,
        calibration: ResourceCalibration,
    ) -> Result<Self, AnalyticalCostError> {
        calibration.validate()?;
        Ok(Self {
            inputs: AnalyticalInputs::from_workload(
                physical,
                data,
                query,
                planning_time_ms,
                horizon_ms,
            )?,
            calibration,
        })
    }

    pub fn raw_cost(&self) -> Result<f64, AnalyticalCostError> {
        estimate_raw_aggregation(self.inputs)?.calibrated_cost(&self.calibration)
    }

    pub fn candidate_resources(
        &self,
        candidate: &ReplacementSubDAG,
    ) -> Result<ResourceEstimate, AnalyticalCostError> {
        let Replacement::Summary(node) = &candidate.replacement else {
            return Err(AnalyticalCostError::UnsupportedCandidate);
        };
        let mut states = Vec::new();
        let mut visited = HashSet::new();
        collect_sketches(node, &mut visited, &mut states)?;
        if states.is_empty() {
            return Err(AnalyticalCostError::UnsupportedCandidate);
        }
        if states.len() != 1 {
            // The compact summary bridge has no edge/source identities with
            // which to decide whether several builds share an input read.
            // Such plans must use `estimate_physical_dag`.
            return Err(AnalyticalCostError::UnsupportedCandidate);
        }
        let mut total = ResourceEstimate {
            cpu_ops: 0.0,
            peak_memory_bytes: 0,
            scan_bytes: 0,
        };
        for (kind, grouping) in states {
            let inputs = self.inputs.validate()?;
            let physical_sketch_count =
                if matches!(grouping, GroupingStrategy::SharedMultiSubpopulation { .. }) {
                    1
                } else {
                    inputs.group_count
                };
            total = add_resources(
                total,
                estimate_sketch_aggregation_with_instances(
                    kind.algorithm().clone(),
                    kind.params(),
                    inputs,
                    physical_sketch_count,
                )?,
            )?;
        }
        Ok(total)
    }

    /// Target-local estimate used for nested plans. An outer Top-K consumes
    /// the grouped child's rows, not the original source rows, and its
    /// intermediate input performs no second source/disk scan.
    pub fn candidate_resources_for_target(
        &self,
        candidate: &ReplacementSubDAG,
        target: &TargetSubDAG<'_>,
    ) -> Result<ResourceEstimate, AnalyticalCostError> {
        require_supported_target(target.root)?;
        if topk_intent(target.root).is_some() {
            let Replacement::Summary(node) = &candidate.replacement else {
                return Err(AnalyticalCostError::UnsupportedCandidate);
            };
            let mut states = Vec::new();
            let mut visited = HashSet::new();
            collect_sketches(node, &mut visited, &mut states)?;
            if states.is_empty() {
                return Err(AnalyticalCostError::UnsupportedCandidate);
            }
            if states.len() != 1 {
                return Err(AnalyticalCostError::UnsupportedCandidate);
            }
            let fused_topk = states.len() == 1
                && matches!(
                    states[0].0.algorithm(),
                    SketchAlgorithm::CmsWithHeap | SketchAlgorithm::CountSketchWithHeap
                );
            let mut total = ResourceEstimate {
                cpu_ops: 0.0,
                peak_memory_bytes: 0,
                scan_bytes: 0,
            };
            for (kind, grouping) in states {
                let is_topk = matches!(
                    kind.algorithm(),
                    SketchAlgorithm::CmsWithHeap | SketchAlgorithm::CountSketchWithHeap
                );
                let inputs = if is_topk && fused_topk {
                    let mut base = self.inputs.validate()?;
                    base.group_count = 1;
                    base.topk_k = None;
                    base
                } else if is_topk {
                    self.inputs_for_target(target.root)?
                } else {
                    let mut base = self.inputs.validate()?;
                    base.topk_k = None;
                    base
                };
                let instances =
                    if matches!(grouping, GroupingStrategy::SharedMultiSubpopulation { .. }) {
                        1
                    } else {
                        inputs.group_count
                    };
                let mut estimate = estimate_sketch_aggregation_with_instances(
                    kind.algorithm().clone(),
                    kind.params(),
                    inputs,
                    instances,
                )?;
                if fused_topk {
                    // One global keyed sketch has no outer hash map from
                    // group -> sketch instance. Its keys live in the heap;
                    // remove the generic one-entry registry charged by the
                    // per-subpopulation estimator.
                    estimate.peak_memory_bytes = estimate
                        .peak_memory_bytes
                        .checked_sub(
                            inputs
                                .group_key_bytes
                                .checked_add(16)
                                .ok_or(AnalyticalCostError::Overflow)?,
                        )
                        .ok_or(AnalyticalCostError::Overflow)?;
                }
                total = add_resources(total, estimate)?;
            }
            return Ok(total);
        }
        let Replacement::Summary(node) = &candidate.replacement else {
            return Err(AnalyticalCostError::UnsupportedCandidate);
        };
        // Validate the complete candidate even when the target is not Top-K.
        // In particular, never price a merge/delete/subtract as if that
        // physical operation were free.
        let mut states = Vec::new();
        let mut visited = HashSet::new();
        collect_sketches(node, &mut visited, &mut states)?;
        let scoped = self.inputs_for_target(target.root)?;
        let model = Self {
            inputs: scoped,
            calibration: self.calibration.clone(),
        };
        model.candidate_resources(candidate)
    }

    pub fn raw_resources_for_target(
        &self,
        target: &asap_types::pre_asap::QueryExpr,
    ) -> Result<ResourceEstimate, AnalyticalCostError> {
        require_supported_target(target)?;
        let scoped = self.inputs_for_target(target)?;
        if let Some(k) = topk_intent(target) {
            let mut base = self.inputs.validate()?;
            base.topk_k = None;
            let grouped = estimate_raw_aggregation(base)?;
            let cpu_ops = scoped.evaluation_count as f64
                * scoped.input_rows as f64
                * (k.max(2) as f64).log2().ceil();
            let row_bytes = scoped
                .group_key_bytes
                .checked_add(8)
                .ok_or(AnalyticalCostError::Overflow)?;
            return add_resources(
                grouped,
                ResourceEstimate {
                    cpu_ops,
                    peak_memory_bytes: checked_bytes(&[k.min(scoped.input_rows), row_bytes])?,
                    scan_bytes: 0,
                },
            );
        }
        match single_intent(target) {
            Some(AggIntent::Quantile { .. }) => estimate_raw_quantile(scoped),
            Some(AggIntent::Count { .. }) => estimate_raw_aggregation(scoped),
            // Exact distinct state needs the measure's distinct cardinality,
            // which is not the number of GROUP BY tuples.
            Some(AggIntent::Cardinality { .. }) => Err(AnalyticalCostError::MissingOrStale(
                "measure_distinct_count",
            )),
            _ => Err(AnalyticalCostError::UnsupportedCandidate),
        }
    }

    pub fn raw_cost_for_target(
        &self,
        target: &asap_types::pre_asap::QueryExpr,
    ) -> Result<f64, AnalyticalCostError> {
        self.raw_resources_for_target(target)?
            .calibrated_cost(&self.calibration)
    }

    pub fn inputs_for_target(
        &self,
        target: &asap_types::pre_asap::QueryExpr,
    ) -> Result<AnalyticalInputs, AnalyticalCostError> {
        let mut scoped = self.inputs.validate()?;
        if topk_intent(target).is_some() {
            scoped.input_rows = self.inputs.group_count;
            scoped.input_bytes = checked_bytes(&[
                self.inputs.group_count,
                self.inputs
                    .group_key_bytes
                    .checked_add(8)
                    .ok_or(AnalyticalCostError::Overflow)?,
            ])?;
            scoped.source_scan_bytes = 0;
            scoped.group_count = 1;
            // CMSWithHeap owns its heap. Do not add an exact Top-K heap on
            // top of the selected sketch implementation.
            scoped.topk_k = None;
        } else {
            // A nested grouped aggregate is one decision region. Its parent
            // Top-K is costed by the parent region, not charged here too.
            scoped.topk_k = None;
        }
        Ok(scoped)
    }
}

fn add_resources(
    left: ResourceEstimate,
    right: ResourceEstimate,
) -> Result<ResourceEstimate, AnalyticalCostError> {
    let cpu_ops = left.cpu_ops + right.cpu_ops;
    if !cpu_ops.is_finite() {
        return Err(AnalyticalCostError::Overflow);
    }
    Ok(ResourceEstimate {
        cpu_ops,
        // Nested summary states coexist for the retained plan. This is an
        // additive retained-state bound; transient pipelined buffers are not
        // assumed to disappear without execution evidence.
        peak_memory_bytes: left
            .peak_memory_bytes
            .checked_add(right.peak_memory_bytes)
            .ok_or(AnalyticalCostError::Overflow)?,
        scan_bytes: left
            .scan_bytes
            .checked_add(right.scan_bytes)
            .ok_or(AnalyticalCostError::Overflow)?,
    })
}

fn topk_intent(target: &asap_types::pre_asap::QueryExpr) -> Option<u64> {
    let asap_types::pre_asap::QueryExpr::Aggregate { measures, .. } = target else {
        return None;
    };
    measures.iter().find_map(|intent| match intent {
        AggIntent::TopK { k, .. } => Some(*k as u64),
        _ => None,
    })
}

fn single_intent(target: &asap_types::pre_asap::QueryExpr) -> Option<&AggIntent> {
    let asap_types::pre_asap::QueryExpr::Aggregate { measures, .. } = target else {
        return None;
    };
    match measures.as_slice() {
        [intent] => Some(intent),
        _ => None,
    }
}

fn estimate_raw_quantile(
    inputs: AnalyticalInputs,
) -> Result<ResourceEstimate, AnalyticalCostError> {
    let inputs = inputs.validate()?;
    let rows = inputs.input_rows.max(2);
    let sort_ops = inputs.input_rows as f64 * (rows as f64).log2().ceil();
    let cpu_ops = inputs.evaluation_count as f64 * (inputs.input_rows as f64 + sort_ops);
    if !cpu_ops.is_finite() {
        return Err(AnalyticalCostError::Overflow);
    }
    Ok(ResourceEstimate {
        cpu_ops,
        // Without a projected-value width, logical input bytes are the safe
        // upper bound for the in-memory exact sort.
        peak_memory_bytes: inputs.input_bytes,
        scan_bytes: inputs
            .source_scan_bytes
            .checked_mul(inputs.evaluation_count)
            .ok_or(AnalyticalCostError::Overflow)?,
    })
}

fn require_supported_target(
    target: &asap_types::pre_asap::QueryExpr,
) -> Result<(), AnalyticalCostError> {
    let asap_types::pre_asap::QueryExpr::Aggregate { child, .. } = target else {
        return Err(AnalyticalCostError::UnsupportedCandidate);
    };
    match single_intent(target) {
        Some(AggIntent::Count { .. } | AggIntent::Quantile { .. } | AggIntent::TopK { .. }) => {}
        Some(AggIntent::Cardinality { .. }) => {
            return Err(AnalyticalCostError::MissingOrStale(
                "measure_distinct_count",
            ));
        }
        _ => return Err(AnalyticalCostError::UnsupportedCandidate),
    }
    match child.as_ref() {
        asap_types::pre_asap::QueryExpr::Scan { predicates, .. } if predicates.is_empty() => Ok(()),
        asap_types::pre_asap::QueryExpr::Aggregate {
            measures,
            having: None,
            child: raw_child,
            ..
        } if topk_intent(target).is_some()
            && matches!(measures.as_slice(), [AggIntent::Count { .. }])
            && matches!(
                raw_child.as_ref(),
                asap_types::pre_asap::QueryExpr::Scan { predicates, .. }
                    if predicates.is_empty()
            ) =>
        {
            Ok(())
        }
        // Filters, joins, projections, windows, and nested aggregates require
        // per-node statistics and must be supplied as a PhysicalDagNode plan.
        _ => Err(AnalyticalCostError::UnsupportedCandidate),
    }
}

pub fn query_topk_k(target: &asap_types::pre_asap::QueryExpr) -> Option<u64> {
    topk_intent(target)
}

fn collect_sketches<'a>(
    node: &'a SummaryNode,
    visited: &mut HashSet<*const SummaryNode>,
    out: &mut Vec<(&'a asap_types::post_asap::SketchKind, &'a GroupingStrategy)>,
) -> Result<(), AnalyticalCostError> {
    let identity = node as *const SummaryNode;
    if !visited.insert(identity) {
        return Ok(());
    }
    match &node.expr {
        asap_types::post_asap::SummaryExpr::SummaryAgg {
            family,
            child,
            grouping: node_grouping,
            ..
        } => {
            collect_sketches(child, visited, out)?;
            if let SummaryFamilyType::Sketch(kind, grouping) = family {
                if grouping != node_grouping {
                    return Err(AnalyticalCostError::InvalidPhysicalDag(
                        "summary grouping metadata disagrees with its family",
                    ));
                }
                out.push((kind, grouping));
            } else {
                return Err(AnalyticalCostError::UnsupportedCandidate);
            }
        }
        asap_types::post_asap::SummaryExpr::SummaryEstimate { summary_input, .. } => {
            collect_sketches(summary_input, visited, out)?;
        }
        asap_types::post_asap::SummaryExpr::KeepPreAsap(_) => {}
        asap_types::post_asap::SummaryExpr::SummaryJoin { .. } => {
            return Err(AnalyticalCostError::UnsupportedSummaryOperation("join"));
        }
        asap_types::post_asap::SummaryExpr::SummarySubtract { .. } => {
            return Err(AnalyticalCostError::UnsupportedSummaryOperation("subtract"));
        }
        asap_types::post_asap::SummaryExpr::SummaryDelete { .. } => {
            return Err(AnalyticalCostError::UnsupportedSummaryOperation("delete"));
        }
        asap_types::post_asap::SummaryExpr::SummaryMerge { .. } => {
            return Err(AnalyticalCostError::UnsupportedSummaryOperation("merge"));
        }
    }
    Ok(())
}

impl CostModel for AnalyticalCostModel {
    fn candidate_cost(
        &self,
        candidate: &ReplacementSubDAG,
        target: &TargetSubDAG<'_>,
    ) -> Option<Cost> {
        self.candidate_resources_for_target(candidate, target)
            .and_then(|estimate| estimate.calibrated_cost(&self.calibration))
            .ok()
            .map(Cost)
    }

    fn rank_candidates(
        &self,
        intent: &AggIntent,
        candidates: &[SketchAlgorithm],
    ) -> Vec<SketchAlgorithm> {
        let Some(accuracy) = accuracy_target(intent) else {
            return DefaultCostModel.rank_candidates(intent, candidates);
        };
        let (epsilon, delta) = accuracy_budget(accuracy);
        let mut ranked = DefaultCostModel.rank_candidates(intent, candidates);
        ranked.sort_by(|left, right| {
            let cost = |algorithm: &SketchAlgorithm| {
                let params = self.size_params(algorithm.clone(), intent, epsilon, delta);
                let mut inputs = self.inputs;
                inputs.topk_k = None;
                estimate_sketch_aggregation(algorithm.clone(), &params, inputs)
                    .and_then(|resources| resources.calibrated_cost(&self.calibration))
                    .unwrap_or(f64::INFINITY)
            };
            cost(left).total_cmp(&cost(right))
        });
        ranked
    }

    fn estimated_subpopulation_count(
        &self,
        _target: &asap_types::pre_asap::QueryExpr,
    ) -> Option<usize> {
        usize::try_from(self.inputs.group_count).ok()
    }

    fn estimate_cost(&self, candidate: &ReplacementSubDAG, target: &TargetSubDAG<'_>) -> f64 {
        self.candidate_cost(candidate, target)
            .map_or(f64::NAN, |cost| cost.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn inputs(evaluation_count: u64) -> AnalyticalInputs {
        AnalyticalInputs {
            data_arrival: DataArrival::AtRest,
            input_rows: 1_000_000,
            input_bytes: 64_000_000,
            source_scan_bytes: 64_000_000,
            group_count: 1,
            group_key_bytes: 16,
            topk_k: None,
            evaluation_count,
        }
    }

    fn calibration() -> ResourceCalibration {
        ResourceCalibration {
            cost_per_cpu_op: 1e-6,
            cost_per_scan_byte: 1e-8,
            cost_per_retained_byte: 1e-9,
            version: "test-calibration-v1".to_string(),
        }
    }

    #[test]
    fn cms_formula_matches_hand_calculation() {
        let estimate = estimate_sketch_aggregation(
            SketchAlgorithm::Cms,
            &SketchParams::Cms {
                width: 100,
                depth: 4,
            },
            AnalyticalInputs {
                data_arrival: DataArrival::AtRest,
                input_rows: 1_000,
                input_bytes: 8_000,
                source_scan_bytes: 8_000,
                group_count: 2,
                group_key_bytes: 16,
                topk_k: None,
                evaluation_count: 10,
            },
        )
        .unwrap();
        assert_eq!(estimate.cpu_ops, 5_080.0);
        assert_eq!(estimate.peak_memory_bytes, 6_464);
        assert_eq!(estimate.scan_bytes, 8_000);
    }

    #[test]
    fn repeated_raw_scans_make_a_more_complex_sketch_plan_cheaper() {
        let raw = estimate_raw_aggregation(inputs(100))
            .unwrap()
            .calibrated_cost(&calibration())
            .unwrap();
        let sketch = estimate_sketch_aggregation(
            SketchAlgorithm::Cms,
            &SketchParams::Cms {
                width: 2_000,
                depth: 5,
            },
            inputs(100),
        )
        .unwrap()
        .calibrated_cost(&calibration())
        .unwrap();
        assert!(sketch < raw, "sketch={sketch}, raw={raw}");
    }

    #[test]
    fn raw_cost_is_monotone_in_rows_scans_and_evaluations() {
        let one = estimate_raw_aggregation(inputs(1)).unwrap();
        let many = estimate_raw_aggregation(inputs(10)).unwrap();
        assert!(many.cpu_ops > one.cpu_ops);
        assert!(many.scan_bytes > one.scan_bytes);
        assert_eq!(many.peak_memory_bytes, one.peak_memory_bytes);
    }

    #[test]
    fn exact_quantile_baseline_costs_sort_not_hash_counting() {
        let mut quantile_inputs = inputs(3);
        quantile_inputs.input_rows = 1_000;
        quantile_inputs.input_bytes = 8_000;
        quantile_inputs.source_scan_bytes = 8_000;
        let estimate = estimate_raw_quantile(quantile_inputs).unwrap();
        assert_eq!(estimate.cpu_ops, 33_000.0); // 3 * (scan 1K + sort 10K)
        assert_eq!(estimate.peak_memory_bytes, 8_000);
        assert_eq!(estimate.scan_bytes, 24_000);
    }

    #[test]
    fn grouped_topk_accounts_for_keys_hash_entries_and_selection() {
        let mut topk = inputs(100);
        topk.group_count = 100_000;
        topk.group_key_bytes = 32;
        topk.topk_k = Some(10);
        let raw = estimate_raw_aggregation(topk).unwrap();
        assert_eq!(raw.peak_memory_bytes, 5_600_400);
        assert_eq!(raw.cpu_ops, 240_000_000.0);

        let sketch = estimate_sketch_aggregation(
            SketchAlgorithm::Cms,
            &SketchParams::Cms {
                width: 272,
                depth: 5,
            },
            topk,
        )
        .unwrap();
        assert_eq!(sketch.peak_memory_bytes, 1_092_800_400);
        assert_eq!(sketch.cpu_ops, 96_000_000.0);
    }

    #[test]
    fn sketch_memory_is_monotone_in_width_depth_and_groups() {
        let small = estimate_sketch_aggregation(
            SketchAlgorithm::Cms,
            &SketchParams::Cms {
                width: 10,
                depth: 2,
            },
            inputs(1),
        )
        .unwrap();
        let mut larger_inputs = inputs(1);
        larger_inputs.group_count = 3;
        let large = estimate_sketch_aggregation(
            SketchAlgorithm::Cms,
            &SketchParams::Cms {
                width: 20,
                depth: 4,
            },
            larger_inputs,
        )
        .unwrap();
        assert!(large.peak_memory_bytes > small.peak_memory_bytes);
        assert!(large.cpu_ops > small.cpu_ops);
    }

    #[test]
    fn missing_inputs_and_invalid_calibration_fail_closed() {
        let mut missing = inputs(1);
        missing.input_rows = 0;
        assert_eq!(
            estimate_raw_aggregation(missing),
            Err(AnalyticalCostError::MissingOrZero("input_rows"))
        );
        let mut invalid = calibration();
        invalid.cost_per_cpu_op = f64::NAN;
        assert!(matches!(
            invalid.validate(),
            Err(AnalyticalCostError::InvalidCalibration("cost_per_cpu_op", value)) if value.is_nan()
        ));
    }

    #[test]
    fn ddsketch_without_distribution_never_invents_a_bin_count() {
        assert_eq!(
            estimate_sketch_aggregation(
                SketchAlgorithm::DDSketch,
                &SketchParams::DDSketch { alpha: 0.01 },
                inputs(1),
            ),
            Err(AnalyticalCostError::UnsupportedWithoutDistribution(
                "DDSketch"
            ))
        );
    }

    #[test]
    fn planner_ranks_supported_candidates_by_the_same_calibrated_formula() {
        let model = AnalyticalCostModel {
            inputs: inputs(100),
            calibration: calibration(),
        };
        let intent = AggIntent::Count {
            accuracy: asap_types::types::AccuracyTarget::EpsilonDelta {
                epsilon: 0.01,
                delta: 0.01,
            },
        };
        let ranked = model.rank_candidates(
            &intent,
            &[SketchAlgorithm::CountSketch, SketchAlgorithm::Cms],
        );
        assert_eq!(
            ranked,
            vec![SketchAlgorithm::Cms, SketchAlgorithm::CountSketch]
        );

        let (epsilon, delta) = accuracy_budget(accuracy_target(&intent).unwrap());
        let costs: Vec<f64> = ranked
            .iter()
            .map(|algorithm| {
                let params = model.size_params(algorithm.clone(), &intent, epsilon, delta);
                estimate_sketch_aggregation(algorithm.clone(), &params, model.inputs)
                    .unwrap()
                    .calibrated_cost(&model.calibration)
                    .unwrap()
            })
            .collect();
        assert!(costs[0] < costs[1], "ranked costs were {costs:?}");
    }

    #[test]
    fn physical_operator_formulas_keep_disk_at_scan_and_require_join_stats() {
        let scan = estimate_operator(
            PhysicalOperator::Scan,
            OperatorInputs {
                input_rows: 1_000,
                input_bytes: 64_000,
                output_rows: 1_000,
                output_bytes: 64_000,
                group_count: None,
                key_bytes: None,
                aggregate_value_bytes: None,
                k: None,
                right_rows: None,
                right_bytes: None,
                hash_join_build_side: None,
            },
        )
        .unwrap();
        assert_eq!(scan.scan_bytes, 64_000);

        let topk = estimate_operator(
            PhysicalOperator::TopK,
            OperatorInputs {
                input_rows: 1_000,
                input_bytes: 40_000,
                output_rows: 10,
                output_bytes: 400,
                group_count: None,
                key_bytes: None,
                aggregate_value_bytes: None,
                k: Some(10),
                right_rows: None,
                right_bytes: None,
                hash_join_build_side: None,
            },
        )
        .unwrap();
        assert_eq!(topk.scan_bytes, 0);
        assert_eq!(topk.cpu_ops, 4_000.0);
        assert_eq!(topk.peak_memory_bytes, 400);

        let missing_join_stats = estimate_operator(
            PhysicalOperator::HashJoin,
            OperatorInputs {
                input_rows: 1_000,
                input_bytes: 64_000,
                output_rows: 100,
                output_bytes: 12_800,
                group_count: None,
                key_bytes: None,
                aggregate_value_bytes: None,
                k: None,
                right_rows: None,
                right_bytes: None,
                hash_join_build_side: None,
            },
        );
        assert_eq!(
            missing_join_stats,
            Err(AnalyticalCostError::MissingOrZero("right_rows"))
        );

        let missing_build_side = estimate_operator(
            PhysicalOperator::HashJoin,
            OperatorInputs {
                input_rows: 1_000,
                input_bytes: 64_000,
                output_rows: 100,
                output_bytes: 12_800,
                group_count: None,
                key_bytes: None,
                aggregate_value_bytes: None,
                k: None,
                right_rows: Some(10),
                right_bytes: Some(1_280),
                hash_join_build_side: None,
            },
        );
        assert_eq!(
            missing_build_side,
            Err(AnalyticalCostError::MissingOrZero("hash_join_build_side"))
        );

        let build_left = estimate_operator(
            PhysicalOperator::HashJoin,
            OperatorInputs {
                input_rows: 1_000,
                input_bytes: 64_000,
                output_rows: 100,
                output_bytes: 12_800,
                group_count: None,
                key_bytes: None,
                aggregate_value_bytes: None,
                k: None,
                right_rows: Some(10),
                right_bytes: Some(1_280),
                hash_join_build_side: Some(HashJoinBuildSide::Left),
            },
        )
        .unwrap();
        assert_eq!(build_left.peak_memory_bytes, 64_000);

        let aggregate = estimate_operator(
            PhysicalOperator::HashAggregate,
            OperatorInputs {
                input_rows: 1_000,
                input_bytes: 64_000,
                output_rows: 100,
                output_bytes: 4_000,
                group_count: Some(100),
                key_bytes: Some(16),
                aggregate_value_bytes: Some(24),
                k: None,
                right_rows: None,
                right_bytes: None,
                hash_join_build_side: None,
            },
        )
        .unwrap();
        assert_eq!(aggregate.peak_memory_bytes, 5_600);
    }

    #[test]
    fn recurrence_is_derived_over_a_finite_horizon() {
        use asap_types::workload::{
            QueryRecurrence, RepeatedDemand, RepetitionInterval, TimestampMs,
        };

        assert_eq!(
            evaluations_in_horizon(
                &QueryRecurrence::Repeated(RepeatedDemand::FixedInterval(RepetitionInterval(
                    10_000
                ),)),
                100_000,
                60_000,
            )
            .unwrap(),
            6
        );
        assert_eq!(
            evaluations_in_horizon(
                &QueryRecurrence::Repeated(RepeatedDemand::Scheduled(vec![
                    TimestampMs(99_999),
                    TimestampMs(100_000),
                    TimestampMs(160_000),
                    TimestampMs(160_001),
                ])),
                100_000,
                60_000,
            )
            .unwrap(),
            2
        );
        assert_eq!(
            evaluations_in_horizon(&QueryRecurrence::Unknown, 0, 1_000),
            Err(AnalyticalCostError::InvalidRecurrence)
        );
    }

    #[test]
    fn workload_adapter_rejects_stale_cardinality() {
        use asap_types::workload::{
            DataWorkload, Evidence, EvidenceSource, Predictability, Query, QueryRequirements,
            QueryTimeScope, TimeSelection,
        };
        let data = DataWorkload {
            input_cardinality: Evidence {
                value: Some(1_000),
                source: EvidenceSource::Observed,
                observed_at_ms: Some(100),
                valid_for_ms: Some(10),
            },
            ..DataWorkload::default()
        };
        let query = QueryWorkloadEntry {
            query: Query("SELECT count(*) FROM t".into()),
            requirements: QueryRequirements::default(),
            predictability: Predictability::Unknown,
            recurrence: QueryRecurrence::OneTime {
                invocations: 1,
                execute_at: None,
            },
            time_selection: TimeSelection {
                scope: QueryTimeScope::Unknown,
                lookback: None,
                as_of: None,
            },
        };
        assert_eq!(
            AnalyticalInputs::from_workload(
                PhysicalInputEvidence {
                    input_bytes: 8_000,
                    source_scan_bytes: 8_000,
                    group_count: 1,
                    group_key_bytes: 8,
                },
                &data,
                &query,
                111,
                1_000,
            ),
            Err(AnalyticalCostError::MissingOrStale("input_cardinality"))
        );
    }

    #[test]
    fn workload_adapter_rejects_continuously_ingesting_data() {
        use asap_types::workload::{
            DataWorkload, Evidence, EvidenceSource, Predictability, Query, QueryRequirements,
            QueryTimeScope, TimeSelection,
        };
        let data = DataWorkload {
            arrival: DataArrival::ContinuouslyIngesting,
            input_cardinality: Evidence {
                value: Some(1_000),
                source: EvidenceSource::Observed,
                observed_at_ms: Some(100),
                valid_for_ms: Some(1_000),
            },
            ..DataWorkload::default()
        };
        let query = QueryWorkloadEntry {
            query: Query("SELECT count(*) FROM t".into()),
            requirements: QueryRequirements::default(),
            predictability: Predictability::Unknown,
            recurrence: QueryRecurrence::OneTime {
                invocations: 1,
                execute_at: None,
            },
            time_selection: TimeSelection {
                scope: QueryTimeScope::Unknown,
                lookback: None,
                as_of: None,
            },
        };

        assert_eq!(
            AnalyticalInputs::from_workload(
                PhysicalInputEvidence {
                    input_bytes: 8_000,
                    source_scan_bytes: 8_000,
                    group_count: 1,
                    group_key_bytes: 8,
                },
                &data,
                &query,
                100,
                1_000,
            ),
            Err(AnalyticalCostError::UnsupportedDataArrival(
                DataArrival::ContinuouslyIngesting
            ))
        );
    }

    #[test]
    fn unavailable_complete_dag_keeps_the_raw_plan() {
        use asap_types::pre_asap::agg_intent::default_quantile;
        use asap_types::pre_asap::query_expr::{QueryExpr, Reduction, Source};
        use asap_types::pre_asap::schema::{Column, DataType, Schema};
        use std::rc::Rc;

        let scan = Rc::new(QueryExpr::Scan {
            source: Source::TimeSeries { metric: "m".into() },
            predicates: vec![],
            schema: Schema::with_time_index(
                vec![
                    Column::new("ts", DataType::Timestamp, false),
                    Column::new("value", DataType::Float64, false),
                    Column::new("job", DataType::Utf8, false),
                ],
                0,
                vec![],
            ),
        });
        let root = Rc::new(QueryExpr::Aggregate {
            reduction: Reduction::by(vec![2]),
            measures: vec![default_quantile(0.99)],
            output_names: vec![],
            having: None,
            child: scan,
        });
        let space = crate::replacement::search_workload_with(
            vec![("q", Rc::clone(&root))],
            &crate::replacement::default_strategies(),
        );
        let planned_root = Rc::clone(&space.roots[0].1);
        let mut unavailable_inputs = inputs(10);
        unavailable_inputs.input_rows = 0;
        let selected = space.global_selection(&AnalyticalCostModel {
            inputs: unavailable_inputs,
            calibration: calibration(),
        });
        assert!(
            selected.for_target(&planned_root).unwrap().chosen.is_none(),
            "an unavailable physical DAG must keep the pre-ASAP target"
        );
    }

    #[test]
    fn physical_dag_counts_shared_scan_once_and_uses_live_memory() {
        let input = |input_rows, input_bytes, output_rows, output_bytes| OperatorInputs {
            input_rows,
            input_bytes,
            output_rows,
            output_bytes,
            group_count: None,
            key_bytes: None,
            aggregate_value_bytes: None,
            k: None,
            right_rows: None,
            right_bytes: None,
            hash_join_build_side: None,
        };
        let nodes = vec![
            PhysicalDagNode {
                id: "scan".into(),
                operator: PhysicalOperator::Scan,
                inputs: input(100, 1_000, 100, 1_000),
                children: vec![],
                output_buffer_bytes: 10,
                retained_bytes: 0,
                execution: ExecutionMultiplicity::PerEvaluation,
            },
            PhysicalDagNode {
                id: "left".into(),
                operator: PhysicalOperator::Filter,
                inputs: input(100, 1_000, 40, 400),
                children: vec!["scan".into()],
                output_buffer_bytes: 4,
                retained_bytes: 0,
                execution: ExecutionMultiplicity::PerEvaluation,
            },
            PhysicalDagNode {
                id: "right".into(),
                operator: PhysicalOperator::Filter,
                inputs: input(100, 1_000, 40, 400),
                children: vec!["scan".into()],
                output_buffer_bytes: 4,
                retained_bytes: 0,
                execution: ExecutionMultiplicity::PerEvaluation,
            },
            PhysicalDagNode {
                id: "root".into(),
                operator: PhysicalOperator::Concat,
                inputs: input(80, 800, 80, 800),
                children: vec!["left".into(), "right".into()],
                output_buffer_bytes: 8,
                retained_bytes: 0,
                execution: ExecutionMultiplicity::PerEvaluation,
            },
        ];
        let estimate = estimate_physical_dag(&nodes, "root", 2).unwrap();
        assert_eq!(estimate.cpu_ops, 760.0);
        assert_eq!(estimate.scan_bytes, 2_000);
        // This is neither the sum of every node's memory nor just the largest
        // node: it is the maximum state simultaneously live at the fan-out.
        assert_eq!(estimate.peak_memory_bytes, 24);
    }

    #[test]
    fn unmodeled_summary_lifecycle_operations_fail_closed() {
        use std::rc::Rc;

        use asap_types::post_asap::{SummaryExpr, SummarySchema};
        let leaf = Rc::new(SummaryNode {
            expr: SummaryExpr::KeepPreAsap(Rc::new(
                asap_types::pre_asap::QueryExpr::promql_scalar(1.0),
            )),
            schema: SummarySchema {
                fields: vec![],
                time_index: None,
            },
            guarantee: None,
        });
        let merge = SummaryNode {
            expr: SummaryExpr::SummaryMerge {
                children: vec![Rc::clone(&leaf), Rc::clone(&leaf)],
            },
            schema: leaf.schema.clone(),
            guarantee: None,
        };
        assert_eq!(
            collect_sketches(&merge, &mut HashSet::new(), &mut Vec::new()),
            Err(AnalyticalCostError::UnsupportedSummaryOperation("merge"))
        );
    }

    #[test]
    fn physical_dag_separates_build_once_from_per_evaluation_work() {
        let nodes = vec![
            PhysicalDagNode {
                id: "scan".into(),
                operator: PhysicalOperator::Scan,
                inputs: OperatorInputs {
                    input_rows: 100,
                    input_bytes: 1_000,
                    output_rows: 100,
                    output_bytes: 1_000,
                    group_count: None,
                    key_bytes: None,
                    aggregate_value_bytes: None,
                    k: None,
                    right_rows: None,
                    right_bytes: None,
                    hash_join_build_side: None,
                },
                children: vec![],
                output_buffer_bytes: 10,
                retained_bytes: 0,
                execution: ExecutionMultiplicity::Once,
            },
            PhysicalDagNode {
                id: "state".into(),
                operator: PhysicalOperator::HashAggregate,
                inputs: OperatorInputs {
                    input_rows: 100,
                    input_bytes: 1_000,
                    output_rows: 1,
                    output_bytes: 16,
                    group_count: Some(1),
                    key_bytes: Some(8),
                    aggregate_value_bytes: Some(8),
                    k: None,
                    right_rows: None,
                    right_bytes: None,
                    hash_join_build_side: None,
                },
                children: vec!["scan".into()],
                output_buffer_bytes: 16,
                retained_bytes: 32,
                execution: ExecutionMultiplicity::Once,
            },
            PhysicalDagNode {
                id: "read".into(),
                operator: PhysicalOperator::Limit,
                inputs: OperatorInputs {
                    input_rows: 1,
                    input_bytes: 16,
                    output_rows: 1,
                    output_bytes: 16,
                    group_count: None,
                    key_bytes: None,
                    aggregate_value_bytes: None,
                    k: None,
                    right_rows: None,
                    right_bytes: None,
                    hash_join_build_side: None,
                },
                children: vec!["state".into()],
                output_buffer_bytes: 16,
                retained_bytes: 0,
                execution: ExecutionMultiplicity::PerEvaluation,
            },
        ];
        let estimate = estimate_physical_dag(&nodes, "read", 10).unwrap();
        assert_eq!(estimate.cpu_ops, 210.0);
        assert_eq!(estimate.scan_bytes, 1_000);
    }
}

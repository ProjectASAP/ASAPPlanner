//! Analytical, resource-dimensional cost estimates.
//!
//! This module deliberately keeps CPU work, retained state, and scan I/O
//! separate.  They become one planner objective only through an explicit
//! [`ResourceCalibration`]; without that calibration the dimensional
//! estimate is still useful for explanations, but is not silently comparable.

use std::collections::HashSet;

use serde::{Deserialize, Serialize};

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
    /// Rows actually consumed by a physical Limit, including rows skipped by
    /// OFFSET. This is distinct from `output_rows`.
    pub limit_rows_consumed: Option<u64>,
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
    validate_operator_semantics(operator, &input)?;
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
            let heap_rows = k.min(input.input_rows);
            ResourceEstimate {
                cpu_ops: input.input_rows as f64 * (heap_rows.max(2) as f64).log2().ceil(),
                peak_memory_bytes: checked_bytes(&[
                    heap_rows,
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
                    HashJoinBuildSide::Left => input
                        .input_rows
                        .checked_mul(16)
                        .and_then(|metadata| input.input_bytes.checked_add(metadata)),
                    HashJoinBuildSide::Right => right_rows
                        .checked_mul(16)
                        .and_then(|metadata| right_bytes.checked_add(metadata)),
                }
                .ok_or(AnalyticalCostError::Overflow)?,
                scan_bytes: 0,
            }
        }
        PhysicalOperator::Concat => ResourceEstimate {
            cpu_ops: input.output_rows as f64,
            peak_memory_bytes: per_row_width(input.output_rows, input.output_bytes)?,
            scan_bytes: 0,
        },
        PhysicalOperator::Limit => {
            let consumed = input
                .limit_rows_consumed
                .ok_or(AnalyticalCostError::MissingOrZero("limit_rows_consumed"))?;
            if consumed > input.input_rows || consumed < input.output_rows {
                return Err(AnalyticalCostError::InconsistentOperatorStatistics(
                    "Limit rows consumed must cover its output without exceeding its input",
                ));
            }
            ResourceEstimate {
                cpu_ops: consumed as f64,
                peak_memory_bytes: per_row_width(input.output_rows, input.output_bytes)?,
                scan_bytes: 0,
            }
        }
    };
    if estimate.cpu_ops.is_finite() {
        Ok(estimate)
    } else {
        Err(AnalyticalCostError::Overflow)
    }
}

fn validate_operator_semantics(
    operator: PhysicalOperator,
    input: &OperatorInputs,
) -> Result<(), AnalyticalCostError> {
    let inconsistent = |reason| Err(AnalyticalCostError::InconsistentOperatorStatistics(reason));
    match operator {
        PhysicalOperator::Scan => {
            if input.input_rows != input.output_rows || input.input_bytes != input.output_bytes {
                return inconsistent("Scan input and output edges differ");
            }
        }
        PhysicalOperator::Filter => {
            if input.output_rows > input.input_rows || input.output_bytes > input.input_bytes {
                return inconsistent("Filter output expands its input");
            }
        }
        PhysicalOperator::Project => {
            if input.output_rows != input.input_rows {
                return inconsistent("Project changes row cardinality");
            }
        }
        PhysicalOperator::HashAggregate | PhysicalOperator::Deduplicate => {
            let groups = input
                .group_count
                .filter(|groups| *groups > 0)
                .ok_or(AnalyticalCostError::MissingOrZero("group_count"))?;
            if groups > input.input_rows || input.output_rows != groups {
                return inconsistent("grouped output differs from distinct group cardinality");
            }
        }
        PhysicalOperator::Sort
        | PhysicalOperator::Window
        | PhysicalOperator::PassThrough
        | PhysicalOperator::Concat => {
            if input.input_rows != input.output_rows || input.input_bytes != input.output_bytes {
                return inconsistent("cardinality-preserving operator changes its edge");
            }
        }
        PhysicalOperator::TopK => {
            let k = input
                .k
                .filter(|k| *k > 0)
                .ok_or(AnalyticalCostError::MissingOrZero("k"))?;
            if input.output_rows != input.input_rows.min(k) {
                return inconsistent("Top-K output differs from its cardinality bound");
            }
        }
        PhysicalOperator::Limit => {
            if input.output_rows > input.input_rows {
                return inconsistent("Limit output exceeds its input");
            }
        }
        PhysicalOperator::HashJoin => {}
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
    #[error("required analytical input {0} is missing or zero")]
    MissingOrZero(&'static str),
    #[error("calibration {0} must be finite and non-negative, got {1}")]
    InvalidCalibration(&'static str, f64),
    #[error("at least one calibration coefficient must be positive")]
    ZeroCalibration,
    #[error("analytical arithmetic overflowed")]
    Overflow,
    #[error("invalid physical DAG: {0}")]
    InvalidPhysicalDag(&'static str),
    #[error("inconsistent physical operator statistics: {0}")]
    InconsistentOperatorStatistics(&'static str),
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

    fn unary_inputs(input_rows: u64, output_rows: u64) -> OperatorInputs {
        OperatorInputs {
            input_rows,
            input_bytes: input_rows.saturating_mul(8),
            output_rows,
            output_bytes: output_rows.saturating_mul(8),
            group_count: None,
            key_bytes: None,
            aggregate_value_bytes: None,
            k: None,
            limit_rows_consumed: None,
            right_rows: None,
            right_bytes: None,
            hash_join_build_side: None,
        }
    }

    #[test]
    fn operator_estimator_rejects_contradictory_cardinality_evidence() {
        assert!(matches!(
            estimate_operator(PhysicalOperator::Filter, unary_inputs(10, 11)),
            Err(AnalyticalCostError::InconsistentOperatorStatistics(_))
        ));
        assert!(matches!(
            estimate_operator(PhysicalOperator::Project, unary_inputs(10, 9)),
            Err(AnalyticalCostError::InconsistentOperatorStatistics(_))
        ));

        let mut aggregate = unary_inputs(10, 3);
        aggregate.group_count = Some(2);
        aggregate.key_bytes = Some(8);
        aggregate.aggregate_value_bytes = Some(8);
        assert!(matches!(
            estimate_operator(PhysicalOperator::HashAggregate, aggregate),
            Err(AnalyticalCostError::InconsistentOperatorStatistics(_))
        ));

        let mut topk = unary_inputs(10, 4);
        topk.k = Some(3);
        assert!(matches!(
            estimate_operator(PhysicalOperator::TopK, topk),
            Err(AnalyticalCostError::InconsistentOperatorStatistics(_))
        ));
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
                limit_rows_consumed: None,
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
                limit_rows_consumed: None,
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
                limit_rows_consumed: None,
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
                limit_rows_consumed: None,
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
                limit_rows_consumed: None,
                right_rows: Some(10),
                right_bytes: Some(1_280),
                hash_join_build_side: Some(HashJoinBuildSide::Left),
            },
        )
        .unwrap();
        assert_eq!(build_left.peak_memory_bytes, 80_000);

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
                limit_rows_consumed: None,
                right_rows: None,
                right_bytes: None,
                hash_join_build_side: None,
            },
        )
        .unwrap();
        assert_eq!(aggregate.peak_memory_bytes, 5_600);

        let oversized_topk = estimate_operator(
            PhysicalOperator::TopK,
            OperatorInputs {
                input_rows: 4,
                input_bytes: 160,
                output_rows: 4,
                output_bytes: 160,
                group_count: None,
                key_bytes: None,
                aggregate_value_bytes: None,
                k: Some(1_000),
                limit_rows_consumed: None,
                right_rows: None,
                right_bytes: None,
                hash_join_build_side: None,
            },
        )
        .unwrap();
        assert_eq!(oversized_topk.cpu_ops, 8.0);

        let offset_limit = estimate_operator(
            PhysicalOperator::Limit,
            OperatorInputs {
                input_rows: 1_000_000,
                input_bytes: 40_000_000,
                output_rows: 10,
                output_bytes: 400,
                group_count: None,
                key_bytes: None,
                aggregate_value_bytes: None,
                k: None,
                limit_rows_consumed: Some(900_010),
                right_rows: None,
                right_bytes: None,
                hash_join_build_side: None,
            },
        )
        .unwrap();
        assert_eq!(offset_limit.cpu_ops, 900_010.0);
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
            limit_rows_consumed: None,
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
                    limit_rows_consumed: None,
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
                    limit_rows_consumed: None,
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
                    limit_rows_consumed: Some(1),
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

//! Analytical, resource-dimensional cost estimates (issue #323).
//!
//! This module deliberately keeps CPU work, retained state, and scan I/O
//! separate.  They become one planner objective only through an explicit
//! [`ResourceCalibration`]; without that calibration the dimensional
//! estimate is still useful for explanations, but is not silently comparable.

use asap_types::post_asap::{
    GroupingStrategy, SketchAlgorithm, SketchParams, SummaryFamilyType, SummaryNode,
};
use asap_types::pre_asap::agg_intent::AggIntent;
use serde::{Deserialize, Serialize};

use crate::cost_model::{CostModel, DefaultCostModel};
use crate::replacement::{
    accuracy_budget, accuracy_target, Replacement, ReplacementSubDAG, TargetSubDAG,
};

pub const ANALYTICAL_MODEL_VERSION: &str = "analytical-resource-v1";

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct AnalyticalInputs {
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
    pub k: Option<u64>,
    pub right_rows: Option<u64>,
    pub right_bytes: Option<u64>,
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
        PhysicalOperator::HashAggregate | PhysicalOperator::Deduplicate => {
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
                    key.checked_add(8 + 16)
                        .ok_or(AnalyticalCostError::Overflow)?,
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
            ResourceEstimate {
                cpu_ops: input.input_rows as f64 + right_rows as f64 + input.output_rows as f64,
                peak_memory_bytes: input.input_bytes.min(right_bytes),
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
    #[error("required analytical input {0} is missing or zero")]
    MissingOrZero(&'static str),
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
    Ok(ResourceEstimate {
        cpu_ops: inputs.input_rows as f64 * inputs.evaluation_count as f64 + topk_cpu_ops(inputs),
        peak_memory_bytes: checked_bytes(&[inputs.group_count, group_entry_bytes])?
            .checked_add(topk_memory)
            .ok_or(AnalyticalCostError::Overflow)?,
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
        cpu_ops: inputs.input_rows as f64 * update_ops
            + inputs.evaluation_count as f64 * read_ops * physical_sketch_count as f64
            + topk_cpu_ops(inputs),
        peak_memory_bytes: bytes_per_group
            .checked_mul(physical_sketch_count)
            .and_then(|bytes| bytes.checked_add(keyed_state))
            .and_then(|bytes| bytes.checked_add(topk_heap_bytes(inputs).ok()?))
            .ok_or(AnalyticalCostError::Overflow)?,
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
        let (kind, grouping) =
            sketch_from(node).ok_or(AnalyticalCostError::UnsupportedCandidate)?;
        let inputs = self.inputs.validate()?;
        let physical_sketch_count =
            if matches!(grouping, GroupingStrategy::SharedMultiSubpopulation { .. }) {
                1
            } else {
                inputs.group_count
            };
        estimate_sketch_aggregation_with_instances(
            kind.algorithm().clone(),
            kind.params(),
            inputs,
            physical_sketch_count,
        )
    }

    /// Target-local estimate used for nested plans. An outer Top-K consumes
    /// the grouped child's rows, not the original source rows, and its
    /// intermediate input performs no second source/disk scan.
    pub fn candidate_resources_for_target(
        &self,
        candidate: &ReplacementSubDAG,
        target: &TargetSubDAG<'_>,
    ) -> Result<ResourceEstimate, AnalyticalCostError> {
        if topk_intent(target.root).is_some() {
            let Replacement::Summary(node) = &candidate.replacement else {
                return Err(AnalyticalCostError::UnsupportedCandidate);
            };
            let mut states = Vec::new();
            collect_sketches(node, &mut states);
            if states.is_empty() {
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
        estimate_raw_aggregation(scoped)
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

fn sketch_from(
    node: &SummaryNode,
) -> Option<(&asap_types::post_asap::SketchKind, &GroupingStrategy)> {
    // A caller-visible candidate is normally rooted at `SummaryEstimate`,
    // whose output schema is deliberately plain.  The physical sketch and
    // its sized parameters live on the nested `SummaryAgg`, so inspecting
    // only the root schema silently loses precisely the information this
    // model needs.
    match &node.expr {
        asap_types::post_asap::SummaryExpr::SummaryAgg { family, child, .. } => match family {
            SummaryFamilyType::Sketch(kind, grouping) => Some((kind, grouping)),
            _ => sketch_from(child),
        },
        asap_types::post_asap::SummaryExpr::SummaryJoin {
            family,
            outer,
            inner,
            ..
        } => match family {
            SummaryFamilyType::Sketch(kind, grouping) => Some((kind, grouping)),
            _ => sketch_from(outer).or_else(|| sketch_from(inner)),
        },
        asap_types::post_asap::SummaryExpr::SummaryEstimate { summary_input, .. }
        | asap_types::post_asap::SummaryExpr::SummaryDelete { summary_input, .. } => {
            sketch_from(summary_input)
        }
        asap_types::post_asap::SummaryExpr::SummarySubtract { left, right } => {
            sketch_from(left).or_else(|| sketch_from(right))
        }
        asap_types::post_asap::SummaryExpr::SummaryMerge { children } => {
            children.iter().find_map(|child| sketch_from(child))
        }
        asap_types::post_asap::SummaryExpr::KeepPreAsap(_) => None,
    }
}

fn collect_sketches<'a>(
    node: &'a SummaryNode,
    out: &mut Vec<(&'a asap_types::post_asap::SketchKind, &'a GroupingStrategy)>,
) {
    match &node.expr {
        asap_types::post_asap::SummaryExpr::SummaryAgg { family, child, .. } => {
            collect_sketches(child, out);
            if let SummaryFamilyType::Sketch(kind, grouping) = family {
                out.push((kind, grouping));
            }
        }
        asap_types::post_asap::SummaryExpr::SummaryJoin {
            family,
            outer,
            inner,
            ..
        } => {
            collect_sketches(outer, out);
            collect_sketches(inner, out);
            if let SummaryFamilyType::Sketch(kind, grouping) = family {
                out.push((kind, grouping));
            }
        }
        asap_types::post_asap::SummaryExpr::SummaryEstimate { summary_input, .. }
        | asap_types::post_asap::SummaryExpr::SummaryDelete { summary_input, .. } => {
            collect_sketches(summary_input, out);
        }
        asap_types::post_asap::SummaryExpr::SummarySubtract { left, right } => {
            collect_sketches(left, out);
            collect_sketches(right, out);
        }
        asap_types::post_asap::SummaryExpr::SummaryMerge { children } => {
            for child in children {
                collect_sketches(child, out);
            }
        }
        asap_types::post_asap::SummaryExpr::KeepPreAsap(_) => {}
    }
}

impl CostModel for AnalyticalCostModel {
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
                estimate_sketch_aggregation(algorithm.clone(), &params, self.inputs)
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
        self.candidate_resources_for_target(candidate, target)
            .and_then(|estimate| estimate.calibrated_cost(&self.calibration))
            .unwrap_or(f64::NAN)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn inputs(evaluation_count: u64) -> AnalyticalInputs {
        AnalyticalInputs {
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
        assert_eq!(estimate.cpu_ops, 4_080.0);
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
    fn grouped_topk_accounts_for_keys_hash_entries_and_selection() {
        let mut topk = inputs(100);
        topk.group_count = 100_000;
        topk.group_key_bytes = 32;
        topk.topk_k = Some(10);
        let raw = estimate_raw_aggregation(topk).unwrap();
        assert_eq!(raw.peak_memory_bytes, 5_600_400);
        assert_eq!(raw.cpu_ops, 140_000_000.0);

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
        assert_eq!(sketch.cpu_ops, 95_000_000.0);
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
                k: None,
                right_rows: None,
                right_bytes: None,
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
                k: Some(10),
                right_rows: None,
                right_bytes: None,
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
                k: None,
                right_rows: None,
                right_bytes: None,
            },
        );
        assert_eq!(
            missing_join_stats,
            Err(AnalyticalCostError::MissingOrZero("right_rows"))
        );
    }
}

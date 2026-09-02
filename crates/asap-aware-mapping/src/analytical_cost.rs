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

use crate::cost_model::{CostModel, DefaultCostModel};
use crate::replacement::{Replacement, ReplacementSubDAG, TargetSubDAG};

pub const ANALYTICAL_MODEL_VERSION: &str = "analytical-resource-v1";

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct AnalyticalInputs {
    pub input_rows: u64,
    pub input_bytes: u64,
    pub group_count: u64,
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
        if self.evaluation_count == 0 {
            return Err(AnalyticalCostError::MissingOrZero("evaluation_count"));
        }
        Ok(self)
    }
}

/// Conversion from physical dimensions to one deployment-specific objective.
/// Memory's coefficient means cost units per retained byte over this model's
/// explicit comparison scope; it is not mixed with a rate implicitly.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ResourceCalibration {
    pub cost_per_cpu_op: f64,
    pub cost_per_scan_byte: f64,
    pub cost_per_retained_byte: f64,
    pub version: &'static str,
}

impl ResourceCalibration {
    pub fn validate(self) -> Result<Self, AnalyticalCostError> {
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
        Ok(self)
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ResourceEstimate {
    pub cpu_ops: f64,
    pub peak_memory_bytes: u64,
    pub scan_bytes: u64,
}

impl ResourceEstimate {
    pub fn calibrated_cost(
        self,
        calibration: ResourceCalibration,
    ) -> Result<f64, AnalyticalCostError> {
        let calibration = calibration.validate()?;
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
    Ok(ResourceEstimate {
        cpu_ops: inputs.input_rows as f64 * inputs.evaluation_count as f64,
        peak_memory_bytes: checked_bytes(&[inputs.group_count, 16])?,
        scan_bytes: inputs
            .input_bytes
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

    Ok(ResourceEstimate {
        cpu_ops: inputs.input_rows as f64 * update_ops + inputs.evaluation_count as f64 * read_ops,
        peak_memory_bytes: bytes_per_group
            .checked_mul(inputs.group_count)
            .ok_or(AnalyticalCostError::Overflow)?,
        // One initial build scan; subsequent evaluations read the sketch.
        scan_bytes: inputs.input_bytes,
    })
}

/// Public cost-model adapter. It preserves candidate legality/ranking order,
/// but supplies resource-derived numeric estimates to plan costing/export.
pub struct AnalyticalCostModel {
    pub inputs: AnalyticalInputs,
    pub calibration: ResourceCalibration,
}

impl AnalyticalCostModel {
    pub fn raw_cost(&self) -> Result<f64, AnalyticalCostError> {
        estimate_raw_aggregation(self.inputs)?.calibrated_cost(self.calibration)
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
        let mut inputs = self.inputs.validate()?;
        if matches!(grouping, GroupingStrategy::SharedMultiSubpopulation { .. }) {
            inputs.group_count = 1;
        }
        estimate_sketch_aggregation(kind.algorithm().clone(), kind.params(), inputs)
    }
}

fn sketch_from(
    node: &SummaryNode,
) -> Option<(&asap_types::post_asap::SketchKind, &GroupingStrategy)> {
    node.schema
        .fields
        .iter()
        .find_map(|field| match &field.dtype {
            SummaryFamilyType::Sketch(kind, grouping) => Some((kind, grouping)),
            _ => None,
        })
}

impl CostModel for AnalyticalCostModel {
    fn rank_candidates(
        &self,
        intent: &AggIntent,
        candidates: &[SketchAlgorithm],
    ) -> Vec<SketchAlgorithm> {
        DefaultCostModel.rank_candidates(intent, candidates)
    }

    fn estimated_subpopulation_count(
        &self,
        _target: &asap_types::pre_asap::QueryExpr,
    ) -> Option<usize> {
        usize::try_from(self.inputs.group_count).ok()
    }

    fn estimate_cost(&self, candidate: &ReplacementSubDAG, _target: &TargetSubDAG<'_>) -> f64 {
        self.candidate_resources(candidate)
            .and_then(|estimate| estimate.calibrated_cost(self.calibration))
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
            group_count: 1,
            evaluation_count,
        }
    }

    fn calibration() -> ResourceCalibration {
        ResourceCalibration {
            cost_per_cpu_op: 1e-6,
            cost_per_scan_byte: 1e-8,
            cost_per_retained_byte: 1e-9,
            version: "test-calibration-v1",
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
                group_count: 2,
                evaluation_count: 10,
            },
        )
        .unwrap();
        assert_eq!(estimate.cpu_ops, 4_040.0);
        assert_eq!(estimate.peak_memory_bytes, 6_400);
        assert_eq!(estimate.scan_bytes, 8_000);
    }

    #[test]
    fn repeated_raw_scans_make_a_more_complex_sketch_plan_cheaper() {
        let raw = estimate_raw_aggregation(inputs(100))
            .unwrap()
            .calibrated_cost(calibration())
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
        .calibrated_cost(calibration())
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
}

//! DDSketch relative value error under its estimator/domain contract.
use super::*;

pub(super) fn guarantee(params: &SketchParams) -> Option<AccuracyGuarantee> {
    let SketchParams::DDSketch { alpha } = params else {
        return None;
    };
    Some(super::bounded_guarantee(
        AccuracyMetric::Relative,
        *alpha,
        ProbabilityExpr::Zero,
    ))
}

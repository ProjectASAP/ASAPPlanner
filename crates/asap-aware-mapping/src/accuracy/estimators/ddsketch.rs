//! DDSketch relative value error under its estimator/domain contract.
use super::*;

pub(super) fn guarantee(
    algorithm: &SketchAlgorithm,
    params: &SketchParams,
    query: &SketchQuery,
) -> Option<ResultGuarantee> {
    let SketchParams::DDSketch { alpha } = params else {
        return None;
    };
    Some(super::bounded_guarantee(
        algorithm,
        params,
        query,
        ErrorMetric::RelativeValue,
        *alpha,
        ProbabilityExpr::Zero,
        "ddsketch_relative_error_alpha_v1",
    ))
}

pub(super) fn size_params(epsilon: f64) -> SketchParams {
    SketchParams::DDSketch { alpha: epsilon }
}

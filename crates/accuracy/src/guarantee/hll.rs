//! HLL's relative standard error, without a failure probability.

use super::*;

pub(super) fn generic_guarantee(
    algorithm: &SketchAlgorithm,
    params: &SketchParams,
    query: &SketchStatistic,
) -> Option<AccuracyGuarantee> {
    let SketchParams::Hll { precision, .. } = params else {
        return None;
    };
    Some(super::bounded_guarantee(
        algorithm,
        params,
        query,
        AccuracyMetric::Relative,
        relative_standard_error(*precision),
        ProbabilityExpr::Unknown {
            statistic: "hll_estimator_failure_probability".into(),
        },
        "generic_hll_rse_only_no_confidence_v1",
    ))
}
/// HLL's relative standard error, `1.04 / √m` for `m = 2^precision` buckets.
fn relative_standard_error(precision: u8) -> f64 {
    1.04 / 2f64.powi(i32::from(precision)).sqrt()
}

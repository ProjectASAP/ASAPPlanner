//! DDSketch relative value error under its estimator/domain contract:
//! the bound holds only over values of one sign (`accuracy-models.md`,
//! "Sign conditions"), as the sketch's buckets around 0 have no relative bound.
use super::*;

/// DDSketch's guarantee at `params`: relative error α, failing never when
/// `state_sign`, the sign of the values the state was updated with, is
/// one sign ([`ValueSign::is_one_sign`]); else with an unknown failure
/// probability, so its readout serves no query with an accuracy target.
/// `None` for an α outside (0, 1), which no sketch is built with (α = 0
/// would claim an exact quantile).
///
/// Example: `quantile_over_time(0.99, latency[1h])` with `latency`
/// declared non-negative holds at α; over a gauge of unknown sign its
/// failure probability is unknown.
pub(super) fn guarantee(
    params: &SketchParams,
    state_sign: Option<ValueSign>,
) -> Option<AccuracyGuarantee> {
    let SketchParams::DDSketch { alpha } = params else {
        return None;
    };
    if !(*alpha > 0.0 && *alpha < 1.0) {
        return None;
    }
    let failure_probability = match state_sign.is_some_and(ValueSign::is_one_sign) {
        true => ProbabilityExpr::Zero,
        false => ProbabilityExpr::Unknown {
            statistic: "ddsketch_values_of_unknown_sign".into(),
        },
    };
    Some(super::bounded_guarantee(
        AccuracyMetric::Relative,
        *alpha,
        failure_probability,
    ))
}

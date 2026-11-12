//! Dispatch committed estimator parameters to their accuracy models.
use asap_types::accuracy::{AccuracyGuarantee, AccuracyMetric, BoundExpr, ProbabilityExpr};
use asap_types::ir::schema::FieldDataType;
use asap_types::summary_type::{SketchParams, SketchStatistic};

use asap_types::summary_type::SummaryTypeCapability;

use crate::within_limit;

mod binomial;
mod cardinality;
mod cms;
mod count_sketch;
mod ddsketch;
mod hll;
mod kll;
mod univmon;

use binomial::binomial_upper_tail;

/// The guarantee of θ for `query`: where one parameter sets both the error
/// and the failure probability (KLL's and KMV's `k`), the one that holds at
/// `failure_probability`; where structural parameters set them apart (a
/// Count-Min's width and depth, HLL's precision), the one θ fixes, whose
/// failure probability may exceed `failure_probability` or be unknown
/// (HLL's). A Hydra state reads each subpopulation's state, so it has
/// that state's guarantee.
fn guarantee_of_parameters(
    params: &SketchParams,
    query: &SketchStatistic,
    failure_probability: f64,
) -> Option<AccuracyGuarantee> {
    match params {
        SketchParams::Kll { .. } => kll::guarantee(params, failure_probability),
        SketchParams::Kmv { .. } => cardinality::guarantee(params, failure_probability),
        SketchParams::Hydra(hydra) => {
            guarantee_of_parameters(&hydra.per_subpopulation(), query, failure_probability)
        }
        SketchParams::DDSketch { .. } => ddsketch::guarantee(params),
        SketchParams::Hll { .. } => hll::guarantee(params),
        SketchParams::Cms { .. } | SketchParams::CmsWithHeap { .. } => cms::guarantee(params),
        SketchParams::CountSketch { .. } | SketchParams::CountSketchWithHeap { .. } => {
            count_sketch::guarantee(params)
        }
        SketchParams::UnivMon { .. } => univmon::guarantee(params, query),
    }
}

/// The guarantee [`theoretical_guarantee`] gives, before it checks that θ
/// fails at most `failure_probability` rarely: its metric, and whether its
/// failure probability is known at all. `None` when the summary type does
/// not answer `statistic` or has no error model for it.
pub(crate) fn guarantee_before_failure_probability_check(
    summary_type: &FieldDataType,
    parameters: &SketchParams,
    statistic: &SketchStatistic,
    failure_probability: f64,
) -> Option<AccuracyGuarantee> {
    if !matches!(summary_type, FieldDataType::Sketch(_))
        || !SummaryTypeCapability::of(summary_type).answers(statistic)
    {
        return None;
    }
    guarantee_of_parameters(parameters, statistic, failure_probability)
}

/// The guarantee of an error at most `bound` in `metric`, failing with
/// probability at most `failure_probability`.
fn bounded_guarantee(
    metric: AccuracyMetric,
    bound: f64,
    failure_probability: ProbabilityExpr,
) -> AccuracyGuarantee {
    AccuracyGuarantee::new(
        metric,
        BoundExpr::Constant { value: bound },
        failure_probability,
    )
}

/// The theoretical error bound of reading `statistic` from a state of
/// `summary_type` with parameters `parameters` at failure probability
/// `failure_probability` (the guarantee's own failure probability): the
/// algorithm's error formula (concepts doc, section 2, "Where the guarantee
/// comes from"). Where one parameter sets both the error and the failure
/// probability (KLL's and KMV's `k`), the error is the one that holds at
/// `failure_probability`; where structural parameters set them apart (a
/// Count-Min's width and depth), the bound holds only when θ fails at most
/// that rarely. `None` when this θ cannot fail that rarely, when the
/// summary type does not answer `statistic` (its capability row) or has no
/// error model (`SummarySample`, `Wavelet`, `StatModel`, and sketch
/// readouts without a certified bound).
pub fn theoretical_guarantee(
    summary_type: &FieldDataType,
    parameters: &SketchParams,
    statistic: &SketchStatistic,
    failure_probability: f64,
) -> Option<AccuracyGuarantee> {
    guarantee_before_failure_probability_check(
        summary_type,
        parameters,
        statistic,
        failure_probability,
    )
    .filter(|guarantee| holds_at_failure_probability(guarantee, failure_probability))
}

/// Whether `guarantee` fails at most `failure_probability` rarely: its own
/// failure probability is known and within it ([`within_limit`]).
pub(crate) fn holds_at_failure_probability(
    guarantee: &AccuracyGuarantee,
    failure_probability: f64,
) -> bool {
    guarantee
        .spec
        .failure_probability
        .evaluate()
        .is_some_and(|own| within_limit(own, failure_probability))
}

#[cfg(test)]
mod tests;

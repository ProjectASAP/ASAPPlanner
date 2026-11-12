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
mod tests {
    use super::*;
    use asap_types::ir::scalar::expr_ir::ColumnRef;
    use asap_types::summary_type::SketchAlgorithm;

    /// Every listed θ has a theoretical guarantee for the statistic its
    /// algorithm is listed for, at failure probability 0.01, in the
    /// algorithm's metric; along a list of one structure the error never
    /// grows (the lists run weakest first).
    #[test]
    fn every_listed_parameters_has_a_guarantee_at_one_percent() {
        use asap_types::summary_type::parameter_list;
        let point = SketchStatistic::PointCount {
            key: ColumnRef::SampleValue,
            value: Some("1".into()),
        };
        for (algorithm, statistic, metric) in [
            (
                SketchAlgorithm::Kll,
                SketchStatistic::Quantile { q: 0.5 },
                AccuracyMetric::Rank,
            ),
            (
                SketchAlgorithm::DDSketch,
                SketchStatistic::Quantile { q: 0.5 },
                AccuracyMetric::Relative,
            ),
            (
                SketchAlgorithm::Kmv,
                SketchStatistic::Cardinality,
                AccuracyMetric::Relative,
            ),
            (
                SketchAlgorithm::Cms,
                point.clone(),
                AccuracyMetric::ABSOLUTE_STREAM_L1,
            ),
            (
                SketchAlgorithm::CountSketch,
                point,
                AccuracyMetric::ABSOLUTE_STREAM_L2,
            ),
            (
                SketchAlgorithm::CmsWithHeap,
                SketchStatistic::TopK { k: 10 },
                AccuracyMetric::ABSOLUTE_STREAM_L1,
            ),
        ] {
            let summary_type = FieldDataType::Sketch(algorithm);
            let errors: Vec<f64> = parameter_list(algorithm)
                .iter()
                .map(|parameters| {
                    let guarantee =
                        theoretical_guarantee(&summary_type, parameters, &statistic, 0.01)
                            .unwrap_or_else(|| panic!("{algorithm:?} {parameters:?}"));
                    assert_eq!(guarantee.spec.accuracy_metric, metric, "{algorithm:?}");
                    guarantee.spec.error.evaluate().unwrap()
                })
                .collect();
            assert!(!errors.is_empty(), "{algorithm:?}");
            if matches!(
                algorithm,
                SketchAlgorithm::Kll | SketchAlgorithm::DDSketch | SketchAlgorithm::Kmv
            ) {
                assert!(
                    errors.windows(2).all(|pair| pair[1] <= pair[0]),
                    "{algorithm:?}: {errors:?}"
                );
            }
        }
    }

    /// One parameter sets both (KLL's k): its rank error grows as the
    /// required failure probability shrinks. Separate parameters (CMS width
    /// and depth): the bound holds only when the depth fails rarely enough.
    #[test]
    fn a_guarantee_is_evaluated_at_the_required_failure_probability() {
        let quantile = SketchStatistic::Quantile { q: 0.5 };
        let kll = FieldDataType::Sketch(SketchAlgorithm::Kll);
        let k = SketchParams::Kll { k: 547 };
        let error_at = |delta| {
            theoretical_guarantee(&kll, &k, &quantile, delta)
                .unwrap()
                .spec
                .error
                .evaluate()
                .unwrap()
        };
        assert_eq!(error_at(1.0), error_at(0.01));
        assert!(error_at(1e-6) > error_at(0.01));
        let cms = FieldDataType::Sketch(SketchAlgorithm::Cms);
        let point = SketchStatistic::PointCount {
            key: ColumnRef::SampleValue,
            value: None,
        };
        let depth = |depth| SketchParams::Cms { width: 272, depth };
        assert!(theoretical_guarantee(&cms, &depth(5), &point, 1e-2).is_some());
        assert!(theoretical_guarantee(&cms, &depth(5), &point, 1e-4).is_none());
        assert!(theoretical_guarantee(&cms, &depth(10), &point, 1e-4).is_some());
        // A statistic the summary type does not answer has no guarantee.
        assert!(theoretical_guarantee(&kll, &k, &SketchStatistic::Cardinality, 0.01).is_none());
    }
}

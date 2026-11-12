//! Dispatch committed estimator parameters to their accuracy models.
use asap_types::accuracy::{
    AccuracyGuarantee, AccuracyMetric, BoundExpr, GuaranteeSource, ProbabilityExpr,
};
use asap_types::ir::schema::FieldDataType;
use asap_types::summary_type::{SketchAlgorithm, SketchParams, SketchStatistic};

use asap_types::summary_type::SummaryTypeCapability;

use crate::target_limit;

mod binomial;
pub(crate) mod cardinality;
pub(crate) mod cms;
pub(crate) mod count_sketch;
pub(crate) mod ddsketch;
pub(crate) mod hll;
pub(crate) mod kll;
pub(crate) mod univmon;

use binomial::binomial_upper_tail;

/// The guarantee of θ for `query`: where one parameter sets both the error
/// and the failure probability (KLL's and KMV's `k`), the one that holds at
/// `failure_probability`; where structural parameters set them apart (a
/// Count-Min's width and depth, HLL's precision), the one θ fixes, whose
/// failure probability may exceed `failure_probability` or be unknown
/// (HLL's). A Hydra state reads each subpopulation's state, so it has
/// that state's guarantee.
fn guarantee_of_parameters(
    algorithm: &SketchAlgorithm,
    params: &SketchParams,
    query: &SketchStatistic,
    failure_probability: f64,
) -> Option<AccuracyGuarantee> {
    match params {
        SketchParams::Kll { .. } => kll::guarantee(algorithm, params, query, failure_probability),
        SketchParams::Kmv { .. } => {
            cardinality::guarantee(algorithm, params, query, failure_probability)
        }
        SketchParams::Hydra(hydra) => guarantee_of_parameters(
            algorithm,
            &hydra.per_subpopulation(),
            query,
            failure_probability,
        ),
        SketchParams::DDSketch { .. } => ddsketch::guarantee(algorithm, params, query),
        SketchParams::Hll { .. } => hll::generic_guarantee(algorithm, params, query),
        SketchParams::Cms { .. } | SketchParams::CmsWithHeap { .. } => {
            cms::guarantee(algorithm, params, query)
        }
        SketchParams::CountSketch { .. } | SketchParams::CountSketchWithHeap { .. } => {
            count_sketch::guarantee(algorithm, params, query)
        }
        SketchParams::UnivMon { .. } => univmon::guarantee(algorithm, params, query),
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
    let FieldDataType::Sketch(algorithm) = summary_type else {
        return None;
    };
    if !SummaryTypeCapability::of(summary_type).answers(statistic) {
        return None;
    }
    guarantee_of_parameters(algorithm, parameters, statistic, failure_probability)
}

fn bounded_guarantee(
    algorithm: &SketchAlgorithm,
    params: &SketchParams,
    query: &SketchStatistic,
    metric: AccuracyMetric,
    bound: f64,
    delta: ProbabilityExpr,
    contract: &str,
) -> AccuracyGuarantee {
    AccuracyGuarantee::new(
        metric,
        BoundExpr::Constant { value: bound },
        delta,
        vec![GuaranteeSource::SketchEvaluation {
            algorithm: format!("{algorithm:?}"),
            contract: contract.into(),
            params: serde_json::to_value(params).unwrap_or(serde_json::Value::Null),
            query: format!("{query:?}"),
        }],
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
    .filter(|guarantee| {
        guarantee
            .spec
            .failure_probability
            .evaluate()
            .is_some_and(|own| own <= target_limit(failure_probability))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use asap_types::ir::scalar::expr_ir::ColumnRef;

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

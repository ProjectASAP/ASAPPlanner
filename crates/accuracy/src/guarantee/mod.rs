//! Dispatch committed estimator parameters to their accuracy models.
use asap_types::accuracy::{
    AccuracyGuarantee, AccuracyMetric, BoundExpr, GuaranteeSource, ProbabilityExpr,
};
use asap_types::ir::schema::FieldDataType;
use asap_types::summary_type::{SketchAlgorithm, SketchParams, SketchStatistic};

use asap_types::summary_type::SummaryTypeCapability;

pub(crate) mod cardinality;
pub(crate) mod cms;
pub(crate) mod count_sketch;
pub(crate) mod ddsketch;
pub mod hll;
pub(crate) mod kll;
pub mod univmon;

/// The guarantee of reading `query` from a state of `algorithm` with
/// parameters `params` at failure probability `failure_probability`
/// ([`theoretical_guarantee`], without its capability check). A Hydra
/// state reads each subpopulation's state, so it has that state's
/// guarantee.
fn sketch_guarantee(
    algorithm: &SketchAlgorithm,
    params: &SketchParams,
    query: &SketchStatistic,
    failure_probability: f64,
) -> Option<AccuracyGuarantee> {
    let fixed = match params {
        SketchParams::Kll { .. } => {
            return kll::guarantee(algorithm, params, query, failure_probability)
        }
        SketchParams::Kmv { .. } => {
            return cardinality::guarantee(algorithm, params, query, failure_probability)
        }
        SketchParams::Hydra(hydra) => {
            return sketch_guarantee(
                algorithm,
                &hydra.per_subpopulation(),
                query,
                failure_probability,
            )
        }
        SketchParams::DDSketch { .. } => ddsketch::guarantee(algorithm, params, query),
        SketchParams::Hll { .. } => hll::generic_guarantee(algorithm, params, query),
        SketchParams::Cms { .. } | SketchParams::CmsWithHeap { .. } => {
            cms::guarantee(algorithm, params, query)
        }
        SketchParams::CountSketch { .. } | SketchParams::CountSketchWithHeap { .. } => {
            count_sketch::guarantee(algorithm, params, query)
        }
        SketchParams::UnivMon { .. } => univmon::guarantee(algorithm, params, query),
    };
    // Structural parameters set the failure probability: the bound holds
    // only when θ fails at most that rarely.
    fixed.filter(|guarantee| {
        guarantee
            .spec
            .failure_probability
            .evaluate()
            .is_some_and(|own| own <= target_limit(failure_probability))
    })
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
    let FieldDataType::Sketch(algorithm) = summary_type else {
        return None;
    };
    if !SummaryTypeCapability::of(summary_type).answers(statistic) {
        return None;
    }
    sketch_guarantee(algorithm, parameters, statistic, failure_probability)
}

/// Small relative tolerance for comparing an evaluated bound against a
/// target, so a parameter sized by `⌈·⌉` to *exactly* meet ε is not rejected
/// by floating-point noise.
const SATISFACTION_TOLERANCE: f64 = 1e-9;

/// The largest evaluated error (or failure probability) that meets a
/// required `limit`: `limit` widened by [`SATISFACTION_TOLERANCE`], so a
/// bound equal to it up to floating-point noise meets it.
pub fn target_limit(limit: f64) -> f64 {
    limit * (1.0 + SATISFACTION_TOLERANCE) + f64::EPSILON
}

/// `P(Binomial(n, p) ≥ k)`, summed term by term in log space and rounded
/// up by a relative 1e-12 so rounding never certifies a smaller tail.
pub(crate) fn binomial_upper_tail(n: u32, p: f64, k: u32) -> f64 {
    if k == 0 || p >= 1.0 {
        return 1.0;
    }
    if k > n || p <= 0.0 {
        return 0.0;
    }
    let (ln_p, ln_q) = (p.ln(), (-p).ln_1p());
    let mut ln_choose = 0.0;
    let mut tail = 0.0;
    for i in 0..=n {
        if i > 0 {
            ln_choose += f64::from(n - i + 1).ln() - f64::from(i).ln();
        }
        if i >= k {
            tail += (ln_choose + f64::from(i) * ln_p + f64::from(n - i) * ln_q).exp();
        }
    }
    (tail * (1.0 + 1e-12)).min(1.0)
}

/// Whether `guarantee`'s evaluated error is within `max_error` and, when
/// `failure_probability` is set, its evaluated failure probability within
/// it ([`target_limit`]); unknown values do not meet.
#[cfg(test)]
fn meets(guarantee: &AccuracyGuarantee, max_error: f64, failure_probability: Option<f64>) -> bool {
    let within = |value: Option<f64>, limit: f64| value.is_some_and(|v| v <= target_limit(limit));
    within(guarantee.spec.error.evaluate(), max_error)
        && failure_probability
            .is_none_or(|delta| within(guarantee.spec.failure_probability.evaluate(), delta))
}

#[cfg(test)]
mod tests {
    use super::*;
    use asap_types::ir::scalar::expr_ir::ColumnRef;

    /// A bound equal to the limit up to floating-point noise meets it; a
    /// bound one part in a million above it does not.
    #[test]
    fn target_limit_admits_only_floating_point_noise() {
        assert!(0.01 * (1.0 + 1e-12) <= target_limit(0.01));
        assert!(0.01 * (1.0 + 1e-6) > target_limit(0.01));
        assert!(0.0 <= target_limit(0.0));
    }

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

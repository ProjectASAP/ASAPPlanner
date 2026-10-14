//! KLL normalized rank error at the registered empirical 99% calibration
//! and, below a 0.01 failure probability, that calibration scaled by the KLL
//! bound's dependence on the failure probability.
use super::*;
use asap_types::post_asap::sketch::KLL_MAX_K;

pub(super) fn guarantee(
    algorithm: &SketchAlgorithm,
    params: &SketchParams,
    query: &SketchStatistic,
) -> Option<ResultGuarantee> {
    let SketchParams::Kll {
        k,
        failure_probability,
    } = params
    else {
        return None;
    };
    if *k > KLL_MAX_K {
        return None;
    }
    let delta = failure_probability.unwrap_or(KLL_CALIBRATED_FAILURE_PROBABILITY);
    let scale = delta_scale(delta)?;
    let contract = if delta < KLL_CALIBRATED_FAILURE_PROBABILITY {
        KLL_DELTA_SCALED_CONTRACT
    } else {
        KLL_EMPIRICAL_99_CONTRACT
    };
    Some(super::bounded_guarantee(
        algorithm,
        params,
        query,
        ErrorMetric::Rank,
        kll_rank_error_99(*k) * scale,
        ProbabilityExpr::Constant { value: delta },
        contract,
    ))
}
/// The empirical 99% calibration of the pinned Apache DataSketches KLL.
const KLL_EMPIRICAL_99_CONTRACT: &str = "apache_datasketches_kll_empirical_99_a9b42755072b";
/// That calibration scaled to a failure probability below 0.01 by the KLL
/// bound's dependence on it ([`delta_scale`]): a model, not a measurement,
/// so it is named apart from the calibration.
const KLL_DELTA_SCALED_CONTRACT: &str =
    "apache_datasketches_kll_empirical_99_delta_scaled_a9b42755072b";
pub(crate) const KLL_RANK_ERROR_COEFFICIENT_99: f64 = 2.296;
pub(crate) const KLL_RANK_ERROR_EXPONENT_99: f64 = 0.9723;
/// The failure probability of the empirical 99% calibration.
pub(crate) const KLL_CALIBRATED_FAILURE_PROBABILITY: f64 = 0.01;

pub(crate) fn kll_rank_error_99(k: u32) -> f64 {
    KLL_RANK_ERROR_COEFFICIENT_99 / f64::from(k).powf(KLL_RANK_ERROR_EXPONENT_99)
}

/// Invert Apache DataSketches' empirical 99th-percentile, single-sided KLL
/// normalized rank-error fit: `epsilon = 2.296 / k^0.9723`. Capped at
/// [`KLL_MAX_K`], the largest `k` `asap_sketchlib` builds; an `eps` that
/// needs more gets that `k`, whose guarantee then misses the target.
pub(crate) fn kll_k(eps: f64) -> u32 {
    saturating_ceil(
        (KLL_RANK_ERROR_COEFFICIENT_99 / eps).powf(1.0 / KLL_RANK_ERROR_EXPONENT_99),
        8,
        KLL_MAX_K,
    )
}

/// How much the rank error at failure probability `delta` exceeds the error
/// at the calibrated 0.01: Karnin, Lang & Liberty (FOCS 2016) bound the
/// error at `delta` by `O(sqrt(ln(1/delta)) / k)`, so the factor is
/// `sqrt(ln(1/delta) / ln(1/0.01))`, with the constant taken from the 99%
/// calibration. 1 for `delta >= 0.01`; `None` unless `0 < delta < 1`.
fn delta_scale(delta: f64) -> Option<f64> {
    if !(delta > 0.0 && delta < 1.0) {
        None
    } else if delta >= KLL_CALIBRATED_FAILURE_PROBABILITY {
        Some(1.0)
    } else {
        Some((delta.ln() / KLL_CALIBRATED_FAILURE_PROBABILITY.ln()).sqrt())
    }
}

/// `k` for rank error `eps` at failure probability `delta`: the empirical
/// 99% calibration for `delta >= 0.01`; below, the calibration at
/// `eps / delta_scale(delta)`, recording `delta` so the guarantee reports it.
pub(crate) fn size_params(eps: f64, delta: f64) -> SketchParams {
    if delta.is_nan() || delta <= 0.0 || delta >= KLL_CALIBRATED_FAILURE_PROBABILITY {
        return SketchParams::Kll {
            k: kll_k(eps),
            failure_probability: None,
        };
    }
    SketchParams::Kll {
        k: kll_k(eps / delta_scale(delta).unwrap_or(1.0)),
        failure_probability: Some(delta),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_guarantee_inverts_rank_sizing() {
        use crate::realization::default_size_params;
        use asap_types::post_asap::{GroupingStrategy, SketchKind};
        use asap_types::pre_asap::agg_intent::default_quantile;
        let q = default_quantile(0.99);
        let params = default_size_params(SketchAlgorithm::Kll, &q, 0.01, 0.01);
        let g = DefaultAccuracyModel
            .local_guarantee(
                &FieldDataType::Sketch(
                    SketchKind::new(SketchAlgorithm::Kll, params),
                    GroupingStrategy::default(),
                ),
                &SketchStatistic::Quantile { q: 0.99 },
            )
            .unwrap();
        assert_eq!(g.metric, ErrorMetric::Rank);
        assert!(DefaultAccuracyModel.satisfies(&g, &AccuracyTarget::Epsilon(0.01)));
        assert_eq!(g.failure_probability.evaluate(), Some(0.01));
        assert!(DefaultAccuracyModel.satisfies(
            &g,
            &AccuracyTarget::EpsilonDelta {
                epsilon: 0.01,
                delta: 0.01,
            }
        ));
        assert_eq!(g.approximate_layer_count(), 1);
        assert!(g.provenance.iter().any(|source| matches!(
            source,
            GuaranteeSource::SketchEvaluation { contract, .. }
                if contract == "apache_datasketches_kll_empirical_99_a9b42755072b"
        )));
    }

    /// Below δ = 0.01, KLL is sized by the KLL bound's δ dependence: k is
    /// larger than at δ = 0.01, and the guarantee meets ε = 0.01, δ = 0.001.
    #[test]
    fn sizing_meets_a_failure_probability_below_one_percent() {
        use crate::realization::default_size_params;
        use asap_types::pre_asap::agg_intent::default_quantile;
        let q = default_quantile(0.99);
        let k = |delta| match default_size_params(SketchAlgorithm::Kll, &q, 0.01, delta) {
            SketchParams::Kll { k, .. } => k,
            other => panic!("{other:?}"),
        };
        assert!(k(0.001) > k(0.01), "{} vs {}", k(0.001), k(0.01));
        let params = default_size_params(SketchAlgorithm::Kll, &q, 0.01, 0.001);
        let g = guarantee(
            &SketchAlgorithm::Kll,
            &params,
            &SketchStatistic::Quantile { q: 0.99 },
        )
        .unwrap();
        assert!(DefaultAccuracyModel.satisfies(
            &g,
            &AccuracyTarget::EpsilonDelta {
                epsilon: 0.01,
                delta: 0.001,
            }
        ));
        assert_eq!(g.failure_probability.evaluate(), Some(0.001));
    }

    /// A KLL sized below δ = 0.01 names its own contract, not the empirical
    /// 99% calibration's.
    #[test]
    fn sizing_below_one_percent_names_a_distinct_contract() {
        let contract = |delta| {
            let g = guarantee(
                &SketchAlgorithm::Kll,
                &size_params(0.01, delta),
                &SketchStatistic::Quantile { q: 0.99 },
            )
            .unwrap();
            match &g.provenance[..] {
                [GuaranteeSource::SketchEvaluation { contract, .. }] => contract.clone(),
                other => panic!("{other:?}"),
            }
        };
        assert_eq!(
            contract(0.01),
            "apache_datasketches_kll_empirical_99_a9b42755072b"
        );
        assert_eq!(
            contract(0.001),
            "apache_datasketches_kll_empirical_99_delta_scaled_a9b42755072b"
        );
    }

    /// `k` never exceeds the 26 602 `asap_sketchlib` silently caps it at, so
    /// a guarantee never claims a larger `k` than runs.
    #[test]
    fn kll_k_is_capped_at_the_library_maximum() {
        assert_eq!(kll_k(1e-6), KLL_MAX_K);
        assert_eq!(KLL_MAX_K, 26_602);
    }

    /// A hand-built `k` above the library's cap gives no guarantee.
    #[test]
    fn a_k_above_the_library_cap_gives_no_guarantee() {
        let g = |k| {
            guarantee(
                &SketchAlgorithm::Kll,
                &SketchParams::Kll {
                    k,
                    failure_probability: None,
                },
                &SketchStatistic::Quantile { q: 0.99 },
            )
        };
        assert!(g(KLL_MAX_K).is_some());
        assert!(g(KLL_MAX_K + 1).is_none());
    }

    /// A hand-built failure probability outside (0, 1) gives no guarantee
    /// rather than an infinite or NaN error bound.
    #[test]
    fn an_invalid_failure_probability_gives_no_guarantee() {
        for delta in [0.0, -0.1, 1.0, 1.5, f64::NAN, f64::INFINITY] {
            let params = SketchParams::Kll {
                k: 200,
                failure_probability: Some(delta),
            };
            assert!(
                guarantee(
                    &SketchAlgorithm::Kll,
                    &params,
                    &SketchStatistic::Quantile { q: 0.99 },
                )
                .is_none(),
                "{delta}"
            );
        }
    }
}

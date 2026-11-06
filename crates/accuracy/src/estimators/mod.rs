//! Dispatch committed estimator parameters to their accuracy models.
use super::*;
use asap_types::ir::operator::AggIntent;
use asap_types::ir::schema::FamilyCapability;

pub(crate) mod cardinality;
pub(crate) mod cms;
pub(crate) mod count_sketch;
pub(crate) mod ddsketch;
pub mod hll;
pub(crate) mod kll;
pub mod univmon;

#[allow(dead_code)] // Read again for the error bound of each listed θ (`parameter_list`), in #1207 PRs 3–4.
pub(super) fn sketch_guarantee(
    algorithm: &SketchAlgorithm,
    params: &SketchParams,
    query: &SketchStatistic,
) -> Option<AccuracyGuarantee> {
    match params {
        SketchParams::Kll { .. } => kll::guarantee(algorithm, params, query),
        SketchParams::DDSketch { .. } => ddsketch::guarantee(algorithm, params, query),
        SketchParams::Hll { .. } => hll::generic_guarantee(algorithm, params, query),
        SketchParams::Cms { .. } | SketchParams::CmsWithHeap { .. } => {
            cms::guarantee(algorithm, params, query)
        }
        SketchParams::CountSketch { .. } | SketchParams::CountSketchWithHeap { .. } => {
            count_sketch::guarantee(algorithm, params, query)
        }
        SketchParams::Kmv { .. } => cardinality::guarantee(algorithm, params, query),
        SketchParams::UnivMon { .. } => univmon::guarantee(algorithm, params, query),
        SketchParams::Hydra(hydra) => {
            sketch_guarantee(algorithm, &hydra.per_subpopulation(), query)
        }
    }
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

/// The built-in guarantee of reading `query` out of a summary of family
/// `family` built over an **exact** input, derived from the family's
/// committed parameters by inverting the same sizing formulas
/// [`size_params`] uses. Exact for a plain value
/// or an exact aggregate. `None` when a sketch does not answer `query` (its
/// family-capability table row), and when the family has no error model
/// (`SummarySample`, `Wavelet`, `StatModel`, and sketch evaluations without a
/// certified bound): Pass 1 then leaves the guarantee `None` and keeps the
/// candidate for plan selection to decide.
pub(crate) fn local_guarantee(
    family: &FieldDataType,
    query: &SketchStatistic,
) -> Option<AccuracyGuarantee> {
    match family {
        FieldDataType::Plain(_) => Some(AccuracyGuarantee::exact("Plain value")),
        FieldDataType::ExactAggregate(kind, _) => Some(AccuracyGuarantee::exact(format!(
            "ExactAggregate({kind:?})"
        ))),
        FieldDataType::Sketch(..) if FamilyCapability::of(family).answers(query) => {
            todo!(
                "#1207 PR 4: the guarantee of θ from the candidate's LogicalProperties::parameters"
            )
        }
        FieldDataType::Sketch(..) => None,
        // No error model is registered for these families.
        FieldDataType::SummarySample(..)
        | FieldDataType::Wavelet(..)
        | FieldDataType::StatModel(..) => None,
    }
}
/// `params` with the parameters its per-value failure probability depends
/// on enlarged, never reduced, so that each value it answers fails with
/// probability at most `delta` and its error bound is unchanged or smaller
/// (#669b): Count-Min's depth, Count Sketch's depth (the binomial tail of
/// its median), KLL's failure probability (its δ scale), a distinct count
/// UnivMon's heap, and KMV's `k` at the relative error it was sized for
/// (#694). Other parameters are returned as they are: DDSketch never fails;
/// classic HLL's failure probability comes from its source's distinct bound
/// ([`hll::min_precision`]); a KMV not
/// sized for a relative error and an L2 or entropy UnivMon are not
/// enlarged.
pub fn with_failure_probability(params: &SketchParams, delta: f64) -> SketchParams {
    if !(delta > 0.0 && delta < 1.0) {
        return params.clone();
    }
    match params {
        SketchParams::Cms { width, depth } => SketchParams::Cms {
            width: *width,
            depth: (*depth).max(cms::cms_depth(delta)),
        },
        SketchParams::CmsWithHeap {
            width,
            depth,
            heap_size,
        } => SketchParams::CmsWithHeap {
            width: *width,
            depth: (*depth).max(cms::cms_depth(delta)),
            heap_size: *heap_size,
        },
        SketchParams::CountSketch { width, depth } => SketchParams::CountSketch {
            width: *width,
            depth: (*depth).max(count_sketch::count_sketch_depth(*width, delta)),
        },
        SketchParams::CountSketchWithHeap {
            width,
            depth,
            heap_size,
        } => SketchParams::CountSketchWithHeap {
            width: *width,
            depth: (*depth).max(count_sketch::count_sketch_depth(*width, delta)),
            heap_size: *heap_size,
        },
        SketchParams::Kll { .. } => kll::with_failure_probability(params, delta),
        SketchParams::Kmv {
            k,
            relative_error: Some(eps),
        } => SketchParams::Kmv {
            k: (*k).max(cardinality::kmv_k(*eps, delta)),
            relative_error: Some(*eps),
        },
        SketchParams::UnivMon { .. } => univmon::with_failure_probability(params, delta),
        _ => params.clone(),
    }
}

#[allow(dead_code)] // Read again for the predefined θ lists (`parameter_list`), in #1207 PR 1.
pub(crate) fn size_params(
    kind: SketchAlgorithm,
    intent: &AggIntent,
    eps: f64,
    delta: f64,
) -> SketchParams {
    match kind {
        SketchAlgorithm::UnivMon => univmon::size_params(intent, eps, delta),
        SketchAlgorithm::Kll => kll::size_params(eps, delta),
        SketchAlgorithm::Cms => SketchParams::Cms {
            width: cms::cms_width(eps),
            depth: cms::cms_depth(delta),
        },
        SketchAlgorithm::Hll => SketchParams::Hll {
            precision: hll::hll_precision(eps, delta),
            relative_error: Some(eps),
        },
        SketchAlgorithm::CmsWithHeap => {
            let k = match intent {
                AggIntent::TopK { k, .. } => *k,
                _ => unreachable!("CmsWithHeap is only a TopK candidate"),
            };
            SketchParams::CmsWithHeap {
                width: cms::cms_width(eps),
                depth: cms::cms_depth(delta),
                heap_size: topk_capacity(k, eps),
            }
        }
        // Non-preferred candidates (DDSketch / Kmv / CountSketch /
        // CountSketchWithHeap) are only reachable once a cost model picks
        // them; sized here so that wiring is local.
        SketchAlgorithm::DDSketch => ddsketch::size_params(eps),
        SketchAlgorithm::Kmv => SketchParams::Kmv {
            k: cardinality::kmv_k(eps, delta),
            relative_error: Some(eps),
        },
        SketchAlgorithm::CountSketch => {
            let width = count_sketch::count_sketch_width(eps);
            SketchParams::CountSketch {
                width,
                depth: count_sketch::count_sketch_depth(width, delta),
            }
        }
        SketchAlgorithm::CountSketchWithHeap => {
            let k = match intent {
                AggIntent::TopK { k, .. } => *k,
                _ => unreachable!("CountSketchWithHeap is only a TopK candidate"),
            };
            let width = count_sketch::count_sketch_width(eps);
            SketchParams::CountSketchWithHeap {
                width,
                depth: count_sketch::count_sketch_depth(width, delta),
                heap_size: topk_capacity(k, eps),
            }
        }
        // Listed, not `_`: a family `asap_sketchlib` adds fails to compile
        // here until the planner sizes it or lists it as not offered.
        SketchAlgorithm::CountSketchWithL2Norm
        | SketchAlgorithm::CmsWithSpaceSaving
        | SketchAlgorithm::CountSketchWithSpaceSaving
        | SketchAlgorithm::CmsFold
        | SketchAlgorithm::CountSketchFold
        | SketchAlgorithm::HllHip
        | SketchAlgorithm::KllDynamic
        | SketchAlgorithm::SpaceSaving
        | SketchAlgorithm::Elastic
        | SketchAlgorithm::Coco
        | SketchAlgorithm::Bloom
        | SketchAlgorithm::SetAggregator
        | SketchAlgorithm::UnivMonTwoTier
        | SketchAlgorithm::UnivMonQ
        | SketchAlgorithm::HydraCms
        | SketchAlgorithm::HydraCountSketch
        | SketchAlgorithm::HydraKll
        | SketchAlgorithm::HydraHll
        | SketchAlgorithm::HydraUnivMon => {
            unreachable!("Pass 1 offers only the families in FAMILY_PREFERENCE, not {kind:?}")
        }
    }
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

/// Accuracy-dependent candidate budget, not a completeness theorem. The
/// membership model must still certify the selected set independently.
pub(crate) fn topk_capacity(k: usize, eps: f64) -> u32 {
    u32::try_from(k)
        .unwrap_or(u32::MAX)
        .max(saturating_ceil(1.0 / eps, 1, 1 << 26))
}

/// `⌈x⌉` clamped to `[lo, hi]`; NaN / non-positive x saturate to `hi`
/// (a degenerate ε means "as accurate as this family goes").
pub(crate) fn saturating_ceil(x: f64, lo: u32, hi: u32) -> u32 {
    if !x.is_finite() || x <= 0.0 {
        return hi;
    }
    (x.ceil() as u32).clamp(lo, hi)
}

#[cfg(test)]
mod tests {
    use super::*;
    use asap_types::ir::operator::agg_intent::{default_cardinality, default_quantile};

    fn delta_and_error(params: &SketchParams, query: &SketchStatistic) -> (f64, f64) {
        let algorithm = match params {
            SketchParams::Cms { .. } => SketchAlgorithm::Cms,
            SketchParams::CountSketch { .. } => SketchAlgorithm::CountSketch,
            SketchParams::Kll { .. } => SketchAlgorithm::Kll,
            other => panic!("{other:?}"),
        };
        let g = sketch_guarantee(&algorithm, params, query).unwrap();
        (
            g.spec.failure_probability.evaluate().unwrap(),
            g.spec.error.evaluate().unwrap(),
        )
    }

    /// Enlarging for a smaller failure probability deepens Count-Min and
    /// Count Sketch and grows KLL until each meets it, at no larger error;
    /// it never shrinks a sketch that already meets it. (Count Sketch's
    /// depth is capped at 255 rows, which a non-power-of-two width reaches
    /// before 1e-6.)
    #[test]
    fn enlarging_meets_the_failure_probability_at_no_larger_error() {
        let point = SketchStatistic::PointCount {
            key: asap_types::ir::scalar::expr_ir::ColumnRef::SampleValue,
            value: None,
        };
        let quantile = SketchStatistic::Quantile { q: 0.99 };
        for (params, query, delta) in [
            (
                size_params(SketchAlgorithm::Cms, &default_cardinality(), 0.01, 0.01),
                &point,
                1e-6,
            ),
            (
                size_params(
                    SketchAlgorithm::CountSketch,
                    &default_cardinality(),
                    0.01,
                    0.01,
                ),
                &point,
                1e-3,
            ),
            (
                size_params(SketchAlgorithm::Kll, &default_quantile(0.99), 0.01, 0.01),
                &quantile,
                1e-6,
            ),
        ] {
            let (_, error) = delta_and_error(&params, query);
            let enlarged = with_failure_probability(&params, delta);
            let (new_delta, new_error) = delta_and_error(&enlarged, query);
            assert!(new_delta <= delta, "{enlarged:?}");
            assert!(new_error <= error * (1.0 + 1e-9), "{enlarged:?}");
            assert_ne!(enlarged, params);
            assert_eq!(with_failure_probability(&enlarged, 0.01), enlarged);
        }
        let ddsketch = SketchParams::DDSketch { alpha: 0.01 };
        assert_eq!(with_failure_probability(&ddsketch, 1e-6), ddsketch);
    }

    /// A distinct-count UnivMon grows its heap for a smaller failure
    /// probability at no larger relative error.
    #[test]
    fn a_distinct_count_univmon_grows_its_heap() {
        let params = size_params(SketchAlgorithm::UnivMon, &default_cardinality(), 0.05, 0.01);
        let enlarged = with_failure_probability(&params, 1e-4);
        let error = |p: &SketchParams| {
            univmon::distinct_guarantee(p, "m")
                .unwrap()
                .spec
                .error
                .evaluate()
                .unwrap()
        };
        let (
            SketchParams::UnivMon { heap_size: k, .. },
            SketchParams::UnivMon {
                heap_size: grown,
                distinct_failure_probability,
                ..
            },
        ) = (&params, &enlarged)
        else {
            panic!("{enlarged:?}")
        };
        assert!(grown > k);
        assert_eq!(*distinct_failure_probability, Some(1e-4));
        assert!(error(&enlarged) <= error(&params) * (1.0 + 1e-9));
    }

    /// Classic HLL's precision grows until its failure probability over the
    /// source's distinct bound meets the target, up to 18.
    #[test]
    fn classic_hll_precision_grows_for_its_failure_probability() {
        let p = hll::min_precision(0.05, 0.01, 64, 4);
        assert!(p > 4);
        assert!(hll::classic_failure_probability(p, 0.05, 64) <= 0.01);
        assert!(hll::classic_failure_probability(p - 1, 0.05, 64) > 0.01);
        assert_eq!(hll::min_precision(0.05, 1e-30, 4096, 12), 12);
    }

    /// A sketch read for a statistic its table row does not answer (an HLL
    /// for a total) has no guarantee; one it answers keeps its guarantee,
    /// and a plain value or exact aggregate stays exact.
    #[test]
    fn local_guarantee_needs_a_statistic_the_family_answers() {
        use asap_types::ir::scalar::expr_ir::ColumnRef;
        use asap_types::ir::schema::{DataType, ExactKind, ExactParams, GroupingStrategy};
        let hll = FieldDataType::Sketch(SketchAlgorithm::Hll, GroupingStrategy::default());
        let total = SketchStatistic::PointCount {
            key: ColumnRef::SampleValue,
            value: None,
        };
        assert!(local_guarantee(&hll, &total).is_none());
        for exact in [
            FieldDataType::Plain(DataType::Float64),
            FieldDataType::ExactAggregate(ExactKind::Count, ExactParams::Count),
        ] {
            assert!(local_guarantee(&exact, &total).is_some(), "{exact:?}");
        }
    }
}

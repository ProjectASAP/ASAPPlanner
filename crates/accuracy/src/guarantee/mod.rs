//! Dispatch committed estimator parameters to their accuracy models.
use asap_types::accuracy::{
    AccuracyGuarantee, AccuracyMetric, BoundExpr, GuaranteeSource, ProbabilityExpr,
};
use asap_types::ir::schema::FieldDataType;
use asap_types::summary_type::{SketchAlgorithm, SketchParams, SketchStatistic};

#[cfg(test)]
use crate::requirements::meets;
#[cfg(test)]
use asap_types::ir::operator::AggIntent;
use asap_types::summary_type::SummaryTypeCapability;

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

/// The theoretical error bound of reading `statistic` from a state of
/// `summary_type` with parameters `parameters` at failure probability
/// `failure_probability` (the guarantee's own failure probability): the
/// algorithm's error formula (concepts doc, section 2, "Where the guarantee
/// comes from"). `None` when this θ cannot fail that rarely, when the
/// summary type does not answer `statistic` (its capability row) or has no
/// error model (`SummarySample`, `Wavelet`, `StatModel`, and sketch
/// readouts without a certified bound).
pub fn theoretical_guarantee(
    summary_type: &FieldDataType,
    parameters: &SketchParams,
    statistic: &SketchStatistic,
    failure_probability: f64,
) -> Option<AccuracyGuarantee> {
    match summary_type {
        FieldDataType::Sketch(_) if SummaryTypeCapability::of(summary_type).answers(statistic) => {
            let _ = (parameters, failure_probability);
            todo!("#1207 PR 4: the algorithm's error formula at θ, at failure_probability")
        }
        _ => None,
    }
}

/// Today's sizing: the parameters of algorithm `kind` for error `eps` at failure
/// probability `delta`. Test-only: the predefined lists (`parameter_list`)
/// are this sizing over an error grid, which a test keeps equal.
#[cfg(test)]
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
#[cfg(test)]
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

    /// Each algorithm's predefined list (`parameter_list`) is today's sizing
    /// over the error grid ε = 0.1 … 0.001 at failure probability 0.01,
    /// smallest first, without repeats; KMV's and UnivMon's distinct counts
    /// stop at ε = 0.01. Count-Min lists each sized width with depths 5, 7,
    /// 10 and 12, Count Sketch with 47, 81, 117 and 155, and a heap variant
    /// each of those with
    /// every heap size of the 1-2-5 series 10 … 10 000. UnivMon's list also
    /// combines each L2 shape with each distinct-count shape (28 entries).
    #[test]
    fn parameter_list_is_the_sizing_over_the_error_grid() {
        use asap_types::ir::operator::agg_intent::{default_cardinality, default_quantile};
        use asap_types::summary_type::parameter_list;
        let full_grid = [0.1, 0.05, 0.02, 0.01, 0.005, 0.002, 0.001];
        let distinct_grid = [0.1, 0.05, 0.02, 0.01];
        let cms_depths = [5, 7, 10, 12];
        let count_sketch_depths = [47, 81, 117, 155];
        let heap_sizes = [10, 20, 50, 100, 200, 500, 1_000, 2_000, 5_000, 10_000];
        let l2 = AggIntent::FrequencyL2 { col: None };
        let top_1 = AggIntent::TopK { k: 1 };
        let quantile = default_quantile(0.5);
        let distinct = default_cardinality();
        for (algorithm, intents) in [
            (SketchAlgorithm::Kll, vec![&quantile]),
            (SketchAlgorithm::DDSketch, vec![&quantile]),
            (SketchAlgorithm::Hll, vec![&distinct]),
            (SketchAlgorithm::Kmv, vec![&distinct]),
            (SketchAlgorithm::Cms, vec![&distinct]),
            (SketchAlgorithm::CountSketch, vec![&distinct]),
            (SketchAlgorithm::CmsWithHeap, vec![&top_1]),
            (SketchAlgorithm::CountSketchWithHeap, vec![&top_1]),
            (SketchAlgorithm::UnivMon, vec![&l2, &distinct, &quantile]),
        ] {
            let mut sized: Vec<SketchParams> = Vec::new();
            for intent in intents {
                let grid: &[f64] = match (algorithm, intent) {
                    (
                        SketchAlgorithm::Kmv | SketchAlgorithm::UnivMon,
                        AggIntent::Cardinality { .. },
                    ) => &distinct_grid,
                    _ => &full_grid,
                };
                for &eps in grid {
                    let listed: Vec<SketchParams> = match size_params(algorithm, intent, eps, 0.01)
                    {
                        SketchParams::Cms { width, .. } => cms_depths
                            .iter()
                            .map(|&depth| SketchParams::Cms { width, depth })
                            .collect(),
                        SketchParams::CountSketch { width, .. } => count_sketch_depths
                            .iter()
                            .map(|&depth| SketchParams::CountSketch { width, depth })
                            .collect(),
                        SketchParams::CmsWithHeap { width, .. } => cms_depths
                            .iter()
                            .flat_map(|&depth| {
                                heap_sizes
                                    .iter()
                                    .map(move |&heap_size| SketchParams::CmsWithHeap {
                                        width,
                                        depth,
                                        heap_size,
                                    })
                            })
                            .collect(),
                        SketchParams::CountSketchWithHeap { width, .. } => count_sketch_depths
                            .iter()
                            .flat_map(|&depth| {
                                heap_sizes.iter().map(move |&heap_size| {
                                    SketchParams::CountSketchWithHeap {
                                        width,
                                        depth,
                                        heap_size,
                                    }
                                })
                            })
                            .collect(),
                        other => vec![other],
                    };
                    for params in listed {
                        if !sized.contains(&params) {
                            sized.push(params);
                        }
                    }
                }
            }
            let list = parameter_list(algorithm);
            let (listed_sized, combined): (Vec<_>, Vec<_>) =
                list.iter().partition(|params| sized.contains(params));
            assert_eq!(
                format!("{listed_sized:?}"),
                format!("{:?}", sized.iter().collect::<Vec<_>>()),
                "{algorithm:?}"
            );
            let expected_combined = if algorithm == SketchAlgorithm::UnivMon {
                28
            } else {
                0
            };
            assert_eq!(combined.len(), expected_combined, "{algorithm:?}");
        }
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
}

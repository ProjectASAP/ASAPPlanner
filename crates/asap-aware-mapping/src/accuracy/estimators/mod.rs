//! Dispatch committed estimator parameters to their accuracy models.
use super::*;
use asap_types::pre_asap::AggIntent;

pub mod cardinality;
pub mod cms;
pub mod count_sketch;
pub mod ddsketch;
pub mod hll;
pub mod kll;
pub mod univmon;

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
        SketchParams::Kmv { .. } | SketchParams::Theta { .. } => {
            cardinality::guarantee(algorithm, params, query)
        }
        SketchParams::UnivMon { .. } => univmon::guarantee(query),
    }
}

fn bounded_guarantee(
    algorithm: &SketchAlgorithm,
    params: &SketchParams,
    query: &SketchStatistic,
    metric: ErrorMetric,
    bound: f64,
    delta: ProbabilityExpr,
    contract: &str,
) -> AccuracyGuarantee {
    AccuracyGuarantee {
        metric,
        bound: BoundExpr::Constant { value: bound },
        failure_probability: delta,
        provenance: vec![GuaranteeSource::SketchEvaluation {
            algorithm: format!("{algorithm:?}"),
            contract: contract.into(),
            params: serde_json::to_value(params).unwrap_or(serde_json::Value::Null),
            query: format!("{query:?}"),
        }],
    }
}

pub(super) fn local_guarantee(
    family: &FieldDataType,
    query: &SketchStatistic,
) -> Option<AccuracyGuarantee> {
    match family {
        FieldDataType::Plain(_) => Some(AccuracyGuarantee::exact("Plain value")),
        FieldDataType::ExactAggregate(kind, _) => Some(AccuracyGuarantee::exact(format!(
            "ExactAggregate({kind:?})"
        ))),
        FieldDataType::Sketch(kind, _) => sketch_guarantee(kind.algorithm(), kind.params(), query),
        // No error model is registered for these families.
        FieldDataType::Sample(..) | FieldDataType::Wavelet(..) | FieldDataType::StatModel(..) => {
            None
        }
    }
}
pub(crate) fn size_params(
    kind: SketchAlgorithm,
    intent: &AggIntent,
    eps: f64,
    delta: f64,
) -> SketchParams {
    match kind {
        // Baseline dimensions are candidates, not an inverted error bound.
        // Empirical models may size these; no theoretical guarantee is claimed.
        SketchAlgorithm::UnivMon => univmon::size_params(),
        SketchAlgorithm::Kll => kll::size_params(eps, delta),
        SketchAlgorithm::Cms => SketchParams::Cms {
            width: cms::cms_width(eps),
            depth: cms::cms_depth(delta),
        },
        SketchAlgorithm::Hll => SketchParams::Hll {
            precision: hll::hll_precision(eps),
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
        // Non-preferred candidates (DDSketch / Theta / Kmv / CountSketch /
        // CountSketchWithHeap) are only reachable once a cost model picks
        // them; sized here so that wiring is local.
        SketchAlgorithm::DDSketch => ddsketch::size_params(eps),
        SketchAlgorithm::Theta => SketchParams::Theta {
            k: cardinality::kmv_k_99(eps),
        },
        SketchAlgorithm::Kmv => SketchParams::Kmv {
            k: cardinality::kmv_k_99(eps),
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

/// `P(c_i = c_j)` for two keys in a Count Sketch row of `cols` columns.
/// `asap_sketchlib` takes a column as a `⌈log2 cols⌉`-bit hash slice modulo
/// `cols`: uniform for a power of two, and otherwise at most `9/(8 cols)`
/// (the slice values past `cols` fold onto the low columns; the worst case,
/// `cols = ¾ · 2^⌈log2 cols⌉`, doubles a third of them).
pub(crate) fn column_collision(cols: u32) -> f64 {
    let cols = cols.max(1);
    if cols.is_power_of_two() {
        1.0 / f64::from(cols)
    } else {
        9.0 / (8.0 * f64::from(cols))
    }
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

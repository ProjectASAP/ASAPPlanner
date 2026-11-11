//! The closed-form sizing per summary type that the predefined parameter
//! lists (`asap_types::summary_type::parameter_list`) are checked against:
//! the parameters of a summary type for error `ε` at failure probability
//! `δ`. Test-only; the planner itself only reads the predefined lists.
use super::*;
use asap_types::ir::operator::AggIntent;
use asap_types::summary_type::KLL_MAX_K;

/// The parameters of algorithm `kind` for error `eps` at failure
/// probability `delta`. The predefined lists (`parameter_list`) are this
/// sizing over an error grid, which a test keeps equal.
pub(super) fn size_params(
    kind: SketchAlgorithm,
    intent: &AggIntent,
    eps: f64,
    delta: f64,
) -> SketchParams {
    match kind {
        SketchAlgorithm::UnivMon => univmon_params(intent, eps, delta),
        SketchAlgorithm::Kll => kll_params(eps, delta),
        SketchAlgorithm::Cms => SketchParams::Cms {
            width: cms_width(eps),
            depth: cms_depth(delta),
        },
        SketchAlgorithm::Hll => SketchParams::Hll {
            precision: hll_precision(eps, delta),
        },
        SketchAlgorithm::CmsWithHeap => {
            let k = match intent {
                AggIntent::TopK { k, .. } => *k,
                _ => unreachable!("CmsWithHeap is only a TopK candidate"),
            };
            SketchParams::CmsWithHeap {
                width: cms_width(eps),
                depth: cms_depth(delta),
                heap_size: topk_capacity(k, eps),
            }
        }
        // Non-preferred candidates (DDSketch / Kmv / CountSketch /
        // CountSketchWithHeap) are only reachable once a cost model picks
        // them; sized here so that wiring is local.
        SketchAlgorithm::DDSketch => SketchParams::DDSketch { alpha: eps },
        SketchAlgorithm::Kmv => SketchParams::Kmv {
            k: kmv_k(eps, delta),
        },
        SketchAlgorithm::CountSketch => {
            let width = count_sketch_width(eps);
            SketchParams::CountSketch {
                width,
                depth: count_sketch_depth(width, delta),
            }
        }
        SketchAlgorithm::CountSketchWithHeap => {
            let k = match intent {
                AggIntent::TopK { k, .. } => *k,
                _ => unreachable!("CountSketchWithHeap is only a TopK candidate"),
            };
            let width = count_sketch_width(eps);
            SketchParams::CountSketchWithHeap {
                width,
                depth: count_sketch_depth(width, delta),
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
            unreachable!("Pass 1 offers only the families in ALGORITHM_PREFERENCE, not {kind:?}")
        }
    }
}

/// Accuracy-dependent candidate budget, not a completeness theorem. The
/// membership model must still certify the selected set independently.
fn topk_capacity(k: usize, eps: f64) -> u32 {
    u32::try_from(k)
        .unwrap_or(u32::MAX)
        .max(saturating_ceil(1.0 / eps, 1, 1 << 26))
}

/// `⌈x⌉` clamped to `[lo, hi]`; NaN / non-positive x saturate to `hi`
/// (a degenerate ε means "as accurate as this family goes").
fn saturating_ceil(x: f64, lo: u32, hi: u32) -> u32 {
    if !x.is_finite() || x <= 0.0 {
        return hi;
    }
    (x.ceil() as u32).clamp(lo, hi)
}

// Count-Min Sketch.

/// CMS: over-count ≤ ε·N with width `w = ⌈e/ε⌉` columns.
fn cms_width(eps: f64) -> u32 {
    saturating_ceil(std::f64::consts::E / eps, 2, 1 << 26)
}

/// CMS: failure probability ≤ δ with depth `d = ⌈ln(1/δ)⌉` rows.
/// δ = 0.01 → depth 5.
fn cms_depth(delta: f64) -> u32 {
    saturating_ceil((1.0 / delta).ln(), 1, 32)
}

// Count Sketch.

/// CountSketch `L2` point-query width: ε = sqrt(3/w).
pub(super) fn count_sketch_width(eps: f64) -> u32 {
    saturating_ceil(3.0 / (eps * eps), 2, 1 << 26)
}

/// The smallest positive odd depth whose binomial median failure
/// probability at `width` is at most `delta`, capped at 255.
pub(super) fn count_sketch_depth(width: u32, delta: f64) -> u32 {
    if !(delta.is_finite() && delta > 0.0 && delta < 1.0) {
        return 255;
    }
    (1..=255)
        .step_by(2)
        .find(|&depth| {
            count_sketch::count_sketch_failure_probability(width, depth)
                .is_some_and(|tail| tail <= delta)
        })
        .unwrap_or(255)
}

// KMV.

/// The largest `k` sizing returns.
const KMV_MAX_K: u32 = 1 << 26;

/// The smallest `k` whose KMV failure probability at relative error `eps`
/// is at most `delta` ([`cardinality::kmv_failure_probability`]), up to
/// `2^26`; a degenerate `eps` or `delta` saturates to `2^26`.
fn kmv_k(eps: f64, delta: f64) -> u32 {
    use cardinality::{kmv_failure_probability, KMV_MIN_K};
    if !(eps.is_finite() && eps > 0.0 && delta.is_finite() && delta > 0.0) {
        return KMV_MAX_K;
    }
    // The overestimate term alone at most `δ` bounds `k` below; both terms
    // at most `δ/2` bound it above.
    let k_at = |ln_inverse: f64| {
        saturating_ceil(
            1.0 + (1.0 + eps) * (2.0 + eps) * ln_inverse / (eps * eps),
            KMV_MIN_K,
            KMV_MAX_K,
        )
    };
    let (mut lo, mut hi) = (k_at((1.0 / delta).ln()), k_at((2.0 / delta).ln()));
    if kmv_failure_probability(hi, eps) > delta {
        return KMV_MAX_K;
    }
    while lo < hi {
        let mid = lo + (hi - lo) / 2;
        if kmv_failure_probability(mid, eps) <= delta {
            hi = mid;
        } else {
            lo = mid + 1;
        }
    }
    hi
}

// HLL.

/// Classic HLL's precision for relative error `eps` with failure
/// probability `delta`, without data. Below `1/ε` distinct items a single
/// bucket collision already exceeds the error, and the chance of one is
/// about `N²/(2m)`; a certificate for any bound of at least `1/ε`
/// therefore needs `m ≥ 1/(2ε²δ)`. This is the smallest such precision at
/// which [`hll::ClassicHllConfidence`] certifies `delta` at `eps` for every
/// bound up to `⌈1/ε⌉` (18 when none up to 18 does).
fn hll_precision(eps: f64, delta: f64) -> u8 {
    if !(eps.is_finite() && eps > 0.0 && delta.is_finite() && delta > 0.0) {
        return 18;
    }
    let start = saturating_ceil((1.0 / (2.0 * eps * eps * delta.min(1.0))).log2(), 4, 18) as u8;
    let reference = (1.0 / eps)
        .ceil()
        .min(f64::from(hll::MAX_CONTRACT_DISTINCT)) as u64;
    (start..=18)
        .find(|&p| hll::classic_failure_probability(p, eps, reference) <= delta)
        .unwrap_or(18)
}

/// The smallest precision, at least `at_least`, whose classic failure
/// probability at `relative_error` over a source of at most `max_distinct`
/// distinct items is at most `failure_probability`; `at_least` when none up
/// to 18 is: [`hll_precision`]'s rule for a source's own distinct bound.
fn min_precision(
    relative_error: f64,
    failure_probability: f64,
    max_distinct: u64,
    at_least: u8,
) -> u8 {
    (at_least..=18)
        .find(|&p| {
            hll::classic_failure_probability(p, relative_error, max_distinct) <= failure_probability
        })
        .unwrap_or(at_least)
}

/// The smallest precision at which classic HLL certifies failure
/// probability `delta` at `relative_error` over at most `max_distinct`
/// distinct items ([`hll::ClassicHllConfidence`]); `None` when `delta` is
/// not in (0, 1), the bound or error is outside the contract, or no
/// precision up to 18 certifies it.
fn classic_hll_precision(max_distinct: u32, relative_error: f64, delta: f64) -> Option<u8> {
    if !(delta.is_finite() && delta > 0.0 && delta < 1.0) {
        return None;
    }
    let model = hll::ClassicHllConfidence::new(max_distinct, relative_error)?;
    (4..=hll::MAX_PRECISION).find(|&p| model.failure_probability(p).is_some_and(|d| d <= delta))
}

// KLL.

/// Invert Apache DataSketches' empirical 99th-percentile, single-sided KLL
/// normalized rank-error fit: `epsilon = 2.296 / k^0.9723`. Capped at
/// [`KLL_MAX_K`], the largest `k` `asap_sketchlib` builds; an `eps` that
/// needs more gets that `k`, whose guarantee then misses the target.
pub(super) fn kll_k(eps: f64) -> u32 {
    saturating_ceil(
        (kll::KLL_RANK_ERROR_COEFFICIENT_99 / eps).powf(1.0 / kll::KLL_RANK_ERROR_EXPONENT_99),
        8,
        KLL_MAX_K,
    )
}

/// `k` for rank error `eps` at failure probability `delta`: the empirical
/// 99% calibration for `delta >= 0.01`; below, the calibration at
/// `eps / delta_scale(delta)`, recording `delta` so the guarantee reports it.
pub(super) fn kll_params(eps: f64, delta: f64) -> SketchParams {
    if delta.is_nan() || delta <= 0.0 || delta >= kll::KLL_CALIBRATED_FAILURE_PROBABILITY {
        return SketchParams::Kll { k: kll_k(eps) };
    }
    SketchParams::Kll {
        k: kll_k(eps / kll::delta_scale(delta).unwrap_or(1.0)),
    }
}

// UnivMon.

/// Layers `asap_sketchlib` builds: layer `i` hashes at seed index `i`, and
/// seed 19 picks the layers a key reaches.
const UNIVMON_MAX_LAYERS: u8 = asap_sketchlib::sketch_framework::univmon::UNIVMON_MAX_LAYERS as u8;
/// The most rows an L2 sketch is sized with: the deepest odd shape within
/// the 20 rows the executor builds ([`univmon::hash_independent`]), so the
/// row median is one row's estimate, and within the hash budget at the
/// more than 20 columns an L2 guarantee needs (5 column bits:
/// 19 · 6 = 114 ≤ 128).
const UNIVMON_MAX_L2_ROWS: u32 = 19;
/// Each row of a layer reads its column and sign bits from one hash.
const UNIVMON_HASH_BITS: u32 = 128;
/// The largest distinct-count heap sizing returns.
const UNIVMON_MAX_HEAP: u32 = 1 << 26;

/// The shape baseline for readouts without a size model: the total (exact
/// whatever the shape) and entropy (uncertified).
pub(super) const UNIVMON_BASELINE: SketchParams = SketchParams::UnivMon {
    heap_size: 256,
    sketch_rows: 5,
    sketch_cols: 1024,
    layers: 16,
};

/// The smallest rows and columns whose L2 guarantee meets `(eps, delta)`,
/// clamped to the hash budget: with more rows than
/// [`UNIVMON_MAX_L2_ROWS`], or more columns than `rows` leave bits for, the
/// shape is the largest one built and its guarantee misses the target.
fn univmon_l2_shape(eps: f64, delta: f64) -> (u32, u32) {
    use univmon::{l2_failure_probability, l2_relative_error, L2_ROW_FAILURE};
    let rows = (1..=UNIVMON_MAX_L2_ROWS)
        .find(|&rows| l2_failure_probability(rows) <= delta)
        .unwrap_or(UNIVMON_MAX_L2_ROWS);
    let max_bits = (UNIVMON_HASH_BITS / rows - 1).min(26);
    let max_cols = 1u32 << max_bits;
    let meets = |cols: u32| l2_relative_error(cols).is_some_and(|error| error <= eps);
    if !(eps.is_finite() && eps > 0.0) || !meets(max_cols) {
        return (rows, max_cols);
    }
    // The norm's error `ε` needs `ε₂ ≤ 1 − (1 − ε)²`; a power-of-two width
    // needs `2/(0.1 ε₂²)` columns and any other `9/(4 · 0.1 ε₂²)`.
    let f2_error = 1.0 - (1.0 - eps.min(1.0)).powi(2);
    let general = 9.0 / (4.0 * L2_ROW_FAILURE * f2_error * f2_error);
    // Off powers of two the error falls as the width grows, and a power of
    // two meets the target whenever its non-power-of-two neighbours do, so
    // walking down from the estimate stops at the smallest such width.
    let mut general = saturating_ceil(general, 1, max_cols);
    while general > 1 && meets(general - 1) {
        general -= 1;
    }
    while !meets(general) {
        general += 1;
    }
    let power_of_two = (0..=max_bits)
        .map(|bits| 1u32 << bits)
        .find(|&cols| meets(cols))
        .unwrap_or(max_cols);
    (rows, general.min(power_of_two))
}

/// The Chebyshev share of the distinct count's failure probability at
/// heap `k` and relative error `eps`: `1.35/(k ε²)`.
fn univmon_distinct_chebyshev(heap_size: u32, eps: f64) -> f64 {
    univmon::DISTINCT_VARIANCE / (f64::from(heap_size) * eps * eps)
}

/// The smallest heap `k ≥ 8` whose distinct count misses by more than
/// `eps · F0` with probability at most `delta`, top-layer completeness
/// included: `1.35/(k ε²) + (e/4)^(k/2) ≤ δ`. Capped; a capped heap's
/// guarantee misses the target.
fn univmon_distinct_heap(eps: f64, delta: f64) -> u32 {
    if !(eps.is_finite() && eps > 0.0 && delta.is_finite() && delta > 0.0) {
        return UNIVMON_MAX_HEAP;
    }
    let meets = |k: u32| {
        univmon_distinct_chebyshev(k, eps) + univmon::incomplete_top_layer_bound(k) <= delta
    };
    let mut k = saturating_ceil(
        univmon::DISTINCT_VARIANCE / (delta * eps * eps),
        univmon::DISTINCT_MIN_HEAP,
        UNIVMON_MAX_HEAP,
    );
    while !meets(k) && k < UNIVMON_MAX_HEAP {
        k += 1;
    }
    k
}

/// Size a UnivMon for one readout's `(eps, delta)`:
/// - L2: the smallest rows and columns meeting the target
///   ([`univmon_l2_shape`]), one layer (L2 reads only layer 0) and a
///   one-key heap;
/// - distinct count: the smallest heap meeting it
///   ([`univmon_distinct_heap`]) and the most layers, since the layers the
///   top layer needs depend on the source's distinct bound, which plan
///   selection binds. The bound holds whatever the Count Sketch keeps in
///   the heaps, so each layer's is one counter;
/// - total and entropy: the baseline shape (the total is exact; entropy
///   has no certified bound).
fn univmon_params(intent: &AggIntent, eps: f64, delta: f64) -> SketchParams {
    match intent {
        AggIntent::FrequencyL2 { .. } => {
            let (sketch_rows, sketch_cols) = univmon_l2_shape(eps, delta);
            SketchParams::UnivMon {
                heap_size: 1,
                sketch_rows,
                sketch_cols,
                layers: 1,
            }
        }
        AggIntent::Cardinality { .. } => SketchParams::UnivMon {
            heap_size: univmon_distinct_heap(eps, delta),
            sketch_rows: 1,
            sketch_cols: 1,
            layers: UNIVMON_MAX_LAYERS,
        },
        _ => UNIVMON_BASELINE,
    }
}

mod tests {
    use super::*;
    use asap_types::ir::operator::agg_intent::{default_cardinality, default_quantile};

    /// Classic HLL's precision grows until its failure probability over the
    /// source's distinct bound meets the target, up to 18.
    #[test]
    fn classic_hll_precision_grows_for_its_failure_probability() {
        let p = min_precision(0.05, 0.01, 64, 4);
        assert!(p > 4);
        assert!(hll::classic_failure_probability(p, 0.05, 64) <= 0.01);
        assert!(hll::classic_failure_probability(p - 1, 0.05, 64) > 0.01);
        assert_eq!(min_precision(0.05, 1e-30, 4096, 12), 12);
    }

    /// A supported estimator contract supplies a probability, unlike
    /// generic HLL RSE: sized for δ = 0.01 over at most 128 distinct items,
    /// the classic guarantee binds to a known failure probability.
    #[test]
    fn bounded_classic_hll_has_a_feasible_confidence_target() {
        let precision =
            classic_hll_precision(128, 0.05, 0.01).expect("finite confidence-sized HLL");
        let guarantee = hll::classic_guarantee(precision, Some(0.05), "m")
            .unwrap()
            .bind(&|statistic| {
                let (precision, eps, _) = hll::parse_classic_failure_statistic(statistic)?;
                Some(hll::classic_failure_probability(precision, eps, 128))
            });
        assert!(!guarantee.has_unknown());
        assert!(guarantee.spec.failure_probability.evaluate().unwrap() <= 0.01);
        assert_eq!(guarantee.spec.error.evaluate(), Some(0.05));
    }

    /// Tighter confidence must increase precision or explicitly become unavailable.
    #[test]
    fn sizing_and_domain_limits_are_consistent() {
        let model = hll::ClassicHllConfidence::new(128, 0.05).unwrap();
        let precision = |delta| classic_hll_precision(128, 0.05, delta);
        assert!(precision(0.001).unwrap() > precision(0.01).unwrap());
        assert!(precision(1e-12).is_none());
        assert!(precision(0.0).is_none());
        assert!(precision(f64::NAN).is_none());
        assert!(model.failure_probability(3).is_none());
        assert!(model.failure_probability(19).is_none());
        assert!(model.failure_probability(7).is_none());
        for (n, e) in [
            (0, 0.05),
            (hll::MAX_CONTRACT_DISTINCT + 1, 0.05),
            (128, 0.0),
            (128, f64::NAN),
        ] {
            assert!(hll::ClassicHllConfidence::new(n, e).is_none());
        }
    }

    /// Each algorithm's predefined list (`parameter_list`) is this sizing
    /// over the error grid ε = 0.1 … 0.001 at failure probability 0.01,
    /// smallest first, without repeats; KMV's and UnivMon's distinct counts
    /// stop at ε = 0.01. Count-Min lists each sized width with depths 5, 7,
    /// 10 and 12, Count Sketch with 47, 81, 117 and 155, and a heap variant
    /// each of those with
    /// every heap size of the 1-2-5 series 10 … 10 000. UnivMon's list also
    /// combines each L2 shape with each distinct-count shape (28 entries).
    #[test]
    fn parameter_list_is_the_sizing_over_the_error_grid() {
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

    /// CMS width is `⌈e/ε⌉` and depth `⌈ln(1/δ)⌉`.
    #[test]
    fn epsilon_delta_sizes_cms_depth() {
        let intent = AggIntent::Count;
        assert_eq!(
            size_params(SketchAlgorithm::Cms, &intent, 0.001, 0.001),
            SketchParams::Cms {
                width: 2719,
                depth: 7
            } // ⌈e/0.001⌉, ⌈ln 1000⌉
        );
        assert_eq!(
            size_params(SketchAlgorithm::Cms, &intent, 0.001, 0.01),
            SketchParams::Cms {
                width: 2719,
                depth: 5
            }
        );
        assert_eq!(
            size_params(SketchAlgorithm::Cms, &intent, 0.01, 0.01),
            SketchParams::Cms {
                width: 272,
                depth: 5
            }
        );
    }

    /// A top-k heap holds `max(k, ⌈1/ε⌉)` entries over a CMS sized as for counts.
    #[test]
    fn topk_heap_capacity_respects_accuracy_and_output_count() {
        let intent = AggIntent::TopK { k: 25 };
        assert_eq!(
            size_params(SketchAlgorithm::CmsWithHeap, &intent, 0.01, 0.01),
            SketchParams::CmsWithHeap {
                width: 272, // ⌈e/0.01⌉
                depth: 5,
                heap_size: 100,
            }
        );
    }

    /// The executor's `CountSketch<_, RegularPath>` takes a column as the
    /// low 32 hash bits modulo the width, so a row fails with probability
    /// `1/3` (to within `2^-32`) at any width, not `3/8` off powers of two:
    /// ε = 0.05 (width 1200) needs depth 47 at δ = 0.01, 81 at 1e-3 and
    /// 117 at 1e-4, not 85, 149 and 215.
    #[test]
    fn depth_follows_the_libraries_modulo_column_hash() {
        let width = count_sketch_width(0.05);
        assert_eq!(width, 1200);
        assert!((count_sketch::row_failure_probability(width) - 1.0 / 3.0).abs() < 1e-6);
        assert_eq!(
            [0.01, 1e-3, 1e-4].map(|delta| count_sketch_depth(width, delta)),
            [47, 81, 117]
        );
    }

    /// The depth is the smallest odd one whose binomial tail meets δ. Each
    /// is at most Hoeffding's `ln(1/δ) / (2 (1/2 − p)²)` at the row's
    /// failure probability `p ≈ 1/3`: `18 ln(1/δ)` at any width.
    #[test]
    fn count_sketch_depth_is_the_smallest_meeting_the_binomial_tail() {
        use count_sketch::count_sketch_failure_probability;
        for width in [1024, 3000, 30_000] {
            let hoeffding = 18.0 * (1.0 + 1e-4);
            for delta in [0.1, 0.01, 0.001] {
                let depth = count_sketch_depth(width, delta);
                assert!(depth % 2 == 1);
                assert!(count_sketch_failure_probability(width, depth).unwrap() <= delta);
                if depth > 1 {
                    assert!(count_sketch_failure_probability(width, depth - 2).unwrap() > delta);
                }
                assert!(
                    f64::from(depth) <= hoeffding * (1.0 / delta).ln(),
                    "{width} {delta}"
                );
            }
        }
        // Any width fails a row with probability 1/3 (to within 2^-32 w/3):
        // P(Bin(3, 1/3) ≥ 2) = 7/27.
        for width in [1024, 1000] {
            let tail = count_sketch_failure_probability(width, 3).unwrap();
            assert!((tail - 7.0 / 27.0).abs() < 1e-6, "{width}");
        }
        assert_eq!(count_sketch_failure_probability(1000, 4), None);
    }

    /// Pass 1 sizes classic HLL from (ε, δ) without data: the smallest
    /// precision with `m ≥ 1/(2ε²δ)` that the tail formula certifies at the
    /// target ε, which its guarantee states as its error bound.
    #[test]
    fn precision_is_sized_from_epsilon_and_delta() {
        for (eps, delta, precision) in [(0.05, 0.01, 15), (0.02, 0.01, 17)] {
            let params = size_params(SketchAlgorithm::Hll, &default_cardinality(), eps, delta);
            let SketchParams::Hll { precision: sized } = params else {
                panic!("{params:?}")
            };
            assert_eq!(sized, precision);
        }
    }

    /// `k` never exceeds the 26 602 `asap_sketchlib` silently caps it at, so
    /// a guarantee never claims a larger `k` than runs.
    #[test]
    fn kll_k_is_capped_at_the_library_maximum() {
        assert_eq!(kll_k(1e-6), KLL_MAX_K);
        assert_eq!(KLL_MAX_K, 26_602);
    }

    /// KLL `k` inverts the rank-error fit: ε = 0.01 → 269, ε = 0.05 → 52.
    #[test]
    fn accuracy_target_drives_kll_k() {
        let q = default_quantile(0.99);
        assert_eq!(
            size_params(SketchAlgorithm::Kll, &q, 0.01, 0.01),
            SketchParams::Kll { k: 269 }
        );
        assert_eq!(
            size_params(SketchAlgorithm::Kll, &q, 0.05, 0.01),
            SketchParams::Kll { k: 52 }
        );
    }

    /// A zero ε saturates KLL `k` to the tightest buildable value.
    #[test]
    fn degenerate_epsilon_saturates_to_tightest_kll_params() {
        assert_eq!(
            size_params(SketchAlgorithm::Kll, &default_quantile(0.99), 0.0, 0.01),
            SketchParams::Kll { k: 26_602 }
        );
    }

    /// Example 2's Q1 (ε = 0.02, δ = 0.01) needs a heap of 337,500 keys,
    /// `⌈1.35/(δ ε²)⌉`: the top layer's term is negligible at that heap.
    /// At (0.5, 0.5) it is not: 1.35/(12 · 0.25) + (e/4)^6 > 0.5, while 13
    /// keys give 0.415 + 0.081.
    #[test]
    fn distinct_heap_for_example2_q1() {
        assert_eq!(univmon_distinct_heap(0.02, 0.01), 337_500);
        assert_eq!(univmon_distinct_heap(0.5, 0.5), 13);
    }
}

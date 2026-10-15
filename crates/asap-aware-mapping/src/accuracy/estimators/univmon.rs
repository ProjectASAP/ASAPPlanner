//! UnivMon's readouts and the guarantees `asap_sketchlib`'s standard-mode
//! estimators carry (the derivations are in the library's `calc_l2` and
//! `calc_card` doc comments):
//! - the total (a bare `PointCount`) is the exact unit-update count;
//! - L2 reads layer 0's Count Sketch directly: per row, Chebyshev with
//!   `Var ≤ 2q F2²`, and the median of the rows fails only when at least
//!   `⌈r/2⌉` rows fail, an exact binomial tail;
//! - the distinct count is unbiased with `Var ≤ 1.35 F0²/k` for a heap of
//!   `k ≥ 8` once the top layer is complete, which needs the source's
//!   distinct bound, so its guarantee is built where the source is known
//!   ([`distinct_guarantee`]) and plan selection binds it;
//! - entropy has no certified bound.
//!
//! Every bound assumes the key hash is uniformly random and the shape keeps
//! the hashes independent ([`hash_independent`]). Terminal-mode states
//! (OctoSketch aggregators fed promoted deltas) are not covered: the
//! executor rejects them.
use super::*;
use asap_types::pre_asap::AggIntent;

/// Layers `asap_sketchlib` builds: layer `i` hashes at seed index `i`, and
/// seed 19 picks the layers a key reaches.
pub(crate) const MAX_LAYERS: u8 = asap_sketchlib::sketch_framework::univmon::MAX_LAYER_SIZE as u8;
/// Rows the executor builds an L2 sketch with: the deepest odd shape under
/// its 20-row limit.
const MAX_L2_ROWS: u32 = 19;
/// Each row of a layer reads its column and sign bits from one hash.
const HASH_BITS: u32 = 128;
/// The per-row failure probability the L2 guarantee fixes: a row misses by
/// more than `ε₂ F2` with probability at most `2q/ε₂² = 0.1`. Fixing it
/// lets a shape state one `(ε, δ)`: the columns give `ε`, the rows `δ`.
const L2_ROW_FAILURE: f64 = 0.1;
/// `Var[calc_card] ≤ 1.35 F0²/k` for `k ≥ 8`, the constant the library
/// proves (it also states the looser `2 F0²/k`).
const DISTINCT_VARIANCE: f64 = 1.35;
/// The smallest heap the distinct-count variance bound holds for.
const DISTINCT_MIN_HEAP: u32 = 8;
const CARD_CAP: u32 = 1 << 26;

const L2_CONTRACT: &str = "univmon_layer0_f2_median_chebyshev_binomial_v1";
const DISTINCT_CONTRACT: &str = "univmon_distinct_complete_top_layer_chebyshev_v1";
/// Names the probability that the top layer is incomplete, left symbolic
/// in `max_distinct[source]` ([`incomplete_top_layer_statistic`]).
const INCOMPLETE_TOP_LAYER: &str = "univmon_incomplete_top_layer_probability";
const MAX_DISTINCT: &str = "max_distinct";

/// The shape baseline for readouts without a size model: the total (exact
/// whatever the shape) and entropy (uncertified).
const BASELINE: SketchParams = SketchParams::UnivMon {
    heap_size: 256,
    sketch_rows: 5,
    sketch_cols: 1024,
    layers: 16,
    distinct_failure_probability: None,
};

pub(super) fn guarantee(
    algorithm: &SketchAlgorithm,
    params: &SketchParams,
    query: &SketchStatistic,
) -> Option<AccuracyGuarantee> {
    let SketchParams::UnivMon {
        sketch_rows,
        sketch_cols,
        layers,
        ..
    } = params
    else {
        return None;
    };
    match query {
        SketchStatistic::PointCount { value: None, .. } => {
            Some(AccuracyGuarantee::exact("univmon_unit_update_total"))
        }
        SketchStatistic::FrequencyL2 if hash_independent(*sketch_rows, *sketch_cols, *layers) => {
            Some(super::bounded_guarantee(
                algorithm,
                params,
                query,
                AccuracyMetric::Relative,
                l2_relative_error(*sketch_cols)?,
                ProbabilityExpr::Constant {
                    value: l2_failure_probability(*sketch_rows),
                },
                L2_CONTRACT,
            ))
        }
        // The distinct count needs the source's distinct bound
        // ([`distinct_guarantee`]); entropy has no certified bound.
        _ => None,
    }
}

/// Whether `asap_sketchlib` builds this shape, with independent hashes: at
/// most [`MAX_LAYERS`] layers and 20 rows, and `rows · (⌈log2 cols⌉ + 1) ≤
/// 128` ([`asap_sketchlib::UnivMon::check_shape`]; the heap size is not part
/// of the hash rules).
pub(crate) fn hash_independent(rows: u32, cols: u32, layers: u8) -> bool {
    asap_sketchlib::UnivMon::check_shape(1, rows as usize, cols as usize, usize::from(layers))
        .is_ok()
}

/// The relative error of the L2 norm at `cols` columns. Per row, the
/// squared norm misses by more than `ε₂ F2` with probability at most
/// `2q/ε₂²` ([`super::column_collision`]), so fixing that at
/// [`L2_ROW_FAILURE`] gives `ε₂ = √(2q/0.1)`. The norm then misses by at
/// most `1 − √(1 − ε₂)` (the low side; the high side `√(1 + ε₂) − 1` is
/// smaller). `None` when `ε₂ ≥ 1`.
fn l2_relative_error(cols: u32) -> Option<f64> {
    let f2_error = (2.0 * super::column_collision(cols) / L2_ROW_FAILURE).sqrt();
    (f2_error < 1.0).then(|| 1.0 - (1.0 - f2_error).sqrt())
}

/// The median of `rows` independent rows misses only when at least
/// `⌈rows/2⌉` of them miss: `P(Binomial(rows, 0.1) ≥ ⌈rows/2⌉)`.
fn l2_failure_probability(rows: u32) -> f64 {
    super::binomial_upper_tail(rows, L2_ROW_FAILURE, rows.div_ceil(2))
}

/// The smallest rows and columns whose L2 guarantee meets `(eps, delta)`,
/// clamped to the hash budget: with more rows than [`MAX_L2_ROWS`], or more
/// columns than `rows` leave bits for, the shape is the largest one built
/// and its guarantee misses the target.
fn l2_shape(eps: f64, delta: f64) -> (u32, u32) {
    let rows = (1..=MAX_L2_ROWS)
        .find(|&rows| l2_failure_probability(rows) <= delta)
        .unwrap_or(MAX_L2_ROWS);
    let max_bits = (HASH_BITS / rows - 1).min(26);
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

/// `(e/4)^(k/2)`: the chance the top layer of a heap-`k` pyramid misses a
/// key, once the expected keys reaching it are at most `k/2` (Chernoff).
fn incomplete_top_layer_bound(heap_size: u32) -> f64 {
    (f64::from(heap_size) / 2.0 * (1.0 - 2.0 * std::f64::consts::LN_2)).exp()
}

/// The Chebyshev share of the distinct count's failure probability at
/// heap `k` and relative error `eps`: `1.35/(k ε²)`.
fn distinct_chebyshev(heap_size: u32, eps: f64) -> f64 {
    DISTINCT_VARIANCE / (f64::from(heap_size) * eps * eps)
}

/// The smallest heap `k ≥ 8` whose distinct count misses by more than
/// `eps · F0` with probability at most `delta`, top-layer completeness
/// included: `1.35/(k ε²) + (e/4)^(k/2) ≤ δ`. Capped; a capped heap's
/// guarantee misses the target.
fn distinct_heap(eps: f64, delta: f64) -> u32 {
    if !(eps.is_finite() && eps > 0.0 && delta.is_finite() && delta > 0.0) {
        return CARD_CAP;
    }
    let meets = |k: u32| distinct_chebyshev(k, eps) + incomplete_top_layer_bound(k) <= delta;
    let mut k = saturating_ceil(
        DISTINCT_VARIANCE / (delta * eps * eps),
        DISTINCT_MIN_HEAP,
        CARD_CAP,
    );
    while !meets(k) && k < CARD_CAP {
        k += 1;
    }
    k
}

/// The statistic a UnivMon distinct count over `source` leaves its
/// top-layer failure probability as.
pub(crate) fn incomplete_top_layer_statistic(heap_size: u32, layers: u8, source: &str) -> String {
    format!("{INCOMPLETE_TOP_LAYER}[k={heap_size},layers={layers}]({MAX_DISTINCT}[{source}])")
}

/// The heap, layers and source an [`incomplete_top_layer_statistic`] names.
pub(crate) fn parse_incomplete_top_layer_statistic(statistic: &str) -> Option<(u32, u8, &str)> {
    let rest = statistic
        .strip_prefix(INCOMPLETE_TOP_LAYER)?
        .strip_prefix("[k=")?;
    let (heap_size, rest) = rest.split_once(",layers=")?;
    let (layers, rest) = rest.split_once(']')?;
    let source = rest
        .strip_prefix('(')?
        .strip_prefix(MAX_DISTINCT)?
        .strip_prefix('[')?
        .strip_suffix("])")?;
    Some((heap_size.parse().ok()?, layers.parse().ok()?, source))
}

/// The probability an [`incomplete_top_layer_statistic`] names, given the
/// source's distinct bound `F0_max`:
/// - 0 when `F0_max ≤ k`: the top layer cannot receive more keys than its
///   heap holds;
/// - `(e/4)^(k/2)` when `layers ≥ ⌈log2(F0_max/k)⌉ + 2`, i.e.
///   `F0_max ≤ k · 2^(layers−2)`, so at most `k/2` keys reach the top layer
///   in expectation;
/// - 1 (no certificate) otherwise: the pyramid is too shallow.
pub(crate) fn incomplete_top_layer_probability(
    heap_size: u32,
    layers: u8,
    max_distinct: u64,
) -> f64 {
    let (k, max_distinct) = (u128::from(heap_size), u128::from(max_distinct));
    if max_distinct <= k {
        0.0
    } else if layers >= 2 && max_distinct <= k << (layers - 2) {
        incomplete_top_layer_bound(heap_size)
    } else {
        1.0
    }
}

/// A UnivMon distinct count over `source`'s own rows: relative error at
/// most `√(1.35/(k δ_c))`, where `δ_c` is the share of the sized failure
/// probability left after the top layer's, failing with probability
/// `δ_c` plus the chance of an incomplete top layer, left symbolic in the
/// source's distinct bound. `None` for a shape that was not sized for a
/// distinct count, a heap under 8, or a shape the library would not build.
pub(crate) fn distinct_guarantee(params: &SketchParams, source: &str) -> Option<AccuracyGuarantee> {
    let SketchParams::UnivMon {
        heap_size,
        sketch_rows,
        sketch_cols,
        layers,
        distinct_failure_probability: Some(delta),
    } = params
    else {
        return None;
    };
    if *heap_size < DISTINCT_MIN_HEAP
        || !hash_independent(*sketch_rows, *sketch_cols, *layers)
        || !(*delta > 0.0 && *delta < 1.0)
    {
        return None;
    }
    let chebyshev = delta - incomplete_top_layer_bound(*heap_size);
    if chebyshev <= 0.0 {
        return None;
    }
    let relative_error = (DISTINCT_VARIANCE / (f64::from(*heap_size) * chebyshev)).sqrt();
    let statistic = incomplete_top_layer_statistic(*heap_size, *layers, source);
    Some(AccuracyGuarantee::new(
        AccuracyMetric::Relative,
        BoundExpr::Constant {
            value: relative_error,
        },
        ProbabilityExpr::UnionBound {
            terms: vec![
                ProbabilityExpr::Constant { value: chebyshev },
                ProbabilityExpr::Unknown { statistic },
            ],
        },
        vec![GuaranteeSource::SketchEvaluation {
            algorithm: "UnivMon".into(),
            contract: DISTINCT_CONTRACT.into(),
            params: serde_json::json!({"heap_size": heap_size, "layers": layers,
                "max_distinct": format!("{MAX_DISTINCT}[{source}]"),
                "relative_error": relative_error,
                "variance": "1.35 F0^2 / k",
                "hash_assumption": "independent_uniform_layer_hashes",
                "mode": "standard",
                "population_scope": "complete_evaluation_including_merged_panes"}),
            query: "Cardinality".into(),
        }],
    ))
}

/// A distinct-count `params` resized to fail with probability at most
/// `delta` at no larger relative error: the heap for its current error at
/// `delta` ([`distinct_heap`]), never smaller. Unchanged for a shape not
/// sized for a distinct count, or one that already fails no more often.
pub(crate) fn with_failure_probability(params: &SketchParams, delta: f64) -> SketchParams {
    let SketchParams::UnivMon {
        heap_size,
        sketch_rows,
        sketch_cols,
        layers,
        distinct_failure_probability: Some(current),
    } = params
    else {
        return params.clone();
    };
    let chebyshev = current - incomplete_top_layer_bound(*heap_size);
    if delta >= *current || chebyshev <= 0.0 {
        return params.clone();
    }
    let relative_error = (DISTINCT_VARIANCE / (f64::from(*heap_size) * chebyshev)).sqrt();
    SketchParams::UnivMon {
        heap_size: distinct_heap(relative_error, delta).max(*heap_size),
        sketch_rows: *sketch_rows,
        sketch_cols: *sketch_cols,
        layers: *layers,
        distinct_failure_probability: Some(delta),
    }
}

/// Size a UnivMon for one readout's `(eps, delta)`:
/// - L2: the smallest rows and columns meeting the target ([`l2_shape`]),
///   one layer (L2 reads only layer 0) and a one-key heap;
/// - distinct count: the smallest heap meeting it ([`distinct_heap`]) and
///   the most layers, since the layers the top layer needs depend on the
///   source's distinct bound, which plan selection binds. The bound holds
///   whatever the Count Sketch keeps in the heaps, so each layer's is one
///   counter;
/// - total and entropy: the baseline shape (the total is exact; entropy
///   has no certified bound).
pub(super) fn size_params(intent: &AggIntent, eps: f64, delta: f64) -> SketchParams {
    match intent {
        AggIntent::FrequencyL2 { .. } => {
            let (sketch_rows, sketch_cols) = l2_shape(eps, delta);
            SketchParams::UnivMon {
                heap_size: 1,
                sketch_rows,
                sketch_cols,
                layers: 1,
                distinct_failure_probability: None,
            }
        }
        AggIntent::Cardinality { .. } => SketchParams::UnivMon {
            heap_size: distinct_heap(eps, delta),
            sketch_rows: 1,
            sketch_cols: 1,
            layers: MAX_LAYERS,
            distinct_failure_probability: Some(delta),
        },
        _ => BASELINE,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use asap_types::pre_asap::agg_intent::default_cardinality;

    fn l2_intent() -> AggIntent {
        AggIntent::FrequencyL2 {
            col: None,
            accuracy: AccuracyTarget::exact(),
        }
    }

    fn univmon(
        heap_size: u32,
        rows: u32,
        cols: u32,
        layers: u8,
        delta: Option<f64>,
    ) -> SketchParams {
        SketchParams::UnivMon {
            heap_size,
            sketch_rows: rows,
            sketch_cols: cols,
            layers,
            distinct_failure_probability: delta,
        }
    }

    fn l2(params: &SketchParams) -> Option<AccuracyGuarantee> {
        guarantee(
            &SketchAlgorithm::UnivMon,
            params,
            &SketchStatistic::FrequencyL2,
        )
    }

    fn meets(guarantee: &AccuracyGuarantee, epsilon: f64, delta: f64) -> bool {
        DefaultAccuracyModel.satisfies(
            guarantee,
            &crate::accuracy::native_target(
                &AccuracyTarget::max_error(epsilon).failure_probability(delta),
                guarantee,
            ),
        )
    }

    fn with(params: &SketchParams, f: impl FnOnce(&mut u32, &mut u32, &mut u32)) -> SketchParams {
        let mut params = params.clone();
        let SketchParams::UnivMon {
            heap_size,
            sketch_rows,
            sketch_cols,
            ..
        } = &mut params
        else {
            unreachable!()
        };
        f(heap_size, sketch_rows, sketch_cols);
        params
    }

    const TARGETS: [(f64, f64); 6] = [
        (0.01, 0.01),
        (0.02, 0.01),
        (0.05, 0.01),
        (0.05, 0.001),
        (0.1, 0.05),
        (0.2, 0.2),
    ];

    /// Each L2 shape meets its target as `RelativeValue`, and one fewer
    /// row or column does not.
    #[test]
    fn l2_shape_is_the_smallest_meeting_its_target() {
        for (eps, delta) in TARGETS {
            let params = size_params(&l2_intent(), eps, delta);
            let guarantee = l2(&params).expect("an L2 guarantee");
            assert_eq!(guarantee.spec.accuracy_metric, AccuracyMetric::Relative);
            assert!(
                meets(&guarantee, eps, delta),
                "({eps}, {delta}): {params:?}"
            );
            let fewer_rows = with(&params, |_, rows, _| *rows -= 1);
            assert!(
                l2(&fewer_rows).is_none_or(|g| !meets(&g, eps, delta)),
                "({eps}, {delta}): {fewer_rows:?}"
            );
            let fewer_cols = with(&params, |_, _, cols| *cols -= 1);
            assert!(
                l2(&fewer_cols).is_none_or(|g| !meets(&g, eps, delta)),
                "({eps}, {delta}): {fewer_cols:?}"
            );
        }
    }

    /// The general collision bound `9/(8w)` applies off powers of two: a
    /// width just past a power of two is worse than that power of two.
    #[test]
    fn l2_error_uses_the_general_collision_bound_off_powers_of_two() {
        let error = |cols| l2_relative_error(cols).unwrap();
        assert!(error(1 << 12) < error((1 << 12) + 1));
        let f2_error = |cols: u32| 1.0 - (1.0 - error(cols)).powi(2);
        assert!((f2_error(1 << 12) - (20.0 / 4096.0f64).sqrt()).abs() < 1e-12);
        assert!((f2_error(3000) - (22.5 / 3000.0f64).sqrt()).abs() < 1e-12);
    }

    /// The L2 failure probability is the exact binomial tail of the rows.
    #[test]
    fn l2_failure_probability_is_the_binomial_tail() {
        // P(Bin(1, 0.1) ≥ 1) and P(Bin(3, 0.1) ≥ 2) = 3·0.01·0.9 + 0.001.
        assert!((l2_failure_probability(1) - 0.1).abs() < 1e-12);
        assert!((l2_failure_probability(3) - 0.028).abs() < 1e-12);
        // An even count needs half the rows to miss: worse than one fewer.
        assert!(l2_failure_probability(4) > l2_failure_probability(3));
    }

    /// Shapes past the hash budget or the layer limit certify nothing, and
    /// L2 sizing never builds one: a target it cannot reach gets the
    /// largest shape it can build, whose guarantee misses it.
    #[test]
    fn l2_shapes_stay_within_the_hash_budget() {
        // 8 · (16 + 1) = 136 > 128.
        assert!(l2(&univmon(1, 8, 1 << 16, 1, None)).is_none());
        assert!(l2(&univmon(1, 7, 1 << 16, 1, None)).is_some());
        assert!(l2(&univmon(1, 5, 1024, 20, None)).is_none());
        for (eps, delta) in [(0.001, 1e-6), (1e-5, 0.01), (0.01, 1e-12)] {
            let params = size_params(&l2_intent(), eps, delta);
            let SketchParams::UnivMon {
                sketch_rows,
                sketch_cols,
                layers,
                ..
            } = params
            else {
                unreachable!()
            };
            assert!(
                hash_independent(sketch_rows, sketch_cols, layers),
                "{params:?}"
            );
            assert!(!meets(&l2(&params).unwrap(), eps, delta), "{params:?}");
        }
    }

    /// The library builds at most 20 rows, so a 21-row UnivMon certifies
    /// nothing even though its hashes fit the budget (21 · (3 + 1) = 84 ≤
    /// 128 at 8 columns).
    #[test]
    fn univmon_past_twenty_rows_is_not_hash_independent() {
        assert!(hash_independent(20, 8, 1));
        assert!(!hash_independent(21, 8, 1));
        assert!(l2(&univmon(1, 21, 8, 1, None)).is_none());
    }

    /// Each distinct-count heap meets its target once the source's bound
    /// is bound, and one fewer key does not.
    #[test]
    fn distinct_heap_is_the_smallest_meeting_its_target() {
        for (eps, delta) in TARGETS {
            let params = size_params(&default_cardinality(), eps, delta);
            let bind = |params: &SketchParams, max_distinct| {
                let guarantee = distinct_guarantee(params, "m").expect("a distinct guarantee");
                guarantee.bind(&|statistic| {
                    let (k, layers, source) = parse_incomplete_top_layer_statistic(statistic)?;
                    (source == "m")
                        .then(|| incomplete_top_layer_probability(k, layers, max_distinct))
                })
            };
            for max_distinct in [9, 10_000_000] {
                let guarantee = bind(&params, max_distinct);
                assert_eq!(guarantee.spec.accuracy_metric, AccuracyMetric::Relative);
                assert!(
                    meets(&guarantee, eps, delta),
                    "({eps}, {delta}): {params:?}"
                );
                let smaller = with(&params, |heap, _, _| *heap -= 1);
                assert!(
                    !meets(&bind(&smaller, max_distinct), eps, delta),
                    "({eps}, {delta}): {smaller:?}"
                );
            }
        }
    }

    /// Example 2's Q1 (ε = 0.02, δ = 0.01) needs a heap of 337,500 keys,
    /// `⌈1.35/(δ ε²)⌉`: the top layer's term is negligible at that heap.
    /// At (0.5, 0.5) it is not: 1.35/(12 · 0.25) + (e/4)^6 > 0.5, while 13
    /// keys give 0.415 + 0.081.
    #[test]
    fn distinct_heap_for_example2_q1() {
        assert_eq!(distinct_heap(0.02, 0.01), 337_500);
        assert_eq!(distinct_heap(0.5, 0.5), 13);
    }

    /// Without the source's bound the failure probability stays unknown
    /// and no target is met; a bound the pyramid is too shallow for gives
    /// probability 1.
    #[test]
    fn distinct_guarantee_needs_the_source_bound_and_enough_layers() {
        let params = size_params(&default_cardinality(), 0.05, 0.01);
        let guarantee = distinct_guarantee(&params, "m").unwrap();
        assert_eq!(guarantee.spec.failure_probability.evaluate(), None);
        assert!(!meets(&guarantee, 0.05, 0.01));
        let SketchParams::UnivMon { heap_size: k, .. } = params else {
            unreachable!()
        };
        // At most k keys: complete for certain.
        assert_eq!(incomplete_top_layer_probability(k, 2, u64::from(k)), 0.0);
        // F0_max = k · 2^(L−2) is the deepest bound L layers cover.
        let deepest = u64::from(k) << 17;
        assert!(incomplete_top_layer_probability(k, 19, deepest) < 1e-100);
        assert_eq!(incomplete_top_layer_probability(k, 19, deepest + 1), 1.0);
        assert_eq!(incomplete_top_layer_probability(k, 18, deepest), 1.0);
        assert_eq!(
            incomplete_top_layer_probability(k, 1, u64::from(k) + 1),
            1.0
        );
        assert_eq!(incomplete_top_layer_probability(8, 19, u64::MAX), 1.0);
    }

    /// Only a shape sized for a distinct count, with a heap of at least 8
    /// and a buildable shape, gets a distinct guarantee; the guarantee
    /// without a source is none.
    #[test]
    fn distinct_guarantee_needs_a_distinct_sizing() {
        assert!(distinct_guarantee(&univmon(256, 5, 1024, 16, None), "m").is_none());
        assert!(distinct_guarantee(&univmon(7, 1, 1, 19, Some(0.5)), "m").is_none());
        assert!(distinct_guarantee(&univmon(8, 1, 1, 20, Some(0.5)), "m").is_none());
        assert!(distinct_guarantee(&univmon(8, 1, 1, 19, Some(0.5)), "m").is_some());
        let sized = size_params(&default_cardinality(), 0.05, 0.01);
        assert!(guarantee(
            &SketchAlgorithm::UnivMon,
            &sized,
            &SketchStatistic::Cardinality
        )
        .is_none());
    }

    /// The statistic names its heap, layers and source, any source name.
    #[test]
    fn incomplete_top_layer_statistics_round_trip() {
        for source in ["flows_src_ip", "a[b]", "x])(", ""] {
            let statistic = incomplete_top_layer_statistic(337_500, 19, source);
            assert_eq!(
                parse_incomplete_top_layer_statistic(&statistic),
                Some((337_500, 19, source))
            );
        }
        assert_eq!(
            parse_incomplete_top_layer_statistic("max_distinct[m]"),
            None
        );
    }

    /// The total stays exact; entropy stays uncertified.
    #[test]
    fn total_is_exact_and_entropy_uncertified() {
        let params = size_params(&default_cardinality(), 0.05, 0.01);
        let total = guarantee(
            &SketchAlgorithm::UnivMon,
            &params,
            &SketchStatistic::PointCount {
                key: asap_types::pre_asap::expr_ir::ColumnRef::SampleValue,
                value: None,
            },
        )
        .unwrap();
        assert!(total.is_exact());
        assert!(guarantee(
            &SketchAlgorithm::UnivMon,
            &BASELINE,
            &SketchStatistic::FrequencyEntropy
        )
        .is_none());
    }
}

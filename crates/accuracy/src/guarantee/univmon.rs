//! UnivMon's readouts and the guarantees `asap_sketchlib`'s standard-mode
//! estimators carry (the derivations are in the library's
//! `docs/api/api_univmon.md`, the bounds `estimate_l2` and
//! `estimate_cardinality` cite):
//! - the total (a bare `PointCount`) is the exact unit-update count;
//! - L2 reads layer 0's Count Sketch directly: per row, Chebyshev with
//!   `Var ≤ 2q F2²`, and the median of the rows fails only when at least
//!   `⌈r/2⌉` rows fail, an exact binomial tail;
//! - the distinct count is unbiased with `Var ≤ 1.35 F0²/k` for a heap of
//!   `k ≥ 8` once the top layer is complete, which needs the source's
//!   distinct bound, so its guarantee is built where the source is known
//!   (`distinct_guarantee`) and plan selection binds it;
//! - entropy has no certified bound.
//!
//! Every bound assumes the key hash is uniformly random and the shape keeps
//! the hashes independent (`hash_independent`). Terminal-mode states
//! (OctoSketch aggregators fed promoted deltas) are not covered: the
//! executor rejects them.
use super::*;

/// The per-row failure probability the L2 guarantee fixes: a row misses by
/// more than `ε₂ F2` with probability at most `2q/ε₂² = 0.1`. Fixing it
/// lets a shape state one `(ε, δ)`: the columns give `ε`, the rows `δ`.
const L2_ROW_FAILURE: f64 = 0.1;
/// `Var[estimate_cardinality] ≤ 1.35 F0²/k` for `k ≥ 8`, the constant the library
/// proves (it also states the looser `2 F0²/k`).
const DISTINCT_VARIANCE: f64 = 1.35;
/// The smallest heap the distinct-count variance bound holds for.
const DISTINCT_MIN_HEAP: u32 = 8;

const L2_CONTRACT: &str = "univmon_layer0_f2_median_chebyshev_binomial_v1";
const DISTINCT_CONTRACT: &str = "univmon_distinct_complete_top_layer_chebyshev_v1";
/// Names the probability that the top layer is incomplete, left symbolic
/// in `max_distinct[source]` ([`incomplete_top_layer_statistic`]).
const INCOMPLETE_TOP_LAYER: &str = "univmon_incomplete_top_layer_probability";
const MAX_DISTINCT: &str = "max_distinct";

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

/// Whether the executor builds this shape, with independent hashes: at
/// most `UNIVMON_MAX_LAYERS` (the library's) layers and `MATRIX_MAX_ROWS` (20) rows, the rows a
/// UnivMon state encodes, and `rows · (⌈log2 cols⌉ + 1) ≤ 128`
/// ([`asap_sketchlib::UnivMon::check_shape`]; the heap size is not part of
/// the hash rules).
pub(crate) fn hash_independent(rows: u32, cols: u32, layers: u8) -> bool {
    rows as usize <= asap_sketchlib::MATRIX_MAX_ROWS
        && asap_sketchlib::UnivMon::check_shape(
            1,
            rows as usize,
            cols as usize,
            usize::from(layers),
        )
        .is_ok()
}

/// The relative error of the L2 norm at `cols` columns. Per row, the
/// squared norm misses by more than `ε₂ F2` with probability at most
/// `2q/ε₂²` ([`column_collision`]), so fixing that at
/// [`L2_ROW_FAILURE`] gives `ε₂ = √(2q/0.1)`. The norm then misses by at
/// most `1 − √(1 − ε₂)` (the low side; the high side `√(1 + ε₂) − 1` is
/// smaller). `None` when `ε₂ ≥ 1`.
fn l2_relative_error(cols: u32) -> Option<f64> {
    let f2_error = (2.0 * column_collision(cols) / L2_ROW_FAILURE).sqrt();
    (f2_error < 1.0).then(|| 1.0 - (1.0 - f2_error).sqrt())
}

/// `P(c_i = c_j)` for two keys in a row of a UnivMon layer of `cols`
/// columns. `asap_sketchlib` takes a UnivMon column as a `⌈log2 cols⌉`-bit
/// slice of one 128-bit hash modulo `cols` ([`hash_independent`]): uniform
/// for a power of two, and otherwise at most `9/(8 cols)` (the slice values
/// past `cols` fold onto the low columns; the worst case,
/// `cols = ¾ · 2^⌈log2 cols⌉`, doubles a third of them). A standalone
/// Count Sketch hashes modulo `cols` instead (`count_sketch`).
fn column_collision(cols: u32) -> f64 {
    let cols = cols.max(1);
    if cols.is_power_of_two() {
        1.0 / f64::from(cols)
    } else {
        9.0 / (8.0 * f64::from(cols))
    }
}

/// The median of `rows` independent rows misses only when at least
/// `⌈rows/2⌉` of them miss: `P(Binomial(rows, 0.1) ≥ ⌈rows/2⌉)`.
fn l2_failure_probability(rows: u32) -> f64 {
    super::binomial_upper_tail(rows, L2_ROW_FAILURE, rows.div_ceil(2))
}

/// `(e/4)^(k/2)`: the chance the top layer of a heap-`k` pyramid misses a
/// key, once the expected keys reaching it are at most `k/2` (Chernoff).
fn incomplete_top_layer_bound(heap_size: u32) -> f64 {
    (f64::from(heap_size) / 2.0 * (1.0 - 2.0 * std::f64::consts::LN_2)).exp()
}

/// The statistic a UnivMon distinct count over `source` leaves its
/// top-layer failure probability as.
#[allow(dead_code)] // Read again in #1207 PR 8: δ of a distinct count over a source's own rows, with θ.
pub(crate) fn incomplete_top_layer_statistic(heap_size: u32, layers: u8, source: &str) -> String {
    format!("{INCOMPLETE_TOP_LAYER}[k={heap_size},layers={layers}]({MAX_DISTINCT}[{source}])")
}

/// The heap, layers and source an `incomplete_top_layer_statistic` names.
pub fn parse_incomplete_top_layer_statistic(statistic: &str) -> Option<(u32, u8, &str)> {
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

/// The probability an `incomplete_top_layer_statistic` names, given the
/// source's distinct bound `F0_max`:
/// - 0 when `F0_max ≤ k`: the top layer cannot receive more keys than its
///   heap holds;
/// - `(e/4)^(k/2)` when `layers ≥ ⌈log2(F0_max/k)⌉ + 2`, i.e.
///   `F0_max ≤ k · 2^(layers−2)`, so at most `k/2` keys reach the top layer
///   in expectation;
/// - 1 (no certificate) otherwise: the pyramid is too shallow.
pub fn incomplete_top_layer_probability(heap_size: u32, layers: u8, max_distinct: u64) -> f64 {
    let (k, max_distinct) = (u128::from(heap_size), u128::from(max_distinct));
    if max_distinct <= k {
        0.0
    } else if layers >= 2 && max_distinct <= k << (layers - 2) {
        incomplete_top_layer_bound(heap_size)
    } else {
        1.0
    }
}

/// A UnivMon distinct count over `source`'s own rows, at failure
/// probability `delta`: relative error at most `√(1.35/(k δ_c))`, where
/// `δ_c` is the share of `delta` left after the top layer's, failing with
/// probability `δ_c` plus the chance of an incomplete top layer, left
/// symbolic in the source's distinct bound. `None` for a heap under 8, a
/// `delta` too small for the heap, or a shape the library would not build.
#[allow(dead_code)] // Read again in #1207 PR 8: δ of a distinct count over a source's own rows, with θ.
pub(crate) fn distinct_guarantee(
    params: &SketchParams,
    delta: f64,
    source: &str,
) -> Option<AccuracyGuarantee> {
    let SketchParams::UnivMon {
        heap_size,
        sketch_rows,
        sketch_cols,
        layers,
    } = params
    else {
        return None;
    };
    if *heap_size < DISTINCT_MIN_HEAP
        || !hash_independent(*sketch_rows, *sketch_cols, *layers)
        || !(delta > 0.0 && delta < 1.0)
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
                "population_scope": "complete_evaluation_including_merged_tumbling_windows"}),
            query: "Cardinality".into(),
        }],
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Layers `asap_sketchlib` builds.
    const MAX_LAYERS: u8 = asap_sketchlib::sketch_framework::univmon::UNIVMON_MAX_LAYERS as u8;

    /// A shape for readouts without a size model: the total (exact
    /// whatever the shape) and entropy (uncertified).
    const BASELINE: SketchParams = SketchParams::UnivMon {
        heap_size: 256,
        sketch_rows: 5,
        sketch_cols: 1024,
        layers: 16,
    };

    fn univmon(heap_size: u32, rows: u32, cols: u32, layers: u8) -> SketchParams {
        SketchParams::UnivMon {
            heap_size,
            sketch_rows: rows,
            sketch_cols: cols,
            layers,
        }
    }

    fn l2(params: &SketchParams) -> Option<AccuracyGuarantee> {
        guarantee(
            &SketchAlgorithm::UnivMon,
            params,
            &SketchStatistic::FrequencyL2,
        )
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

    /// The executor builds at most 20 rows, the rows a UnivMon state
    /// encodes, so a 21-row UnivMon certifies nothing even though its hashes
    /// fit the budget (21 · (5 + 1) = 126 ≤ 128 at 32 columns).
    #[test]
    fn univmon_past_twenty_rows_is_not_hash_independent() {
        assert!(hash_independent(20, 32, 1));
        assert!(!hash_independent(21, 32, 1));
        assert!(l2(&univmon(1, 20, 32, 1)).is_some());
        assert!(l2(&univmon(1, 21, 32, 1)).is_none());
    }

    /// Without the source's bound the failure probability stays unknown
    /// and no target is met; a bound the pyramid is too shallow for gives
    /// probability 1.
    #[test]
    fn distinct_guarantee_needs_the_source_bound_and_enough_layers() {
        let params = univmon(54_000, 1, 1, MAX_LAYERS);
        let guarantee = distinct_guarantee(&params, 0.01, "m").unwrap();
        assert_eq!(guarantee.spec.failure_probability.evaluate(), None);
        assert!(!meets(&guarantee, 0.05, Some(0.01)));
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

    /// Only a heap of at least 8 and a buildable shape gets a distinct
    /// guarantee; the guarantee without a source is none.
    #[test]
    fn distinct_guarantee_needs_a_heap_of_eight_and_a_buildable_shape() {
        assert!(distinct_guarantee(&univmon(7, 1, 1, 19), 0.5, "m").is_none());
        assert!(distinct_guarantee(&univmon(8, 1, 1, 20), 0.5, "m").is_none());
        assert!(distinct_guarantee(&univmon(8, 1, 1, 19), 0.5, "m").is_some());
        let sized = univmon(54_000, 1, 1, MAX_LAYERS);
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
        let params = univmon(54_000, 1, 1, MAX_LAYERS);
        let total = guarantee(
            &SketchAlgorithm::UnivMon,
            &params,
            &SketchStatistic::PointCount {
                key: asap_types::ir::scalar::expr_ir::ColumnRef::SampleValue,
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

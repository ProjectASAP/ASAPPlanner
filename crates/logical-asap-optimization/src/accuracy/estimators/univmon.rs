//! UnivMon's readouts and the one guarantee `asap_sketchlib` documents for
//! them (`docs/api/api_univmon.md` § Paper, Guarantee).
//!
//! UnivMon follows Liu et al., "One Sketch to Rule Them All: Rethinking
//! Network Flow Monitoring with UnivMon" (SIGCOMM 2016), Algorithm 2: every
//! readout is a G-sum `Σ g(f_i)` computed by the recursion over the layers'
//! heaps. The paper states no `(ε, δ)` bound of its own; it defers to
//! Braverman and Ostrovsky's Recursive Sum (arXiv:1011.2571, Algorithm 4.1,
//! Lemma 3.8, Theorem 4.2). That statement is the only rule here: the G-sum
//! estimate is within `(1 ± ε)` of `Σ g(f_i)` with probability at least 0.7
//! (failure probability [`FAILURE_PROBABILITY`] = 0.3), provided that
//! - **layers:** `φ ≥ ⌈log2 n⌉ + 1`, `n` the distinct keys of one
//!   evaluation ([`layers_for`]). `n` is the source's
//!   `max_distinct_per_evaluation`, which only plan selection reads, so the
//!   failure probability is left symbolic in it ([`layers_statistic`]);
//! - **heap:** `heap_size ≥ φ³/ε²` (`1/α` with Algorithm 4.1's
//!   `α = ε²/φ³`), room for every key whose `g(f_i)` is at least `α` of its
//!   layer's G-sum ([`cover_size`]);
//! - **Count Sketch:** those keys' estimates give `g` within `(1 ± ε)`
//!   ([`count_sketch_meets`], which states the rule used).
//!
//! What each readout gets:
//! - the total (a bare `PointCount`, the L1 norm) is the exact unit-update
//!   count (§4.3, footnote 4: a single counter);
//! - L2 is `sqrt` of the `g(x) = x²` G-sum: a `(1 ± ε)` F2 gives a
//!   `(1 ± ε)` L2, relative error `ε` at failure probability 0.3;
//! - the distinct count is the `g(x) = x⁰` G-sum: relative error `ε` at
//!   failure probability 0.3;
//! - entropy has no guarantee. Its G-sum is `Σ f log2 f` and the entropy
//!   `log2 m − Y/m`, so a `(1 ± ε)` `Y` bounds the entropy only to
//!   `± ε · Y/m ≤ ε log2 m`, an absolute error that grows with `m`; and the
//!   Count Sketch rule does not cover `x log x`'s heavy keys (one key of
//!   frequency 4 is heavy among a million singletons, which contribute
//!   nothing to `Y` but all of the Count Sketch's noise).
//!
//! A target asking for a failure probability below 0.3 is not met: the
//! library takes no median of copies. Any condition the shape does not
//! meet, or an unknown `n`, leaves no guarantee (failure probability 1).
//!
//! Every bound assumes the key hashes are uniformly random and independent
//! ([`hash_independent`]). Terminal-mode states (OctoSketch aggregators fed
//! promoted deltas) are not covered: the executor rejects them.
use super::*;
use asap_types::ir::operator::AggIntent;

/// Layers `asap_sketchlib` builds: layer `i` hashes at seed index `i`, and
/// seed 19 picks the layers a key reaches.
pub(crate) const MAX_LAYERS: u8 = asap_sketchlib::sketch_framework::univmon::MAX_LAYER_SIZE as u8;
/// The most rows a sized Count Sketch gets: the deepest odd shape under the
/// library's 20-row limit.
const MAX_ROWS: u32 = 19;
/// Each row of a layer reads its column and sign bits from one hash.
const HASH_BITS: u32 = 128;
/// The widest sized Count Sketch: `2^26` columns.
const MAX_COL_BITS: u32 = 26;
const MAX_HEAP: u32 = 1 << 26;
/// Theorem 4.2: success probability at least 0.7.
pub const FAILURE_PROBABILITY: f64 = 0.3;

const CONTRACT: &str = "univmon_recursive_sum_theorem_4_2_v1";
/// Names the failure probability of a UnivMon of `layers` layers, left
/// symbolic in `max_distinct[source]` ([`layers_statistic`]).
const LAYERS_COVER: &str = "univmon_failure_probability";
const MAX_DISTINCT: &str = "max_distinct";

/// The shape for readouts without a guarantee to size for: the total
/// (exact whatever the shape) and entropy. Three rows leave a shared state
/// room for the widest sized Count Sketch (`3 · (26 + 1) ≤ 128`).
const BASELINE: SketchParams = SketchParams::UnivMon {
    heap_size: 256,
    sketch_rows: 3,
    sketch_cols: 1024,
    layers: 16,
    relative_error: None,
};

/// The guarantee of `query` read from `params` without a source: only the
/// total, which is exact. L2 and the distinct count need the source's
/// distinct bound ([`source_guarantee`]); entropy has none.
pub(super) fn guarantee(
    _algorithm: &SketchAlgorithm,
    params: &SketchParams,
    query: &SketchStatistic,
) -> Option<AccuracyGuarantee> {
    match (params, query) {
        (SketchParams::UnivMon { .. }, SketchStatistic::PointCount { value: None, .. }) => {
            Some(AccuracyGuarantee::exact("univmon_unit_update_total"))
        }
        _ => None,
    }
}

/// The paper's guarantee of an L2 norm or distinct count read from `params`
/// over `source`'s own rows: relative error `ε` (the error `params` was
/// sized for), failing with probability 0.3 when `params` meets the heap
/// and Count Sketch conditions, left symbolic in the source's distinct
/// bound for the layers ([`layers_statistic`]), and with probability 1
/// when it does not. `None` for any other readout, a shape not sized for
/// an error in `(0, 1)`, or one the library would not build.
pub(crate) fn source_guarantee(
    params: &SketchParams,
    query: &SketchStatistic,
    source: &str,
) -> Option<AccuracyGuarantee> {
    let SketchParams::UnivMon {
        heap_size,
        sketch_rows,
        sketch_cols,
        layers,
        relative_error: Some(eps),
    } = params
    else {
        return None;
    };
    if !matches!(
        query,
        SketchStatistic::FrequencyL2 | SketchStatistic::Cardinality
    ) || !(*eps > 0.0 && *eps < 1.0)
        || !hash_independent(*sketch_rows, *sketch_cols, *layers)
    {
        return None;
    }
    let meets = f64::from(*heap_size) >= cover_size(*eps, *layers)
        && count_sketch_meets(*eps, *layers, *sketch_rows, *sketch_cols);
    let failure_probability = if meets {
        ProbabilityExpr::Unknown {
            statistic: layers_statistic(*layers, source),
        }
    } else {
        ProbabilityExpr::Constant { value: 1.0 }
    };
    Some(AccuracyGuarantee::new(
        AccuracyMetric::Relative,
        BoundExpr::Constant { value: *eps },
        failure_probability,
        vec![GuaranteeSource::SketchEvaluation {
            algorithm: "UnivMon".into(),
            contract: CONTRACT.into(),
            params: serde_json::json!({"heap_size": heap_size, "sketch_rows": sketch_rows,
                "sketch_cols": sketch_cols, "layers": layers, "relative_error": eps,
                "max_distinct": format!("{MAX_DISTINCT}[{source}]"),
                "source": "arXiv:1011.2571 Algorithm 4.1, Lemma 3.8, Theorem 4.2",
                "hash_assumption": "independent_uniform_layer_hashes",
                "mode": "standard",
                "population_scope": "complete_evaluation_including_merged_panes"}),
            query: format!("{query:?}"),
        }],
    ))
}

/// Whether `asap_sketchlib` builds this shape, with independent hashes: at
/// most [`MAX_LAYERS`] layers and 20 rows, and `rows · (⌈log2 cols⌉ + 1) ≤
/// 128` ([`asap_sketchlib::UnivMon::check_shape`]; the heap size is not part
/// of the hash rules).
pub(crate) fn hash_independent(rows: u32, cols: u32, layers: u8) -> bool {
    asap_sketchlib::UnivMon::check_shape(1, rows as usize, cols as usize, usize::from(layers))
        .is_ok()
}

/// `1/α = φ³/ε²`: the most keys whose `g(f_i)` is at least `α = ε²/φ³` of
/// their layer's G-sum (their values sum to at most that G-sum), the heap
/// size the cover needs.
fn cover_size(eps: f64, layers: u8) -> f64 {
    f64::from(layers).powi(3) / (eps * eps)
}

/// The relative error `ε_f` of a frequency estimate that keeps `g(x) = x²`
/// within `(1 ± ε)`: `(1 + ε_f)² ≤ 1 + ε`, and then `(1 − ε_f)² ≥ 1 − ε`
/// too. A distinct count needs only a positive estimate, which any
/// `ε_f < 1` gives, so one rule serves both readouts.
fn estimate_error(eps: f64) -> f64 {
    (1.0 + eps).sqrt() - 1.0
}

/// The Count Sketch rule: every key the cover needs gets an estimate
/// within `(1 ± ε_f)` ([`estimate_error`]) with probability at least
/// `1 − 1/φ`, Algorithm 4.1's per-layer `δ_j = 1/φ`. With `K = φ³/ε²`
/// ([`cover_size`]) and `q` the chance two keys share a column
/// ([`super::column_collision`]), a key with `f_i² ≥ F2/K` misses in one
/// row only when one of the other `K` largest keys shares its column
/// (probability at most `qK`) or the rest add more than `ε_f f_i`
/// (Chebyshev, variance at most `q F2`: probability at most `qK/ε_f²`). A
/// distinct count's heavy keys are every key of a layer of at most `K`
/// keys, where only the first term applies. The median of the rows misses
/// only when `⌈rows/2⌉` rows do, a binomial tail.
///
/// Two limits: the rule bounds each needed key alone, not all of them at
/// once (a union bound over `K` keys needs widths no hash budget allows),
/// and it bounds no key the heap holds below the cover's threshold, which
/// Definition 3.2 also asks to be within `(1 ± ε)`.
fn count_sketch_meets(eps: f64, layers: u8, rows: u32, cols: u32) -> bool {
    let error = estimate_error(eps);
    let row_failure =
        super::column_collision(cols) * cover_size(eps, layers) * (1.0 + 1.0 / (error * error));
    row_failure < 1.0
        && super::binomial_upper_tail(rows, row_failure, rows.div_ceil(2))
            <= 1.0 / f64::from(layers)
}

/// The smallest Count Sketch (rows times columns, odd rows, power-of-two
/// columns) meeting [`count_sketch_meets`] within the hash budget, or
/// `None` when none does.
fn count_sketch_shape(eps: f64, layers: u8) -> Option<(u32, u32)> {
    (1..=MAX_ROWS)
        .step_by(2)
        .filter_map(|rows| {
            let max_bits = (HASH_BITS / rows - 1).min(MAX_COL_BITS);
            (0..=max_bits)
                .map(|bits| 1u32 << bits)
                .find(|&cols| count_sketch_meets(eps, layers, rows, cols))
                .map(|cols| (rows, cols))
        })
        .min_by_key(|&(rows, cols)| u64::from(rows) * u64::from(cols))
}

/// `⌈log2 n⌉ + 1`: the layers the Recursive Sum needs over `n` distinct
/// keys.
pub fn layers_for(max_distinct: u64) -> u32 {
    let ceil_log2 = match max_distinct {
        0 | 1 => 0,
        n => u64::BITS - (n - 1).leading_zeros(),
    };
    ceil_log2 + 1
}

/// The statistic a UnivMon of `layers` layers over `source` leaves its
/// failure probability as.
pub(crate) fn layers_statistic(layers: u8, source: &str) -> String {
    format!("{LAYERS_COVER}[layers={layers}]({MAX_DISTINCT}[{source}])")
}

/// The layers and source a [`layers_statistic`] names.
pub fn parse_layers_statistic(statistic: &str) -> Option<(u8, &str)> {
    let rest = statistic
        .strip_prefix(LAYERS_COVER)?
        .strip_prefix("[layers=")?;
    let (layers, rest) = rest.split_once(']')?;
    let source = rest
        .strip_prefix('(')?
        .strip_prefix(MAX_DISTINCT)?
        .strip_prefix('[')?
        .strip_suffix("])")?;
    Some((layers.parse().ok()?, source))
}

/// The probability a [`layers_statistic`] names, given the source's
/// distinct bound `n`: 0.3 when `layers ≥ ⌈log2 n⌉ + 1`, and 1 (no
/// guarantee) when the pyramid is too shallow.
pub fn failure_probability(layers: u8, max_distinct: u64) -> f64 {
    if u32::from(layers) >= layers_for(max_distinct) {
        FAILURE_PROBABILITY
    } else {
        1.0
    }
}

/// A UnivMon for relative error `eps` on `layers` layers: the cover's heap
/// ([`cover_size`]) and the smallest Count Sketch meeting the rule
/// ([`count_sketch_shape`]). Past the limits it gets the largest heap and
/// one row of the widest Count Sketch, and its guarantee fails with
/// probability 1.
fn sized(eps: f64, layers: u8) -> SketchParams {
    let (sketch_rows, sketch_cols) =
        count_sketch_shape(eps, layers).unwrap_or((1, 1 << MAX_COL_BITS));
    SketchParams::UnivMon {
        heap_size: saturating_ceil(cover_size(eps, layers), 1, MAX_HEAP),
        sketch_rows,
        sketch_cols,
        layers,
        relative_error: Some(eps),
    }
}

/// A UnivMon sized for an error resized for its source's distinct bound
/// `n`: `⌈log2 n⌉ + 1` layers ([`layers_for`]), and the heap and Count
/// Sketch they need at the same error. `None` when `params` was not sized
/// for an error, or `n` needs more layers than the library builds.
pub fn sized_for_distinct_bound(params: &SketchParams, max_distinct: u64) -> Option<SketchParams> {
    let SketchParams::UnivMon {
        relative_error: Some(eps),
        ..
    } = params
    else {
        return None;
    };
    let layers = u8::try_from(layers_for(max_distinct))
        .ok()
        .filter(|&layers| layers <= MAX_LAYERS)?;
    Some(sized(*eps, layers))
}

/// Size a UnivMon for one readout's relative error `eps`:
/// - L2 and the distinct count: [`MAX_LAYERS`] layers, since the layers
///   depend on the source's distinct bound, which only plan selection
///   reads; it resizes the UnivMon for that bound
///   ([`sized_for_distinct_bound`]). The failure probability is the
///   paper's 0.3 whatever the target asks;
/// - total and entropy: the baseline shape (the total is exact; entropy
///   has no guarantee).
pub(super) fn size_params(intent: &AggIntent, eps: f64) -> SketchParams {
    match intent {
        AggIntent::FrequencyL2 { .. } | AggIntent::Cardinality { .. } => sized(eps, MAX_LAYERS),
        _ => BASELINE,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use asap_types::ir::operator::agg_intent::default_cardinality;

    fn l2_intent() -> AggIntent {
        AggIntent::FrequencyL2 {
            col: None,
            accuracy: AccuracyTarget::exact(),
        }
    }

    /// A UnivMon sized for relative error 0.1.
    fn univmon(heap_size: u32, sketch_rows: u32, sketch_cols: u32, layers: u8) -> SketchParams {
        SketchParams::UnivMon {
            heap_size,
            sketch_rows,
            sketch_cols,
            layers,
            relative_error: Some(0.1),
        }
    }

    /// The guarantee with the source's distinct bound `n` bound.
    fn bound(
        params: &SketchParams,
        query: &SketchStatistic,
        max_distinct: u64,
    ) -> Option<AccuracyGuarantee> {
        Some(source_guarantee(params, query, "m")?.bind(&|statistic| {
            let (layers, source) = parse_layers_statistic(statistic)?;
            (source == "m").then(|| failure_probability(layers, max_distinct))
        }))
    }

    fn meets(guarantee: &AccuracyGuarantee, epsilon: f64, delta: f64) -> bool {
        satisfies(
            guarantee,
            &crate::accuracy::native_target(
                &AccuracyTarget::max_error(epsilon).failure_probability(delta),
                guarantee,
            ),
        )
    }

    /// At ε = 0.1 over 16 distinct keys: 5 layers, a heap of
    /// `5³/0.1² = 12,500`, and a 1 × 2^25 Count Sketch (one row fails with
    /// probability `2^-25 · 12,500 · (1 + 1/ε_f²) ≈ 0.157 ≤ 1/5`). L2 and
    /// the distinct count carry relative error 0.1 at failure probability
    /// 0.3: a target at 0.3 is met, one at 0.05 is not.
    #[test]
    fn paper_sizing_and_guarantee_at_eps_0_1() {
        let params = sized_for_distinct_bound(&size_params(&l2_intent(), 0.1), 16).unwrap();
        assert_eq!(params, univmon(12_500, 1, 1 << 25, 5));
        for query in [SketchStatistic::FrequencyL2, SketchStatistic::Cardinality] {
            let guarantee = bound(&params, &query, 16).unwrap();
            assert_eq!(guarantee.spec.accuracy_metric, AccuracyMetric::Relative);
            assert_eq!(guarantee.spec.error.evaluate(), Some(0.1));
            assert_eq!(guarantee.spec.failure_probability.evaluate(), Some(0.3));
            assert!(meets(&guarantee, 0.1, 0.3), "{query:?}");
            assert!(!meets(&guarantee, 0.1, 0.05), "{query:?}");
        }
        // One column fewer than the rule's: the Count Sketch fails, and so
        // does the guarantee.
        let narrow = univmon(12_500, 1, 1 << 24, 5);
        let guarantee = bound(&narrow, &SketchStatistic::FrequencyL2, 16).unwrap();
        assert_eq!(guarantee.spec.failure_probability.evaluate(), Some(1.0));
        // A heap under φ³/ε² fails too.
        let small_heap = univmon(12_499, 1, 1 << 25, 5);
        let guarantee = bound(&small_heap, &SketchStatistic::Cardinality, 16).unwrap();
        assert_eq!(guarantee.spec.failure_probability.evaluate(), Some(1.0));
    }

    /// Each sized shape meets its conditions, and is the smallest that does
    /// among its rows' power-of-two widths.
    #[test]
    fn sized_shapes_meet_the_conditions() {
        for (eps, n) in [(0.1, 4), (0.1, 64), (0.2, 1000), (0.5, 1 << 18)] {
            let params = sized_for_distinct_bound(&size_params(&l2_intent(), eps), n).unwrap();
            let SketchParams::UnivMon {
                heap_size,
                sketch_rows,
                sketch_cols,
                layers,
                ..
            } = params
            else {
                unreachable!()
            };
            assert_eq!(u32::from(layers), layers_for(n));
            assert!(f64::from(heap_size) >= cover_size(eps, layers));
            assert!(hash_independent(sketch_rows, sketch_cols, layers));
            assert!(count_sketch_meets(eps, layers, sketch_rows, sketch_cols));
            assert!(!count_sketch_meets(
                eps,
                layers,
                sketch_rows,
                sketch_cols / 2
            ));
            let guarantee = bound(&params, &SketchStatistic::FrequencyL2, n).unwrap();
            assert!(meets(&guarantee, eps, 0.3), "({eps}, {n}): {params:?}");
        }
    }

    /// A target the Count Sketch rule cannot reach within the hash budget
    /// (ε = 0.1 on 19 layers: a heap of 685,900 needs about 2^34 columns)
    /// gets the largest shape, whose guarantee fails with probability 1.
    #[test]
    fn unreachable_targets_have_no_guarantee() {
        let params = sized_for_distinct_bound(&size_params(&l2_intent(), 0.1), 1 << 18).unwrap();
        let SketchParams::UnivMon {
            sketch_rows,
            sketch_cols,
            layers,
            ..
        } = params
        else {
            unreachable!()
        };
        assert_eq!((sketch_rows, sketch_cols, layers), (1, 1 << 26, 19));
        let guarantee = bound(&params, &SketchStatistic::FrequencyL2, 1 << 18).unwrap();
        assert_eq!(guarantee.spec.failure_probability.evaluate(), Some(1.0));
    }

    /// Without the source's bound the failure probability stays unknown;
    /// a bound the layers do not cover gives probability 1.
    #[test]
    fn the_layers_need_the_source_bound() {
        let params = sized_for_distinct_bound(&size_params(&default_cardinality(), 0.1), 16).unwrap();
        let guarantee = source_guarantee(&params, &SketchStatistic::Cardinality, "m").unwrap();
        assert_eq!(guarantee.spec.failure_probability.evaluate(), None);
        assert!(!meets(&guarantee, 0.1, 0.3));
        assert_eq!(failure_probability(5, 16), 0.3);
        assert_eq!(failure_probability(5, 17), 1.0);
        assert_eq!(failure_probability(1, 1), 0.3);
        assert_eq!(layers_for(1), 1);
        assert_eq!(layers_for(2), 2);
        assert_eq!(layers_for(1 << 18), 19);
        assert_eq!(layers_for((1 << 18) + 1), 20);
        // 20 layers are past what the library builds: no resize.
        assert_eq!(sized_for_distinct_bound(&params, (1 << 18) + 1), None);
    }

    /// Only L2 and the distinct count of a UnivMon sized for an error get
    /// the paper's guarantee; the total stays exact; entropy has none.
    #[test]
    fn readouts_and_shapes_without_a_guarantee() {
        let params = sized_for_distinct_bound(&size_params(&l2_intent(), 0.1), 16).unwrap();
        assert!(source_guarantee(&params, &SketchStatistic::FrequencyEntropy, "m").is_none());
        assert!(source_guarantee(&BASELINE, &SketchStatistic::FrequencyL2, "m").is_none());
        let unbuildable = univmon(12_500, 21, 8, 5);
        assert!(source_guarantee(&unbuildable, &SketchStatistic::FrequencyL2, "m").is_none());
        for query in [SketchStatistic::FrequencyL2, SketchStatistic::Cardinality] {
            assert!(guarantee(&SketchAlgorithm::UnivMon, &params, &query).is_none());
        }
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

    /// The statistic names its layers and source, any source name.
    #[test]
    fn layers_statistics_round_trip() {
        for source in ["flows_src_ip", "a[b]", "x])(", ""] {
            let statistic = layers_statistic(19, source);
            assert_eq!(parse_layers_statistic(&statistic), Some((19, source)));
        }
        assert_eq!(parse_layers_statistic("max_distinct[m]"), None);
    }

    /// The library builds at most 20 rows, so a 21-row UnivMon is not
    /// built even though its hashes fit the budget.
    #[test]
    fn univmon_past_twenty_rows_is_not_hash_independent() {
        assert!(hash_independent(20, 8, 1));
        assert!(!hash_independent(21, 8, 1));
        assert!(!hash_independent(8, 1 << 16, 1));
        assert!(hash_independent(7, 1 << 16, 1));
    }
}

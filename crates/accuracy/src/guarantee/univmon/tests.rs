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
    guarantee(params, &SketchStatistic::FrequencyL2)
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

/// The total stays exact; entropy stays uncertified.
#[test]
fn total_is_exact_and_entropy_uncertified() {
    let params = univmon(54_000, 1, 1, MAX_LAYERS);
    let total = guarantee(
        &params,
        &SketchStatistic::PointCount {
            key: asap_types::ir::scalar::expr_ir::ColumnRef::SampleValue,
            value: None,
        },
    )
    .unwrap();
    assert_eq!(total.spec.error.evaluate(), Some(0.0));
    assert_eq!(total.spec.failure_probability.evaluate(), Some(0.0));
    assert!(guarantee(&BASELINE, &SketchStatistic::FrequencyEntropy).is_none());
}

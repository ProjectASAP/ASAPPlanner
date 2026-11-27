use super::*;

/// A DDSketch takes zero and negative samples (its zero bucket and
/// negative store) as well as positive ones; it refuses NaN, ±Inf, and
/// magnitudes outside its indexable range, including a nonzero magnitude
/// below it, which would lose its relative error in the zero bucket.
#[test]
fn accepts_zero_and_signed_samples_within_its_indexable_range() {
    let updater = DDSketchAccumulatorUpdater::new(0.01);
    let (minimum, maximum) = asap_sketchlib::sketches::ddsketch::ddsketch_indexable_bounds(0.01);
    for value in [0.0, -0.0, 3.0, -3.0, minimum, -minimum, maximum, -maximum] {
        assert!(updater.validate_single_input(value).is_ok(), "{value}");
    }
    for value in [
        f64::NAN,
        f64::INFINITY,
        f64::NEG_INFINITY,
        minimum / 2.0,
        -minimum / 2.0,
        maximum * 2.0,
        -maximum * 2.0,
    ] {
        assert!(updater.validate_single_input(value).is_err(), "{value}");
    }
}

/// A row's validation reads the bounds its updater computed once, not
/// `ddsketch_indexable_bounds` again (66 ns a call; with the call in
/// `DdSketch::try_add`, two thirds of a row's update before).
#[test]
fn validation_computes_the_indexable_bounds_once_per_updater() {
    let mut updater = DDSketchAccumulatorUpdater::new(0.05);
    let calls_before = INDEXABLE_BOUNDS_CALLS.with(std::cell::Cell::get);
    for i in 1..=1000 {
        let value = f64::from(i);
        updater.validate_single_input(value).unwrap();
        updater.update_single(value, 0).unwrap();
    }
    updater.reset();
    updater.validate_single_input(3.0).unwrap();
    let calls = INDEXABLE_BOUNDS_CALLS.with(std::cell::Cell::get) - calls_before;
    assert_eq!(
        calls, 0,
        "validation recomputed the indexable bounds {calls} times"
    );
}

/// Quantiles over 0, 0, 3, 5 and over signed samples are within alpha of
/// PromQL's interpolated quantile, at rank q (n − 1) between adjacent
/// order statistics: here each pair of neighbours shares a sign or
/// includes zero.
#[test]
fn quantiles_over_zero_and_signed_samples() {
    for (samples, expected) in [
        (
            vec![0.0, 0.0, 3.0, 5.0],
            [(0.25, 0.0), (0.5, 1.5), (0.75, 3.5), (1.0, 5.0)],
        ),
        (
            vec![-7.0, -3.0, 0.0, 4.0],
            [(0.25, -4.0), (0.5, -1.5), (0.75, 1.0), (1.0, 4.0)],
        ),
    ] {
        let mut updater = DDSketchAccumulatorUpdater::new(0.01);
        for value in &samples {
            updater.validate_single_input(*value).unwrap();
            updater.update_single(*value, 0).unwrap();
        }
        for (q, exact) in expected {
            let estimate = updater
                .acc
                .estimate(&planner_types::summary_type::SketchStatistic::Quantile { q })
                .unwrap();
            assert!(
                (estimate - exact).abs() <= 0.01 * f64::abs(exact),
                "{samples:?} q={q}: {estimate} vs {exact}"
            );
        }
    }
}

use super::*;
use asap_types::ir::operator::arguments::Reduction;
use asap_types::ir::operator::AggIntent;
use asap_types::ir::scalar::expr_ir::ArithmeticOpKind;
use asap_types::ir::QueryRoot;
use asap_types::stages::MemoQuery;
use asap_types::summary_type::{SketchAlgorithm, SketchStatistic};
use asap_types::test_support::memo_builders::{
    estimate, group, own_candidate, quantile_over_time, query, scan_group, sketch_state,
    with_series,
};
use asap_types::test_support::memo_fixtures::{aggregate, arithmetic};

/// The levels [`readout_levels`] gives the readout candidate `readout`.
fn levels_of(memo: &Memo, readout: MemoGroupId) -> Vec<RequiredAccuracy> {
    let order = memo.topological_order().expect("acyclic");
    readout_levels(memo, &ErrorMetricsOfMemo::of(memo, &order), &order)
        .remove(&own_candidate(readout))
        .expect("a sketch readout")
        .levels
}

/// A whole-result failure probability is split over the values one
/// evaluation returns: δ = 0.01 over 100 series is 1e-4 per value.
#[test]
fn a_whole_result_failure_probability_is_split_per_value() {
    let mut memo = Memo::new(&with_series("m", 100));
    let (_, _, readout) = quantile_over_time(&mut memo, SketchAlgorithm::Kll, 0.9, 300);
    memo.add_query(MemoQuery {
        root: QueryRoot::Operator(readout),
        accuracy_target: Some(AccuracyTarget::max_error(0.01).failure_probability(0.01)),
        number_of_values_per_query_evaluation: Some(100),
    });
    let [required] = levels_of(&memo, readout)[..] else {
        panic!("one requirement");
    };
    assert!((required.failure_probability - 1e-4).abs() < 1e-12);
}

/// The snapshot workload
/// `ratio-of-quantiles-1h-mvp`: `quantile_over_time(0.9, data[1h]) /
/// quantile_over_time(0.5, data[1h])` with relative error 0.05 and
/// δ = 0.01 over the whole result of 10 000 series is δ = 1e-6 per
/// value, below the grid's 1e-5. The δ levels continue in powers of ten
/// below it, so both readouts find levels whose sum stays within 1e-6
/// and (K) can hold.
#[test]
fn a_ratio_whose_per_value_failure_probability_is_below_the_grid_splits_it_over_its_readouts() {
    let mut memo = Memo::new(&with_series("m", 10_000));
    let (_, state, p90) = quantile_over_time(&mut memo, SketchAlgorithm::DDSketch, 0.9, 3600);
    let p50 = estimate(&mut memo, state, SketchStatistic::Quantile { q: 0.5 });
    let ratio = group(&mut memo, arithmetic(ArithmeticOpKind::Div, p90, p50));
    memo.add_query(MemoQuery {
        root: QueryRoot::Operator(ratio),
        accuracy_target: Some(
            AccuracyTarget::max_error(0.05)
                .failure_probability(0.01)
                .accuracy_metric(AccuracyMetric::Relative),
        ),
        number_of_values_per_query_evaluation: Some(10_000),
    });
    let smallest_failure_probability = |readout| {
        let levels = levels_of(&memo, readout);
        assert!(!levels.is_empty(), "every readout has a level");
        levels
            .into_iter()
            .map(|required| required.failure_probability)
            .fold(f64::INFINITY, f64::min)
    };
    let (p90_delta, p50_delta) = (
        smallest_failure_probability(p90),
        smallest_failure_probability(p50),
    );
    assert!(
        p90_delta + p50_delta <= 1e-6,
        "{p90_delta} + {p50_delta} > 1e-6"
    );
}

/// A `max` reads every readout of one evaluation, and fails when any of
/// them does: `max(quantile_over_time(0.9, m[5m]))` over 1000 series
/// with δ = 0.01 returns one value, yet each of its 1000 readouts must
/// fail at most 1e-5 for the union bound to hold.
#[test]
fn a_max_over_many_readouts_splits_the_failure_probability_over_them() {
    let mut memo = Memo::new(&with_series("m", 1000));
    let (_, _, readout) = quantile_over_time(&mut memo, SketchAlgorithm::DDSketch, 0.9, 300);
    let max = group(
        &mut memo,
        aggregate(Reduction::by(vec![]), AggIntent::Max { col: None }, readout),
    );
    memo.add_query(MemoQuery {
        root: QueryRoot::Operator(max),
        accuracy_target: Some(
            AccuracyTarget::max_error(0.05)
                .failure_probability(0.01)
                .accuracy_metric(AccuracyMetric::Relative),
        ),
        number_of_values_per_query_evaluation: Some(1),
    });
    let levels = levels_of(&memo, readout);
    assert!(!levels.is_empty());
    for level in levels {
        assert!(
            level.failure_probability <= 1e-5 * (1.0 + 1e-9),
            "{level:?}"
        );
    }
}

/// A count reads no value (β = 0), so the readouts below it get no
/// level from it: `count(quantile_over_time(0.9, m[5m]))` requires
/// nothing of its readout, not an infinite error.
#[test]
fn a_count_over_readouts_gives_them_no_level() {
    let mut memo = Memo::new(&with_series("m", 1000));
    let (_, _, readout) = quantile_over_time(&mut memo, SketchAlgorithm::DDSketch, 0.9, 300);
    let count = group(
        &mut memo,
        aggregate(Reduction::by(vec![]), AggIntent::Count, readout),
    );
    query(
        &mut memo,
        count,
        Some(AccuracyTarget::max_error(0.05).accuracy_metric(AccuracyMetric::Relative)),
    );
    assert_eq!(levels_of(&memo, readout), vec![]);
}

/// A readout of one value per evaluation splits no failure probability
/// over its scanned series: `SELECT count(DISTINCT dst) FROM flows` at
/// δ = 0.01 over 1000 groups of rows reads one distinct count, so its
/// KMV readout gets 0.01, not 1e-5.
#[test]
fn a_readout_of_one_value_keeps_the_whole_failure_probability() {
    let mut memo = Memo::new(&with_series("flows", 1000));
    let rows = scan_group(&mut memo, "flows", &["dst"], vec![]);
    let state = sketch_state(&mut memo, rows, SketchAlgorithm::Kmv, Reduction::by(vec![]));
    let distinct = estimate(&mut memo, state, SketchStatistic::Cardinality);
    memo.add_query(MemoQuery {
        root: QueryRoot::Operator(distinct),
        accuracy_target: Some(
            AccuracyTarget::max_error(0.05)
                .failure_probability(0.01)
                .accuracy_metric(AccuracyMetric::Relative),
        ),
        number_of_values_per_query_evaluation: Some(1),
    });
    let [required] = levels_of(&memo, distinct)[..] else {
        panic!("one requirement");
    };
    assert!(
        (required.failure_probability - 0.01).abs() < 1e-12,
        "{required:?}"
    );
}

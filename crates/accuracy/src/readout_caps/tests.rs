use super::*;
use crate::NoMeasuredGuarantees;
use asap_types::accuracy::AccuracyScope;
use asap_types::ir::operator::arguments::{PromQLVectorSetOpKind, Reduction};
use asap_types::ir::operator::AggIntent;
use asap_types::ir::scalar::expr_ir::ArithmeticOpKind;
use asap_types::ir::{NonASAPOp, QueryRoot};
use asap_types::stages::{MemoQuery, ValueSign};
use asap_types::summary_type::{SketchAlgorithm, SketchStatistic};
use asap_types::test_support::memo_builders::{
    candidate, estimate, group, own_candidate, quantile_over_time, query, scan_group, sketch_state,
    with_series,
};
use asap_types::test_support::memo_fixtures::{aggregate, arithmetic, range, set_operator};
use asap_types::workload::DataWorkload;

/// The levels [`readout_levels`] gives the readout candidate `readout`.
fn levels_of(memo: &Memo, readout: MemoGroupId) -> Vec<RequiredAccuracy> {
    let order = memo.topological_order().expect("acyclic");
    readout_levels(
        memo,
        &ErrorMetricsOfMemo::of(memo, &order),
        &order,
        &NoMeasuredGuarantees,
    )
    .of_readout
    .remove(&own_candidate(readout))
    .expect("a sketch readout")
    .levels
}

/// A whole-result failure probability is split over the values one
/// evaluation returns: δ = 0.01 over 100 series is 1e-4 per value.
#[test]
fn a_whole_result_failure_probability_is_split_per_value() {
    let mut memo = Memo::new(&with_series("m", 100));
    let (_, _, readout) = quantile_over_time(&mut memo, SketchAlgorithm::KLL, 0.9, 300);
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
    let state = sketch_state(&mut memo, rows, SketchAlgorithm::KMV, Reduction::by(vec![]));
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

/// `quantile_over_time(0.9, m[5m]) and quantile_over_time(0.5, m[5m])`
/// copies the left values and reads only which series the right has
/// (β = 0), so it combines no errors: the left readout gets the single
/// level of its cap, 0.05 at δ = 0.01 over 100 series (1e-4), not the
/// composed grid, and the right one gets none.
#[test]
fn an_and_combines_no_readout_errors() {
    let target = AccuracyTarget::max_error(0.05)
        .failure_probability(0.01)
        .accuracy_metric(AccuracyMetric::Relative);
    let readouts_under = |kind| {
        let mut memo = Memo::new(&with_series("m", 100));
        let (_, state, p90) = quantile_over_time(&mut memo, SketchAlgorithm::DDSketch, 0.9, 300);
        let p50 = estimate(&mut memo, state, SketchStatistic::Quantile { q: 0.5 });
        let root = group(&mut memo, set_operator(kind, p90, p50));
        memo.add_query(MemoQuery {
            root: QueryRoot::Operator(root),
            accuracy_target: Some(target.clone()),
            number_of_values_per_query_evaluation: Some(100),
        });
        (levels_of(&memo, p90), levels_of(&memo, p50))
    };
    let (left, right) = readouts_under(PromQLVectorSetOpKind::And);
    let [required] = left[..] else {
        panic!("one level for the left readout: {left:?}");
    };
    assert!((required.max_error - 0.05).abs() < 1e-12, "{required:?}");
    assert!(
        (required.failure_probability - 1e-4).abs() < 1e-12,
        "{required:?}"
    );
    assert_eq!(right, vec![]);
}

/// `quantile_over_time(0.9, m[5m]) or quantile_over_time(0.5, m[5m])`
/// copies each output value from one side, so its error is the larger of
/// the two readouts' errors, not their sum: each readout gets the single
/// level of its cap, 0.05. Its whole result covers its 100 output values,
/// each copied from one of the 200 readout values below it, so δ = 0.01
/// splits over 100 of them: 1e-4 per value (min(200, 100 × 1)). A ratio
/// of the same readouts combines both errors and keeps the composed grid.
/// The values are non-negative, so DDSketch's guarantee holds.
#[test]
fn an_or_takes_each_value_from_one_readout() {
    let target = AccuracyTarget::max_error(0.05)
        .failure_probability(0.01)
        .accuracy_metric(AccuracyMetric::Relative);
    let mut data = with_series("m", 100);
    data.sources
        .get_mut("m")
        .expect("m")
        .column_signs
        .insert("value".into(), ValueSign::NonNegative);
    let readouts_under = |operator: fn(MemoGroupId, MemoGroupId) -> _| {
        let mut memo = Memo::new(&data);
        let (_, state, p90) = quantile_over_time(&mut memo, SketchAlgorithm::DDSketch, 0.9, 300);
        let p50 = estimate(&mut memo, state, SketchStatistic::Quantile { q: 0.5 });
        let root = group(&mut memo, operator(p90, p50));
        memo.add_query(MemoQuery {
            root: QueryRoot::Operator(root),
            accuracy_target: Some(target.clone()),
            number_of_values_per_query_evaluation: Some(100),
        });
        [levels_of(&memo, p90), levels_of(&memo, p50)]
    };
    for levels in readouts_under(|q, r| set_operator(PromQLVectorSetOpKind::Or, q, r)) {
        let [required] = levels[..] else {
            panic!("one level for each readout of `or`: {levels:?}");
        };
        assert!((required.max_error - 0.05).abs() < 1e-12, "{required:?}");
        assert!(
            (required.failure_probability - 1e-4).abs() < 1e-12,
            "{required:?}"
        );
    }
    for levels in readouts_under(|q, r| arithmetic(ArithmeticOpKind::Div, q, r)) {
        assert!(
            levels.len() > 1,
            "the composed grid for a ratio: {levels:?}"
        );
    }
}

/// A reduction above an `or` reads every readout value of both sides:
/// `max by (job) (quantile_over_time(0.9, m[5m]) or
/// quantile_over_time(0.5, m[5m]))` over 3 series returns 3 values, but
/// fails when any of the 6 readout values below it does, so each readout
/// gets δ = 0.01 / 6, not 0.01 / 3 (one side's values), which would let
/// the union bound reach 2δ.
#[test]
fn a_reduction_above_an_or_splits_the_failure_probability_over_both_sides() {
    let mut memo = Memo::new(&with_series("m", 3));
    let (_, state, p90) = quantile_over_time(&mut memo, SketchAlgorithm::DDSketch, 0.9, 300);
    let p50 = estimate(&mut memo, state, SketchStatistic::Quantile { q: 0.5 });
    let either = group(&mut memo, set_operator(PromQLVectorSetOpKind::Or, p90, p50));
    let max = group(
        &mut memo,
        aggregate(Reduction::by(vec![0]), AggIntent::Max { col: None }, either),
    );
    memo.add_query(MemoQuery {
        root: QueryRoot::Operator(max),
        accuracy_target: Some(
            AccuracyTarget::max_error(0.05)
                .failure_probability(0.01)
                .accuracy_metric(AccuracyMetric::Relative),
        ),
        number_of_values_per_query_evaluation: Some(3),
    });
    for readout in [p90, p50] {
        let levels = levels_of(&memo, readout);
        let [required] = levels[..] else {
            panic!("one level for each readout: {levels:?}");
        };
        assert!(
            (required.failure_probability - 0.01 / 6.0).abs() < 1e-12,
            "{required:?}"
        );
    }
}

/// A count reads which readouts exist, not their values (β = 0), so an
/// operator above it combines no error of them:
/// `quantile_over_time(0.9, m[5m]) / count(quantile_over_time(0.5, m[5m]))`
/// over 100 series at relative error 0.05 and δ = 0.01 reads one
/// readout's error. The p90 readout gets the single level of its cap,
/// 0.05 · (1 − 0.1) = 0.045 (the division's factor 1 / (1 − 0.1)) at
/// 1e-4 per value, not the composed grid.
#[test]
fn an_operator_above_a_count_of_readouts_combines_one_readout_error() {
    let mut memo = Memo::new(&with_series("m", 100));
    let (_, state, p90) = quantile_over_time(&mut memo, SketchAlgorithm::DDSketch, 0.9, 300);
    let p50 = estimate(&mut memo, state, SketchStatistic::Quantile { q: 0.5 });
    let count = group(
        &mut memo,
        aggregate(Reduction::by(vec![]), AggIntent::Count, p50),
    );
    let ratio = group(&mut memo, arithmetic(ArithmeticOpKind::Div, p90, count));
    memo.add_query(MemoQuery {
        root: QueryRoot::Operator(ratio),
        accuracy_target: Some(
            AccuracyTarget::max_error(0.05)
                .failure_probability(0.01)
                .accuracy_metric(AccuracyMetric::Relative),
        ),
        number_of_values_per_query_evaluation: Some(100),
    });
    let levels = levels_of(&memo, p90);
    let [required] = levels[..] else {
        panic!("one level for the p90 readout: {levels:?}");
    };
    assert!((required.max_error - 0.045).abs() < 1e-12, "{required:?}");
    assert!(
        (required.failure_probability - 1e-4).abs() < 1e-12,
        "{required:?}"
    );
}

// The failure-probability split: δ splits over the readout values the
// output values one (ε, δ) statement covers depend on,
// min(R, covered × fan-in), where R is the readout values one evaluation
// reads, covered the output values the scope covers and fan-in the most
// readout values one output value depends on.

/// The failure probability of the single level `readout` gets from the one
/// query of `memo`, or `None` when the query is listed without a per-value
/// failure probability.
fn failure_probability_of(memo: &Memo, readout: MemoGroupId) -> Option<f64> {
    let order = memo.topological_order().expect("acyclic");
    let required = readout_levels(
        memo,
        &ErrorMetricsOfMemo::of(memo, &order),
        &order,
        &NoMeasuredGuarantees,
    )
    .of_readout
    .remove(&own_candidate(readout))
    .expect("a sketch readout");
    if !required
        .queries_without_per_value_failure_probability
        .is_empty()
    {
        assert_eq!(required.levels, vec![], "a listed query gives no level");
        return None;
    }
    let [level] = required.levels[..] else {
        panic!("one level: {:?}", required.levels);
    };
    Some(level.failure_probability)
}

/// Asserts that `actual` is `Some(expected)`, up to rounding.
fn assert_failure_probability(actual: Option<f64>, expected: f64) {
    let actual = actual.expect("a per-value failure probability");
    assert!(
        (actual - expected).abs() <= expected * 1e-9,
        "{actual} != {expected}"
    );
}

/// A relative-error target at δ = 0.01 over `scope`.
fn relative_target(scope: AccuracyScope) -> AccuracyTarget {
    AccuracyTarget::max_error(0.05)
        .failure_probability(0.01)
        .accuracy_metric(AccuracyMetric::Relative)
        .accuracy_scope(scope)
}

/// `memo` with one query at `root` with `target`, returning `n_q` values.
fn with_query(memo: &mut Memo, root: MemoGroupId, target: AccuracyTarget, n_q: Option<u64>) {
    memo.add_query(MemoQuery {
        root: QueryRoot::Operator(root),
        accuracy_target: Some(target),
        number_of_values_per_query_evaluation: n_q,
    });
}

/// A data workload where `m` holds `series` non-negative series over
/// `jobs` jobs (a sum carries a relative error over values of one sign).
fn with_series_over_jobs(series: u64, jobs: u64) -> DataWorkload {
    let mut data = with_series("m", series);
    let facts = data.sources.get_mut("m").expect("m");
    facts.distinct_values_per_label.insert("job".into(), jobs);
    facts
        .column_signs
        .insert("value".into(), ValueSign::NonNegative);
    data
}

/// `quantile_over_time(0.99, m[5m])` over 8 series with a per-value rank
/// target of 0.0025 at δ = 0.01: each output value is one series' readout,
/// so the readout keeps δ = 0.01 (KLL k = 1404), not 0.01 / 8 (k = 2863).
/// Divisor min(8, 1 × 1) = 1.
#[test]
fn a_per_value_target_without_a_reduction_keeps_the_whole_failure_probability() {
    let mut memo = Memo::new(&with_series("m", 8));
    let (_, _, readout) = quantile_over_time(&mut memo, SketchAlgorithm::KLL, 0.99, 300);
    let target = AccuracyTarget::max_error(0.0025)
        .failure_probability(0.01)
        .accuracy_metric(AccuracyMetric::Rank)
        .accuracy_scope(AccuracyScope::PerValue);
    with_query(&mut memo, readout, target, Some(8));
    assert_failure_probability(failure_probability_of(&memo, readout), 0.01);
}

/// The same query over its whole result covers all 8 output values, each
/// one readout value: divisor min(8, 8 × 1) = 8.
#[test]
fn a_whole_result_target_without_a_reduction_splits_over_the_values_returned() {
    let mut memo = Memo::new(&with_series("m", 8));
    let (_, _, readout) = quantile_over_time(&mut memo, SketchAlgorithm::KLL, 0.99, 300);
    let target = AccuracyTarget::max_error(0.0025)
        .failure_probability(0.01)
        .accuracy_metric(AccuracyMetric::Rank);
    with_query(&mut memo, readout, target, Some(8));
    assert_failure_probability(failure_probability_of(&memo, readout), 0.01 / 8.0);
}

/// A global `max` over 1000 series: its one output value depends on all
/// 1000 readout values, whatever the scope: divisor min(1000, 1 × 1000).
#[test]
fn a_global_max_splits_over_every_readout_value_in_any_scope() {
    for scope in [
        AccuracyScope::PerValue,
        AccuracyScope::WholeResult,
        AccuracyScope::GroupsWithRecordShareAtLeast(0.5),
    ] {
        let mut memo = Memo::new(&with_series("m", 1000));
        let (_, _, readout) = quantile_over_time(&mut memo, SketchAlgorithm::DDSketch, 0.9, 300);
        let max = group(
            &mut memo,
            aggregate(Reduction::by(vec![]), AggIntent::Max { col: None }, readout),
        );
        with_query(&mut memo, max, relative_target(scope), Some(1));
        assert_failure_probability(failure_probability_of(&memo, readout), 0.01 / 1000.0);
    }
}

/// `sum by (job)` over 1000 series in 10 jobs, per value: one output value
/// depends on the series of its job. The declared facts bound the jobs
/// (`distinct_values_per_label` is an upper bound), not how many series
/// one job holds: the largest job may hold 991 of them. So the fan-in is
/// unknown and the split falls back to R = 1000.
#[test]
fn a_grouped_sum_per_value_splits_over_every_readout_value_when_group_sizes_are_unbound() {
    let mut memo = Memo::new(&with_series_over_jobs(1000, 10));
    let (_, _, readout) = quantile_over_time(&mut memo, SketchAlgorithm::DDSketch, 0.9, 300);
    let sum = group(
        &mut memo,
        aggregate(
            Reduction::by(vec![0]),
            AggIntent::Sum { col: None },
            readout,
        ),
    );
    with_query(
        &mut memo,
        sum,
        relative_target(AccuracyScope::PerValue),
        Some(10),
    );
    assert_failure_probability(failure_probability_of(&memo, readout), 0.01 / 1000.0);
}

/// `sum by (job)` over 1000 series in 10 jobs, whole result: the 10 output
/// values together depend on all 1000 readout values: divisor 1000.
#[test]
fn a_grouped_sum_over_its_whole_result_splits_over_every_readout_value() {
    let mut memo = Memo::new(&with_series_over_jobs(1000, 10));
    let (_, _, readout) = quantile_over_time(&mut memo, SketchAlgorithm::DDSketch, 0.9, 300);
    let sum = group(
        &mut memo,
        aggregate(
            Reduction::by(vec![0]),
            AggIntent::Sum { col: None },
            readout,
        ),
    );
    with_query(
        &mut memo,
        sum,
        relative_target(AccuracyScope::WholeResult),
        Some(10),
    );
    assert_failure_probability(failure_probability_of(&memo, readout), 0.01 / 1000.0);
}

/// `q or r` copies each output value from one readout value: per value,
/// each readout keeps δ = 0.01 (divisor min(200, 1 × 1) = 1), not
/// δ / 200 over both sides' readout values.
#[test]
fn an_or_per_value_keeps_the_whole_failure_probability() {
    let mut memo = Memo::new(&with_series("m", 100));
    let (_, state, p90) = quantile_over_time(&mut memo, SketchAlgorithm::DDSketch, 0.9, 300);
    let p50 = estimate(&mut memo, state, SketchStatistic::Quantile { q: 0.5 });
    let either = group(&mut memo, set_operator(PromQLVectorSetOpKind::Or, p90, p50));
    with_query(
        &mut memo,
        either,
        relative_target(AccuracyScope::PerValue),
        Some(100),
    );
    for readout in [p90, p50] {
        assert_failure_probability(failure_probability_of(&memo, readout), 0.01);
    }
}

/// Nested reductions: `max(sum by (job) (q))` over 1000 series in 10 jobs.
/// Its one output value depends on every readout value through both
/// reductions: divisor 1000 per value.
#[test]
fn nested_reductions_split_over_every_readout_value_below_them() {
    let mut memo = Memo::new(&with_series_over_jobs(1000, 10));
    let (_, _, readout) = quantile_over_time(&mut memo, SketchAlgorithm::DDSketch, 0.9, 300);
    let sum = group(
        &mut memo,
        aggregate(
            Reduction::by(vec![0]),
            AggIntent::Sum { col: None },
            readout,
        ),
    );
    let max = group(
        &mut memo,
        aggregate(Reduction::by(vec![]), AggIntent::Max { col: None }, sum),
    );
    with_query(
        &mut memo,
        max,
        relative_target(AccuracyScope::PerValue),
        Some(1),
    );
    assert_failure_probability(failure_probability_of(&memo, readout), 0.01 / 1000.0);
}

/// Unknown readout values (no `series_count`): without a reduction, a
/// per-value target still splits over the one readout value each output
/// value depends on (divisor min(unknown, 1 × 1) = 1); under a global `max`
/// the fan-in is the unknown R, so no per-value failure probability follows
/// and the query is listed.
#[test]
fn unknown_readout_values_split_by_the_fan_in_when_it_is_known() {
    let mut memo = Memo::new(&DataWorkload::default());
    let (_, _, readout) = quantile_over_time(&mut memo, SketchAlgorithm::DDSketch, 0.9, 300);
    with_query(
        &mut memo,
        readout,
        relative_target(AccuracyScope::PerValue),
        None,
    );
    assert_failure_probability(failure_probability_of(&memo, readout), 0.01);

    let mut memo = Memo::new(&DataWorkload::default());
    let (_, _, readout) = quantile_over_time(&mut memo, SketchAlgorithm::DDSketch, 0.9, 300);
    let max = group(
        &mut memo,
        aggregate(Reduction::by(vec![]), AggIntent::Max { col: None }, readout),
    );
    with_query(
        &mut memo,
        max,
        relative_target(AccuracyScope::PerValue),
        Some(1),
    );
    assert_eq!(failure_probability_of(&memo, readout), None);
}

/// A whole-result target with unknown `n_q` but known readout values: the
/// union bound over all R = 100 readout values holds: δ / 100.
#[test]
fn a_whole_result_with_unknown_values_returned_splits_over_the_readout_values() {
    let mut memo = Memo::new(&with_series("m", 100));
    let (_, _, readout) = quantile_over_time(&mut memo, SketchAlgorithm::DDSketch, 0.9, 300);
    with_query(
        &mut memo,
        readout,
        relative_target(AccuracyScope::WholeResult),
        None,
    );
    assert_failure_probability(failure_probability_of(&memo, readout), 0.01 / 100.0);
}

/// `GroupsWithRecordShareAtLeast(0.25)` over 8 series without a reduction
/// covers at most ⌊1 / 0.25⌋ = 4 output values, one readout value each:
/// divisor min(8, 4 × 1) = 4, not 8.
#[test]
fn a_record_share_scope_splits_over_the_readout_values_of_the_groups_it_covers() {
    let mut memo = Memo::new(&with_series("m", 8));
    let (_, _, readout) = quantile_over_time(&mut memo, SketchAlgorithm::DDSketch, 0.9, 300);
    with_query(
        &mut memo,
        readout,
        relative_target(AccuracyScope::GroupsWithRecordShareAtLeast(0.25)),
        Some(8),
    );
    assert_failure_probability(failure_probability_of(&memo, readout), 0.01 / 4.0);
}

/// A ratio of two readouts over 8 series, per value: one output value
/// depends on one readout value of each side, fan-in 2, so the two
/// readouts' failure probabilities sum within δ = 0.01: divisor
/// min(16, 1 × 2) = 2, and the loosest grid level is 1e-3 (≤ 0.005),
/// not 1e-4 (≤ 0.01 / 16). Checked on the counts and the query's grid,
/// since DDSketch's error does not depend on δ, so the readout keeps only
/// the strictest δ of each error level.
#[test]
fn a_per_value_ratio_splits_over_the_two_readout_values_of_each_output_value() {
    let mut memo = Memo::new(&with_series("m", 8));
    let (_, state, p90) = quantile_over_time(&mut memo, SketchAlgorithm::DDSketch, 0.9, 300);
    let p50 = estimate(&mut memo, state, SketchStatistic::Quantile { q: 0.5 });
    let ratio = group(&mut memo, arithmetic(ArithmeticOpKind::Div, p90, p50));
    let target = relative_target(AccuracyScope::PerValue);
    with_query(&mut memo, ratio, target.clone(), Some(8));
    let order = memo.topological_order().expect("acyclic");
    let error_metrics = ErrorMetricsOfMemo::of(&memo, &order);
    let read = readouts_read_by_queries(&memo, &error_metrics, &order);
    assert!(read.composed.contains(&0));
    let counts = read.readout_values[&0];
    assert_eq!(
        counts,
        ReadoutValueCounts {
            per_evaluation: Some(16),
            per_output_value: Some(2),
        }
    );
    let levels = query_levels(
        &target,
        Some(8),
        counts,
        AccuracyMetric::Relative,
        0.05,
        true,
    )
    .expect("a per-value failure probability");
    let loosest = levels
        .into_iter()
        .map(|required| required.failure_probability)
        .fold(0.0, f64::max);
    assert!((loosest - 1e-3).abs() < 1e-15, "{loosest}");
}

/// A readout group whose candidates are a readout and a reduction over
/// other readouts: `quantile(0.9)` over every sample of 1000 series, read
/// from one global sketch, or summed over the 1000 per-series readouts
/// below it (an alternative with the same output). A per-value target on
/// it covers one output value, which the reduction makes depend on all
/// 1000 per-series readout values: each of them gets δ / 1000, not the δ
/// the global readout's own fan-in of 1 would give.
#[test]
fn a_readout_group_holding_a_reduction_splits_over_the_readout_values_below_it() {
    let mut memo = Memo::new(&with_series_over_jobs(1000, 10));
    let (window, _, per_series) =
        quantile_over_time(&mut memo, SketchAlgorithm::DDSketch, 0.9, 300);
    let global_state = sketch_state(
        &mut memo,
        window,
        SketchAlgorithm::DDSketch,
        Reduction::by(vec![]),
    );
    let global = estimate(
        &mut memo,
        global_state,
        SketchStatistic::Quantile { q: 0.9 },
    );
    let sum_of_readouts = Operator::NonASAP(NonASAPOp::Aggregate {
        child: per_series,
        reduction: Reduction::by(vec![]),
        measures: vec![AggIntent::Sum { col: None }],
        output_names: vec!["quantile".into()],
        filters: vec![],
        having: None,
    });
    candidate(&mut memo, global, sum_of_readouts, None);
    with_query(
        &mut memo,
        global,
        relative_target(AccuracyScope::PerValue),
        Some(1),
    );
    assert_failure_probability(failure_probability_of(&memo, per_series), 0.01 / 1000.0);
}

/// A top-k readout chooses its output keys by their estimated values, so
/// one output value (a key in the top 10) depends on the estimates of
/// every key it was ranked against: CMS-with-heap top-10 over 1000 series
/// gets δ / 1000 per value, not δ.
#[test]
fn a_topk_readout_splits_over_every_value_it_ranks() {
    let mut memo = Memo::new(&with_series("m", 1000));
    let input = scan_group(&mut memo, "m", &["job"], vec![]);
    let window = group(&mut memo, range(300_000, input));
    let state = sketch_state(
        &mut memo,
        window,
        SketchAlgorithm::CountMinSketchWithHeap,
        Reduction::PerEntity,
    );
    let topk = estimate(&mut memo, state, SketchStatistic::TopK { k: 10 });
    with_query(
        &mut memo,
        topk,
        AccuracyTarget::max_error(0.05)
            .failure_probability(0.01)
            .accuracy_scope(AccuracyScope::PerValue),
        Some(10),
    );
    assert_failure_probability(failure_probability_of(&memo, topk), 0.01 / 1000.0);
}

/// A concatenation copies each output row from one of its inputs, as `or`
/// does: per value, each of two quantile readouts over 100 series keeps
/// the single level of its cap at δ = 0.01, neither the composed grid nor
/// δ / 2.
#[test]
fn a_concatenation_takes_each_value_from_one_readout() {
    let mut memo = Memo::new(&with_series("m", 100));
    let (_, state, p90) = quantile_over_time(&mut memo, SketchAlgorithm::DDSketch, 0.9, 300);
    let p50 = estimate(&mut memo, state, SketchStatistic::Quantile { q: 0.5 });
    let both = group(
        &mut memo,
        Operator::NonASAP(NonASAPOp::Concat {
            children: vec![p90, p50],
        }),
    );
    with_query(
        &mut memo,
        both,
        relative_target(AccuracyScope::PerValue),
        Some(200),
    );
    for readout in [p90, p50] {
        assert_failure_probability(failure_probability_of(&memo, readout), 0.01);
    }
}

/// Two queries on one DDSketch quantile readout over 100 series, one per
/// value (δ = 0.01) and one over the whole result (δ / 100 = 1e-4), both
/// at relative error 0.05. DDSketch's relative error does not depend on
/// δ, so the same listed builds meet both levels: the looser one is
/// redundant and only the stricter, (0.05, 1e-4), is kept.
#[test]
fn a_level_met_by_the_same_builds_as_a_stricter_one_is_dropped() {
    let mut memo = Memo::new(&with_series("m", 100));
    let (_, _, readout) = quantile_over_time(&mut memo, SketchAlgorithm::DDSketch, 0.9, 300);
    with_query(
        &mut memo,
        readout,
        relative_target(AccuracyScope::PerValue),
        Some(100),
    );
    with_query(
        &mut memo,
        readout,
        relative_target(AccuracyScope::WholeResult),
        Some(100),
    );
    let [level] = levels_of(&memo, readout)[..] else {
        panic!("one level: {:?}", levels_of(&memo, readout));
    };
    assert!((level.max_error - 0.05).abs() < 1e-12, "{level:?}");
    assert_failure_probability(Some(level.failure_probability), 1e-4);
}

/// Levels that different builds meet are both kept: a KLL rank error of
/// 0.0025 over 8 series at δ = 0.01 per value needs k = 1404 and over the
/// whole result (δ / 8) k = 2863, so k = 1404 meets only the first.
#[test]
fn levels_met_by_different_builds_are_both_kept() {
    let mut memo = Memo::new(&with_series("m", 8));
    let (_, _, readout) = quantile_over_time(&mut memo, SketchAlgorithm::KLL, 0.99, 300);
    let rank = |scope| {
        AccuracyTarget::max_error(0.0025)
            .failure_probability(0.01)
            .accuracy_metric(AccuracyMetric::Rank)
            .accuracy_scope(scope)
    };
    with_query(&mut memo, readout, rank(AccuracyScope::PerValue), Some(8));
    with_query(
        &mut memo,
        readout,
        rank(AccuracyScope::WholeResult),
        Some(8),
    );
    let failure_probabilities: Vec<f64> = levels_of(&memo, readout)
        .into_iter()
        .map(|level| level.failure_probability)
        .collect();
    assert_eq!(failure_probabilities, vec![0.01, 0.01 / 8.0]);
}

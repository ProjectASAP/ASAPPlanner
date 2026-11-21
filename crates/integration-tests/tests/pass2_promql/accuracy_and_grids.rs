//! PromQL → Pass 2's error contracts and evaluation grids, through
//! production lowering: the frontend's `Project`s of a function of the
//! value keep a readout's error bounded, and a subquery's resolution is the
//! evaluation grid of the groups below it.
use crate::planning_common::{promql_pass2, promql_pass2_over};
use asap_types::ir::operator::asap::summary_type_of_operator;
use asap_types::ir::schema::FieldDataType;
use asap_types::ir::{ASAPOp, NonASAPOp, Operator};
use asap_types::stages::{CandidateForMemoGroupId, LogicalASAPMemoCandidates, MemoGroupId};
use serde_json::{json, Value};

/// Pass 2's output for `query` at `accuracy`.
fn pass2(query: &str, accuracy: Value) -> LogicalASAPMemoCandidates {
    promql_pass2(&[(query, accuracy)])
}

/// The sketch readouts (`SummaryEstimate` over a sketch state) that
/// survive Pass 2.
fn live_sketch_readouts(logical: &LogicalASAPMemoCandidates) -> usize {
    let memo = &logical.memo;
    let mut live = 0;
    for (group, memo_group) in memo.groups().iter().enumerate() {
        for (index_in_group, candidate) in memo_group.candidates.iter().enumerate() {
            let Operator::ASAP(ASAPOp::SummaryEstimate { summary_input, .. }) =
                &candidate.expression.operator
            else {
                continue;
            };
            let sketch = matches!(
                summary_type_of_operator(&memo.groups()[summary_input.0 as usize].expression),
                Some(FieldDataType::Sketch(_))
            );
            let id = CandidateForMemoGroupId {
                group: MemoGroupId(group as u32),
                index_in_group,
            };
            if sketch && logical.pruned.iter().all(|pruned| pruned.candidate != id) {
                live += 1;
            }
        }
    }
    live
}

/// The frontend lowers `abs(v)`, `-v` and `v / 2` to a `Project` that
/// replaces the value and passes the series identity through
/// `promql_drop_metric_name`. Its contract judges only the value: `abs`
/// and the negation keep an absolute error, scaling by a constant a
/// relative one, so a sketch readout below each still serves the query, as
/// it does for the bare readout (DDSketch's quantile over `latency`, whose
/// values are declared non-negative).
#[test]
fn a_function_of_the_value_keeps_the_readouts_below_it() {
    let absolute = json!({"max_error": 0.05, "accuracy_metric": "absolute"});
    let relative = json!({"max_error": 0.05, "accuracy_metric": "relative"});
    for (query, accuracy) in [
        ("count_over_time(m[1m])", &absolute),
        ("abs(count_over_time(m[1m]))", &absolute),
        ("-count_over_time(m[1m])", &absolute),
        ("quantile_over_time(0.9, latency[5m]) / 2", &relative),
        ("quantile_over_time(0.9, latency[5m]) * 2", &relative),
    ] {
        let logical = pass2(query, accuracy.clone());
        assert!(live_sketch_readouts(&logical) > 0, "{query}");
    }
}

/// The groups whose expression is a tumbling-window chain built from the
/// rows, with its width. A wider level (a chain group whose every
/// candidate merges a narrower chain's windows, `SummaryMergeWindows`) is
/// read only through a `TimeRangeCover` whose narrowest level is one of
/// these, so it is left out: [`wider_levels_feed_only_covers`] checks it.
fn chain_widths(logical: &LogicalASAPMemoCandidates) -> Vec<u64> {
    logical
        .memo
        .groups()
        .iter()
        .filter(|group| !is_wider_level(group))
        .filter_map(|group| match group.expression {
            Operator::ASAP(ASAPOp::SummaryTumblingWindowChain {
                tumbling_window_width_ms,
                ..
            }) => Some(tumbling_window_width_ms),
            _ => None,
        })
        .collect()
}

/// Whether `group` is a wider chain level: a chain group whose every
/// candidate merges a narrower chain's windows.
fn is_wider_level(group: &asap_types::stages::MemoGroup) -> bool {
    !group.candidates.is_empty()
        && group.candidates.iter().all(|candidate| {
            matches!(
                candidate.expression.operator,
                Operator::ASAP(ASAPOp::SummaryMergeWindows { .. })
            )
        })
}

/// Every reader of a wider level is a wider level built from it or a
/// `TimeRangeCover` that reads it as other than its narrowest level.
fn wider_levels_feed_only_covers(logical: &LogicalASAPMemoCandidates) {
    let memo = &logical.memo;
    for (index, group) in memo.groups().iter().enumerate() {
        for candidate in &group.candidates {
            for (input, child) in candidate
                .expression
                .operator
                .children()
                .into_iter()
                .enumerate()
            {
                if !is_wider_level(&memo.groups()[child.0 as usize]) {
                    continue;
                }
                let ok = candidate
                    .expression
                    .operator
                    .reads_input_off_the_grid(input);
                assert!(ok, "G{index} reads wider level G{}: {candidate:?}", child.0);
            }
        }
    }
}

/// `quantile_over_time(0.9, rate(c[2m])[10m:30s])` read every 60 s
/// evaluates the inner `rate` every 30 s: its groups are on the 30 s grid
/// and read only chains whose width divides 30 s. Without a resolution the
/// inner step is the server's default, unknown: no grid, no chain.
#[test]
fn a_subquery_s_resolution_is_the_grid_of_the_groups_below_it() {
    let rank = json!({"max_error": 0.01, "accuracy_metric": "rank"});
    let logical = pass2(
        "quantile_over_time(0.9, rate(c[2m])[10m:30s])",
        rank.clone(),
    );
    let subquery = logical
        .memo
        .groups()
        .iter()
        .position(|group| {
            matches!(
                group.expression,
                Operator::NonASAP(NonASAPOp::PromqlSubquery { .. })
            )
        })
        .expect("a subquery group");
    let child = logical.memo.groups()[subquery].expression.children()[0].0 as usize;
    let period = |group: usize| logical.evaluation_grid_of_group[group].map(|grid| grid.period_ms);
    assert_eq!(period(subquery), Some(60_000));
    assert_eq!(period(child), Some(30_000));
    let widths = chain_widths(&logical);
    assert!(!widths.is_empty());
    assert!(widths.iter().all(|width| 30_000 % width == 0), "{widths:?}");
    wider_levels_feed_only_covers(&logical);
    let without_resolution = pass2("max_over_time(rate(c[2m])[10m:])", rank);
    assert_eq!(chain_widths(&without_resolution), Vec::<u64>::new());
}

/// $n_q$ of a PromQL result is the series bound, tightened by the declared
/// values of its `by` keys: `sum by (job) (rate(m[5m]))` returns one value
/// per `job`, the 20 `m` declares, or, undeclared, at most `m`'s 1000
/// series; `rate(m[5m])` one per series, 1000.
#[test]
fn a_promql_result_returns_at_most_its_keys_declared_values_or_its_series() {
    let values_per_evaluation = |query: &str, job_values: Option<u64>| {
        let distinct_values = job_values.map_or(json!({}), |job| json!({"job": job}));
        promql_pass2_over(
            &[(
                query,
                json!({"max_error": 0.05, "accuracy_metric": "relative"}),
            )],
            json!({"m": {
                "series_count": 1000,
                "metric_type": "gauge",
                "sample_interval": 1000,
                "distinct_values_per_label": distinct_values,
            }}),
        )
        .memo
        .queries()[0]
            .number_of_values_per_query_evaluation
    };
    let by_job = "sum by (job) (rate(m[5m]))";
    assert_eq!(values_per_evaluation(by_job, Some(20)), Some(20));
    assert_eq!(values_per_evaluation(by_job, None), Some(1000));
    assert_eq!(values_per_evaluation("rate(m[5m])", None), Some(1000));
}

/// A many-to-one match between two selectors of one metric pairs labels of
/// different series: `m{side="l"} * on(x) group_left(instance)
/// m{side="r"}` keeps each left series' `job` and copies `instance` from
/// the right series with the same `x`. `m` declares 200 series, 10 `job`
/// and 10 `instance` values, and 10 (`job`, `instance`) pairs; the left
/// series `(job_a, instance_a, x = 10a + b)` and the right series
/// `(job_b, instance_b, x = 10a + b)` give the output pairs
/// `(job_a, instance_b)`: 100, not the 10 pairs one series holds.
/// `group_right(job)` and an `offset` on one side pair them the same way.
#[test]
fn a_many_to_one_match_within_one_metric_returns_up_to_the_product_of_both_sides_labels() {
    let values_per_evaluation = |query: &str| {
        promql_pass2_over(
            &[(
                query,
                json!({"max_error": 0.05, "failure_probability": 0.01, "accuracy_metric": "relative"}),
            )],
            json!({"m": {
                "series_count": 200,
                "metric_type": "gauge",
                "sample_interval": 15_000,
                "distinct_values_per_label": {"job": 10, "instance": 10},
                "distinct_values_per_label_set": [
                    {"labels": ["job", "instance"], "distinct_values": 10},
                ],
            }}),
        )
        .memo
        .queries()[0]
            .number_of_values_per_query_evaluation
    };
    for query in [
        r#"quantile by (job, instance) (0.5, m{side="l"} * on(x) group_left(instance) m{side="r"})"#,
        r#"quantile by (job, instance) (0.5, m{side="l"} * on(x) group_right(job) m{side="r"})"#,
        r#"quantile by (job, instance) (0.5, m * on(x) group_left(instance) m offset 1h)"#,
    ] {
        assert_eq!(values_per_evaluation(query), Some(100), "{query}");
    }
    // One series' own labels still count once: 10 pairs.
    assert_eq!(
        values_per_evaluation("quantile by (job, instance) (0.5, m)"),
        Some(10)
    );
}

//! The groups plan selection prices a `by (...)` at, from each label's
//! declared distinct-value count (`SourceFacts::distinct_values_per_label`),
//! over the workload of #840: two per-series quantiles and two grouped
//! quantiles of one gauge with 10,000 series.

mod planner_layering_common;

use std::collections::BTreeSet;

use asap_types::workload::PlanningWorkload;
use planner_layering_common::*;

/// #840's workload (Q1–Q4) with `distinct_values_per_label` declared for
/// `data`.
fn workload(distinct_values_per_label: serde_json::Value) -> PlanningWorkload {
    let query = |text: &str| {
        serde_json::json!({
            "query": text, "demand": {"fixed_interval": 10000},
            "requirements": {"accuracy": {"max_error": 0.01, "failure_probability": 0.001}},
            "predictability": {"predictable": {"known_at": null}}
        })
    };
    serde_json::from_value(serde_json::json!({
        "query_workload": {"language": "promql", "query_batch": null, "repeating_queries": [
            query("quantile_over_time(0.5, data[1m])"),
            query("quantile_over_time(0.9, data[5m])"),
            query("quantile by (host, job) (0.5, data)"),
            query("quantile by (job) (0.5, data)"),
        ]},
        "data_workload": {"arrival": {"ingesting": {"rows_per_second": 666.67}},
            "sources": {"data": {"series_count": 10000, "metric_type": "gauge",
                "sample_interval": 15000,
                "distinct_values_per_label": distinct_values_per_label}}}
    }))
    .expect("workload")
}

/// The group counts every priced plan gives the builds and aggregates read
/// only by `query`, from their cost details (`... into N states`, `... into
/// N groups`).
fn priced_groups(run: &Run, query: usize) -> BTreeSet<u64> {
    let mut groups = BTreeSet::new();
    for physical in &run.physical {
        let Some(cost) = run.selection.costs.get(&physical.id) else {
            continue;
        };
        for node in &physical.dag().nodes {
            if readers(physical.dag(), &physical.query_roots, node.id) != BTreeSet::from([query]) {
                continue;
            }
            let detail = &cost.per_node[&node.id].detail;
            let words: Vec<&str> = detail.split([' ', ':']).collect();
            for window in words.windows(3) {
                if window[0] == "into" && matches!(window[2], "states" | "groups") {
                    groups.insert(window[1].parse().expect("a group count"));
                }
            }
        }
    }
    groups
}

const Q3_BY_HOST_JOB: usize = 2;
const Q4_BY_JOB: usize = 3;

/// With `host` and `job` declared, `by (host, job)` is priced at 1,000 × 10
/// groups, at most the 10,000 series, and `by (job)` at 10.
#[test]
fn a_grouping_is_priced_at_the_product_of_its_labels_counts() {
    let run = run_promql(&workload(serde_json::json!({"host": 1000, "job": 10})));
    assert_eq!(
        priced_groups(&run, Q3_BY_HOST_JOB),
        BTreeSet::from([10_000])
    );
    assert_eq!(priced_groups(&run, Q4_BY_JOB), BTreeSet::from([10]));
}

/// A product above `series_count` is capped at it: each group holds at
/// least one series.
#[test]
fn a_grouping_has_at_most_one_group_per_series() {
    let run = run_promql(&workload(serde_json::json!({"host": 5000, "job": 10})));
    assert_eq!(
        priced_groups(&run, Q3_BY_HOST_JOB),
        BTreeSet::from([10_000])
    );
}

/// A grouping with an undeclared label gets no invented count: each of the
/// 10,000 series is in one group, and each of the 10,000 samples the
/// instant selector reads in at most one, so `by (host, job)` with only
/// `host` declared, and both groupings with nothing declared, are priced at
/// 10,000 groups.
#[test]
fn a_grouping_with_an_undeclared_label_is_bound_by_the_series_read() {
    let run = run_promql(&workload(serde_json::json!({"host": 1000})));
    assert_eq!(
        priced_groups(&run, Q3_BY_HOST_JOB),
        BTreeSet::from([10_000])
    );
    assert_eq!(priced_groups(&run, Q4_BY_JOB), BTreeSet::from([10_000]));
    let run = run_promql(&workload(serde_json::json!({})));
    assert_eq!(
        priced_groups(&run, Q3_BY_HOST_JOB),
        BTreeSet::from([10_000])
    );
}

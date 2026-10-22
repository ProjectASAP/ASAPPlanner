//! A whole-result accuracy target over SQL `GROUP BY` groups (#863): the
//! distinct values the data workload declares for the grouping columns
//! (`SourceFacts::distinct_values_per_label`) bound the groups one
//! evaluation returns, so a per-group sketch can be sized to meet the target
//! over all of them. Without declarations the groups stay unbound and only
//! the exact plan meets it.

mod planner_layering_common;

use asap_devtools::planning_stages::lower_sql_roots;
use asap_devtools::SqlCatalog;
use asap_types::ir::schema::{DataType, Field, Schema, SketchAlgorithm};
use asap_types::workload::PlanningWorkload;
use planner_layering_common::*;

/// The `http` table of the RollupHeavy workload.
fn http_catalog() -> SqlCatalog {
    let utf8 = |name| Field::plain(name, DataType::Utf8, false);
    let float = |name| Field::plain(name, DataType::Float64, false);
    SqlCatalog::new().with_table(
        "http",
        Schema::with_time_index(
            vec![
                Field::plain("ts", DataType::Timestamp, false),
                utf8("region"),
                utf8("service"),
                utf8("endpoint"),
                utf8("status"),
                float("user_id"),
                float("latency"),
            ],
            0,
            vec![],
        ),
    )
}

/// `query` every minute with a whole-result target (`metric` error 0.05,
/// failure probability 0.05) over `http`: 10,000 label combinations, and
/// each label's distinct values when `declared`.
fn workload(query: &str, metric: &str, declared: bool) -> PlanningWorkload {
    let mut http = serde_json::json!({
        "series_count": 10000, "sample_interval": 5, "values_non_negative": true,
        "max_distinct_per_evaluation": 1000000
    });
    if declared {
        http["distinct_values_per_label"] =
            serde_json::json!({"region": 4, "service": 25, "endpoint": 25, "status": 4});
    }
    serde_json::from_value(serde_json::json!({
        "query_workload": {"language": {"sql": "datafusion_sql"}, "query_batch": null,
            "repeating_queries": [{
                "query": query, "demand": {"fixed_interval": 60000},
                "requirements": {"accuracy": {"accuracy_metric": metric, "max_error": 0.05,
                    "failure_probability": 0.05, "accuracy_scope": "whole_result"}},
                "predictability": {"predictable": {"known_at": null}}}]},
        "data_workload": {"arrival": {"ingesting": {"rows_per_second": 2e6}},
            "sources": {"http": http}}
    }))
    .expect("workload")
}

/// The sketches the selected plan builds (empty for the exact plan).
fn selected_sketches(query: &str, metric: &str, declared: bool) -> Vec<SketchAlgorithm> {
    let workload = workload(query, metric, declared);
    let roots = lower_sql_roots(&workload, &http_catalog()).expect("lowers");
    let run = run_stages(&workload, roots);
    sketch_builds(run.physical(&run.selection.selected).dag())
        .into_iter()
        .map(|(_, algorithm, _)| algorithm)
        .collect()
}

/// Template 14: distinct users per (service, endpoint).
const DISTINCT_USERS: &str = "SELECT service, endpoint, COUNT(DISTINCT user_id) AS value \
     FROM http WHERE ts >= now() - INTERVAL '5 minutes' GROUP BY service, endpoint";

/// Template 13: the p99 latency per (region, service).
const P99_LATENCY: &str = "SELECT region, service, approx_percentile_cont(latency, 0.99) AS value \
     FROM http WHERE ts >= now() - INTERVAL '5 minutes' GROUP BY region, service";

/// With 25 services and 25 endpoints declared, at most 625 groups: a
/// distinct-count sketch per group, each failing with probability at most
/// 0.05 / 625, meets the whole-result target.
#[test]
fn declared_grouping_columns_bound_the_groups_of_a_distinct_count() {
    let sketches = selected_sketches(DISTINCT_USERS, "relative", true);
    assert!(
        matches!(
            sketches.as_slice(),
            [SketchAlgorithm::Hll | SketchAlgorithm::Kmv]
        ),
        "{sketches:?}"
    );
}

/// With 4 regions and 25 services declared, at most 100 groups: a KLL per
/// group meets the whole-result rank target.
#[test]
fn declared_grouping_columns_bound_the_groups_of_a_quantile() {
    let sketches = selected_sketches(P99_LATENCY, "rank", true);
    assert_eq!(sketches, [SketchAlgorithm::Kll]);
}

/// Without declared distinct values the groups are unbound: no sketch
/// meets a whole-result target, and the exact plan is selected.
#[test]
fn undeclared_grouping_columns_leave_only_the_exact_plan() {
    for (query, metric) in [(DISTINCT_USERS, "relative"), (P99_LATENCY, "rank")] {
        let sketches = selected_sketches(query, metric, false);
        assert!(sketches.is_empty(), "{query}: {sketches:?}");
    }
}

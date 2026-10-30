//! A whole-result accuracy target over SQL `GROUP BY` groups (#863): the
//! distinct values the data workload declares for the grouping columns
//! (`SourceFacts::distinct_values_per_label`) bound the groups one
//! evaluation returns, so a per-group sketch can be sized to meet the target
//! over all of them. Without declarations the groups stay unbound and only
//! the exact plan meets it.

use crate::planner_layering_common;

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
/// `declared` as each column's distinct values.
fn workload(query: &str, metric: &str, declared: serde_json::Value) -> PlanningWorkload {
    let http = serde_json::json!({
        "series_count": 10000, "sample_interval": 5, "values_non_negative": true,
        "max_distinct_per_evaluation": 1000000, "distinct_values_per_label": declared,
        "rows_held": 1000000000
    });
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

/// Every `http` column's distinct values: 4 regions, 25 services, 25
/// endpoints, 4 statuses.
fn every_column() -> serde_json::Value {
    serde_json::json!({"region": 4, "service": 25, "endpoint": 25, "status": 4})
}

/// Evaluate the exact computation and each supported local summary choice.
/// These tests exercise group/error models, not a timed search's visited archive.
fn run_group_choices(workload: &PlanningWorkload) -> Run {
    use asap_logical_asap_optimization::pass1::per_computation_candidates::{
        compose_logical_candidate, generate_per_computation_candidates,
    };
    let roots = lower_sql_roots(workload, &http_catalog()).expect("lowers");
    let local = generate_per_computation_candidates(
        &asap_logical_asap_optimization::pass2::memo::DagOfLogicalMemoGroups::from_roots(
            &roots.iter().cloned().enumerate().collect::<Vec<_>>(),
        ),
    )
    .unwrap();
    assert_eq!(local.targets.len(), 1, "one grouped statistic per fixture");
    let candidates = (0..local.targets[0].ways_to_compute.len())
        .map(|choice| compose_logical_candidate(&local, &[choice]).unwrap())
        .collect();
    run_explicit_candidates(
        workload,
        roots,
        candidates,
        asap_plan_selection::PlanningModels::builtin(),
    )
}

/// The sketches the selected plan builds (empty for the exact plan).
fn selected_sketches(
    query: &str,
    metric: &str,
    declared: serde_json::Value,
) -> Vec<SketchAlgorithm> {
    let workload = workload(query, metric, declared);
    let run = run_group_choices(&workload);
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
    let sketches = selected_sketches(DISTINCT_USERS, "relative", every_column());
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
    let sketches = selected_sketches(P99_LATENCY, "rank", every_column());
    assert_eq!(sketches, [SketchAlgorithm::Kll]);
}

/// Without declared distinct values the groups are unbound: no sketch
/// meets a whole-result target, and the exact plan is selected.
#[test]
fn undeclared_grouping_columns_leave_only_the_exact_plan() {
    for (query, metric) in [(DISTINCT_USERS, "relative"), (P99_LATENCY, "rank")] {
        let sketches = selected_sketches(query, metric, serde_json::json!({}));
        assert!(sketches.is_empty(), "{query}: {sketches:?}");
    }
}

/// One grouping column declared is not enough: `service` has no declared
/// count, so the groups of `GROUP BY region, service` stay unbound.
#[test]
fn a_partly_declared_grouping_leaves_only_the_exact_plan() {
    let sketches = selected_sketches(P99_LATENCY, "rank", serde_json::json!({"region": 4}));
    assert!(sketches.is_empty(), "{sketches:?}");
}

/// A computed key named like a declared column no longer holds that
/// column's values: `upper(service) AS service` gives no bound.
#[test]
fn a_computed_grouping_column_leaves_only_the_exact_plan() {
    let query = "SELECT service, COUNT(DISTINCT user_id) AS value FROM \
         (SELECT concat(service, endpoint) AS service, user_id, ts FROM http) \
         WHERE ts >= now() - INTERVAL '5 minutes' GROUP BY service";
    let sketches = selected_sketches(query, "relative", every_column());
    assert!(sketches.is_empty(), "{sketches:?}");
}

/// The groups every priced plan's grouped builds and aggregates are priced
/// at (`... into N states`, `... into N groups`), and the rows its scans read.
fn priced_groups_and_scan_rows(
    workload: PlanningWorkload,
) -> (
    std::collections::BTreeSet<u64>,
    std::collections::BTreeSet<u64>,
) {
    let run = run_group_choices(&workload);
    let (mut groups, mut scanned) = (
        std::collections::BTreeSet::new(),
        std::collections::BTreeSet::new(),
    );
    for cost in run.selection.costs.values() {
        for node in cost.per_node.values() {
            let words: Vec<&str> = node.detail.split([' ', ':', ';']).collect();
            for window in words.windows(3) {
                if window[0] == "into" && matches!(window[2], "states" | "groups") {
                    groups.insert(window[1].parse().expect("a group count"));
                }
                if window[0] == "scan" && window[2] == "samples" {
                    scanned.insert(window[1].parse().expect("a row count"));
                }
            }
        }
    }
    (groups, scanned)
}

/// A SQL grouping with an undeclared column gets no invented count: each
/// of the table's 10,000 declared groups of rows is in one group, so its
/// builds are priced at 10,000; without `series_count`, each row read is
/// in at most one group, so they are priced at the rows the scan reads.
/// Declared, `GROUP BY service, endpoint` is 25 x 25.
#[test]
fn an_undeclared_sql_grouping_is_priced_at_the_groups_of_rows_or_the_rows_read() {
    let undeclared = || workload(DISTINCT_USERS, "relative", serde_json::json!({}));
    let (groups, _) = priced_groups_and_scan_rows(undeclared());
    assert_eq!(groups, std::collections::BTreeSet::from([10_000]));
    let mut without_series = undeclared();
    let http = without_series
        .data_workload
        .as_mut()
        .unwrap()
        .sources
        .get_mut("http")
        .unwrap();
    http.series_count = None;
    http.sample_interval = None;
    let (groups, scanned) = priced_groups_and_scan_rows(without_series);
    assert_eq!(groups, scanned);
    let (groups, _) =
        priced_groups_and_scan_rows(workload(DISTINCT_USERS, "relative", every_column()));
    assert_eq!(groups, std::collections::BTreeSet::from([625]));
}

/// Each endpoint is in one service: declaring `(service, endpoint)`'s 625
/// combinations prices `GROUP BY service, endpoint` at 625 groups, where
/// the per-column product (25 x 625) is capped only by the 10,000 groups of
/// rows.
#[test]
fn a_declared_column_set_prices_a_sql_grouping_at_its_combinations() {
    let hierarchical =
        serde_json::json!({"region": 4, "service": 25, "endpoint": 625, "status": 4});
    let (groups, _) =
        priced_groups_and_scan_rows(workload(DISTINCT_USERS, "relative", hierarchical.clone()));
    assert_eq!(groups, std::collections::BTreeSet::from([10_000]));
    let mut declared = workload(DISTINCT_USERS, "relative", hierarchical);
    declared
        .data_workload
        .as_mut()
        .unwrap()
        .sources
        .get_mut("http")
        .unwrap()
        .distinct_values_per_label_set = vec![asap_types::workload::LabelSetDistinctValues {
        labels: vec!["service".to_string(), "endpoint".to_string()],
        distinct_values: 625,
    }];
    let (groups, _) = priced_groups_and_scan_rows(declared);
    assert_eq!(groups, std::collections::BTreeSet::from([625]));
}

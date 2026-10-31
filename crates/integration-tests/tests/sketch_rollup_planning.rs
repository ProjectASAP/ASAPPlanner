//! Sketch rollup in planning: the memo's rollup rule offers answering a
//! coarse sketch grouping from the finest build of the same sketch over the
//! same rows, and plan selection selects it when one build and a merge cost
//! less than a build per query over the raw rows. Selection prices fixed
//! plans made of the memo's candidates, so no assertion depends on a timed
//! search.
use asap_executor::physical_planner::promql_rows;
use asap_frontend_sql::{lower_sql, SqlCatalog};
use asap_integration_tests::fixtures::lower_promql;
use asap_integration_tests::planning::{builds_and_rollups, expanded_memo, select_rollup};
use asap_types::ir::properties::{AccuracyMetric, AccuracyScope, AccuracyTarget};
use asap_types::ir::schema::{DataType, Field, Schema};
use asap_types::ir::QueryRoot;
use asap_types::workload::{
    DataWorkload, Predictability, QueryRecurrence, QueryRequirements, QueryWorkloadEntry,
    RepeatedDemand, RepetitionInterval,
};

fn catalog() -> SqlCatalog {
    SqlCatalog::new().with_table(
        "http",
        Schema::new(vec![
            Field::plain("ts", DataType::Timestamp, false),
            Field::plain("region", DataType::Utf8, false),
            Field::plain("service", DataType::Utf8, false),
            Field::plain("user_id", DataType::Float64, false),
            Field::plain("latency", DataType::Float64, false),
        ]),
    )
}

/// A predictable query repeating every `interval_ms` at `accuracy`.
fn entry(accuracy: &AccuracyTarget, interval_ms: u32) -> QueryWorkloadEntry {
    QueryWorkloadEntry {
        query: Default::default(),
        requirements: QueryRequirements {
            accuracy: accuracy.clone(),
            max_latency_ms: None,
        },
        predictability: Predictability::Predictable { known_at: None },
        recurrence: QueryRecurrence::Repeated(RepeatedDemand::FixedInterval(RepetitionInterval(
            interval_ms,
        ))),
        range_evaluation: None,
    }
}

/// `http` arrives at 2e6 rows/s (W-RollupHeavy's table), 10,000 series,
/// a million distinct users at most per evaluation, and holds a minute of
/// rows (the catalog has no time index, so a query's time predicate is a
/// filter over every row held).
fn data() -> DataWorkload {
    serde_json::from_value(serde_json::json!({
        "arrival": {"ingesting": {"rows_per_second": 2_000_000.0}},
        "sources": {"http": {
            "series_count": 10_000,
            "sample_interval": 5,
            "values_non_negative": true,
            "max_distinct_per_evaluation": 1_000_000,
            "rows_held": 120_000_000,
        }},
    }))
    .unwrap()
}

/// The SQL `queries` lowered at `accuracy`.
async fn sql_roots(queries: &[&str], accuracy: &AccuracyTarget) -> Vec<QueryRoot> {
    let mut roots = Vec::new();
    for query in queries {
        let root = lower_sql(query, &catalog(), accuracy.clone())
            .await
            .unwrap();
        roots.push(QueryRoot::Operator(root));
    }
    roots
}

/// Distinct users by (region, service) and by service, both at relative
/// ε = 0.05 per value over the same window (W-RollupHeavy's targets): the
/// memo offers the coarse KMV rolled up from the finer one, and plan
/// selection selects one build and a rollup, cheaper than a build per
/// query from the raw rows.
#[tokio::test]
async fn stage3_selects_a_distinct_count_rollup_when_it_is_cheaper() {
    let accuracy = AccuracyTarget::max_error(0.05)
        .failure_probability(0.05)
        .accuracy_metric(AccuracyMetric::Relative)
        .accuracy_scope(AccuracyScope::PerValue);
    let window = "WHERE ts >= now() - INTERVAL '5 minutes'";
    let roots = sql_roots(
        &[
            &format!(
                "SELECT region, service, COUNT(DISTINCT user_id) AS value FROM http {window} \
                 GROUP BY region, service"
            ),
            &format!(
                "SELECT service, COUNT(DISTINCT user_id) AS value FROM http {window} \
                 GROUP BY service"
            ),
        ],
        &accuracy,
    )
    .await;
    let entries = vec![entry(&accuracy, 60_000); 2];
    let memo = expanded_memo(roots, &entries);
    let (selected, separate, rolled_up) = select_rollup(&memo, 2, &entries, &data());
    assert!(rolled_up < separate, "{rolled_up} vs {separate}");
    assert_eq!(builds_and_rollups(&selected), (1, 1));
}

/// p99 latency by (region, service), by region and by service at rank
/// ε = 0.05 (template 13's shape): both coarse groupings roll up from one
/// KLL, and plan selection selects that plan.
#[tokio::test]
async fn coarse_quantile_groupings_roll_up_from_one_kll() {
    let accuracy = AccuracyTarget::max_error(0.05)
        .failure_probability(0.05)
        .accuracy_metric(AccuracyMetric::Rank)
        .accuracy_scope(AccuracyScope::PerValue);
    let window = "WHERE ts >= now() - INTERVAL '5 minutes'";
    let query = |keys: &str| {
        format!(
            "SELECT {keys}, approx_percentile_cont(latency, 0.99) AS value FROM http {window} \
             GROUP BY {keys}"
        )
    };
    let roots = sql_roots(
        &[
            &query("region, service"),
            &query("region"),
            &query("service"),
        ],
        &accuracy,
    )
    .await;
    let entries = vec![entry(&accuracy, 60_000); 3];
    let memo = expanded_memo(roots, &entries);
    let (selected, separate, rolled_up) = select_rollup(&memo, 3, &entries, &data());
    assert!(rolled_up < separate, "{rolled_up} vs {separate}");
    assert_eq!(builds_and_rollups(&selected), (1, 2));
}

/// #840's `quantile by (host, job)` and `quantile by (job)` over 10,000
/// series, every 10 s at rank ε = 0.01, δ = 0.001: the memo offers the
/// coarse KLL rolled up from the finer one, and its guarantee meets the
/// target, so plan selection selects it.
#[test]
fn promql_quantile_by_job_rolls_up_from_by_host_job() {
    let accuracy = AccuracyTarget::max_error(0.01)
        .failure_probability(0.001)
        .accuracy_metric(AccuracyMetric::Rank);
    let roots: Vec<QueryRoot> = [
        "quantile by (host, job) (0.5, data)",
        "quantile by (job) (0.5, data)",
    ]
    .iter()
    .map(|query| {
        let root = lower_promql(query, accuracy.clone()).unwrap();
        QueryRoot::Operator(promql_rows::with_series_identity(&root).unwrap())
    })
    .collect();
    let data: DataWorkload = serde_json::from_value(serde_json::json!({
        "arrival": {"ingesting": {"rows_per_second": 666.67}},
        "sources": {"data": {"series_count": 10_000, "metric_type": "gauge",
                             "sample_interval": 15_000,
                             // 15 minutes of raw rows.
                             "rows_held": 600_000}},
    }))
    .unwrap();
    let entry = entry(&accuracy, 10_000);
    let entries = [entry.clone(), entry];
    let memo = expanded_memo(roots, &entries);
    let (selected, separate, rolled_up) = select_rollup(&memo, 2, &entries, &data);
    assert!(rolled_up < separate, "{rolled_up} vs {separate}");
    assert_eq!(builds_and_rollups(&selected), (1, 1));
}

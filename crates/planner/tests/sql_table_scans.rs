//! A runtime plans a SQL workload over a table with `plan_installable` and
//! runs it with the Planner's executor, binding the table to a data source:
//! the plan compiles with the table's scans (the compile check sees them),
//! `now()` is the evaluation time, `COUNT(DISTINCT)` over an `Int64` id
//! column answers with the type the Planner gives it, from a distinct-count
//! sketch or exactly.
#![cfg(feature = "sql")]

use std::collections::BTreeMap;
use std::sync::Arc;

use asap_executor::physical_planner::bind_with_data_sources;
use asap_executor::runtime::{Limits, RunContext, Scope};
use asap_executor::sources::{DataSources, MemorySource};
use asap_executor::values::{Batch, Value};
use asap_planner::{
    plan_installable, FrontendInput, InstallablePlan, PlanningModels, PlanningWorkload, SqlCatalog,
    UserInput,
};
use asap_types::ir::operator::Source;
use asap_types::ir::physical_export::PhysicalASAPOperatorPayload;
use asap_types::ir::schema::{DataType, Field, FieldDataType, Schema};
use asap_types::ir::NonASAPOp;
use futures::{executor::block_on, StreamExt};

const MINUTE_MS: i64 = 60_000;
/// The evaluation time of every run.
const EVALUATION_MS: i64 = 1_800_000_000_000;

/// `http(ts, service, user_id: Int64, latency)`.
fn http_schema() -> Schema {
    Schema::with_time_index(
        vec![
            Field::plain("ts", DataType::Timestamp, false),
            Field::plain("service", DataType::Utf8, false),
            Field::plain("user_id", DataType::Int64, false),
            Field::plain("latency", DataType::Float64, false),
        ],
        0,
        vec![],
    )
}

/// Repeating SQL `queries`, each every minute with `accuracy`, over `http`
/// ingesting 100 rows per second.
fn workload(queries: &[&str], accuracy: &str) -> PlanningWorkload {
    let entries: Vec<String> = queries
        .iter()
        .map(|query| {
            format!(
                r#"{{"query": {query:?}, "demand": {{"fixed_interval": 60000}},
                "requirements": {{"accuracy": {accuracy}}},
                "predictability": {{"predictable": {{"known_at": null}}}}}}"#
            )
        })
        .collect();
    serde_json::from_str(&format!(
        r#"{{"query_workload": {{"language": {{"sql": "datafusion_sql"}},
              "repeating_queries": [{}]}},
            "data_workload": {{"arrival": {{"ingesting": {{"rows_per_second": 100.0}}}}}}}}"#,
        entries.join(",")
    ))
    .unwrap()
}

/// Relative error 0.05 per value with failure probability 0.05: a
/// distinct-count sketch qualifies.
const APPROXIMATE: &str = r#"{"accuracy_metric": "relative", "max_error": 0.05,
    "failure_probability": 0.05, "accuracy_scope": "per_value"}"#;

/// No approximation.
const EXACT: &str = r#"{"max_error": 0.0, "failure_probability": 0.0}"#;

async fn plan(query: &str, accuracy: &str) -> InstallablePlan {
    plan_all(&[query], accuracy).await
}

async fn plan_all(queries: &[&str], accuracy: &str) -> InstallablePlan {
    let catalog = SqlCatalog::new().with_table("http", http_schema());
    let workload = workload(queries, accuracy);
    plan_installable(UserInput::new(
        &workload,
        FrontendInput::Sql { catalog: &catalog },
        PlanningModels::builtin(),
    ))
    .await
    .expect("plan_installable plans a SQL workload over a table")
}

/// Rows over the 20 minutes before the evaluation: one per second, service
/// `s{i % 3}`, user `i % 50` before the last 5 minutes and `1000 + i % 7`
/// within them (from 5 minutes before the evaluation on).
fn rows() -> Vec<Vec<Value>> {
    (0..20 * 60)
        .map(|i: i64| {
            let ts = EVALUATION_MS - 20 * MINUTE_MS + (i + 1) * 1000;
            let recent = ts >= EVALUATION_MS - 5 * MINUTE_MS;
            vec![
                Value::Timestamp(ts),
                Value::Utf8(format!("s{}", i % 3).into()),
                Value::Int64(if recent { 1000 + i % 7 } else { i % 50 }),
                Value::Float64(i as f64),
            ]
        })
        .collect()
}

/// Runs query 0 of `plan`, every node at query time (its reference plan,
/// which reads the table's rows wherever the plan keeps state), at
/// [`EVALUATION_MS`] with `http` bound to [`rows`]: its output field types
/// and its rows. A `WHERE ts >= now() - INTERVAL` window is a `TimeRange`
/// the executor runs as a filter on `ts` against the evaluation time.
fn run(plan: &InstallablePlan) -> (Vec<FieldDataType>, Vec<Vec<Value>>) {
    run_query(plan, 0)
}

/// [`run`] for query `query` of `plan`.
fn run_query(plan: &InstallablePlan, query: usize) -> (Vec<FieldDataType>, Vec<Vec<Value>>) {
    let reference = plan
        .dag
        .dag
        .with_every_node_at_query_time()
        .expect("the plan runs at query time");
    // The table's rows in the schema the plan's scan reads (its columns
    // qualified by the table).
    let schema = reference
        .nodes
        .iter()
        .find_map(|node| match &node.payload {
            PhysicalASAPOperatorPayload::NonASAP(NonASAPOp::Scan { schema, .. }) => {
                Some(Arc::new(schema.clone()))
            }
            _ => None,
        })
        .expect("the plan scans the table");
    let batch = Batch::try_new(schema.clone(), rows()).unwrap();
    let mut sources = DataSources::default();
    sources
        .register(
            Source::Table {
                table_ref: "http".into(),
            },
            Arc::new(MemorySource::new(schema, vec![batch]).unwrap()),
        )
        .unwrap();
    let roots = [reference.roots[query] as u64];
    let bound = bind_with_data_sources(&reference, BTreeMap::new(), &roots, &sources)
        .expect("the plan binds to the table");
    let context = RunContext::new(
        Scope::Query {
            evaluation_time_ms: EVALUATION_MS,
            revision: 1,
        },
        Limits::default(),
    )
    .unwrap();
    block_on(async {
        let mut output = bound.execute(&roots, context).unwrap().remove(0);
        let mut fields = None;
        let mut rows = Vec::new();
        while let Some(batch) = output.next().await {
            let batch = batch.expect("the query runs");
            fields.get_or_insert_with(|| {
                batch
                    .schema()
                    .fields
                    .iter()
                    .map(|f| f.dtype.clone())
                    .collect()
            });
            rows.extend(batch.to_rows());
        }
        (fields.unwrap_or_default(), rows)
    })
}

/// The answer rows by their first column, each value as it prints (its
/// type and value).
fn by_key(rows: Vec<Vec<Value>>) -> BTreeMap<String, String> {
    rows.into_iter()
        .map(|row| match &row[0] {
            Value::Utf8(key) => (key.to_string(), shown(&row[1])),
            other => panic!("key {other:?}"),
        })
        .collect()
}

/// A value as its type and number.
fn shown(value: &Value) -> String {
    match value {
        Value::Int64(n) => format!("Int64({n})"),
        Value::Float64(x) => format!("Float64({x})"),
        other => format!("{other:?}"),
    }
}

/// The planned output type of query 0's root.
fn planned_output(plan: &InstallablePlan) -> Vec<FieldDataType> {
    let root = plan.queries[0].root_node_id;
    let node = plan.dag.dag.nodes.iter().find(|n| n.id == root).unwrap();
    node.output_schema
        .fields
        .iter()
        .map(|f| f.dtype.clone())
        .collect()
}

const DISTINCT_USERS_LAST_5_MINUTES: &str = "SELECT service, COUNT(DISTINCT user_id) AS value \
     FROM http WHERE ts >= now() - INTERVAL '5 minutes' GROUP BY service";

/// The distinct count of each service's users in the last 5 minutes: 7
/// ids per service (`1000 + i % 7` over every third `i`), none of the
/// older 50.
fn expected_distinct_users() -> BTreeMap<String, String> {
    (0..3)
        .map(|s| (format!("s{s}"), shown(&Value::Int64(7))))
        .collect()
}

#[tokio::test]
async fn a_distinct_count_over_an_int64_id_in_a_now_window_runs_from_a_sketch() {
    let plan = plan(DISTINCT_USERS_LAST_5_MINUTES, APPROXIMATE).await;
    let selected = format!("{:?}", plan.dag.dag.nodes);
    assert!(
        selected.contains("SummaryAgg"),
        "a distinct-count sketch answers it: {selected}"
    );
    let (fields, rows) = run(&plan);
    assert_eq!(
        fields,
        planned_output(&plan),
        "the executor's output type is the plan's"
    );
    // The sketch is exact below its capacity.
    assert_eq!(by_key(rows), expected_distinct_users());
}

#[tokio::test]
async fn an_exact_distinct_count_over_an_int64_id_in_a_now_window_runs() {
    let plan = plan(DISTINCT_USERS_LAST_5_MINUTES, EXACT).await;
    let (fields, rows) = run(&plan);
    assert_eq!(
        fields,
        planned_output(&plan),
        "the executor's output type is the plan's"
    );
    assert_eq!(by_key(rows), expected_distinct_users());
}

#[tokio::test]
async fn now_is_the_evaluation_time() {
    let plan = plan(
        "SELECT service, COUNT(*) AS value FROM http \
         WHERE ts > now() - INTERVAL '1 minute' GROUP BY service",
        EXACT,
    )
    .await;
    let (_, rows) = run(&plan);
    // The 60 rows after the minute before the evaluation: 20 per service.
    let expected: BTreeMap<String, String> = (0..3)
        .map(|s| (format!("s{s}"), shown(&Value::Int64(20))))
        .collect();
    assert_eq!(by_key(rows), expected);
}

/// SQL `percentile_cont` of `values`: linear interpolation at rank
/// `q * (n - 1)` over the sorted values.
fn percentile_cont(q: f64, mut values: Vec<f64>) -> f64 {
    values.sort_by(f64::total_cmp);
    let rank = q * (values.len() - 1) as f64;
    let (low, high) = (rank.floor() as usize, rank.ceil() as usize);
    values[low] + (values[high] - values[low]) * (rank - low as f64)
}

#[tokio::test]
async fn an_exact_grouped_sql_percentile_runs() {
    let plan = plan(
        "SELECT service, approx_percentile_cont(latency, 0.99) AS value FROM http \
         WHERE ts >= now() - INTERVAL '5 minutes' GROUP BY service",
        EXACT,
    )
    .await;
    let (fields, rows) = run(&plan);
    assert_eq!(
        fields,
        planned_output(&plan),
        "the executor's output type is the plan's"
    );
    // Each service's latencies `i` over the last 5 minutes. The window is a
    // range selector's `(T - 5m, T]`, so the row stamped exactly `T - 5m`
    // (i = 899) is not read: i >= 900.
    let expected: BTreeMap<String, String> = (0..3i64)
        .map(|s| {
            let latencies = (900..1200)
                .filter(|i| i % 3 == s)
                .map(|i| i as f64)
                .collect();
            (
                format!("s{s}"),
                shown(&Value::Float64(percentile_cont(0.99, latencies))),
            )
        })
        .collect();
    assert_eq!(by_key(rows), expected);
}

/// A global percentile over an empty window is NULL (SQL), and the plan
/// types it nullable.
#[tokio::test]
async fn an_exact_global_sql_percentile_of_no_row_is_null() {
    let plan = plan(
        "SELECT approx_percentile_cont(latency, 0.5) AS value FROM http \
         WHERE ts > now()",
        EXACT,
    )
    .await;
    let (fields, rows) = run(&plan);
    assert_eq!(
        fields,
        planned_output(&plan),
        "the executor's output type is the plan's"
    );
    assert_eq!(rows.len(), 1);
    assert!(matches!(rows[0][0], Value::Null), "{:?}", rows[0][0]);
}

/// A 5 and a 15 minute distinct count over one table: the plan builds them
/// in tumbling panes, each pane a `TimeRange` over a shifted table `Scan`,
/// which the executor runs as filters on `ts`. Both answer exactly (the
/// sketch is exact below its capacity).
#[tokio::test]
async fn distinct_counts_over_5_and_15_minute_windows_run_from_panes() {
    let fifteen = DISTINCT_USERS_LAST_5_MINUTES.replace("5 minutes", "15 minutes");
    let plan = plan_all(&[DISTINCT_USERS_LAST_5_MINUTES, &fifteen], APPROXIMATE).await;
    let selected = format!("{:?}", plan.dag.dag.nodes);
    assert!(selected.contains("TimeShift"), "panes: {selected}");
    assert_eq!(by_key(run_query(&plan, 0).1), expected_distinct_users());
    // The last 15 minutes (i >= 300): every user `i % 50` of each service
    // (3 and 50 are coprime) and its 7 recent ones.
    let expected: BTreeMap<String, String> = (0..3)
        .map(|s| (format!("s{s}"), shown(&Value::Int64(57))))
        .collect();
    assert_eq!(by_key(run_query(&plan, 1).1), expected);
}

/// Exact row counts over 5 and 15 minutes: a count tells a row read twice
/// or missed at a pane's edge, which a distinct count cannot. Each pane is
/// `(T - s - w, T - s]`, so the 15-minute panes read every row after
/// `T - 15m` exactly once: 300 per service (the row at `T - 15m` is not
/// read), and 100 in the last 5 minutes.
#[tokio::test]
async fn row_counts_over_5_and_15_minute_windows_read_each_row_once() {
    let five = "SELECT service, COUNT(*) AS value FROM http \
                WHERE ts >= now() - INTERVAL '5 minutes' GROUP BY service";
    let fifteen = five.replace("5 minutes", "15 minutes");
    let plan = plan_all(&[five, &fifteen], EXACT).await;
    let selected = format!("{:?}", plan.dag.dag.nodes);
    assert!(selected.contains("TimeShift"), "panes: {selected}");
    for (query, rows_per_service) in [(0, 100), (1, 300)] {
        let expected: BTreeMap<String, String> = (0..3)
            .map(|s| (format!("s{s}"), shown(&Value::Int64(rows_per_service))))
            .collect();
        assert_eq!(by_key(run_query(&plan, query).1), expected, "query {query}");
    }
}

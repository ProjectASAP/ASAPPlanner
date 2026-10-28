//! A runtime plans a SQL workload over a table with `plan_installable` and
//! runs it with the Planner's executor, binding the table to a data source:
//! the plan compiles with the table's scans (the compile check sees them),
//! `now()` is the evaluation time, `COUNT(DISTINCT)` over an `Int64` id
//! column answers with the type the Planner gives it, from a distinct-count
//! sketch or exactly. A plan that keeps panes at ingestion time runs as a
//! runtime runs it: each pane built in an ingestion run over its window from
//! the table bound at the plan's `Scan`, the queries answered from the panes.
#![cfg(feature = "sql")]

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use asap_executor::physical_planner::compile;
use asap_executor::runtime::{Limits, RunContext, Scope};
use asap_executor::sources::{BindingDataSources, DataSources, MemorySource, TableProvider};
use asap_executor::values::{Batch, Value};
use asap_planner::{
    plan_installable, DeploymentCapabilities, FrontendInput, InstallablePlan,
    MaterializedViewRefresh, PlanningModels, PlanningWorkload, SqlCatalog, UserInput,
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

/// Repeating SQL `queries`, each every `interval_ms` with `accuracy`, over
/// `http` ingesting 100 rows per second, whose 3 services and 100,000 rows
/// held are declared.
fn workload(queries: &[&str], accuracy: &str, interval_ms: u64) -> PlanningWorkload {
    let entries: Vec<String> = queries
        .iter()
        .map(|query| {
            format!(
                r#"{{"query": {query:?}, "demand": {{"fixed_interval": {interval_ms}}},
                "requirements": {{"accuracy": {accuracy}}},
                "predictability": {{"predictable": {{"known_at": null}}}}}}"#
            )
        })
        .collect();
    serde_json::from_str(&format!(
        r#"{{"query_workload": {{"language": {{"sql": "datafusion_sql"}},
              "repeating_queries": [{}]}},
            "data_workload": {{"arrival": {{"ingesting": {{"rows_per_second": 100.0}}}},
              "sources": {{"http": {{"distinct_values_per_label": {{"service": 3}},
                  "rows_held": 100000}}}}}}}}"#,
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
    plan_for(
        queries,
        accuracy,
        MINUTE_MS as u64,
        &DeploymentCapabilities::default(),
    )
    .await
}

/// [`plan_all`] for a deployment that keeps no raw data: every window is
/// read by state maintained at ingestion time.
async fn plan_without_raw_data(queries: &[&str], accuracy: &str) -> InstallablePlan {
    plan_without_raw_data_every(queries, accuracy, MINUTE_MS as u64).await
}

/// [`plan_without_raw_data`] with each query every `interval_ms`.
async fn plan_without_raw_data_every(
    queries: &[&str],
    accuracy: &str,
    interval_ms: u64,
) -> InstallablePlan {
    let deployment = DeploymentCapabilities {
        raw_data_retained: false,
        ..DeploymentCapabilities::default()
    };
    plan_for(queries, accuracy, interval_ms, &deployment).await
}

async fn plan_for(
    queries: &[&str],
    accuracy: &str,
    interval_ms: u64,
    deployment: &DeploymentCapabilities,
) -> InstallablePlan {
    let catalog = SqlCatalog::new().with_table("http", http_schema());
    let workload = workload(queries, accuracy, interval_ms);
    plan_installable(UserInput::new(
        &workload,
        FrontendInput::Sql { catalog: &catalog },
        PlanningModels::builtin().with_deployment_capabilities(deployment),
    ))
    .await
    .expect("plan_installable plans a SQL workload over a table")
    .0
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
    let data_sources = BindingDataSources {
        tables: sources,
        ..Default::default()
    };
    let bound = compile(&reference, &BTreeSet::new(), &roots)
        .and_then(|compiled| compiled.instantiate(&data_sources))
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
    let root = plan.dag.roots[0];
    let node = plan.dag.nodes.iter().find(|n| n.id == root).unwrap();
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
    let selected = format!("{:?}", plan.dag.nodes);
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
    let selected = format!("{:?}", plan.dag.nodes);
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
    let selected = format!("{:?}", plan.dag.nodes);
    assert!(selected.contains("TimeShift"), "panes: {selected}");
    for (query, rows_per_service) in [(0, 100), (1, 300)] {
        let expected: BTreeMap<String, String> = (0..3)
            .map(|s| (format!("s{s}"), shown(&Value::Int64(rows_per_service))))
            .collect();
        assert_eq!(by_key(run_query(&plan, query).1), expected, "query {query}");
    }
}

/// Runs `ids` of `dag` in `scope` with each of `inputs` bound to the node
/// its key names and `table` bound as `http`: the batches of each id's
/// output.
fn run_bound(
    dag: &asap_types::ir::physical_export::PhysicalASAPDAG,
    inputs: BTreeMap<u64, Batch>,
    table: &Batch,
    ids: &[u64],
    scope: Scope,
) -> Vec<Vec<Batch>> {
    // Which nodes are bound inputs; each one's schema is its DAG node's.
    let boundaries: BTreeSet<u64> = inputs.keys().copied().collect();
    // The executor's plan for `ids`, cut at the bound inputs.
    let compiled = compile(dag, &boundaries, ids).expect("the executor compiles the plan");
    // Each bound input as an in-memory source of its rows.
    let sources = inputs
        .into_iter()
        .map(|(id, batch)| {
            let source = MemorySource::new(batch.schema().clone(), vec![batch]).unwrap();
            (id, Arc::new(source) as Arc<dyn TableProvider>)
        })
        .collect();
    let mut tables = DataSources::default();
    tables
        .register(
            Source::Table {
                table_ref: "http".into(),
            },
            Arc::new(MemorySource::new(table.schema().clone(), vec![table.clone()]).unwrap()),
        )
        .unwrap();
    let data_sources = BindingDataSources {
        tables,
        materialized_views: sources,
    };
    let instance = compiled.instantiate(&data_sources).unwrap();
    block_on(async {
        let context = RunContext::new(scope, Limits::default()).unwrap();
        let outputs = instance.execute(compiled.roots(), context).unwrap();
        futures::future::join_all(outputs.into_iter().map(|mut output| async move {
            let mut batches = Vec::new();
            while let Some(batch) = output.next().await {
                batches.push((*batch.unwrap()).clone());
            }
            batches
        }))
        .await
    })
}

/// The data plane of `plan` at [`EVALUATION_MS`] over the table rows
/// [`rows`], as a runtime runs it: every materialized view refreshed once,
/// in the plan's order, from the table bound at its `Scan`s and the views
/// it reads, in an ingestion run over its data's span (or the query run,
/// for one refreshed there); then every query answered from the views it
/// reaches. The answer rows of each query.
fn run_data_plane(plan: &InstallablePlan) -> Vec<Vec<Vec<Value>>> {
    let dag = &plan.dag;
    let views = plan.materialized_views().unwrap();
    // The table's rows in the schema the plan's scans read.
    let schema = dag
        .nodes
        .iter()
        .find_map(|node| match &node.payload {
            PhysicalASAPOperatorPayload::NonASAP(NonASAPOp::Scan { schema, .. }) => {
                Some(Arc::new(schema.clone()))
            }
            _ => None,
        })
        .expect("the plan scans the table");
    let table = Batch::try_new(schema, rows()).unwrap();
    // Each view's state, by node id, with its data's span.
    let mut built: BTreeMap<u64, (Batch, (i64, i64))> = BTreeMap::new();
    for view in &views {
        let span = view
            .inputs
            .raw_data_windows
            .iter()
            .map(|window| window.bounds_ms(EVALUATION_MS))
            .chain(
                view.inputs
                    .materialized_views
                    .iter()
                    .map(|id| built[&(*id as u64)].1),
            )
            .reduce(|(a, b), (c, d)| (a.min(c), b.max(d)))
            .expect("a materialized view reads something");
        let mut sources = BTreeMap::new();
        for id in &view.inputs.materialized_views {
            sources.insert(*id as u64, built[&(*id as u64)].0.clone());
        }
        let scope = match view.refresh {
            MaterializedViewRefresh::AtIngestion => Scope::Ingestion {
                window_start_ms: span.0,
                window_end_ms: span.1,
                revision: 1,
            },
            MaterializedViewRefresh::OnFirstRead => Scope::Query {
                evaluation_time_ms: EVALUATION_MS,
                revision: 1,
            },
        };
        let batches = run_bound(dag, sources, &table, &[view.node_id as u64], scope).remove(0);
        let state_rows = batches.iter().flat_map(|b| b.to_rows()).collect();
        let state = Batch::try_new(batches[0].schema().clone(), state_rows).unwrap();
        built.insert(view.node_id as u64, (state, span));
    }
    // The query run reads the views it reaches without passing another
    // one, and the windows it reads at query time.
    let roots: Vec<u64> = plan.dag.roots.iter().map(|&root| root as u64).collect();
    let mut inputs = BTreeMap::new();
    let mut pending = roots.clone();
    let mut seen = std::collections::BTreeSet::new();
    while let Some(id) = pending.pop() {
        if !seen.insert(id) {
            continue;
        }
        match built.get(&id) {
            Some((state, _)) => {
                inputs.insert(id, state.clone());
            }
            None => pending.extend(
                dag.edges
                    .iter()
                    .filter(|edge| edge.consumer as u64 == id)
                    .map(|edge| edge.producer as u64),
            ),
        }
    }
    let query = Scope::Query {
        evaluation_time_ms: EVALUATION_MS,
        revision: 1,
    };
    run_bound(dag, inputs, &table, &roots, query)
        .iter()
        .map(|batches| batches.iter().flat_map(|b| b.to_rows()).collect())
        .collect()
}

/// A 5 and a 15 minute distinct count kept in tumbling panes at ingestion
/// time (the deployment keeps no raw data): each pane is built in an ingestion run from the table rows of its
/// window, and both queries answer exactly from the panes (the sketch is
/// exact below its capacity), as the query-time reference does.
#[tokio::test]
async fn ingestion_time_panes_built_from_table_rows_answer_exactly() {
    let fifteen = DISTINCT_USERS_LAST_5_MINUTES.replace("5 minutes", "15 minutes");
    let plan = plan_without_raw_data(&[DISTINCT_USERS_LAST_5_MINUTES, &fifteen], APPROXIMATE).await;
    let views = plan.materialized_views().unwrap();
    assert!(
        views
            .iter()
            .any(|v| v.refresh == MaterializedViewRefresh::AtIngestion
                && !v.inputs.raw_data_windows.is_empty()),
        "panes built at ingestion time from table rows: {:?}",
        views
    );
    // Older panes read windows that end before the evaluation.
    assert!(views
        .iter()
        .flat_map(|v| &v.inputs.raw_data_windows)
        .any(|w| w.end_offset_ms > 0));
    let answers = run_data_plane(&plan);
    assert_eq!(by_key(answers[0].clone()), expected_distinct_users());
    let expected: BTreeMap<String, String> = (0..3)
        .map(|s| (format!("s{s}"), shown(&Value::Int64(57))))
        .collect();
    assert_eq!(by_key(answers[1].clone()), expected);
}

/// Exact row counts over 5 and 15 minutes from ingestion-time state: each
/// window's table rows are read once, 100 and 300 per service.
#[tokio::test]
async fn ingestion_time_row_counts_read_each_table_row_once() {
    let five = "SELECT service, COUNT(*) AS value FROM http \
                WHERE ts >= now() - INTERVAL '5 minutes' GROUP BY service";
    let fifteen = five.replace("5 minutes", "15 minutes");
    let plan = plan_without_raw_data(&[five, &fifteen], EXACT).await;
    assert!(
        plan.raw_data_retention().unwrap().is_empty(),
        "no raw data at query time"
    );
    let answers = run_data_plane(&plan);
    for (query, rows_per_service) in [(0, 100), (1, 300)] {
        let expected: BTreeMap<String, String> = (0..3)
            .map(|s| (format!("s{s}"), shown(&Value::Int64(rows_per_service))))
            .collect();
        assert_eq!(by_key(answers[query].clone()), expected, "query {query}");
    }
}

/// A 5-minute distinct count every 5 minutes (window equal to interval,
/// the W-RollupHeavy query that had no plan) and an exact 5-minute row
/// count every 5 minutes, for a deployment that keeps no raw data: each is
/// one tumbling window built at ingestion time from the table rows, read
/// no raw data at query time, and answers exactly.
#[tokio::test]
async fn a_window_equal_to_its_interval_is_built_at_ingestion_time_from_table_rows() {
    let five_minutes_ms = 5 * MINUTE_MS as u64;
    let plan = plan_without_raw_data_every(
        &[DISTINCT_USERS_LAST_5_MINUTES],
        APPROXIMATE,
        five_minutes_ms,
    )
    .await;
    let views = plan.materialized_views().unwrap();
    assert!(
        views
            .iter()
            .any(|v| v.refresh == MaterializedViewRefresh::AtIngestion
                && !v.inputs.raw_data_windows.is_empty()),
        "built at ingestion time from table rows: {:?}",
        views
    );
    assert!(
        plan.raw_data_retention().unwrap().is_empty(),
        "no raw data at query time"
    );
    assert_eq!(
        by_key(run_data_plane(&plan).remove(0)),
        expected_distinct_users()
    );

    let row_count = "SELECT service, COUNT(*) AS value FROM http \
                     WHERE ts >= now() - INTERVAL '5 minutes' GROUP BY service";
    let plan = plan_without_raw_data_every(&[row_count], EXACT, five_minutes_ms).await;
    assert!(
        plan.raw_data_retention().unwrap().is_empty(),
        "no raw data at query time"
    );
    let expected: BTreeMap<String, String> = (0..3)
        .map(|s| (format!("s{s}"), shown(&Value::Int64(100))))
        .collect();
    assert_eq!(by_key(run_data_plane(&plan).remove(0)), expected);
}

/// Each run's requested `(start, end]` time range, in order.
type RequestedTimeRanges = std::sync::Mutex<Vec<Option<(i64, i64)>>>;

/// The `http` table as a provider that records each run's requested time
/// range and returns every row (it ignores the request).
struct RecordingTable {
    rows: MemorySource,
    requested_time_ranges_ms: Arc<RequestedTimeRanges>,
}
impl TableProvider for RecordingTable {
    fn schema(&self) -> asap_executor::values::SchemaRef {
        self.rows.schema()
    }
    fn boundedness(&self) -> asap_executor::plan::Boundedness {
        self.rows.boundedness()
    }
    fn scan(
        &self,
        request: &asap_executor::sources::ScanRequest,
        context: RunContext,
    ) -> Result<asap_executor::runtime::OutputStream<'_, Batch>, asap_executor::Error> {
        self.requested_time_ranges_ms
            .lock()
            .unwrap()
            .push(request.time_range_ms);
        self.rows.scan(request, context)
    }
}

// The `http` provider of a `ts >= now() - INTERVAL '5 minutes'` query is
// asked for `(T - 5m, T]` at evaluation time `T`; it returns all 20 minutes
// of rows, and the answer is the one over the window.
#[tokio::test]
async fn a_table_provider_is_asked_for_the_query_window() {
    // The user's query, planned exactly over the `http` table.
    let plan = plan(
        "SELECT service, COUNT(*) AS value FROM http \
         WHERE ts >= now() - INTERVAL '5 minutes' GROUP BY service",
        EXACT,
    )
    .await;
    // The plan with every node at query time: it reads the table at the Scan.
    let reference = plan.dag.with_every_node_at_query_time().unwrap();
    // The schema the plan's Scan reads.
    let schema = reference
        .nodes
        .iter()
        .find_map(|node| match &node.payload {
            PhysicalASAPOperatorPayload::NonASAP(NonASAPOp::Scan { schema, .. }) => {
                Some(Arc::new(schema.clone()))
            }
            _ => None,
        })
        .unwrap();
    // Each run's requested time range, as the provider saw it.
    let requested_time_ranges_ms = Arc::new(RequestedTimeRanges::default());
    // The deployment's catalog: `http` → its provider.
    let mut data_sources = BindingDataSources::default();
    data_sources
        .tables
        .register(
            Source::Table {
                table_ref: "http".into(),
            },
            Arc::new(RecordingTable {
                rows: MemorySource::new(
                    schema.clone(),
                    vec![Batch::try_new(schema, rows()).unwrap()],
                )
                .unwrap(),
                requested_time_ranges_ms: requested_time_ranges_ms.clone(),
            }),
        )
        .unwrap();
    // Compiled once (no reader opened), then instantiated for this run.
    let roots = [reference.roots[0] as u64];
    let compiled = compile(&reference, &BTreeSet::new(), &roots).unwrap();
    let bound = compiled.instantiate(&data_sources).unwrap();
    let context = RunContext::new(
        Scope::Query {
            evaluation_time_ms: EVALUATION_MS,
            revision: 1,
        },
        Limits::default(),
    )
    .unwrap();
    // The answer rows: each service and its count.
    let rows = block_on(async {
        let mut output = bound.execute(&roots, context).unwrap().remove(0);
        let mut rows = Vec::new();
        while let Some(batch) = output.next().await {
            rows.extend(batch.unwrap().to_rows());
        }
        rows
    });
    assert_eq!(
        *requested_time_ranges_ms.lock().unwrap(),
        vec![Some((EVALUATION_MS - 5 * MINUTE_MS, EVALUATION_MS))]
    );
    // The 300 rows of the window, 100 per service.
    let expected: BTreeMap<String, String> = (0..3)
        .map(|s| (format!("s{s}"), shown(&Value::Int64(100))))
        .collect();
    assert_eq!(by_key(rows), expected);
}

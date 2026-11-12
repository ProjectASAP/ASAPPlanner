//! A runtime plans a SQL workload over a table with `plan_installable` and
//! runs it with the Planner's executor, binding the table in a data store:
//! the plan compiles with the table's scans (`check_compiles` sees them),
//! `now()` is the evaluation time, `COUNT(DISTINCT)` over an `Int64` id
//! column answers with the type the Planner gives it, from a distinct-count
//! sketch or exactly. A plan that keeps tumbling windows at ingestion time runs as a
//! runtime runs it: each tumbling window built in an ingestion run over its window from
//! the table bound at the plan's `Scan`, the queries answered from the tumbling windows.

use std::collections::BTreeMap;
use std::sync::Arc;

use asap_executor::physical_planner::compile;
use asap_executor::runtime::{Limits, RunContext, Scope};
use asap_executor::sources::InMemoryDataStore;
use asap_executor::values::{Batch, Value};
use asap_planner::{
    plan_installable, DeploymentCapabilities, FrontendInput, InstallablePlan,
    MaterializedViewRefresh, PlanningModels, PlanningWorkload, SqlCatalog, UserInput,
};

use asap_types::ir::operator::Source;
use asap_types::ir::schema::{DataType, Field, Schema};
use asap_types::ir::{ASAPOp, NonASAPOp};
use asap_types::plan::physical_asap_dag::PhysicalASAPOperatorPayload;
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
                r#"{{"query": {query:?}, "demand": {{"fixed_interval_at": {{"interval": {interval_ms}, "evaluation_phase": 0}}}},
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

/// Runs `ids` of `dag` in `scope` with each of `inputs` (a state and the
/// span of data it summarizes) stored for the node its key names and
/// `table` bound as `http`: the batches of each id's output.
/// One stored state of a view and the `(start, end]` of the data it holds.
type WindowState = (Batch, (i64, i64));

fn run_bound(
    dag: &asap_types::plan::physical_asap_dag::PhysicalASAPDAG,
    inputs: BTreeMap<u64, Vec<WindowState>>,
    table: &Batch,
    ids: &[u64],
    scope: Scope,
) -> Vec<Vec<Batch>> {
    // The plan's views whose states are bound; each one's schema is its DAG
    // node's.
    let views: Vec<_> = dag
        .materialized_views()
        .unwrap()
        .into_iter()
        .filter(|view| inputs.contains_key(&(view.node_id as u64)))
        .collect();
    assert_eq!(views.len(), inputs.len());
    // The executor's plan for `ids`, reading the bound views.
    let compiled = compile(dag, &views, ids).expect("the executor compiles the plan");
    // The table and each bound view's states in an in-memory store.
    let mut store = InMemoryDataStore::default()
        .with_raw_data(
            Source::Table {
                table_ref: "http".into(),
            },
            table.schema().clone(),
            vec![table.clone()],
        )
        .unwrap();
    for view in &views {
        for (batch, span) in &inputs[&(view.node_id as u64)] {
            store
                .append_materialized_view(view, *span, vec![batch.clone()])
                .unwrap();
        }
    }
    let instance = compiled.instantiate(Arc::new(store)).unwrap();
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
    // Each view's states, by node id, each with its data's span: a chain's
    // one per tumbling window its retention keeps, newest last.
    let mut built: BTreeMap<u64, Vec<WindowState>> = BTreeMap::new();
    for view in &views {
        let width_ms = match &dag
            .nodes
            .iter()
            .find(|n| n.id == view.node_id)
            .unwrap()
            .payload
        {
            PhysicalASAPOperatorPayload::ASAP(ASAPOp::SummaryTumblingWindowChain {
                tumbling_window_width_ms,
                ..
            }) => Some(*tumbling_window_width_ms as i64),
            _ => None,
        };
        let spans: Vec<(i64, i64)> = match width_ms {
            Some(width_ms) => {
                let windows = (view.retention_ms() as i64 / width_ms).max(1);
                (0..windows)
                    .rev()
                    .map(|k| {
                        let end = EVALUATION_MS - k * width_ms;
                        (end - width_ms, end)
                    })
                    .collect()
            }
            None => vec![dag
                .refresh_window(view.node_id)
                .map(|refresh| {
                    asap_planner::TimeWindow {
                        end_offset_ms: refresh.phase_ms,
                        width_ms: refresh.width_ms,
                    }
                    .bounds_ms(EVALUATION_MS)
                })
                .into_iter()
                .chain(
                    view.inputs
                        .materialized_views
                        .iter()
                        .flat_map(|id| built[&(*id as u64)].iter().map(|(_, span)| *span)),
                )
                .reduce(|(a, b), (c, d)| (a.min(c), b.max(d)))
                .expect("a materialized view reads something")],
        };
        let mut states = Vec::new();
        for span in spans {
            let mut sources = BTreeMap::new();
            for id in &view.inputs.materialized_views {
                sources.insert(*id as u64, built[&(*id as u64)].clone());
            }
            let scope = match view.refresh() {
                MaterializedViewRefresh::AtIngestion => Scope::Ingestion {
                    window_start_ms: span.0,
                    window_end_ms: span.1,
                },
                MaterializedViewRefresh::OnFirstRead => Scope::Query {
                    evaluation_time_ms: EVALUATION_MS,
                },
            };
            let batches = run_bound(dag, sources, &table, &[view.node_id as u64], scope).remove(0);
            let state_rows = batches.iter().flat_map(|b| b.to_rows()).collect();
            states.push((
                Batch::try_new(batches[0].schema().clone(), state_rows).unwrap(),
                span,
            ));
        }
        built.insert(view.node_id as u64, states);
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
            Some(built) => {
                inputs.insert(id, built.clone());
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
    };
    run_bound(dag, inputs, &table, &roots, query)
        .iter()
        .map(|batches| batches.iter().flat_map(|b| b.to_rows()).collect())
        .collect()
}

/// A 5 and a 15 minute distinct count materialized in tumbling windows at ingestion
/// time (the deployment keeps no raw data): each tumbling window is built in an ingestion run from the table rows of its
/// window, and both queries answer exactly from the tumbling windows (the sketch is
/// exact below its capacity), as the query-time reference does.
#[tokio::test]
async fn ingestion_time_tumbling_windows_built_from_table_rows_answer_exactly() {
    let fifteen = DISTINCT_USERS_LAST_5_MINUTES.replace("5 minutes", "15 minutes");
    let plan = plan_without_raw_data(&[DISTINCT_USERS_LAST_5_MINUTES, &fifteen], APPROXIMATE).await;
    let views = plan.materialized_views().unwrap();
    assert!(
        views
            .iter()
            .any(|v| v.refresh() == MaterializedViewRefresh::AtIngestion
                && v.inputs.materialized_views.is_empty()),
        "tumbling windows built at ingestion time from table rows: {:?}",
        views
    );
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
            .any(|v| v.refresh() == MaterializedViewRefresh::AtIngestion
                && v.inputs.materialized_views.is_empty()),
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

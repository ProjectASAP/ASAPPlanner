//! SQL's per-group top-k idiom, `ROW_NUMBER() OVER (PARTITION BY city
//! ORDER BY COUNT(*) DESC) … WHERE rn <= k`, planned with
//! `plan_installable` and run with the Planner's executor: the exact plan
//! compiles and answers each city's top hosts with their counts, the same
//! rows PromQL `topk by (city)` answers over the same data, and the plan
//! Stage 3 selects under an approximate target installs (each materialized
//! view it keeps refreshes).
#![cfg(all(feature = "sql", feature = "promql"))]

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use asap_executor::physical_planner::compile;
use asap_executor::runtime::{Limits, RunContext, Scope};
use asap_executor::sources::{BindingDataSources, DataSources, MemorySource};
use asap_executor::values::{Batch, Value};
use asap_planner::{
    plan_installable, test_support::query_time_reference, FrontendInput, InstallablePlan,
    PlanningModels, PlanningWorkload, SqlCatalog, UserInput,
};
use asap_types::ir::operator::Source as DataSource;
use asap_types::ir::physical_export::PhysicalASAPOperatorPayload;
use asap_types::ir::schema::{DataType, Field, Schema};
use asap_types::ir::NonASAPOp;
use futures::{executor::block_on, StreamExt};

mod time_series;
use time_series::{time_series_tables, RawDataSample};

/// The evaluation time of every run.
const EVALUATION_MS: i64 = 1_800_000_000_000;

/// The issue's per-city top 5 hosts by row count over the last 5 minutes.
const TOP_5_HOSTS_PER_CITY: &str = "SELECT city, host, c FROM (SELECT city, host, COUNT(*) AS c, \
     ROW_NUMBER() OVER (PARTITION BY city ORDER BY COUNT(*) DESC) AS rn FROM metrics \
     WHERE ts > now() - INTERVAL '5' MINUTE GROUP BY city, host) WHERE rn <= 5";

/// The issue's global top 5 hosts by row count over the last 5 minutes.
const TOP_5_HOSTS: &str = "SELECT host, COUNT(*) AS c FROM metrics \
     WHERE ts > now() - INTERVAL '5' MINUTE GROUP BY host ORDER BY c DESC LIMIT 5";

/// The same per-city top 5 in PromQL, each table row a sample of the
/// series `metrics{city, host}`.
const PROMQL_TOP_5_HOSTS_PER_CITY: &str =
    "topk by (city) (5, sum by (city, host) (count_over_time(metrics[5m])))";

/// `metrics(ts, city, host, value)`, as the issue declares it (all but
/// `ts` nullable).
fn metrics_schema() -> Schema {
    Schema::with_time_index(
        vec![
            Field::plain("ts", DataType::Timestamp, false),
            Field::plain("city", DataType::Utf8, true),
            Field::plain("host", DataType::Utf8, true),
            Field::plain("value", DataType::Float64, true),
        ],
        0,
        vec![],
    )
}

/// Repeating `queries` in `language` (its workload JSON), every 10 s at
/// `accuracy`, over `metrics` ingesting 666.67 rows per second from 100
/// series: the issue's workload.
fn workload(language: &str, queries: &[&str], accuracy: &str) -> PlanningWorkload {
    let entries: Vec<String> = queries
        .iter()
        .map(|query| {
            format!(
                r#"{{"query": {query:?}, "demand": {{"fixed_interval": 10000}},
                  "requirements": {{"accuracy": {accuracy}}},
                  "predictability": {{"predictable": {{"known_at": null}}}}}}"#
            )
        })
        .collect();
    serde_json::from_str(&format!(
        r#"{{"query_workload": {{"language": {language}, "repeating_queries": [{}]}},
            "data_workload": {{"arrival": {{"ingesting": {{"rows_per_second": 666.67}}}},
              "sources": {{"metrics": {{"series_count": 100}}}}}}}}"#,
        entries.join(",")
    ))
    .unwrap()
}

const EXACT: &str = r#"{"max_error": 0.0, "failure_probability": 0.0}"#;
const APPROXIMATE: &str = r#"{"max_error": 0.01, "failure_probability": 0.001}"#;

async fn plan_sql(queries: &[&str], accuracy: &str) -> InstallablePlan {
    let catalog = SqlCatalog::new().with_table("metrics", metrics_schema());
    plan_installable(UserInput::new(
        &workload(r#"{"sql": "datafusion_sql"}"#, queries, accuracy),
        FrontendInput::Sql { catalog: &catalog },
        PlanningModels::builtin(),
    ))
    .await
    .expect("plan_installable plans the SQL workload")
    .0
}

async fn plan_promql(query: &str) -> InstallablePlan {
    plan_installable(UserInput::new(
        &workload(r#""promql""#, &[query], EXACT),
        FrontendInput::Promql { histograms: None },
        PlanningModels::builtin(),
    ))
    .await
    .expect("plan_installable plans the PromQL workload")
    .0
}

/// Rows per `(city, host)` in the last 5 minutes. `nyc` has seven hosts
/// with distinct counts at the 5th place (`h5` 5, `h6` and `h7` 3), so the
/// top 5 does not depend on how ties are broken; `sf` has two hosts, fewer
/// than 5. `h7` also has 100 rows from before the window, which no answer
/// counts.
const RECENT_ROWS: [(&str, &str, i64); 9] = [
    ("nyc", "h1", 9),
    ("nyc", "h2", 8),
    ("nyc", "h3", 7),
    ("nyc", "h4", 6),
    ("nyc", "h5", 5),
    ("nyc", "h6", 3),
    ("nyc", "h7", 3),
    ("sf", "s1", 4),
    ("sf", "s2", 2),
];

/// The table rows: per `(city, host)`, its [`RECENT_ROWS`] count one second
/// apart ending a second before the evaluation, and `h7`'s 100 older rows
/// 10 to 11.65 minutes before it.
fn rows() -> Vec<(i64, &'static str, &'static str)> {
    let mut rows = Vec::new();
    for (city, host, count) in RECENT_ROWS {
        for i in 0..count {
            rows.push((EVALUATION_MS - 1000 * (i + 1), city, host));
        }
    }
    for i in 0..100 {
        rows.push((EVALUATION_MS - 600_000 - 1000 * i, "nyc", "h7"));
    }
    rows
}

/// The exact answer: each city's top 5 hosts by recent row count.
fn expected_top_5_per_city() -> BTreeSet<(String, String, i64)> {
    let mut per_city: BTreeMap<&str, Vec<(&str, i64)>> = BTreeMap::new();
    for (city, host, count) in RECENT_ROWS {
        per_city.entry(city).or_default().push((host, count));
    }
    per_city
        .into_iter()
        .flat_map(|(city, mut hosts)| {
            hosts.sort_by_key(|&(_, count)| std::cmp::Reverse(count));
            hosts
                .into_iter()
                .take(5)
                .map(move |(host, count)| (city.to_string(), host.to_string(), count))
        })
        .collect()
}

/// Each answer row's `city` (when it has one), `host` and the ranked count
/// (the column named `ranked`), read by name.
fn ranked_rows(batches: &[Batch], ranked: &str) -> BTreeSet<(String, String, i64)> {
    batches
        .iter()
        .flat_map(|batch| {
            let names: Vec<&str> = batch
                .schema()
                .fields
                .iter()
                .map(|f| f.name.as_str())
                .collect();
            let column = |name: &str| names.iter().position(|n| *n == name);
            let (city, host, count) = (
                column("city"),
                column("host").unwrap_or_else(|| panic!("a host column in {names:?}")),
                column(ranked).unwrap_or_else(|| panic!("a {ranked} column in {names:?}")),
            );
            batch
                .to_rows()
                .into_iter()
                .map(move |row| {
                    let text = |index: usize| match &row[index] {
                        Value::Utf8(text) => text.to_string(),
                        other => panic!("a label: {other:?}"),
                    };
                    let count = match row[count] {
                        Value::Int64(n) => n,
                        Value::Float64(x) if x.fract() == 0.0 => x as i64,
                        ref other => panic!("a count: {other:?}"),
                    };
                    (city.map(text).unwrap_or_default(), text(host), count)
                })
                .collect::<Vec<_>>()
        })
        .collect()
}

/// Query `query` of `plan` with every node at query time, at
/// [`EVALUATION_MS`], with `metrics` bound to [`rows`]: its batches.
fn run_sql(plan: &InstallablePlan, query: usize) -> Vec<Batch> {
    let reference = plan
        .dag
        .with_every_node_at_query_time()
        .expect("the plan runs at query time");
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
    let rows = rows()
        .into_iter()
        .map(|(ts, city, host)| {
            vec![
                Value::Timestamp(ts),
                Value::Utf8(city.into()),
                Value::Utf8(host.into()),
                Value::Float64(1.0),
            ]
        })
        .collect();
    let batch = Batch::try_new(schema.clone(), rows).unwrap();
    let mut sources = DataSources::default();
    sources
        .register(
            DataSource::Table {
                table_ref: "metrics".into(),
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
    let batches = collect(bound.execute(&roots, query_scope()).unwrap().remove(0));
    batches
}

/// The PromQL plan's query-time reference at [`EVALUATION_MS`], each table
/// row of [`rows`] a sample of `metrics{city, host}`: its batches.
fn run_promql(plan: &InstallablePlan) -> Vec<Batch> {
    let samples: Vec<RawDataSample> = rows()
        .into_iter()
        .map(|(timestamp_ms, city, host)| RawDataSample {
            labels: BTreeMap::from([
                ("__name__".to_string(), "metrics".to_string()),
                ("city".to_string(), city.to_string()),
                ("host".to_string(), host.to_string()),
            ]),
            timestamp_ms,
            value: 1.0,
        })
        .collect();
    let reference = query_time_reference(plan).unwrap();
    let dag = &reference.dag;
    let roots = [reference.dag.roots[0] as u64];
    let compiled = compile(dag, &BTreeSet::new(), &roots).expect("the executor compiles the plan");
    // The samples bound at the plan's `Scan` of `metrics`.
    let data_sources = BindingDataSources {
        tables: time_series_tables(dag, &samples),
        ..Default::default()
    };
    let instance = compiled.instantiate(&data_sources).unwrap();
    let batches = collect(
        instance
            .execute(compiled.roots(), query_scope())
            .unwrap()
            .remove(0),
    );
    batches
}

fn query_scope() -> RunContext {
    RunContext::new(
        Scope::Query {
            evaluation_time_ms: EVALUATION_MS,
            revision: 1,
        },
        Limits::default(),
    )
    .unwrap()
}

/// Every batch of one query's `output`.
fn collect<B, E>(mut output: impl futures::Stream<Item = Result<B, E>> + Unpin) -> Vec<Batch>
where
    B: std::ops::Deref<Target = Batch>,
    E: std::fmt::Debug,
{
    block_on(async {
        let mut batches = Vec::new();
        while let Some(batch) = output.next().await {
            batches.push((*batch.expect("the query runs")).clone());
        }
        batches
    })
}

/// The exact per-city top 5 compiles and answers each city's top hosts
/// with their counts, `sf` with its two.
#[tokio::test]
async fn an_exact_sql_per_city_top_5_runs() {
    let plan = plan_sql(&[TOP_5_HOSTS_PER_CITY], EXACT).await;
    plan.check_compiles().unwrap();
    let answered = ranked_rows(&run_sql(&plan, 0), "topk_5");
    assert_eq!(answered, expected_top_5_per_city());
}

/// SQL's `ROW_NUMBER` per-city top 5 and PromQL's `topk by (city)` answer
/// the same rows over the same data.
#[tokio::test]
async fn sql_and_promql_per_city_top_5_answer_alike() {
    let sql = ranked_rows(
        &run_sql(&plan_sql(&[TOP_5_HOSTS_PER_CITY], EXACT).await, 0),
        "topk_5",
    );
    let promql = ranked_rows(
        &run_promql(&plan_promql(PROMQL_TOP_5_HOSTS_PER_CITY).await),
        "topk_5",
    );
    assert_eq!(sql, promql);
    assert_eq!(sql, expected_top_5_per_city());
}

/// The global top 5 answers its hosts, not only their counts.
#[tokio::test]
async fn an_exact_sql_top_5_answers_its_hosts() {
    let plan = plan_sql(&[TOP_5_HOSTS], EXACT).await;
    let answered: BTreeSet<(String, i64)> = ranked_rows(&run_sql(&plan, 0), "topk_5")
        .into_iter()
        .map(|(_, host, count)| (host, count))
        .collect();
    let expected = [("h1", 9), ("h2", 8), ("h3", 7), ("h4", 6), ("h5", 5)]
        .map(|(host, count)| (host.to_string(), count));
    assert_eq!(answered, BTreeSet::from(expected));
}

/// The issue's two queries under ε 0.01, δ 0.001: the plan Stage 3 selects
/// installs, so the executor refreshes every materialized view it keeps; its
/// compile check rejects a plan whose view it cannot refresh as it
/// rejects one whose query run it cannot compile.
#[tokio::test]
async fn the_selected_approximate_plan_installs_and_compiles() {
    let plan = plan_sql(&[TOP_5_HOSTS, TOP_5_HOSTS_PER_CITY], APPROXIMATE).await;
    plan.check_compiles().unwrap();
}

/// With `rn` selected, the per-city top 5 keeps its window function, which
/// the executor does not run, so no plan of the workload compiles. Stage 3
/// rejects each, wherever its summaries are built: a plan that builds its
/// count sketch at ingestion time, which the executor cannot build either,
/// is rejected like the same plan building it at query time, not selected.
#[tokio::test]
async fn a_plan_whose_materialized_view_does_not_refresh_is_rejected() {
    let with_rn =
        TOP_5_HOSTS_PER_CITY.replace("SELECT city, host, c FROM", "SELECT city, host, c, rn FROM");
    let catalog = SqlCatalog::new().with_table("metrics", metrics_schema());
    let planned = plan_installable(UserInput::new(
        &workload(
            r#"{"sql": "datafusion_sql"}"#,
            &[TOP_5_HOSTS, &with_rn],
            APPROXIMATE,
        ),
        FrontendInput::Sql { catalog: &catalog },
        PlanningModels::builtin(),
    ))
    .await;
    match planned {
        Err(asap_planner::PlanError::Optimize(error)) => {
            // Joint search keeps typed validation failures in its no-incumbent
            // report, including when the search budget ends before exhaustion.
            let diagnostic = error.to_string();
            assert!(
                diagnostic.contains("executor_cannot_compile"),
                "{diagnostic}"
            );
            assert!(
                diagnostic.contains("value operation has no native implementation"),
                "{diagnostic}"
            );
        }
        other => panic!("no plan is selected: {:?}", other.map(|_| ())),
    }
}

/// A per-city top 5 the heavy-hitter `TopK` does not cover runs exactly as
/// the per-partition `Limit{Sort}`, with the query's own columns: ordered by
/// the count then the host (a tie-breaking second key), and with a SELECT
/// list that drops the partition column.
#[tokio::test]
async fn generic_per_city_top_5_forms_run_exactly() {
    let tie_broken = TOP_5_HOSTS_PER_CITY.replace("COUNT(*) DESC)", "COUNT(*) DESC, host)");
    let plan = plan_sql(&[&tie_broken], EXACT).await;
    assert_eq!(
        ranked_rows(&run_sql(&plan, 0), "c"),
        expected_top_5_per_city()
    );
    let hosts_only =
        TOP_5_HOSTS_PER_CITY.replace("SELECT city, host, c FROM", "SELECT host, c FROM");
    let plan = plan_sql(&[&hosts_only], EXACT).await;
    let expected: BTreeSet<_> = expected_top_5_per_city()
        .into_iter()
        .map(|(_, host, count)| (String::new(), host, count))
        .collect();
    assert_eq!(ranked_rows(&run_sql(&plan, 0), "c"), expected);
}

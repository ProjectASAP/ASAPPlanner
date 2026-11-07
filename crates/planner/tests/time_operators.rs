//! Time operators planned exactly with `plan_installable` and run with the
//! Planner's executor over raw data: PromQL subqueries, `@` anchors (with
//! and without `offset`, `start()`/`end()` from the query range) and a
//! SQL `asap_rate`, whose range reads projected rows rather than a `Scan`.
//! Each answer is checked against the value the PromQL definition gives
//! for the same data: a range `(t - r, t]` is left-open, a subquery
//! `[R:s]` evaluates at the multiples of `s` in `(t - R, t]`, and `@`
//! pins the evaluation time (`start()` and `end()` the query range's
//! bounds).

use std::collections::BTreeMap;
use std::sync::Arc;

use asap_executor::physical_planner::compile;
use asap_executor::runtime::{Limits, RunContext, Scope};
use asap_executor::sources::InMemoryDataStore;
use asap_executor::values::{Batch, Value};
use asap_planner::{
    plan_installable, test_support::query_time_reference, FrontendInput, InstallablePlan,
    PlanningModels, PlanningWorkload, SqlCatalog, UserInput,
};
use asap_types::ir::operator::Source;
use asap_types::ir::physical_export::PhysicalASAPOperatorPayload;
use asap_types::ir::schema::{DataType, Field, Schema};
use asap_types::ir::NonASAPOp;
use futures::{executor::block_on, StreamExt};

use crate::time_series::{series_labels, time_series_tables, RawDataSample};

/// The evaluation time of most runs, a multiple of every step used.
const T: i64 = 1_800_000_000_000;
const SECOND: i64 = 1_000;

/// No approximation.
const EXACT: &str = r#"{"max_error": 0.0, "failure_probability": 0.0}"#;

/// One repeating `query` in `language` (its workload JSON), every 10 s,
/// exact, over `source`: 10 series sampled every 10 s, 1,000 rows held.
fn workload(language: &str, query: &str, source: &str) -> PlanningWorkload {
    serde_json::from_str(&format!(
        r#"{{"query_workload": {{"language": {language}, "repeating_queries": [
              {{"query": {query:?}, "demand": {{"fixed_interval": 10000}},
                "requirements": {{"accuracy": {EXACT}}},
                "predictability": {{"predictable": {{"known_at": null}}}}}}]}},
            "data_workload": {{"arrival": {{"ingesting": {{"rows_per_second": 100.0}}}},
              "sources": {{"{source}": {{"series_count": 10, "sample_interval": 10000,
                  "rows_held": 1000}}}}}}}}"#
    ))
    .unwrap()
}

/// The batches of `plan`'s one query, run at query time at
/// `evaluation_ms`, within the query range `range` (an instant query
/// without one), over `store`.
fn run(
    plan: &InstallablePlan,
    evaluation_ms: i64,
    range: Option<(i64, i64)>,
    store: impl FnOnce(&asap_planner::PhysicalASAPDAG) -> InMemoryDataStore,
) -> Vec<Batch> {
    let reference = query_time_reference(plan).unwrap();
    let dag = &reference.dag;
    let roots = [dag.roots[0] as u64];
    let compiled = compile(dag, &[], &roots).expect("the executor compiles the plan");
    let instance = compiled.instantiate(Arc::new(store(dag))).unwrap();
    let mut context = RunContext::new(
        Scope::Query {
            evaluation_time_ms: evaluation_ms,
        },
        Limits::default(),
    )
    .unwrap();
    if let Some((start, end)) = range {
        context = context.with_query_range(start, end).unwrap();
    }
    block_on(async {
        let mut output = instance
            .execute(compiled.roots(), context)
            .unwrap()
            .remove(0);
        let mut batches = Vec::new();
        while let Some(batch) = output.next().await {
            batches.push((*batch.expect("the query runs")).clone());
        }
        batches
    })
}

// ── PromQL ───────────────────────────────────────────────────────────────

/// One sample of `metric` of job `job` at `timestamp_ms`.
fn sample(metric: &str, job: &str, timestamp_ms: i64, value: f64) -> RawDataSample {
    RawDataSample {
        labels: BTreeMap::from([
            ("__name__".to_string(), metric.to_string()),
            ("job".to_string(), job.to_string()),
        ]),
        timestamp_ms,
        value,
    }
}

async fn plan_promql(query: &str, metric: &str) -> InstallablePlan {
    plan_installable(UserInput::new(
        &workload(r#""promql""#, query, metric),
        FrontendInput::Promql,
        PlanningModels::builtin(),
    ))
    .await
    .unwrap_or_else(|error| panic!("{query}: {error}"))
    .0
}

/// Each answer of the PromQL `query` at `evaluation_ms` (within `range`)
/// over `samples`: its job and its value.
async fn promql_at(
    query: &str,
    metric: &str,
    samples: &[RawDataSample],
    evaluation_ms: i64,
    range: Option<(i64, i64)>,
) -> Vec<(String, f64)> {
    let plan = plan_promql(query, metric).await;
    let mut answers: Vec<_> = run(&plan, evaluation_ms, range, |dag| {
        time_series_tables(dag, samples)
    })
    .iter()
    .flat_map(|batch| {
        batch.to_rows().into_iter().map(|row| {
            let value = row
                .iter()
                .rev()
                .find_map(|v| match v {
                    Value::Float64(v) => Some(*v),
                    _ => None,
                })
                .expect("a sample value");
            let job = series_labels(batch.schema(), &row)["job"].clone();
            (job, value)
        })
    })
    .collect();
    answers.sort_by(|a, b| a.0.cmp(&b.0));
    answers
}

async fn promql(query: &str, metric: &str, samples: &[RawDataSample]) -> Vec<(String, f64)> {
    promql_at(query, metric, samples, T, None).await
}

fn assert_close(query: &str, got: &[(String, f64)], expected: &[(&str, f64)]) {
    assert_eq!(
        got.len(),
        expected.len(),
        "{query}: {got:?} vs {expected:?}"
    );
    for ((job, got), (expected_job, value)) in got.iter().zip(expected) {
        assert_eq!(job, expected_job, "{query}: {got:?} vs {expected:?}");
        assert!(
            (got - value).abs() < 1e-9,
            "{query}: job {job}: {got} vs {value}"
        );
    }
}

/// The gauge `g` of job `a`: 100 at `T - 600 s`, 1 at `T - 540 s`, 5 at
/// `T - 300 s`, 9 at `T - 30 s`, 50 at `T + 30 s`. The plan's instant
/// selector looks back 10 s (the sources' sample interval).
fn gauge() -> Vec<RawDataSample> {
    [
        (-600, 100.0),
        (-540, 1.0),
        (-300, 5.0),
        (-30, 9.0),
        (30, 50.0),
    ]
    .into_iter()
    .map(|(seconds, value)| sample("g", "a", T + seconds * SECOND, value))
    .collect()
}

/// `g[10m:1m]` at `T + 30 s` evaluates `g` at the multiples of 1 minute in
/// `(T - 570 s, T + 30 s]`: `T - 540 s` … `T`. No step sees the 100 at
/// `T - 600 s` (before the grid), the 9 at `T - 30 s` (more than 10 s
/// before a step) or the 50 at `T + 30 s` (after the last step): two
/// steps see a sample, the maximum is the 5 at `T - 300 s` and the minimum
/// the 1 at `T - 540 s`.
#[tokio::test]
async fn subquery_of_a_selector_evaluates_at_aligned_steps() {
    for (query, expected) in [
        ("max_over_time(g[10m:1m])", 5.0),
        ("min_over_time(g[10m:1m])", 1.0),
        ("count_over_time(g[10m:1m])", 2.0),
    ] {
        let got = promql_at(query, "g", &gauge(), T + 30 * SECOND, None).await;
        assert_close(query, &got, &[("a", expected)]);
    }
}

/// The counter `c` of job `a`, sampled every 10 s from `T - 700 s` to `T`,
/// starting at 1000: it grows 1 per sample, 6 per sample in `(T - 300 s,
/// T - 240 s]` and 100 per sample in `(T - 660 s, T - 600 s]`.
fn counter() -> Vec<RawDataSample> {
    let mut value = 1000.0;
    (0..=70)
        .map(|k| {
            let seconds = -700 + 10 * k;
            value += match seconds {
                -295..=-240 => 6.0,
                -655..=-600 => 100.0,
                _ => 1.0,
            };
            sample("c", "a", T + seconds * SECOND, value)
        })
        .collect()
}

/// `rate(c[1m])[10m:1m]` at `T` evaluates the rate at `T - 540 s` … `T`.
/// Each window `(t - 60 s, t]` holds 6 samples 10 s apart, so the rate
/// extrapolates their increase by 60/50: 1 per sample gives 0.1/s, the 6
/// per sample at the step `T - 240 s` 0.6/s. The step `T - 600 s` (where
/// the rate is 10/s) is not on the left-open grid. With `offset 5m` the
/// grid is `T - 840 s` … `T - 300 s`: it holds `T - 600 s`.
#[tokio::test]
async fn subquery_of_a_rate_evaluates_at_steps_and_moves_with_offset() {
    let samples = counter();
    for (query, expected) in [
        ("max_over_time(rate(c[1m])[10m:1m])", 0.6),
        ("min_over_time(rate(c[1m])[10m:1m])", 0.1),
        ("max_over_time(rate(c[1m])[10m:1m] offset 5m)", 10.0),
    ] {
        let got = promql(query, "c", &samples).await;
        assert_close(query, &got, &[("a", expected)]);
    }
}

/// The gauge `h` of job `a` around `A = T - 1 h`: 3 at `A - 70 s`, 6 at
/// `A - 60 s`, 2 at `A - 5 s`, 4 at `A`, 99 at `A + 5 s`; and 7 at `T`.
fn anchored_gauge() -> Vec<RawDataSample> {
    let anchor = T - 3_600 * SECOND;
    let mut samples: Vec<_> = [(-70, 3.0), (-60, 6.0), (-5, 2.0), (0, 4.0), (5, 99.0)]
        .into_iter()
        .map(|(seconds, value)| sample("h", "a", anchor + seconds * SECOND, value))
        .collect();
    samples.push(sample("h", "a", T, 7.0));
    samples
}

/// `h @ A` at `T` is `h` evaluated at `A`: the latest sample in `(A - 10 s,
/// A]`, the 4 at `A` (a sample at the window's end is in it, the 99 after
/// it is not). `offset 1m` moves the window to `(A - 70 s, A - 60 s]`:
/// the 6 at `A - 60 s`, not the 3 on its open start. Over a range the
/// anchored window reads `(A - 5 m, A]`.
#[tokio::test]
async fn an_at_timestamp_pins_the_selector_window() {
    let anchor_s = (T - 3_600 * SECOND) / SECOND;
    let samples = anchored_gauge();
    for (query, expected) in [
        (format!("h @ {anchor_s}"), 4.0),
        (format!("h offset 1m @ {anchor_s}"), 6.0),
        (format!("max_over_time(h[5m] @ {anchor_s})"), 6.0),
        (format!("min_over_time(h[5m] @ {anchor_s})"), 2.0),
        (format!("count_over_time(h[1m] @ {anchor_s})"), 2.0),
    ] {
        let got = promql(&query, "h", &samples).await;
        assert_close(&query, &got, &[("a", expected)]);
    }
}

/// A counter's window ends at its `@` anchor: `rate(c[5m] @ t)` at `T` is
/// `rate(c[5m])` at `t`; `@ end()` and `@ start()` are the query range's
/// bounds, at every step of it (each step evaluates the same window), and
/// an instant query's evaluation time without a range.
#[tokio::test]
async fn an_anchored_counter_window_ends_at_the_anchor() {
    let samples = counter();
    let rate_at = |at| async move { promql_at("rate(c[5m])", "c", &counter(), at, None).await };
    let at_240 = rate_at(T - 240 * SECOND).await;
    let at_100 = rate_at(T - 100 * SECOND).await;
    let at_t = rate_at(T).await;
    let t_240_s = (T - 240 * SECOND) / SECOND;
    let anchored = promql(&format!("rate(c[5m] @ {t_240_s})"), "c", &samples).await;
    assert_eq!(anchored, at_240);
    let anchored = promql(
        &format!("rate(c[5m] @ {t_240_s} offset 140s)"),
        "c",
        &samples,
    )
    .await;
    assert_eq!(anchored, rate_at(T - 380 * SECOND).await);
    let range = Some((T - 100 * SECOND, T));
    for step in [T - 100 * SECOND, T - 50 * SECOND, T] {
        let end = promql_at("rate(c[5m] @ end())", "c", &samples, step, range).await;
        assert_eq!(end, at_t, "@ end() at step {step}");
        let start = promql_at("rate(c[5m] @ start())", "c", &samples, step, range).await;
        assert_eq!(start, at_100, "@ start() at step {step}");
    }
    let instant = promql("rate(c[5m] @ start())", "c", &samples).await;
    assert_eq!(instant, at_t);
}

// ── SQL ──────────────────────────────────────────────────────────────────

/// `metrics(ts, service, latency)`, one series per service.
fn metrics_schema() -> Schema {
    Schema::with_time_index(
        vec![
            Field::plain("ts", DataType::Timestamp, false),
            Field::plain("service", DataType::Utf8, false),
            Field::plain("latency", DataType::Float64, false),
        ],
        0,
        vec![vec![0, 1]],
    )
}

/// Each answer row of the SQL `query` at `T` over `rows` of `metrics`
/// `(seconds before T, service, latency)`.
async fn sql(query: &str, rows: &[(i64, &str, f64)]) -> Vec<Vec<Value>> {
    let catalog = SqlCatalog::new().with_table("metrics", metrics_schema());
    let plan = plan_installable(UserInput::new(
        &workload(r#"{"sql": "datafusion_sql"}"#, query, "metrics"),
        FrontendInput::Sql { catalog: &catalog },
        PlanningModels::builtin(),
    ))
    .await
    .unwrap_or_else(|error| panic!("{query}: {error}"))
    .0;
    let mut answers: Vec<_> = run(&plan, T, None, |dag| {
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
        let rows = rows
            .iter()
            .map(|&(seconds, service, latency)| {
                schema
                    .fields
                    .iter()
                    .map(|field| match field.name.as_str() {
                        "ts" => Value::Timestamp(T - seconds * SECOND),
                        "service" => Value::Utf8(service.into()),
                        "latency" => Value::Float64(latency),
                        other => panic!("column {other}"),
                    })
                    .collect()
            })
            .collect();
        let batch = Batch::try_new(schema.clone(), rows).unwrap();
        InMemoryDataStore::default()
            .with_raw_data(
                Source::Table {
                    table_ref: "metrics".into(),
                },
                schema,
                vec![batch],
            )
            .unwrap()
    })
    .iter()
    .flat_map(Batch::to_rows)
    .collect();
    answers.sort_by(|a, b| format!("{a:?}").cmp(&format!("{b:?}")));
    answers
}

/// `asap_rate(latency, ts, 300000)` is `rate(latency[5m])` per service: a
/// counter from 100 sampled every 10 s in `(T - 300 s, T]` grows 58 in
/// 290 s (2 per sample for `a`, 3 for `b`), extrapolated by 300/290 to
/// the window's start, so 0.2/s and 0.3/s. The 0 at `T - 300 s` lies on
/// the window's open start: read, it would be a counter reset.
#[tokio::test]
async fn sql_asap_rate_reads_its_window_of_projected_rows() {
    let mut rows = vec![(300, "a", 0.0), (300, "b", 0.0)];
    for k in 0..30 {
        rows.push((290 - 10 * k, "a", 100.0 + 2.0 * k as f64));
        rows.push((290 - 10 * k, "b", 100.0 + 3.0 * k as f64));
    }
    let answers = sql(
        "SELECT service, asap_rate(latency, ts, 300000) AS r FROM metrics GROUP BY service",
        &rows,
    )
    .await;
    let rates: Vec<(String, f64)> = answers
        .iter()
        .map(|row| {
            let service = row
                .iter()
                .find_map(|v| match v {
                    Value::Utf8(s) => Some(s.to_string()),
                    _ => None,
                })
                .expect("a service");
            let rate = row
                .iter()
                .rev()
                .find_map(|v| match v {
                    Value::Float64(v) => Some(*v),
                    _ => None,
                })
                .expect("a rate");
            (service, rate)
        })
        .collect();
    assert_close("asap_rate", &rates, &[("a", 0.2), ("b", 0.3)]);
}

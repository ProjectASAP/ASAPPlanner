//! Cross-series (`Reduce`) aggregates planned exactly with
//! `plan_installable` and run with the Planner's executor over raw data:
//! PromQL `stddev`/`stdvar`, `group`, `count_values`, `without` grouping
//! and an exact `topk` over a grouped sum of rates; SQL `STDDEV`,
//! `VARIANCE` and `CORR`. Each answer is checked against the value the
//! PromQL or SQL definition gives for the same data.

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

/// The evaluation time of every run.
const EVALUATION_MS: i64 = 1_800_000_000_000;

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

fn query_scope() -> RunContext {
    RunContext::new(
        Scope::Query {
            evaluation_time_ms: EVALUATION_MS,
        },
        Limits::default(),
    )
    .unwrap()
}

/// The batches of `plan`'s one query, run at query time over `store`.
fn run(
    plan: &InstallablePlan,
    store: impl FnOnce(&asap_planner::PhysicalASAPDAG) -> InMemoryDataStore,
) -> Vec<Batch> {
    let reference = query_time_reference(plan).unwrap();
    let dag = &reference.dag;
    let roots = [dag.roots[0] as u64];
    let compiled = compile(dag, &[], &roots).expect("the executor compiles the plan");
    let instance = compiled.instantiate(Arc::new(store(dag))).unwrap();
    block_on(async {
        let mut output = instance
            .execute(compiled.roots(), query_scope())
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

/// One sample of `metric` with `labels` at `timestamp_ms`.
fn sample(metric: &str, labels: &[(&str, &str)], timestamp_ms: i64, value: f64) -> RawDataSample {
    let mut all: BTreeMap<String, String> = labels
        .iter()
        .map(|(name, value)| (name.to_string(), value.to_string()))
        .collect();
    all.insert("__name__".into(), metric.into());
    RawDataSample {
        labels: all,
        timestamp_ms,
        value,
    }
}

/// The gauge `m`, one sample per series 5 s before the evaluation: job
/// `a` holds 1, 2 and 4, job `b` holds 2 and (with a `zone` label) 2.5.
fn gauge_samples() -> Vec<RawDataSample> {
    let at = EVALUATION_MS - 5_000;
    vec![
        sample("m", &[("job", "a"), ("instance", "1")], at, 1.0),
        sample("m", &[("job", "a"), ("instance", "2")], at, 2.0),
        sample("m", &[("job", "a"), ("instance", "3")], at, 4.0),
        sample("m", &[("job", "b"), ("instance", "1")], at, 2.0),
        sample(
            "m",
            &[("job", "b"), ("instance", "2"), ("zone", "z")],
            at,
            2.5,
        ),
    ]
}

/// The counter `c`: per job `j1`..`j4`, two instances counting up `n` and
/// `n + 1` per 10 s, sampled every 10 s over the last 10 minutes.
fn counter_samples() -> Vec<RawDataSample> {
    let mut samples = Vec::new();
    for (job, n) in [("j1", 1.0), ("j2", 2.0), ("j3", 3.0), ("j4", 4.0)] {
        for (instance, slope) in [("1", n), ("2", n + 1.0)] {
            for step in 0..60 {
                samples.push(sample(
                    "c",
                    &[("job", job), ("instance", instance)],
                    EVALUATION_MS - 600_000 + step * 10_000,
                    slope * step as f64,
                ));
            }
        }
    }
    samples
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

/// Each answer of the PromQL `query` over `samples`: its labels (the
/// series identity's and each non-empty label column's) and its value.
async fn promql(
    query: &str,
    metric: &str,
    samples: &[RawDataSample],
) -> Vec<(BTreeMap<String, String>, f64)> {
    let plan = plan_promql(query, metric).await;
    let mut answers: Vec<_> = run(&plan, |dag| time_series_tables(dag, samples))
        .iter()
        .flat_map(|batch| {
            batch.to_rows().into_iter().map(|row| {
                let value = row
                    .iter()
                    .rev()
                    .find_map(|v| match v {
                        Value::Float64(v) => Some(*v),
                        Value::Int64(v) => Some(*v as f64),
                        _ => None,
                    })
                    .expect("a sample value");
                (series_labels(batch.schema(), &row), value)
            })
        })
        .collect();
    answers.sort_by(|a, b| a.0.cmp(&b.0));
    answers
}

fn labels(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
    pairs
        .iter()
        .map(|(name, value)| (name.to_string(), value.to_string()))
        .collect()
}

fn assert_close(
    got: &[(BTreeMap<String, String>, f64)],
    expected: &[(BTreeMap<String, String>, f64)],
) {
    assert_eq!(got.len(), expected.len(), "{got:?} vs {expected:?}");
    for ((got_labels, got), (labels, value)) in got.iter().zip(expected) {
        assert_eq!(got_labels, labels, "{got:?} vs {expected:?}");
        assert!((got - value).abs() < 1e-12, "{labels:?}: {got} vs {value}");
    }
}

/// `stddev` and `stdvar` are population statistics: job `a`'s 1, 2, 4 have
/// mean 7/3 and variance 14/9; job `b`'s 2, 2.5 variance 1/16.
#[tokio::test]
async fn promql_stddev_and_stdvar_by_job() {
    let stddev = promql("stddev by (job) (m)", "m", &gauge_samples()).await;
    assert_close(
        &stddev,
        &[
            (labels(&[("job", "a")]), (14.0f64 / 9.0).sqrt()),
            (labels(&[("job", "b")]), 0.25),
        ],
    );
    let stdvar = promql("stdvar by (job) (m)", "m", &gauge_samples()).await;
    assert_close(
        &stdvar,
        &[
            (labels(&[("job", "a")]), 14.0 / 9.0),
            (labels(&[("job", "b")]), 0.0625),
        ],
    );
}

/// `group` answers 1 per group, whatever the values.
#[tokio::test]
async fn promql_group_by_job() {
    let answers = promql("group by (job) (m)", "m", &gauge_samples()).await;
    assert_close(
        &answers,
        &[
            (labels(&[("job", "a")]), 1.0),
            (labels(&[("job", "b")]), 1.0),
        ],
    );
}

/// `count_values` counts each distinct value under a new label holding the
/// value as Go's `FormatFloat(v, 'f', -1, 64)` writes it.
#[tokio::test]
async fn promql_count_values() {
    let answers = promql(r#"count_values("v", m)"#, "m", &gauge_samples()).await;
    assert_close(
        &answers,
        &[
            (labels(&[("v", "1")]), 1.0),
            (labels(&[("v", "2")]), 2.0),
            (labels(&[("v", "2.5")]), 1.0),
            (labels(&[("v", "4")]), 1.0),
        ],
    );
}

/// `sum without (instance)` groups by every other label, `__name__`
/// dropped: `{job="b"}` and `{job="b", zone="z"}` are different groups.
#[tokio::test]
async fn promql_sum_without_instance() {
    let answers = promql("sum without (instance) (m)", "m", &gauge_samples()).await;
    assert_close(
        &answers,
        &[
            (labels(&[("job", "a")]), 7.0),
            (labels(&[("job", "b")]), 2.0),
            (labels(&[("job", "b"), ("zone", "z")]), 2.5),
        ],
    );
}

/// An exact `topk` over a grouped sum of rates answers the 3 largest of
/// the grouped sum's answers, with their jobs.
#[tokio::test]
async fn promql_exact_topk_of_grouped_rates() {
    let sums = promql("sum by (job) (rate(c[5m]))", "c", &counter_samples()).await;
    assert_eq!(sums.len(), 4);
    let mut expected = sums.clone();
    expected.sort_by(|a, b| b.1.total_cmp(&a.1));
    expected.truncate(3);
    expected.sort_by(|a, b| a.0.cmp(&b.0));
    let top = promql(
        "topk(3, sum by (job) (rate(c[5m])))",
        "c",
        &counter_samples(),
    )
    .await;
    assert_close(&top, &expected);
    // Jobs j2..j4: their rates sum to (2n + 1) / 10 per second.
    assert_close(
        &top,
        &[
            (labels(&[("job", "j2")]), 0.5),
            (labels(&[("job", "j3")]), 0.7),
            (labels(&[("job", "j4")]), 0.9),
        ],
    );
}

// ── SQL ──────────────────────────────────────────────────────────────────

/// `t(ts, k, x, a, b)`; `x`, `a` and `b` nullable.
fn table_schema() -> Schema {
    Schema::with_time_index(
        vec![
            Field::plain("ts", DataType::Timestamp, false),
            Field::plain("k", DataType::Utf8, false),
            Field::plain("x", DataType::Float64, true),
            Field::plain("a", DataType::Float64, true),
            Field::plain("b", DataType::Float64, true),
        ],
        0,
        vec![],
    )
}

/// One row of `t`: `(k, x, a, b)`.
type TableRow = (&'static str, Option<f64>, Option<f64>, Option<f64>);

/// Rows `(k, x, a, b)`: `k1` has x 1, 2, 4 and a NULL; `k2` one x; `k3`
/// only a NULL x. `(a, b)` pairs lie on `b = 2a + 1` but one with a NULL.
fn table_rows() -> Vec<TableRow> {
    vec![
        ("k1", Some(1.0), Some(1.0), Some(3.0)),
        ("k1", Some(2.0), Some(2.0), Some(5.0)),
        ("k1", Some(4.0), Some(3.0), Some(7.0)),
        ("k1", None, Some(4.0), None),
        ("k2", Some(5.0), Some(5.0), Some(11.0)),
        ("k3", None, None, Some(1.0)),
    ]
}

/// Each answer row of the SQL `query` over [`table_rows`].
async fn sql(query: &str) -> Vec<Vec<Value>> {
    let catalog = SqlCatalog::new().with_table("t", table_schema());
    let plan = plan_installable(UserInput::new(
        &workload(r#"{"sql": "datafusion_sql"}"#, query, "t"),
        FrontendInput::Sql { catalog: &catalog },
        PlanningModels::builtin(),
    ))
    .await
    .unwrap_or_else(|error| panic!("{query}: {error}"))
    .0;
    let mut rows: Vec<_> = run(&plan, |dag| {
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
        let float = |v: Option<f64>| v.map_or(Value::Null, Value::Float64);
        let rows = table_rows()
            .into_iter()
            .enumerate()
            .map(|(i, (k, x, a, b))| {
                schema
                    .fields
                    .iter()
                    .map(|field| match field.name.as_str() {
                        "ts" => Value::Timestamp(EVALUATION_MS - 1000 * (i as i64 + 1)),
                        "k" => Value::Utf8(k.into()),
                        "x" => float(x),
                        "a" => float(a),
                        "b" => float(b),
                        other => panic!("column {other}"),
                    })
                    .collect()
            })
            .collect();
        let batch = Batch::try_new(schema.clone(), rows).unwrap();
        InMemoryDataStore::default()
            .with_raw_data(
                Source::Table {
                    table_ref: "t".into(),
                },
                schema,
                vec![batch],
            )
            .unwrap()
    })
    .iter()
    .flat_map(Batch::to_rows)
    .collect();
    rows.sort_by(|a, b| format!("{a:?}").cmp(&format!("{b:?}")));
    rows
}

fn float(value: &Value) -> Option<f64> {
    match value {
        Value::Float64(v) => Some(*v),
        Value::Null => None,
        other => panic!("a DOUBLE: {other:?}"),
    }
}

fn assert_floats(row: &[Value], expected: &[Option<f64>]) {
    let got: Vec<_> = row.iter().map(float).collect();
    assert_eq!(got.len(), expected.len(), "{row:?}");
    for (got, expected) in got.iter().zip(expected) {
        match (got, expected) {
            (Some(got), Some(expected)) => {
                assert!((got - expected).abs() < 1e-12, "{row:?} vs {expected}")
            }
            _ => assert_eq!(got, expected, "{row:?}"),
        }
    }
}

/// SQL `STDDEV` and `VAR_SAMP` are the sample statistics (`STDDEV_SAMP`,
/// `VAR_SAMP`), NULLs ignored, NULL for fewer than two values;
/// `STDDEV_POP` and `VAR_POP` are NULL only for no value. `k1`'s 1, 2, 4:
/// sample variance 7/3, population 14/9.
#[tokio::test]
async fn sql_stddev_and_variance_by_key() {
    let rows =
        sql("SELECT k, STDDEV(x), VAR_SAMP(x), STDDEV_POP(x), VAR_POP(x) FROM t GROUP BY k").await;
    let keys: Vec<_> = rows.iter().map(|row| format!("{:?}", row[0])).collect();
    assert_eq!(
        keys,
        ["k1", "k2", "k3"].map(|k| format!("{:?}", Value::Utf8(k.into())))
    );
    assert_floats(
        &rows[0][1..],
        &[
            Some((7.0f64 / 3.0).sqrt()),
            Some(7.0 / 3.0),
            Some((14.0f64 / 9.0).sqrt()),
            Some(14.0 / 9.0),
        ],
    );
    assert_floats(&rows[1][1..], &[None, None, Some(0.0), Some(0.0)]);
    assert_floats(&rows[2][1..], &[None, None, None, None]);
}

/// `CORR(a, b)` over the rows where both are non-NULL: they lie on a line
/// of positive slope, so the correlation is 1.
#[tokio::test]
async fn sql_corr() {
    let rows = sql("SELECT corr(a, b) FROM t").await;
    assert_eq!(rows.len(), 1);
    assert_floats(&rows[0], &[Some(1.0)]);
}

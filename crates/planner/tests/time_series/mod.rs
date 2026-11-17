//! A stand-in runtime's raw data samples, bound at the plan's `Scan`s with
//! public APIs only: one raw data table per time series in an
//! `InMemoryDataStore`, `(ts, value, *labels)` over the samples it received. The DAG's own
//! `TimeRange`s apply each window, instant lookback and stale marker. It
//! also runs a plan over them as that runtime does ([`run`]) and compares
//! the answer rows with expected ones ([`assert_answers_near`]).
#![allow(dead_code)] // Each test uses part of it.

use std::collections::BTreeMap;
use std::sync::Arc;

use asap_executor::physical_planner::compile;
use asap_executor::runtime::{Limits, RunContext, Scope};
use asap_executor::sources::{DataStore, InMemoryDataStore};
use asap_executor::values::{Batch, Value};
use asap_planner::{InstallablePlan, MaterializedView};
use asap_types::ir::operator::Source;
use asap_types::ir::schema::{DataType, FieldDataType, Schema, PROMQL_SERIES_IDENTITY};
use asap_types::plan::physical_asap_dag::PhysicalASAPDAG;
use futures::{executor::block_on, StreamExt};

/// One raw data sample as it arrives, e.g. one Prometheus Remote Write sample.
#[derive(Debug, Clone, PartialEq)]
pub struct RawDataSample {
    /// Every label of the series, `__name__` (the metric) included.
    pub labels: BTreeMap<String, String>,
    pub timestamp_ms: i64,
    pub value: f64,
}

/// `pairs` as a label set: `labels(&[("job", "a")])` is `{job="a"}`.
pub fn labels(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
    pairs
        .iter()
        .map(|(name, value)| (name.to_string(), value.to_string()))
        .collect()
}

/// One sample of `metric` for the series with `labels` (and `__name__`
/// `metric`) at `timestamp_ms`: `sample("m", &[("job", "a")], 0, 1.0)` is
/// `m{job="a"} 1` at time 0.
pub fn sample(
    metric: &str,
    labels: &[(&str, &str)],
    timestamp_ms: i64,
    value: f64,
) -> RawDataSample {
    let mut all = self::labels(labels);
    all.insert("__name__".into(), metric.into());
    RawDataSample {
        labels: all,
        timestamp_ms,
        value,
    }
}

/// The series identity column's text: the canonical JSON of every label
/// ([`PROMQL_SERIES_IDENTITY`]).
pub fn series_identity(labels: &BTreeMap<String, String>) -> String {
    serde_json::to_string(labels).unwrap()
}

/// The deployment's raw data for `dag`: each metric its time-series `Scan`s
/// read, bound to its samples among `samples` (routed by `__name__`), with
/// every column those `Scan`s read.
pub fn time_series_tables(dag: &PhysicalASAPDAG, samples: &[RawDataSample]) -> InMemoryDataStore {
    // Each metric's columns: the union of its Scans' columns, by name.
    let schemas = dag
        .raw_data_schemas()
        .into_iter()
        .filter_map(|(source, schema)| match source {
            Source::TimeSeries { metric } => Some((metric, schema)),
            Source::Table { .. } => None,
        });
    let mut tables = InMemoryDataStore::default();
    for (metric, schema) in schemas {
        let schema = Arc::new(schema);
        let rows = samples
            .iter()
            .filter(|s| s.labels.get("__name__") == Some(&metric))
            .map(|sample| row(&schema, sample))
            .collect();
        let batch = Batch::try_new(schema.clone(), rows).unwrap();
        tables = tables
            .with_raw_data(Source::TimeSeries { metric }, schema, vec![batch])
            .unwrap();
    }
    tables
}

/// `sample` as a row of `schema`: its identity, time, value, and each label
/// column's value (empty when the series lacks the label).
fn row(schema: &Schema, sample: &RawDataSample) -> Vec<Value> {
    schema
        .fields
        .iter()
        .enumerate()
        .map(|(index, field)| match field.name.as_str() {
            PROMQL_SERIES_IDENTITY => Value::Utf8(series_identity(&sample.labels).into()),
            _ if Some(index) == schema.time_index => Value::Timestamp(sample.timestamp_ms),
            "value" if field.dtype == FieldDataType::Plain(DataType::Float64) => {
                Value::Float64(sample.value)
            }
            label => Value::Utf8(sample.labels.get(label).cloned().unwrap_or_default().into()),
        })
        .collect()
}

/// The labels of the series an answer row belongs to, read from the output
/// schema's label columns: the series identity's labels, when the schema
/// keeps one, and every other non-empty text column (a label the query
/// grouped by).
pub fn series_labels(schema: &Schema, row: &[Value]) -> BTreeMap<String, String> {
    let mut labels = BTreeMap::new();
    for (field, cell) in schema.fields.iter().zip(row) {
        let Value::Utf8(text) = cell else { continue };
        if field.name == PROMQL_SERIES_IDENTITY {
            let identity: BTreeMap<String, String> = serde_json::from_str(text).unwrap();
            labels.extend(identity);
        } else if !text.is_empty() {
            labels.insert(field.name.clone(), text.to_string());
        }
    }
    labels
}

/// One answer row: its series labels and its value.
pub type Answer = (BTreeMap<String, String>, f64);

/// Each answer row as its series labels and value, sorted by labels.
pub fn answers(batches: &[Batch]) -> Vec<Answer> {
    let mut answers: Vec<_> = batches
        .iter()
        .flat_map(|batch| {
            batch.to_rows().into_iter().map(|row| {
                let labels = series_labels(batch.schema(), &row);
                let value = row.iter().find_map(|v| match v {
                    Value::Float64(v) => Some(*v),
                    _ => None,
                });
                (labels, value.expect("a Float64 answer"))
            })
        })
        .collect();
    answers.sort_by(|a, b| a.0.cmp(&b.0));
    answers
}

/// The node that answers each query of `plan`, in workload order.
pub fn query_roots(plan: &InstallablePlan) -> Vec<u64> {
    plan.dag.roots.iter().map(|&root| root as u64).collect()
}

/// Compile `ids` of `dag` and run them in `scope` over `store` (usually
/// [`time_series_tables`], plus the stored states of `views`), as a runtime
/// does with the executor: each id's output batches. Each of `views` is
/// bound as its materialized view slot (nothing below it compiled); pass
/// `&[]` to compute every node from raw data.
pub fn run(
    dag: &PhysicalASAPDAG,
    store: impl DataStore + 'static,
    views: &[MaterializedView],
    ids: &[u64],
    scope: Scope,
) -> Vec<Vec<Batch>> {
    // The executor's plan for `ids`; every `Scan` it reaches and every view
    // in `views` is a slot of `store`.
    let compiled = compile(dag, views, ids).expect("the executor compiles the plan");
    let instance = compiled.instantiate(Arc::new(store)).unwrap();
    let ingestion = matches!(scope, Scope::Ingestion { .. });
    block_on(async {
        // The run's scope (ingestion window or query time) and limits.
        let context = RunContext::new(scope, Limits::default()).unwrap();
        if ingestion {
            // An ingestion run: every id's batches, for the runtime to store.
            let outputs = instance
                .execute_ingestion(compiled.roots(), context)
                .unwrap()
                .await
                .unwrap();
            return outputs
                .into_iter()
                .map(|batches| batches.iter().map(|batch| (**batch).clone()).collect())
                .collect();
        }
        // A query run: one output stream per id.
        let outputs = instance.execute(compiled.roots(), context).unwrap();
        futures::future::join_all(outputs.into_iter().map(|mut output| async move {
            // Its batches, collected.
            let mut batches = Vec::new();
            while let Some(batch) = output.next().await {
                batches.push((*batch.unwrap()).clone());
            }
            batches
        }))
        .await
    })
}

/// Asserts `answered` matches `expected`, label set for label set, each
/// value within absolute error `tolerance`: 10.04 is near 10 at 0.05.
pub fn assert_answers_near(answered: &[Answer], expected: &[Answer], tolerance: f64) {
    assert_answers_within(answered, expected, |_| tolerance);
}

/// Asserts `answered` matches `expected`, label set for label set, each
/// value within relative error `relative_tolerance` of its expected value:
/// 1000.5 is near 1000 at 0.001.
pub fn assert_answers_near_relative(
    answered: &[Answer],
    expected: &[Answer],
    relative_tolerance: f64,
) {
    assert_answers_within(answered, expected, |expected_value| {
        relative_tolerance * expected_value.abs()
    });
}

/// Asserts `answered` matches `expected`, label set for label set, each
/// value within `allowed_error(expected value)`.
fn assert_answers_within(
    answered: &[Answer],
    expected: &[Answer],
    allowed_error: impl Fn(f64) -> f64,
) {
    assert_eq!(
        answered.len(),
        expected.len(),
        "{answered:?} vs {expected:?}"
    );
    for (answer, expected_answer) in answered.iter().zip(expected) {
        assert_eq!(answer.0, expected_answer.0, "{answered:?} vs {expected:?}");
        assert!(
            (answer.1 - expected_answer.1).abs() <= allowed_error(expected_answer.1),
            "{answered:?} vs {expected:?}"
        );
    }
}

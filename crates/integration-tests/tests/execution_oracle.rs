//! The execution oracle (`docs/develop_docs/testing.md`, "Oracles"): the
//! plan Stage 3 selects is run by the executor over synthetic data, as a
//! runtime runs it (every materialized view refreshed over its windows,
//! then the query run), and its answers are compared with an exact
//! evaluation of the query written here over the same samples. Exact
//! queries must give equal answers; an approximate query's answers may be
//! off by more than ε for at most a fraction δ of its values, checked over
//! every series' value with a fixed seed.
//!
//! Each test checks that the plan takes the path it means to execute (a
//! chain, a sketch): the answers of another plan would not test that path.

use std::collections::BTreeMap;
use std::sync::Arc;

use asap_executor::physical_planner::compile;
use asap_executor::runtime::{Limits, Scope};
use asap_executor::sources::InMemoryDataStore;
use asap_executor::values::{Batch, Value};
use asap_types::deployment_capabilities::DeploymentCapabilities;
use asap_types::ir::materialized_view::MaterializedViewRefresh;
use asap_types::ir::physical_export::{PhysicalASAPDAG, PhysicalASAPOperatorPayload};
use asap_types::ir::properties::{AccuracyMetric, AccuracyScope, AccuracyTarget};
use asap_types::ir::schema::{DataType, FieldDataType, Schema, PROMQL_SERIES_IDENTITY};
use asap_types::ir::ASAPOp;
use asap_types::stages::ValueSign;
use asap_types::workload::{DurationMs, PrometheusMetricType, SourceFacts};

use crate::cost_oracle::{operator_kinds, plan_one_promql_query};
use crate::physical_common::{execute, raw_data_store};

/// The evaluation time every query is answered at: 30 min of samples
/// before it (the longest window read is 15 min back), on a 60 s
/// boundary.
const EVALUATION_MS: i64 = 1_800_000;
/// Series of `m`, each labelled `instance="i<k>"`.
const SERIES: usize = 200;
/// Every series has a sample every second from 0 up to [`EVALUATION_MS`].
const SAMPLE_INTERVAL_MS: i64 = 1_000;
/// The fixed seed of the sample values.
const SEED: u64 = 1253;
/// Each sample's value is uniform in [0, [`NOISE_RANGE`]) plus
/// [`DRIFT_PER_MINUTE`] times the minute it falls in, so every 60 s window
/// holds values of its own range: an answer read from the wrong windows
/// moves by far more than any ε.
const NOISE_RANGE: f64 = 100.0;
const DRIFT_PER_MINUTE: f64 = 10.0;
/// The raw data is one 360,000-row batch, above the executor's default
/// 64 MiB per run.
const MAX_BYTES_PER_RUN: usize = 1 << 30;

/// One series of `m`: its `instance` label, all its labels, and its
/// identity column's value (the labels as JSON), encoded once.
#[derive(Debug)]
struct Series {
    instance: String,
    labels: BTreeMap<String, Arc<str>>,
    identity: Arc<str>,
}

/// One raw data sample: its series, time and value.
#[derive(Debug, Clone)]
struct Sample {
    series: Arc<Series>,
    timestamp_ms: i64,
    value: f64,
}

/// A xorshift64* generator: the samples depend only on [`SEED`].
struct Rng(u64);

impl Rng {
    /// A value uniform in [0, 1).
    fn next_unit(&mut self) -> f64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        let bits = self.0.wrapping_mul(0x2545_F491_4F6C_DD1D) >> 11;
        bits as f64 / (1u64 << 53) as f64
    }
}

/// [`SERIES`] series of `m`, a sample every second in [0, EVALUATION_MS]
/// (a non-negative gauge whose values drift up each minute, see
/// [`DRIFT_PER_MINUTE`]), sorted by time.
fn samples() -> Vec<Sample> {
    let mut rng = Rng(SEED);
    let mut samples = Vec::new();
    for series in 0..SERIES {
        let labels = BTreeMap::from([
            ("__name__".to_string(), "m".to_string()),
            ("instance".to_string(), format!("i{series}")),
        ]);
        let series = Arc::new(Series {
            instance: labels["instance"].clone(),
            identity: serde_json::to_string(&labels).unwrap().into(),
            labels: labels
                .into_iter()
                .map(|(name, value)| (name, value.into()))
                .collect(),
        });
        for step in 0..=(EVALUATION_MS / SAMPLE_INTERVAL_MS) {
            let timestamp_ms = step * SAMPLE_INTERVAL_MS;
            let minute = (timestamp_ms / 60_000) as f64;
            samples.push(Sample {
                series: series.clone(),
                timestamp_ms,
                value: rng.next_unit() * NOISE_RANGE + DRIFT_PER_MINUTE * minute,
            });
        }
    }
    samples.sort_by_key(|sample| sample.timestamp_ms);
    samples
}

/// The samples of `samples` (sorted by time) with times in `(start, end]`.
fn between(samples: &[Sample], start_ms: i64, end_ms: i64) -> &[Sample] {
    let from = samples.partition_point(|sample| sample.timestamp_ms <= start_ms);
    let to = samples.partition_point(|sample| sample.timestamp_ms <= end_ms);
    &samples[from..to]
}

/// Each series' sample values with times in `(EVALUATION_MS - offset -
/// range, EVALUATION_MS - offset]`, by its `instance`: PromQL's range
/// selector `m[range] offset offset`.
fn window(samples: &[Sample], range_ms: i64, offset_ms: i64) -> BTreeMap<String, Vec<f64>> {
    let end = EVALUATION_MS - offset_ms;
    let mut windows: BTreeMap<String, Vec<f64>> = BTreeMap::new();
    for sample in between(samples, end - range_ms, end) {
        windows
            .entry(sample.series.instance.clone())
            .or_default()
            .push(sample.value);
    }
    windows
}

/// `m`'s facts: `series_count` series (declared; the data has [`SERIES`])
/// of a non-negative gauge sampled every second.
fn source(series_count: u64) -> SourceFacts {
    SourceFacts {
        series_count: Some(series_count),
        sample_interval: Some(DurationMs(SAMPLE_INTERVAL_MS as u64)),
        metric_type: Some(PrometheusMetricType::Gauge),
        column_signs: BTreeMap::from([("value".to_string(), ValueSign::NonNegative)]),
        ..SourceFacts::default()
    }
}

/// The plan Stage 3 selects for `query` every 60 s at `accuracy` over `m`
/// declared with `series_count` series, on `capabilities`.
fn plan(
    query: &str,
    accuracy: AccuracyTarget,
    series_count: u64,
    capabilities: &DeploymentCapabilities,
) -> PhysicalASAPDAG {
    let planned = plan_one_promql_query(query, accuracy, source(series_count), None, capabilities);
    let (plan, _) = planned.stages.run.selection.expect("a plan is selected");
    plan
}

/// A deployment that keeps no raw data: every window is read from state
/// built at ingestion time.
fn no_raw_data() -> DeploymentCapabilities {
    DeploymentCapabilities {
        raw_data_retained: false,
        ..DeploymentCapabilities::default()
    }
}

/// `sample` as a row of `schema`: its identity, time, value and labels.
fn row(schema: &Schema, sample: &Sample) -> Vec<Value> {
    schema
        .fields
        .iter()
        .enumerate()
        .map(|(index, field)| match field.name.as_str() {
            PROMQL_SERIES_IDENTITY => Value::Utf8(sample.series.identity.clone()),
            _ if Some(index) == schema.time_index => Value::Timestamp(sample.timestamp_ms),
            "value" if field.dtype == FieldDataType::Plain(DataType::Float64) => {
                Value::Float64(sample.value)
            }
            label => Value::Utf8(
                sample
                    .series
                    .labels
                    .get(label)
                    .cloned()
                    .unwrap_or_else(|| "".into()),
            ),
        })
        .collect()
}

/// One stored state of a view and the `(start, end]` of the data it holds.
struct ViewState {
    batch: Batch,
    start_ms: i64,
    end_ms: i64,
}

/// Runs node `id` of `dag` in `scope`, `raw` bound at its `Scan`s of `m`
/// (none bound when `raw` is `None`: a run that reads raw data then fails)
/// and each view of `views` bound to its stored states: the node's output
/// rows.
fn run(
    dag: &PhysicalASAPDAG,
    raw: Option<&[Sample]>,
    views: &BTreeMap<u64, Vec<ViewState>>,
    id: u64,
    scope: Scope,
) -> (Arc<Schema>, Vec<Vec<Value>>) {
    let bound: Vec<_> = dag
        .materialized_views()
        .unwrap()
        .into_iter()
        .filter(|view| views.contains_key(&(view.node_id as u64)))
        .collect();
    let compiled = compile(dag, &bound, &[id]).expect("the executor compiles the plan");
    let mut store = match raw {
        Some(raw) => raw_data_store(dag, |_, schema| {
            raw.iter().map(|sample| row(schema, sample)).collect()
        }),
        None => InMemoryDataStore::default(),
    };
    for view in &bound {
        for state in &views[&(view.node_id as u64)] {
            store
                .append_materialized_view(
                    view,
                    (state.start_ms, state.end_ms),
                    vec![state.batch.clone()],
                )
                .unwrap();
        }
    }
    let limits = Limits {
        max_bytes: MAX_BYTES_PER_RUN,
        ..Limits::default()
    };
    let batches = execute(&compiled, store, scope, limits).remove(0);
    let schema = Arc::new(dag.nodes[id as usize].output_schema.clone());
    (schema, batches.iter().flat_map(|b| b.to_rows()).collect())
}

/// The plan's answer at [`EVALUATION_MS`] as a runtime computes it on
/// `deployment`: every materialized view refreshed in plan order (a chain
/// once per window its retention keeps, newest ending at the evaluation
/// time, in an ingestion run over the samples of that window and the
/// windows beside it), then the query run reading them, bound the raw data
/// only when the deployment keeps it. Each answer row's value (its
/// `value_column`: `value` for a per-series function, the aggregate's name
/// for an aggregate) by its series' `instance` (`""` for an aggregate over
/// every series); two rows of one series fail.
fn answers(
    dag: &PhysicalASAPDAG,
    samples: &[Sample],
    deployment: &DeploymentCapabilities,
    value_column: &str,
) -> BTreeMap<String, f64> {
    let mut built: BTreeMap<u64, Vec<ViewState>> = BTreeMap::new();
    for view in dag.materialized_views().unwrap() {
        let node = &dag.nodes[view.node_id];
        let width_ms = match &node.payload {
            PhysicalASAPOperatorPayload::ASAP(ASAPOp::SummaryTumblingWindowChain {
                tumbling_window_width_ms,
                ..
            }) => *tumbling_window_width_ms as i64,
            other => panic!(
                "a materialized view other than a chain: {}",
                other.kind_name()
            ),
        };
        let windows = (view.retention_ms() as i64 / width_ms).max(1);
        let mut states = Vec::new();
        for k in (0..windows).rev() {
            let end_ms = EVALUATION_MS - k * width_ms;
            let start_ms = end_ms - width_ms;
            let scope = match view.refresh() {
                MaterializedViewRefresh::AtIngestion => Scope::Ingestion {
                    window_start_ms: start_ms,
                    window_end_ms: end_ms,
                },
                MaterializedViewRefresh::OnFirstRead => Scope::Query {
                    evaluation_time_ms: EVALUATION_MS,
                },
            };
            // An ingestion run is bound the samples of its window and of the
            // windows beside it (binding all of them only slows the run).
            let near = between(samples, start_ms - width_ms, end_ms + width_ms);
            let (schema, rows) = run(dag, Some(near), &built, view.node_id as u64, scope);
            states.push(ViewState {
                batch: Batch::try_new(schema, rows).unwrap(),
                start_ms,
                end_ms,
            });
        }
        built.insert(view.node_id as u64, states);
    }
    assert_eq!(dag.roots.len(), 1, "one query");
    let query = Scope::Query {
        evaluation_time_ms: EVALUATION_MS,
    };
    let raw = deployment.raw_data_retained.then_some(samples);
    let (schema, rows) = run(dag, raw, &built, dag.roots[0] as u64, query);
    let value_column = schema
        .fields
        .iter()
        .position(|field| field.name == value_column)
        .unwrap_or_else(|| panic!("no column {value_column} in {:?}", schema.fields));
    let mut answers = BTreeMap::new();
    for row in &rows {
        let mut instance = String::new();
        for (field, cell) in schema.fields.iter().zip(row) {
            match cell {
                Value::Utf8(text) if field.name == PROMQL_SERIES_IDENTITY => {
                    let labels: BTreeMap<String, String> = serde_json::from_str(text).unwrap();
                    instance = labels.get("instance").cloned().unwrap_or_default();
                }
                Value::Utf8(text) if field.name == "instance" => instance = text.to_string(),
                _ => {}
            }
        }
        let value = match &row[value_column] {
            Value::Float64(value) => *value,
            Value::Int64(value) => *value as f64,
            other => panic!("a non-numeric answer {other:?}"),
        };
        if let Some(earlier) = answers.insert(instance.clone(), value) {
            panic!("two answers for series {instance:?}: {earlier} and {value}");
        }
    }
    answers
}

/// `actual` equals `expected` per series, to a relative 1e-9.
fn assert_answers_equal(actual: &BTreeMap<String, f64>, expected: &BTreeMap<String, f64>) {
    assert_eq!(
        actual.keys().collect::<Vec<_>>(),
        expected.keys().collect::<Vec<_>>(),
        "the plan answers other series than the exact evaluation"
    );
    for (series, value) in expected {
        let got = actual[series];
        assert!(
            (got - value).abs() <= 1e-9 * value.abs().max(1.0),
            "series {series:?}: the plan answers {got}, the exact evaluation {value}"
        );
    }
}

/// `sum(m)` (exact, query-time scan of the latest sample of each series):
/// the sum of the 200 series' samples at the evaluation time.
#[test]
fn an_exact_instant_sum_equals_the_sum_of_the_latest_samples() {
    let deployment = DeploymentCapabilities::default();
    let plan = plan(
        "sum(m)",
        AccuracyTarget::exact(),
        SERIES as u64,
        &deployment,
    );
    let samples = samples();
    let expected: f64 = window(&samples, SAMPLE_INTERVAL_MS, 0)
        .values()
        .map(|v| v[v.len() - 1])
        .sum();
    assert_answers_equal(
        &answers(&plan, &samples, &deployment, "sum"),
        &BTreeMap::from([(String::new(), expected)]),
    );
}

/// `sum_over_time(m[5m])` (exact) with raw data kept: each series' sum of
/// its 300 samples in (T − 5 min, T].
#[test]
fn an_exact_range_sum_per_series_equals_the_window_sums() {
    let deployment = DeploymentCapabilities::default();
    let plan = plan(
        "sum_over_time(m[5m])",
        AccuracyTarget::exact(),
        SERIES as u64,
        &deployment,
    );
    let samples = samples();
    let expected = window(&samples, 300_000, 0)
        .into_iter()
        .map(|(series, values)| (series, values.iter().sum()))
        .collect();
    assert_answers_equal(&answers(&plan, &samples, &deployment, "value"), &expected);
}

/// `sum(sum_over_time(m[5m]))` (exact) with no raw data kept: a chain of
/// 60 s windows of exact sums built at ingestion time, 5 windows merged
/// at query time; the sum of every sample in (T − 5 min, T].
#[test]
fn an_exact_sum_over_chain_windows_equals_the_sum_of_the_window() {
    let deployment = no_raw_data();
    let plan = plan(
        "sum(sum_over_time(m[5m]))",
        AccuracyTarget::exact(),
        SERIES as u64,
        &deployment,
    );
    assert!(
        operator_kinds(&plan).contains(&"SummaryTumblingWindowChain"),
        "{:?}",
        operator_kinds(&plan)
    );
    let samples = samples();
    let expected: f64 = window(&samples, 300_000, 0).values().flatten().sum();
    assert_answers_equal(
        &answers(&plan, &samples, &deployment, "sum"),
        &BTreeMap::from([(String::new(), expected)]),
    );
}

/// `sum(sum_over_time(m[5m] offset 10m))` (exact) with no raw data kept:
/// the 5 windows ending 10 min before T, read through a `TimeShift` over
/// the chain; the sum of every sample in (T − 15 min, T − 10 min].
#[test]
fn an_exact_sum_over_shifted_chain_windows_equals_the_shifted_window_sum() {
    let deployment = no_raw_data();
    let plan = plan(
        "sum(sum_over_time(m[5m] offset 10m))",
        AccuracyTarget::exact(),
        SERIES as u64,
        &deployment,
    );
    let shape = operator_kinds(&plan);
    assert!(
        shape.contains(&"SummaryTumblingWindowChain") && shape.contains(&"TimeShift"),
        "{shape:?}"
    );
    let samples = samples();
    let expected: f64 = window(&samples, 300_000, 600_000).values().flatten().sum();
    assert_answers_equal(
        &answers(&plan, &samples, &deployment, "sum"),
        &BTreeMap::from([(String::new(), expected)]),
    );
}

/// `quantile_over_time(0.9, m[5m])` within rank error ε = 0.05 per value
/// at δ = 0.01, with no raw data kept, declared over 10,000,000 series so
/// that a sketch costs less than exact state: a sketch chain answers each
/// of the 200 series. A value is off when the fraction of the series'
/// 300 window samples at or below it is outside [0.9 − ε, 0.9 + ε]; at
/// most δ × 200 = 2 values may be off.
#[test]
fn an_approximate_quantile_is_within_its_rank_error_for_all_but_delta_of_the_series() {
    let epsilon = 0.05;
    let delta = 0.01;
    let deployment = no_raw_data();
    let plan = plan(
        "quantile_over_time(0.9, m[5m])",
        AccuracyTarget::max_error(epsilon)
            .failure_probability(delta)
            .accuracy_metric(AccuracyMetric::Rank)
            .accuracy_scope(AccuracyScope::PerValue),
        10_000_000,
        &deployment,
    );
    assert!(
        plan.nodes.iter().any(|node| node.parameters.is_some()),
        "the plan builds no sketch: {:?}",
        operator_kinds(&plan)
    );
    let samples = samples();
    let windows = window(&samples, 300_000, 0);
    let answers = answers(&plan, &samples, &deployment, "value");
    assert_eq!(answers.len(), windows.len(), "one answer per series");
    let off = windows
        .iter()
        .filter(|(series, values)| {
            let answer = answers[*series];
            let at_or_below = values.iter().filter(|v| **v <= answer).count() as f64;
            let below = values.iter().filter(|v| **v < answer).count() as f64;
            let n = values.len() as f64;
            // A rank within ε of 0.9 n exists among the answer's ties.
            below / n > 0.9 + epsilon || at_or_below / n < 0.9 - epsilon
        })
        .count();
    assert!(
        off as f64 <= delta * windows.len() as f64,
        "{off} of {} values are off by more than rank error {epsilon}",
        windows.len()
    );
}

/// `max_over_time(m[5m])` (exact) with no raw data kept: per series, the
/// largest of its 300 samples in (T − 5 min, T], from exact max states of
/// 60 s windows.
#[test]
fn an_exact_max_over_chain_windows_equals_the_window_maximum() {
    let deployment = no_raw_data();
    let plan = plan(
        "max_over_time(m[5m])",
        AccuracyTarget::exact(),
        SERIES as u64,
        &deployment,
    );
    assert!(
        operator_kinds(&plan).contains(&"SummaryTumblingWindowChain"),
        "{:?}",
        operator_kinds(&plan)
    );
    let samples = samples();
    let expected = window(&samples, 300_000, 0)
        .into_iter()
        .map(|(series, values)| (series, values.into_iter().fold(f64::MIN, f64::max)))
        .collect();
    assert_answers_equal(&answers(&plan, &samples, &deployment, "value"), &expected);
}

/// `quantile_over_time(0.9, m[5m])` within relative error ε = 0.05 per
/// value at δ = 0.01, no raw data kept, declared over 10,000,000 series: a
/// sketch with a relative guarantee answers each of the 200 series. A
/// value is off when it is outside [(1 − ε) x_lo, (1 + ε) x_hi], x_lo and
/// x_hi the window's sorted samples at ranks ⌊0.9 (n − 1)⌋ and ⌈0.9 (n −
/// 1)⌉ (PromQL interpolates between them); at most δ × 200 = 2 values may
/// be off.
#[test]
fn an_approximate_quantile_is_within_its_relative_error_for_all_but_delta_of_the_series() {
    let epsilon = 0.05;
    let delta = 0.01;
    let deployment = no_raw_data();
    let plan = plan(
        "quantile_over_time(0.9, m[5m])",
        AccuracyTarget::max_error(epsilon)
            .failure_probability(delta)
            .accuracy_metric(AccuracyMetric::Relative)
            .accuracy_scope(AccuracyScope::PerValue),
        10_000_000,
        &deployment,
    );
    assert!(
        plan.nodes.iter().any(|node| node.parameters.is_some()),
        "the plan builds no sketch: {:?}",
        operator_kinds(&plan)
    );
    let samples = samples();
    let windows = window(&samples, 300_000, 0);
    let answers = answers(&plan, &samples, &deployment, "value");
    assert_eq!(answers.len(), windows.len(), "one answer per series");
    let off = windows
        .iter()
        .filter(|(series, values)| {
            let mut sorted = (*values).clone();
            sorted.sort_by(f64::total_cmp);
            let position = 0.9 * (sorted.len() - 1) as f64;
            let (low, high) = (
                sorted[position.floor() as usize],
                sorted[position.ceil() as usize],
            );
            let answer = answers[*series];
            answer < (1.0 - epsilon) * low || answer > (1.0 + epsilon) * high
        })
        .count();
    assert!(
        off as f64 <= delta * windows.len() as f64,
        "{off} of {} values are off by more than relative error {epsilon}",
        windows.len()
    );
}

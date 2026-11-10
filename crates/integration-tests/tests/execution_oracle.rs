//! The execution oracle (`docs/develop_docs/testing.md`, "Oracles"): the
//! plan Stage 3 selects is run by the executor over synthetic data, as a
//! runtime runs it (every materialized view refreshed over its windows,
//! then the query run), and its answers are compared with an exact
//! evaluation of the query written here over the same samples. Exact
//! queries must give equal answers; an approximate query's answers may be
//! off by more than ε for at most a fraction δ of its values, checked over
//! every series' value with a fixed seed.

use std::collections::BTreeMap;
use std::sync::Arc;

use asap_devtools::corpus_registry::{plan_workload, repeating_promql_workload};
use asap_devtools::planning_stages::RunUntil;
use asap_executor::physical_planner::compile;
use asap_executor::runtime::{Limits, RunContext, Scope};
use asap_executor::sources::InMemoryDataStore;
use asap_executor::values::{Batch, Value};
use asap_types::deployment_capabilities::DeploymentCapabilities;
use asap_types::ir::materialized_view::MaterializedViewRefresh;
use asap_types::ir::operator::Source;
use asap_types::ir::physical_export::{PhysicalASAPDAG, PhysicalASAPOperatorPayload};
use asap_types::ir::schema::{DataType, FieldDataType, Schema, PROMQL_SERIES_IDENTITY};
use asap_types::ir::{ASAPOp, NonASAPOp};
use futures::{executor::block_on, StreamExt};
use serde_json::{json, Value as Json};

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

/// One raw data sample: its series' labels, time and value.
#[derive(Debug, Clone)]
struct Sample {
    labels: BTreeMap<String, String>,
    timestamp_ms: i64,
    value: f64,
}

/// A xorshift64* generator: the samples depend only on [`SEED`].
struct Rng(u64);

impl Rng {
    /// A value uniform in [0, 100).
    fn next_value(&mut self) -> f64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        let bits = self.0.wrapping_mul(0x2545_F491_4F6C_DD1D) >> 11;
        bits as f64 / (1u64 << 53) as f64 * 100.0
    }
}

/// [`SERIES`] series of `m`, a sample every second in [0, EVALUATION_MS],
/// values uniform in [0, 100) (a non-negative gauge).
fn samples() -> Vec<Sample> {
    let mut rng = Rng(SEED);
    let mut samples = Vec::new();
    for series in 0..SERIES {
        let labels = BTreeMap::from([
            ("__name__".to_string(), "m".to_string()),
            ("instance".to_string(), format!("i{series}")),
        ]);
        for step in 0..=(EVALUATION_MS / SAMPLE_INTERVAL_MS) {
            samples.push(Sample {
                labels: labels.clone(),
                timestamp_ms: step * SAMPLE_INTERVAL_MS,
                value: rng.next_value(),
            });
        }
    }
    samples
}

/// Each series' sample values with times in `(EVALUATION_MS - offset -
/// range, EVALUATION_MS - offset]`, by its `instance`: PromQL's range
/// selector `m[range] offset offset`.
fn window(samples: &[Sample], range_ms: i64, offset_ms: i64) -> BTreeMap<String, Vec<f64>> {
    let end = EVALUATION_MS - offset_ms;
    let mut windows: BTreeMap<String, Vec<f64>> = BTreeMap::new();
    for sample in samples {
        if sample.timestamp_ms > end - range_ms && sample.timestamp_ms <= end {
            windows
                .entry(sample.labels["instance"].clone())
                .or_default()
                .push(sample.value);
        }
    }
    windows
}

/// The plan Stage 3 selects for `query` every 60 s at `accuracy` over `m`
/// declared with `series_count` series sampled every second, on
/// `capabilities`.
fn plan(
    query: &str,
    accuracy: Json,
    series_count: u64,
    capabilities: &DeploymentCapabilities,
) -> PhysicalASAPDAG {
    let workload = repeating_promql_workload(
        &[query],
        60_000,
        accuracy,
        &[(
            "m",
            json!({"series_count": series_count, "sample_interval": SAMPLE_INTERVAL_MS, "metric_type": "gauge", "column_signs": {"value": "non_negative"}}),
        )],
    )
    .unwrap();
    let planned = plan_workload(
        &workload,
        &[],
        capabilities,
        &asap_cost::CostInputs::default(),
        false,
        RunUntil::Stage3,
    )
    .unwrap();
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
            PROMQL_SERIES_IDENTITY => {
                Value::Utf8(serde_json::to_string(&sample.labels).unwrap().into())
            }
            _ if Some(index) == schema.time_index => Value::Timestamp(sample.timestamp_ms),
            "value" if field.dtype == FieldDataType::Plain(DataType::Float64) => {
                Value::Float64(sample.value)
            }
            label => Value::Utf8(sample.labels.get(label).cloned().unwrap_or_default().into()),
        })
        .collect()
}

/// One stored state of a view and the `(start, end]` of the data it holds.
type ViewState = (Batch, (i64, i64));

/// Runs `ids` of `dag` in `scope`, the raw samples bound at its `Scan`s of
/// `m` and each view of `views` bound to its stored states: each id's
/// output rows.
fn run(
    dag: &PhysicalASAPDAG,
    samples: &[Sample],
    views: &BTreeMap<u64, Vec<ViewState>>,
    ids: &[u64],
    scope: Scope,
) -> Vec<(Arc<Schema>, Vec<Vec<Value>>)> {
    let bound: Vec<_> = dag
        .materialized_views()
        .unwrap()
        .into_iter()
        .filter(|view| views.contains_key(&(view.node_id as u64)))
        .collect();
    let compiled = compile(dag, &bound, ids).expect("the executor compiles the plan");
    let schema = dag.nodes.iter().find_map(|node| match &node.payload {
        PhysicalASAPOperatorPayload::NonASAP(NonASAPOp::Scan { schema, .. }) => {
            Some(Arc::new(schema.clone()))
        }
        _ => None,
    });
    let mut store = InMemoryDataStore::default();
    if let Some(schema) = schema {
        let rows = samples.iter().map(|sample| row(&schema, sample)).collect();
        store = store
            .with_raw_data(
                Source::TimeSeries { metric: "m".into() },
                schema.clone(),
                vec![Batch::try_new(schema, rows).unwrap()],
            )
            .unwrap();
    }
    for view in &bound {
        for (batch, span) in &views[&(view.node_id as u64)] {
            store
                .append_materialized_view(view, *span, vec![batch.clone()])
                .unwrap();
        }
    }
    let instance = compiled.instantiate(Arc::new(store)).unwrap();
    block_on(async {
        // The raw data is one 360,000-row batch, above the default 64 MiB.
        let limits = Limits {
            max_bytes: 1 << 30,
            ..Limits::default()
        };
        let context = RunContext::new(scope, limits).unwrap();
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
    .into_iter()
    .zip(ids)
    .map(|(batches, id)| {
        let schema = Arc::new(dag.nodes[*id as usize].output_schema.clone());
        (schema, batches.iter().flat_map(|b| b.to_rows()).collect())
    })
    .collect()
}

/// The plan's answer at [`EVALUATION_MS`] as a runtime computes it: every
/// materialized view refreshed in plan order (a chain once per window its
/// retention keeps, newest ending at the evaluation time, in an ingestion
/// run over that window), then the query run reading them. Each answer
/// row's value by its series' `instance` (`""` for an aggregate over every
/// series).
fn answers(dag: &PhysicalASAPDAG, samples: &[Sample]) -> BTreeMap<String, f64> {
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
            let end = EVALUATION_MS - k * width_ms;
            let span = (end - width_ms, end);
            let scope = match view.refresh() {
                MaterializedViewRefresh::AtIngestion => Scope::Ingestion {
                    window_start_ms: span.0,
                    window_end_ms: span.1,
                },
                MaterializedViewRefresh::OnFirstRead => Scope::Query {
                    evaluation_time_ms: EVALUATION_MS,
                },
            };
            // An ingestion run is bound the samples of its window and of the
            // windows beside it (binding all of them only slows the run).
            let near: Vec<Sample> = samples
                .iter()
                .filter(|s| {
                    s.timestamp_ms > span.0 - width_ms && s.timestamp_ms <= span.1 + width_ms
                })
                .cloned()
                .collect();
            let (schema, rows) = run(dag, &near, &built, &[view.node_id as u64], scope).remove(0);
            states.push((Batch::try_new(schema, rows).unwrap(), span));
        }
        built.insert(view.node_id as u64, states);
    }
    let roots: Vec<u64> = dag.roots.iter().map(|&root| root as u64).collect();
    let query = Scope::Query {
        evaluation_time_ms: EVALUATION_MS,
    };
    let (schema, rows) = run(dag, samples, &built, &roots, query).remove(0);
    rows.iter()
        .map(|row| {
            let mut instance = String::new();
            let mut value = None;
            for (field, cell) in schema.fields.iter().zip(row) {
                match cell {
                    Value::Utf8(text) if field.name == PROMQL_SERIES_IDENTITY => {
                        let labels: BTreeMap<String, String> = serde_json::from_str(text).unwrap();
                        instance = labels.get("instance").cloned().unwrap_or_default();
                    }
                    Value::Utf8(text) if field.name == "instance" => instance = text.to_string(),
                    Value::Float64(v) => value = Some(*v),
                    Value::Int64(v) => value = Some(*v as f64),
                    _ => {}
                }
            }
            (instance, value.expect("a numeric answer"))
        })
        .collect()
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

/// The plan's operators, for a failure message and for checking the plan
/// takes the path the test means to execute.
fn kinds(plan: &PhysicalASAPDAG) -> Vec<&'static str> {
    plan.nodes
        .iter()
        .map(|node| node.payload.kind_name())
        .collect()
}

/// `sum(m)` (exact, query-time scan of the latest sample of each series):
/// the sum of the 200 series' samples at the evaluation time.
#[test]
fn an_exact_instant_sum_equals_the_sum_of_the_latest_samples() {
    let plan = plan(
        "sum(m)",
        json!({"max_error": 0.0}),
        SERIES as u64,
        &DeploymentCapabilities::default(),
    );
    let samples = samples();
    let expected: f64 = window(&samples, SAMPLE_INTERVAL_MS, 0)
        .values()
        .map(|v| v[v.len() - 1])
        .sum();
    assert_answers_equal(
        &answers(&plan, &samples),
        &BTreeMap::from([(String::new(), expected)]),
    );
}

/// `sum_over_time(m[5m])` (exact) with raw data kept: each series' sum of
/// its 300 samples in (T − 5 min, T].
#[test]
fn an_exact_range_sum_per_series_equals_the_window_sums() {
    let plan = plan(
        "sum_over_time(m[5m])",
        json!({"max_error": 0.0}),
        SERIES as u64,
        &DeploymentCapabilities::default(),
    );
    let samples = samples();
    let expected = window(&samples, 300_000, 0)
        .into_iter()
        .map(|(series, values)| (series, values.iter().sum()))
        .collect();
    assert_answers_equal(&answers(&plan, &samples), &expected);
}

/// `sum(sum_over_time(m[5m]))` (exact) with no raw data kept: a chain of
/// 60 s windows of exact sums built at ingestion time, 5 windows merged
/// at query time; the sum of every sample in (T − 5 min, T].
#[test]
fn an_exact_sum_over_chain_windows_equals_the_sum_of_the_window() {
    let plan = plan(
        "sum(sum_over_time(m[5m]))",
        json!({"max_error": 0.0}),
        SERIES as u64,
        &no_raw_data(),
    );
    assert!(
        kinds(&plan).contains(&"SummaryTumblingWindowChain"),
        "{:?}",
        kinds(&plan)
    );
    let samples = samples();
    let expected: f64 = window(&samples, 300_000, 0).values().flatten().sum();
    assert_answers_equal(
        &answers(&plan, &samples),
        &BTreeMap::from([(String::new(), expected)]),
    );
}

/// `sum(sum_over_time(m[5m] offset 10m))` (exact) with no raw data kept:
/// the 5 windows ending 10 min before T, read through a `TimeShift` over
/// the chain; the sum of every sample in (T − 15 min, T − 10 min].
#[test]
fn an_exact_sum_over_shifted_chain_windows_equals_the_shifted_window_sum() {
    let plan = plan(
        "sum(sum_over_time(m[5m] offset 10m))",
        json!({"max_error": 0.0}),
        SERIES as u64,
        &no_raw_data(),
    );
    let shape = kinds(&plan);
    assert!(
        shape.contains(&"SummaryTumblingWindowChain") && shape.contains(&"TimeShift"),
        "{shape:?}"
    );
    let samples = samples();
    let expected: f64 = window(&samples, 300_000, 600_000).values().flatten().sum();
    assert_answers_equal(
        &answers(&plan, &samples),
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
    let plan = plan(
        "quantile_over_time(0.9, m[5m])",
        json!({"max_error": epsilon, "failure_probability": delta, "accuracy_metric": "rank", "accuracy_scope": "per_value"}),
        10_000_000,
        &no_raw_data(),
    );
    assert!(
        plan.nodes.iter().any(|node| node.parameters.is_some()),
        "the plan builds no sketch: {:?}",
        kinds(&plan)
    );
    let samples = samples();
    let windows = window(&samples, 300_000, 0);
    let answers = answers(&plan, &samples);
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
    let plan = plan(
        "max_over_time(m[5m])",
        json!({"max_error": 0.0}),
        SERIES as u64,
        &no_raw_data(),
    );
    assert!(
        kinds(&plan).contains(&"SummaryTumblingWindowChain"),
        "{:?}",
        kinds(&plan)
    );
    let samples = samples();
    let expected = window(&samples, 300_000, 0)
        .into_iter()
        .map(|(series, values)| (series, values.into_iter().fold(f64::MIN, f64::max)))
        .collect();
    assert_answers_equal(&answers(&plan, &samples), &expected);
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
    let plan = plan(
        "quantile_over_time(0.9, m[5m])",
        json!({"max_error": epsilon, "failure_probability": delta, "accuracy_metric": "relative", "accuracy_scope": "per_value"}),
        10_000_000,
        &no_raw_data(),
    );
    assert!(
        plan.nodes.iter().any(|node| node.parameters.is_some()),
        "the plan builds no sketch: {:?}",
        kinds(&plan)
    );
    let samples = samples();
    let windows = window(&samples, 300_000, 0);
    let answers = answers(&plan, &samples);
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

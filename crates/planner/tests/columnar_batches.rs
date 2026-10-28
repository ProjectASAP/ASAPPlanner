//! A runtime's samples reach the executor as Arrow columns (issue #762):
//! the planner-layering #509 Example 4b plan, one pane built by the
//! executor from a time series' raw data whose batch the runtime built as
//! Arrow columns, each series' identity stored once, through the public
//! batch API only.

use asap_executor::sources::InMemoryDataStore;
use std::collections::BTreeMap;
use std::sync::Arc;

use arrow::array::{DictionaryArray, Float64Array, TimestampMillisecondArray};
use arrow::datatypes::UInt32Type;
use asap_executor::physical_planner::compile;
use asap_executor::planner::ir::operator::Source;
use asap_executor::planner::ir::physical_export::PhysicalASAPOperatorPayload as Payload;
use asap_executor::planner::ir::schema::PROMQL_SERIES_IDENTITY;
use asap_executor::planner::ir::NonASAPOp;
use asap_executor::runtime::{Limits, RunContext, Scope};
use asap_executor::values::{Batch, BatchColumn};
use asap_planner::{
    plan_installable, DeploymentCapabilities, FrontendInput, PlanningModels, PlanningWorkload,
    UserInput,
};
use futures::StreamExt;

mod time_series;
use time_series::{series_identity, RawDataSample};

/// Example 4b's evaluation time: the end of the 60-minute window.
const EVALUATION_MS: i64 = 3_600_000;

#[tokio::test]
async fn a_pane_is_built_from_arrow_columns_with_one_label_entry_per_series() {
    // What the user writes: p99 of `latency_ms` over the last hour, every
    // 10 minutes, at rank error 0.01, over 1,000 series sampled every second.
    let workload: PlanningWorkload = serde_json::from_str(
        r#"{"query_workload": {"language": "promql", "repeating_queries": [
              {"query": "quantile_over_time(0.99, latency_ms[60m])",
               "demand": {"fixed_interval": 600000},
               "requirements": {"accuracy": {"accuracy_metric": "rank", "max_error": 0.01,
                   "failure_probability": 0.01, "accuracy_scope": "whole_result"},
                   "max_latency_ms": 200.0},
               "predictability": {"predictable": {"known_at": null}}}]},
            "data_workload": {"arrival": {"ingesting": {"rows_per_second": 1000.0}},
              "sources": {"latency_ms": {"series_count": 1000, "sample_interval": 1000}}}}"#,
    )
    .unwrap();
    // The deployment keeps no raw data samples, so every pane is built at
    // ingestion and materialized.
    let capabilities = DeploymentCapabilities {
        raw_data_retained: false,
        ..DeploymentCapabilities::default()
    };
    // The installable plan: the selected DAG and its materialized KLL panes.
    // Pane width is chosen by the optimizer.
    let plan = plan_installable(UserInput::new(
        &workload,
        FrontendInput::Promql { histograms: None },
        PlanningModels::builtin().with_deployment_capabilities(&capabilities),
    ))
    .await
    .unwrap()
    .0;
    // The newest pane: its node, and the raw-data window it reads.
    let views = plan.materialized_views().unwrap();
    let output = views
        .iter()
        .max_by_key(|output| output.inputs.raw_data_windows[0].bounds_ms(EVALUATION_MS).1)
        .unwrap();
    // The pane's window at this evaluation, `(start_ms, end_ms]`.
    let (start_ms, end_ms) = output.inputs.raw_data_windows[0].bounds_ms(EVALUATION_MS);
    // Feed both series within the selected pane, whose width is an optimizer
    // decision. Keep distinct millisecond timestamps even for very fine panes.
    let width_ms = end_ms - start_ms;
    assert!(width_ms > 0);
    let samples_per_series = width_ms.min(60);
    let expected_rows = (2 * samples_per_series) as usize;
    let samples: Vec<RawDataSample> = ["a", "b"]
        .iter()
        .flat_map(|instance| {
            (1..=samples_per_series).map(move |step| RawDataSample {
                labels: BTreeMap::from([
                    ("__name__".to_string(), "latency_ms".to_string()),
                    ("instance".to_string(), instance.to_string()),
                ]),
                timestamp_ms: start_ms + step * width_ms / samples_per_series,
                value: step as f64,
            })
        })
        .collect();
    // The rows the plan's `Scan` of `latency_ms` reads.
    let schema = plan
        .dag
        .nodes
        .iter()
        .find_map(|node| match &node.payload {
            Payload::NonASAP(NonASAPOp::Scan { schema, .. }) => Some(Arc::new(schema.clone())),
            _ => None,
        })
        .unwrap();
    // The runtime's batch of both series' samples as Arrow columns: the
    // series identity and each label a dictionary holding each series'
    // value once, one u32 key per sample.
    let columns = schema
        .fields
        .iter()
        .enumerate()
        .map(|(index, field)| {
            if Some(index) == schema.time_index {
                BatchColumn::Plain(Arc::new(TimestampMillisecondArray::from_iter_values(
                    samples.iter().map(|s| s.timestamp_ms),
                )))
            } else if field.name == "value" {
                BatchColumn::Plain(Arc::new(Float64Array::from_iter_values(
                    samples.iter().map(|s| s.value),
                )))
            } else {
                let cells: Vec<String> = samples
                    .iter()
                    .map(|s| match field.name.as_str() {
                        PROMQL_SERIES_IDENTITY => series_identity(&s.labels),
                        label => s.labels.get(label).cloned().unwrap_or_default(),
                    })
                    .collect();
                let texts: DictionaryArray<UInt32Type> = cells.iter().map(String::as_str).collect();
                assert!(texts.values().len() <= 2, "one entry per series");
                BatchColumn::Plain(Arc::new(texts))
            }
        })
        .collect();
    let input = Batch::try_from_columns(schema.clone(), columns).unwrap();
    assert_eq!(input.num_rows(), expected_rows);

    // The executor's pane build over the `latency_ms` raw data bound at
    // the `Scan`; its `TimeRange` keeps the pane's window.
    let compiled = compile(&plan.dag, &[], &[output.node_id as u64]).unwrap();
    let store = InMemoryDataStore::default()
        .with_raw_data(
            Source::TimeSeries {
                metric: "latency_ms".into(),
            },
            schema,
            vec![input],
        )
        .unwrap();
    // The build, ready to run in the pane's ingestion window.
    let build = compiled.instantiate(Arc::new(store)).unwrap();
    let context = RunContext::new(
        Scope::Ingestion {
            window_start_ms: start_ms,
            window_end_ms: end_ms,
        },
        Limits::default(),
    )
    .unwrap();
    // The built pane: one row per series, its KLL in a state column.
    let pane = build
        .execute(compiled.roots(), context)
        .unwrap()
        .remove(0)
        .next()
        .await
        .unwrap()
        .unwrap();
    let state_column = pane
        .schema()
        .fields
        .iter()
        .position(|field| !field.is_plain())
        .unwrap();
    assert_eq!(pane.num_rows(), 2);
    assert_eq!(pane.state_column(state_column).unwrap().len(), 2);
}

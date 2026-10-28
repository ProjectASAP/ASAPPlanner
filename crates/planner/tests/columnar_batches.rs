//! A runtime's samples reach the executor as Arrow columns (issue #762):
//! the planner-layering #509 Example 4b plan, one pane fed through its
//! `RawDataSampleSelector` and built by the executor, read through the public
//! batch API only.

use asap_executor::sources::{BindingDataSources, MemorySource, TableProvider};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use arrow::array::{AsArray, Float64Array};
use arrow::datatypes::UInt32Type;
use asap_executor::physical_planner::compile;
use asap_executor::planner::ir::schema::PROMQL_SERIES_IDENTITY;
use asap_executor::runtime::{Limits, RunContext, Scope};
use asap_executor::values::Batch;
use asap_executor::{RawDataSample, RawDataSampleSelector};
use asap_planner::{
    plan_installable, DeploymentCapabilities, FrontendInput, PlanningModels, PlanningWorkload,
    UserInput,
};
use futures::StreamExt;

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
    // ingestion and stored.
    let capabilities = DeploymentCapabilities {
        raw_data_retained: false,
        ..DeploymentCapabilities::default()
    };
    // The installable plan: the selected DAG and its stored KLL panes.
    // Pane width is chosen by the optimizer.
    let plan = plan_installable(UserInput::new(
        &workload,
        FrontendInput::Promql { histograms: None },
        PlanningModels::builtin().with_deployment_capabilities(&capabilities),
    ))
    .await
    .unwrap();
    // The newest pane: its node, and the raw-data-sample window it reads.
    let output = plan
        .stored_outputs
        .iter()
        .max_by_key(|output| {
            output.inputs.raw_data_sample_windows[0]
                .bounds_ms(EVALUATION_MS)
                .1
        })
        .unwrap();
    // The pane's window at this evaluation, `(start_ms, end_ms]`.
    let (start_ms, end_ms) = output.inputs.raw_data_sample_windows[0].bounds_ms(EVALUATION_MS);
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
    // The pane's Scan and TimeRange, prepared once for the window.
    let selector =
        RawDataSampleSelector::new(&plan.dag.dag, &output.inputs.raw_data_sample_windows[0])
            .unwrap();
    // The pane's input: both series' samples as Arrow columns.
    let input: Batch = selector.window_batch(end_ms, &samples).unwrap();
    assert_eq!(input.num_rows(), expected_rows);
    // The series identity column: one u32 key per sample into a dictionary
    // holding each series' identity once.
    let identity_column = input
        .schema()
        .fields
        .iter()
        .position(|field| field.name == PROMQL_SERIES_IDENTITY)
        .unwrap();
    let identities = input.column(identity_column).as_dictionary::<UInt32Type>();
    assert_eq!(identities.keys().len(), expected_rows);
    assert_eq!(identities.values().len(), 2);
    // The sample values: a plain Float64 array.
    let value_column = input
        .schema()
        .fields
        .iter()
        .position(|field| field.name == "value")
        .unwrap();
    let values: &Float64Array = input.column(value_column).as_primitive();
    assert_eq!(values.value(0), 1.0);

    // The executor's pane build, cut at the pane's TimeRange.
    let window = output.inputs.raw_data_sample_windows[0].time_range_node_id as u64;
    let compiled = compile(
        &plan.dag.dag,
        &BTreeSet::from([window]),
        &[output.node_id as u64],
    )
    .unwrap();
    // The input bound as an in-memory source.
    let source: Arc<dyn TableProvider> =
        Arc::new(MemorySource::new(input.schema().clone(), vec![input]).unwrap());
    // The build, ready to run in the pane's ingestion window.
    let data_sources = BindingDataSources {
        views: BTreeMap::from([(window, source)]),
        ..Default::default()
    };
    let build = compiled.instantiate(&data_sources).unwrap();
    let context = RunContext::new(
        Scope::Ingestion {
            window_start_ms: start_ms,
            window_end_ms: end_ms,
            revision: 1,
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

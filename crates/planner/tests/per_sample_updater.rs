//! A runtime that ingests sample by sample refreshes a materialized pane's states
//! with `StateColumnUpdater`, the code the plan's `SummaryAgg` runs on a
//! batch (issue #762): the same states as the executor's pane build.

use asap_executor::sources::{BindingDataSources, MemorySource, TableProvider};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use asap_executor::physical_planner::compile;
use asap_executor::planner::ir::physical_export::PhysicalASAPOperatorPayload as Payload;
use asap_executor::planner::ir::ASAPOp;
use asap_executor::runtime::{Limits, RunContext, Scope};
use asap_executor::summary_kernels::exact::ExactAccumulator;
use asap_executor::StateColumnUpdater;
use asap_planner::{
    plan_installable, test_support::query_time_reference, DeploymentCapabilities, FrontendInput,
    PlanningModels, PlanningWorkload, UserInput,
};
use futures::StreamExt;

mod time_series;
use time_series::{series_identity, series_labels, time_series_tables, RawDataSample};

/// The evaluation time: the end of an hour.
const EVALUATION_MS: i64 = 3_600_000;

#[tokio::test]
async fn per_sample_updates_build_the_materialized_pane_the_plan_refreshes() {
    // What the user writes: the 5-minute rate of a counter, every minute,
    // exactly, over 1,000 series sampled every 15 s.
    let workload: PlanningWorkload = serde_json::from_str(
        r#"{"query_workload": {"language": "promql", "repeating_queries": [{
              "query": "rate(http_requests_total[5m])",
              "demand": {"fixed_interval": 60000},
              "requirements": {},
              "predictability": {"predictable": {"known_at": null}}}]},
            "data_workload": {"arrival": {"ingesting": {"rows_per_second": 66.7}},
              "sources": {"http_requests_total": {"series_count": 1000, "sample_interval": 15000}}}}"#,
    )
    .unwrap();
    // No raw data samples retained: the plan stores rate panes built at ingestion.
    let capabilities = DeploymentCapabilities {
        raw_data_retained: false,
        ..DeploymentCapabilities::default()
    };
    // The installable plan: the selected DAG and its materialized views.
    let plan = plan_installable(UserInput::new(
        &workload,
        FrontendInput::Promql { histograms: None },
        PlanningModels::builtin().with_deployment_capabilities(&capabilities),
    ))
    .await
    .unwrap()
    .0;
    let dag = &plan.dag;
    // The newest pane view and its window at this evaluation.
    let views = plan.materialized_views().unwrap();
    let output = views
        .iter()
        .max_by_key(|output| output.inputs.raw_data_windows[0].bounds_ms(EVALUATION_MS).1)
        .unwrap();
    let (start_ms, end_ms) = output.inputs.raw_data_windows[0].bounds_ms(EVALUATION_MS);
    // Five one-minute panes, the newest (59 min, 60 min].
    assert_eq!(views.len(), 5);
    assert_eq!((start_ms, end_ms), (3_540_000, 3_600_000));
    // The view node's summary family, as the plan declares it: the
    // `SummaryAgg` at or below the view node.
    let family = {
        let mut id = output.node_id;
        loop {
            let node = dag.nodes.iter().find(|n| n.id == id).unwrap();
            if let Payload::ASAP(ASAPOp::SummaryAgg { family, .. }) = &node.payload {
                break family.clone();
            }
            id = dag
                .edges
                .iter()
                .find(|e| e.consumer == id)
                .unwrap()
                .producer;
        }
    };
    // What the runtime received in the pane: 3 series, one counter sample
    // every 15 s, one series with a reset.
    assert_eq!(
        family,
        asap_executor::planner::ir::schema::FieldDataType::ExactAggregate(
            asap_executor::planner::ir::schema::ExactKind::Rate,
            asap_executor::planner::ir::schema::ExactParams::Rate,
        )
    );
    let samples: Vec<RawDataSample> = ["a", "b", "c"]
        .iter()
        .flat_map(|instance| {
            let mut value = 0.0;
            (1..=(end_ms - start_ms) / 15_000).map(move |step| {
                value = if *instance == "c" && step == 3 {
                    2.0
                } else {
                    value + 10.0
                };
                RawDataSample {
                    labels: BTreeMap::from([
                        ("__name__".to_string(), "http_requests_total".to_string()),
                        ("instance".to_string(), instance.to_string()),
                    ]),
                    timestamp_ms: start_ms + step * 15_000,
                    value,
                }
            })
        })
        .collect();

    // The executor's pane build: the samples bound at the plan's `Scan`,
    // its `TimeRange` keeping the pane's window.
    let compiled = compile(dag, &BTreeSet::new(), &[output.node_id as u64]).unwrap();
    let data_sources = BindingDataSources {
        tables: time_series_tables(dag, &samples),
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
    let pane = build
        .execute(compiled.roots(), context)
        .unwrap()
        .remove(0)
        .next()
        .await
        .unwrap()
        .unwrap();
    // Its states, by series labels.
    let state_column = pane
        .schema()
        .fields
        .iter()
        .position(|field| !field.is_plain())
        .unwrap();
    let from_dag: BTreeMap<BTreeMap<String, String>, String> = pane
        .to_rows()
        .iter()
        .map(|row| {
            let asap_executor::values::Value::Summary { state, .. } = &row[state_column] else {
                panic!("a state")
            };
            let exact = state.as_any().downcast_ref::<ExactAccumulator>().unwrap();
            (
                series_labels(pane.schema(), row),
                serde_json::to_string(exact).unwrap(),
            )
        })
        .collect();

    // The per-sample build: one updater for the pane, a group per series.
    let mut updater = StateColumnUpdater::new(&family).unwrap();
    let mut groups: BTreeMap<BTreeMap<String, String>, u32> = BTreeMap::new();
    for sample in &samples {
        let group = match groups.get(&sample.labels) {
            Some(&group) => group,
            None => {
                let group = updater.add_group().unwrap();
                groups.insert(sample.labels.clone(), group);
                group
            }
        };
        updater
            .update(group, sample.value, sample.timestamp_ms)
            .unwrap();
    }
    // Sealed at pane close: one state per group.
    let states = updater.seal();
    let per_sample: BTreeMap<BTreeMap<String, String>, String> = groups
        .into_iter()
        .map(|(mut labels, group)| {
            let state = states.state(group as usize);
            let exact = state.as_any().downcast_ref::<ExactAccumulator>().unwrap();
            // A rate's series drops its metric name, as the DAG's does.
            if !from_dag.keys().any(|l| l.contains_key("__name__")) {
                labels.remove("__name__");
            }
            (labels, serde_json::to_string(exact).unwrap())
        })
        .collect();
    assert_eq!(from_dag.len(), 3);
    assert_eq!(per_sample, from_dag);
}

/// Run `ids` of `dag` with the view states `inputs` bound at their nodes
/// and the raw `samples` at its `Scan`s, in `scope`: each id's answer rows
/// as (series labels, value), sorted.
async fn answers(
    dag: &asap_planner::PhysicalASAPDAG,
    inputs: BTreeMap<u64, asap_executor::values::Batch>,
    samples: &[RawDataSample],
    ids: &[u64],
    scope: Scope,
) -> Vec<Vec<(BTreeMap<String, String>, f64)>> {
    // Which nodes are bound view states; each one's schema is its DAG node's.
    let boundaries: BTreeSet<u64> = inputs.keys().copied().collect();
    // The executor's plan for `ids`, cut at the bound inputs.
    let compiled = compile(dag, &boundaries, ids).unwrap();
    // Each input as an in-memory source.
    let sources = inputs
        .into_iter()
        .map(|(id, batch)| {
            let source = MemorySource::new(batch.schema().clone(), vec![batch]).unwrap();
            (id, Arc::new(source) as Arc<dyn TableProvider>)
        })
        .collect();
    let data_sources = BindingDataSources {
        tables: time_series_tables(dag, samples),
        materialized_views: sources,
    };
    let instance = compiled.instantiate(&data_sources).unwrap();
    let context = RunContext::new(scope, Limits::default()).unwrap();
    let mut results = Vec::new();
    for mut output in instance.execute(compiled.roots(), context).unwrap() {
        let mut rows = Vec::new();
        while let Some(batch) = output.next().await {
            let batch = batch.unwrap();
            for row in &batch.to_rows() {
                let labels = series_labels(batch.schema(), row);
                let value = row
                    .iter()
                    .find_map(|v| match v {
                        asap_executor::values::Value::Float64(v) => Some(*v),
                        _ => None,
                    })
                    .unwrap();
                rows.push((labels, value));
            }
        }
        rows.sort_by(|a, b| a.0.cmp(&b.0));
        results.push(rows);
    }
    results
}

/// A runtime builds every materialized pane sample by sample, installs each as a
/// batch with `Batch::try_from_columns` (no DAG run), and answers the query
/// from them: the same answers as the plan's query-time reference over the
/// raw data samples.
#[tokio::test]
async fn panes_installed_from_per_sample_states_answer_the_query() {
    use asap_executor::arrow::array::{DictionaryArray, TimestampMillisecondArray, UInt32Array};
    use asap_executor::arrow::datatypes::UInt32Type;
    use asap_executor::values::{Batch, BatchColumn};
    use std::sync::Arc;
    // What the user writes: the 5-minute rate of a counter, every minute,
    // exactly, over 1,000 series sampled every 15 s.
    let workload: PlanningWorkload = serde_json::from_str(
        r#"{"query_workload": {"language": "promql", "repeating_queries": [{
              "query": "rate(http_requests_total[5m])",
              "demand": {"fixed_interval": 60000},
              "requirements": {},
              "predictability": {"predictable": {"known_at": null}}}]},
            "data_workload": {"arrival": {"ingesting": {"rows_per_second": 66.7}},
              "sources": {"http_requests_total": {"series_count": 1000, "sample_interval": 15000}}}}"#,
    )
    .unwrap();
    // No raw data samples retained: five one-minute rate panes built at ingestion.
    let capabilities = DeploymentCapabilities {
        raw_data_retained: false,
        ..DeploymentCapabilities::default()
    };
    let plan = plan_installable(UserInput::new(
        &workload,
        FrontendInput::Promql { histograms: None },
        PlanningModels::builtin().with_deployment_capabilities(&capabilities),
    ))
    .await
    .unwrap()
    .0;
    let dag = &plan.dag;
    // What the runtime received: 3 series, a counter sample every 15 s over
    // the hour (one series resets), the last at the evaluation time.
    let samples: Vec<RawDataSample> = ["a", "b", "c"]
        .iter()
        .flat_map(|instance| {
            let mut value = 0.0;
            (1..=EVALUATION_MS / 15_000).map(move |step| {
                value = if *instance == "c" && step == 230 {
                    3.0
                } else {
                    value + 7.0
                };
                RawDataSample {
                    labels: BTreeMap::from([
                        ("__name__".to_string(), "http_requests_total".to_string()),
                        ("instance".to_string(), instance.to_string()),
                    ]),
                    timestamp_ms: step * 15_000,
                    value,
                }
            })
        })
        .collect();
    // Every materialized pane, built sample by sample and installed as a batch.
    let mut panes = BTreeMap::new();
    for output in &plan.materialized_views().unwrap() {
        // The pane's window, `(start_ms, end_ms]`, at this evaluation.
        let (start_ms, end_ms) = output.inputs.raw_data_windows[0].bounds_ms(EVALUATION_MS);
        // The view node's family and output schema, from the plan.
        let node = dag.nodes.iter().find(|n| n.id == output.node_id).unwrap();
        let mut id = output.node_id;
        let family = loop {
            let node = dag.nodes.iter().find(|n| n.id == id).unwrap();
            if let Payload::ASAP(ASAPOp::SummaryAgg { family, .. }) = &node.payload {
                break family.clone();
            }
            id = dag
                .edges
                .iter()
                .find(|e| e.consumer == id)
                .unwrap()
                .producer;
        };
        // The runtime's per-sample build: a group per series ...
        let mut updater = StateColumnUpdater::new(&family).unwrap();
        let mut identities: Vec<String> = Vec::new();
        let mut group_of_series: BTreeMap<BTreeMap<String, String>, u32> = BTreeMap::new();
        for sample in samples
            .iter()
            .filter(|s| start_ms < s.timestamp_ms && s.timestamp_ms <= end_ms)
        {
            let group = match group_of_series.get(&sample.labels) {
                Some(&group) => group,
                None => {
                    let group = updater.add_group().unwrap();
                    group_of_series.insert(sample.labels.clone(), group);
                    identities.push(series_identity(&sample.labels));
                    group
                }
            };
            updater
                .update(group, sample.value, sample.timestamp_ms)
                .unwrap();
        }
        // ... sealed at pane close: one rate record per series.
        let states = Arc::new(updater.seal());
        // The pane's columns, in the view node's schema: the pane end as
        // its time, each series' state, and its identity.
        let schema = Arc::new(node.output_schema.clone());
        let groups = identities.len();
        let columns = schema
            .fields
            .iter()
            .enumerate()
            .map(|(index, field)| {
                if Some(index) == schema.time_index {
                    BatchColumn::Plain(Arc::new(TimestampMillisecondArray::from(vec![
                        end_ms;
                        groups
                    ])))
                } else if !field.is_plain() {
                    BatchColumn::States {
                        indices: UInt32Array::from_iter_values(0..groups as u32),
                        states: states.clone(),
                    }
                } else {
                    BatchColumn::Plain(Arc::new(
                        identities
                            .iter()
                            .map(String::as_str)
                            .collect::<DictionaryArray<UInt32Type>>(),
                    ))
                }
            })
            .collect();
        // Installed without running the DAG.
        panes.insert(
            output.node_id as u64,
            Batch::try_from_columns(schema, columns).unwrap(),
        );
    }
    assert_eq!(panes.len(), 5);
    let query = Scope::Query {
        evaluation_time_ms: EVALUATION_MS,
        revision: 1,
    };
    let roots: Vec<u64> = plan.dag.roots.iter().map(|&r| r as u64).collect();
    // The query's answers from the installed panes.
    let from_panes = answers(dag, panes, &samples, &roots, query.clone()).await;
    // The plan's query-time reference over the raw data samples.
    let reference = query_time_reference(&plan).unwrap();
    let reference_roots: Vec<u64> = reference.dag.roots.iter().map(|&r| r as u64).collect();
    let expected = answers(
        &reference.dag,
        BTreeMap::new(),
        &samples,
        &reference_roots,
        query,
    )
    .await;
    // One rate per series, equal to the reference.
    assert_eq!(from_panes[0].len(), 3);
    assert_eq!(from_panes, expected);
}

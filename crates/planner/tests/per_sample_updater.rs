//! A runtime that ingests sample by sample builds a stored pane's states
//! with `StateColumnUpdater`, the code the plan's `SummaryAgg` runs on a
//! batch (issue #762): the same states as the executor's pane build.

use std::collections::BTreeMap;

use asap_executor::operators::Operator as ExecutorOperator;
use asap_executor::physical_planner::{compile, InputContract, Source};
use asap_executor::planner::ir::physical_export::PhysicalASAPOperatorPayload as Payload;
use asap_executor::planner::ir::ASAPOp;
use asap_executor::runtime::{Limits, RunContext, Scope};
use asap_executor::summary_kernels::exact::ExactAccumulator;
use asap_executor::{series_labels_of_output_row, RawSampleSelector, Sample, StateColumnUpdater};
use asap_planner::{
    plan_installable, DeploymentCapabilities, FrontendInput, PlanningModels, PlanningWorkload,
    UserInput,
};
use futures::StreamExt;

/// The evaluation time: the end of an hour.
const EVALUATION_MS: i64 = 3_600_000;

#[tokio::test]
async fn per_sample_updates_build_the_stored_pane_the_plan_builds() {
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
    // No raw samples retained: the plan stores rate panes built at ingestion.
    let capabilities = DeploymentCapabilities {
        raw_data_retained: false,
        ..DeploymentCapabilities::default()
    };
    // The installable plan: the selected DAG and its stored outputs.
    let plan = plan_installable(UserInput::new(
        &workload,
        FrontendInput::Promql { histograms: None },
        PlanningModels::builtin().with_deployment_capabilities(&capabilities),
    ))
    .await
    .unwrap();
    let dag = &plan.dag.dag;
    // The newest stored pane and its window at this evaluation.
    let output = plan
        .stored_outputs
        .iter()
        .max_by_key(|output| output.pane.bounds_ms(EVALUATION_MS).1)
        .unwrap();
    let (start_ms, end_ms) = output.pane.bounds_ms(EVALUATION_MS);
    // Five one-minute panes, the newest (59 min, 60 min].
    assert_eq!(plan.stored_outputs.len(), 5);
    assert_eq!((start_ms, end_ms), (3_540_000, 3_600_000));
    // The stored node's summary family, as the plan declares it: the
    // `SummaryAgg` at or below the stored node.
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
    let samples: Vec<Sample> = ["a", "b", "c"]
        .iter()
        .flat_map(|instance| {
            let mut value = 0.0;
            (1..=(end_ms - start_ms) / 15_000).map(move |step| {
                value = if *instance == "c" && step == 3 {
                    2.0
                } else {
                    value + 10.0
                };
                Sample {
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

    // The executor's pane build: the pane's input batch through the plan.
    let selector = RawSampleSelector::new(dag, &output.pane).unwrap();
    let input = selector.window_batch(end_ms, &samples).unwrap();
    let window = output.pane.time_range_node_id as u64;
    let compiled = compile(
        dag,
        BTreeMap::from([(window, InputContract::bounded(input.schema().clone()))]),
        &[output.node_id as u64],
    )
    .unwrap();
    let source: Source<'_> =
        Box::new(ExecutorOperator::source(input.schema().clone(), vec![input]).unwrap());
    let build = compiled
        .instantiate(BTreeMap::from([(window, source)]))
        .unwrap();
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
        .rows()
        .iter()
        .map(|row| {
            let asap_executor::values::Value::Summary { state, .. } = &row[state_column] else {
                panic!("a state")
            };
            let exact = state.as_any().downcast_ref::<ExactAccumulator>().unwrap();
            (
                series_labels_of_output_row(pane.schema(), row).unwrap(),
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

//! A deployment runtime installs and runs the Planner's plan with public
//! APIs only: `asap_planner` to plan, `asap_executor` to execute (issue
//! #749). The executor links here without its `test-support` feature, so
//! the compiler checks that no internal is used (CI runs
//! `cargo check -p asap-planner --tests --locked` on its own for that).

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use crate::time_series;
use asap_executor::physical_planner::compile;
use asap_executor::runtime::{Limits, RunContext, Scope};
use asap_executor::values::{Batch, Value};
use asap_logical_asap_optimization::pass1::per_computation_candidates::HowToComputeAggregateMeasure;
use asap_planner::{
    plan_installable, run_planning, CostInputs, DeploymentCapabilities, FrontendInput,
    InstallablePlan, InstallablePlanError, MaterializedView, MaterializedViewRefresh, OperatorKind,
    OptimizationInput, OptimizationPass, OptimizeError, PhysicalASAPDAG, PlanOutput,
    PlanSearchSettings, PlanningModels, PlanningWorkload, SelectedPlanReport, SelectionBox,
    Stage1ToStage3Pass, UserInput,
};
use asap_types::ir::schema::schema_support::with_promql_series_identity;
use asap_types::ir::schema::{FieldDataType, SketchAlgorithm, SketchParams};
use asap_types::ir::{
    apply_materialization_timings_to_roots, split_shared_by_timing, ASAPOp,
    MaterializationAssignment, OperatorNode, QueryRoot,
};
use asap_types::physical::{Materialization, Medium};
use asap_types::stages::PruneReason;
use futures::{executor::block_on, StreamExt};
use std::collections::HashMap;
use std::ops::Bound;
use std::rc::Rc;
use time_series::{series_labels, time_series_tables, RawDataSample};

/// Example 4b's evaluation time: the end of the 60-minute window.
const EVALUATION_MS: i64 = 3_600_000;

/// Repeating PromQL `queries`, each every 10 minutes at rank error 0.01 and
/// δ 0.01, over 1,000 ingesting `latency_ms` series sampled every second:
/// #509 Example 4b when the one query is its p99.
fn workload(queries: &[&str]) -> PlanningWorkload {
    let entries: Vec<String> = queries
        .iter()
        .map(|query| {
            format!(
                r#"{{"query": {query:?}, "demand": {{"fixed_interval": 600000}},
                  "requirements": {{"accuracy": {{"accuracy_metric": "rank", "max_error": 0.01,
                      "failure_probability": 0.01, "accuracy_scope": "whole_result"}},
                      "max_latency_ms": 200.0}},
                  "predictability": {{"predictable": {{"known_at": null}}}}}}"#
            )
        })
        .collect();
    serde_json::from_str(&format!(
        r#"{{"query_workload": {{"language": "promql", "repeating_queries": [{}]}},
            "data_workload": {{"arrival": {{"ingesting": {{"rows_per_second": 1000.0}}}},
              "sources": {{"latency_ms": {{"series_count": 1000, "sample_interval": 1000}}}}}}}}"#,
        entries.join(",")
    ))
    .unwrap()
}

/// One sample of `metric` for series `instance`.
fn sample(metric: &str, instance: &str, timestamp_ms: i64, value: f64) -> RawDataSample {
    RawDataSample {
        labels: BTreeMap::from([
            ("__name__".to_string(), metric.to_string()),
            ("instance".to_string(), instance.to_string()),
        ]),
        timestamp_ms,
        value,
    }
}

/// Raw `latency_ms` samples of series `a` and `b`, one every 10 s from 70
/// minutes before `EVALUATION_MS` to a minute after it (for a second
/// evaluation), off the pane boundaries, and one
/// sample of another metric the plan never reads. A sample's value says
/// which 10-minute pane before `EVALUATION_MS` it falls in: `1000 × pane +
/// base + 0..=100`, with base 100 for `a` and 300 for `b` (pane 0 is the
/// newest), so an answer read from the wrong panes is visibly off.
fn remote_write_samples() -> Vec<RawDataSample> {
    let mut samples = Vec::new();
    for (instance, base) in [("a", 100.0), ("b", 300.0)] {
        for step in 0..426 {
            let timestamp_ms = EVALUATION_MS - 4_195_000 + step * 10_000;
            let pane = (EVALUATION_MS - timestamp_ms) / 600_000;
            samples.push(sample(
                "latency_ms",
                instance,
                timestamp_ms,
                (1000 * pane) as f64 + base + (step * 37 % 101) as f64,
            ));
        }
    }
    samples.push(sample("cpu_seconds", "a", 5_000, 1e9));
    samples
}

/// Compile `ids` of `dag` over the stored states `views` (by node id) and
/// run them in `scope` over the raw `samples`, as a runtime does with the
/// executor: each id's output batches.
fn run(
    dag: &PhysicalASAPDAG,
    views: BTreeMap<u64, Batch>,
    samples: &[RawDataSample],
    ids: &[u64],
    scope: Scope,
) -> Vec<Vec<Batch>> {
    // The materialized views the run reads; each one's schema is its DAG node's.
    let read: Vec<MaterializedView> = if views.is_empty() {
        Vec::new()
    } else {
        dag.materialized_views()
            .unwrap()
            .into_iter()
            .filter(|view| views.contains_key(&(view.node_id as u64)))
            .collect()
    };
    assert_eq!(read.len(), views.len(), "every bound state is a view's");
    // The executor's plan for `ids`, reading the views; every `Scan` it
    // reaches is a raw data slot.
    let compiled = compile(dag, &read, ids).expect("the executor compiles the plan");
    // The plan's data in one store: raw samples at the `Scan`s by metric,
    // and each view's states by node id.
    let mut store = time_series_tables(dag, samples);
    for (id, batch) in views {
        store = store.with_materialized_view(id as _, vec![batch]).unwrap();
    }
    let instance = compiled.instantiate(Arc::new(store)).unwrap();
    block_on(async {
        // The run's scope (ingestion window or query time) and limits.
        let context = RunContext::new(scope, Limits::default()).unwrap();
        // One output stream per id.
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

/// One answer row: its series labels and its value.
type Answer = (BTreeMap<String, String>, f64);

/// Each answer row as its series labels and value, sorted by labels.
fn answers(batches: &[Batch]) -> Vec<Answer> {
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

/// Each answer is a p99 read from pane `pane` of its series
/// ([`remote_write_samples`]): within `1000 × pane + base + 0..=100`.
fn assert_answers_from_pane(answers: &[Answer], pane: i64) {
    for (labels, value) in answers {
        let base = if labels["instance"] == "a" {
            100.0
        } else {
            300.0
        };
        let low = (1000 * pane) as f64 + base;
        assert!(
            (low..=low + 100.0).contains(value),
            "{labels:?}: {value} is not from pane {pane}"
        );
    }
}

/// The control plane: plan `queries` for a deployment that keeps no raw
/// samples, with its cost model read from JSON, and write the plan file.
async fn plan_json(queries: &[&str]) -> String {
    // What the deployment keeps: no raw data samples, no query-time results
    // across evaluations. Supported summaries are left unrestricted, so the
    // Planner offers only what its own executor runs.
    let capabilities = DeploymentCapabilities {
        raw_data_retained: false,
        ..DeploymentCapabilities::default()
    };
    plan_json_for(workload(queries), &capabilities).await
}

/// [`plan_json`] for `workload` on a deployment with `capabilities`.
async fn plan_json_for(
    workload: PlanningWorkload,
    capabilities: &DeploymentCapabilities,
) -> String {
    serde_json::to_string(&plan_with_report_for(workload, capabilities).await.0).unwrap()
}

/// The plan [`plan_json`] writes for `queries`, and Stage 3's report of the
/// selection it made (which the plan file does not carry).
async fn planned_with_report(queries: &[&str]) -> (InstallablePlan, Option<SelectedPlanReport>) {
    let capabilities = DeploymentCapabilities {
        raw_data_retained: false,
        ..DeploymentCapabilities::default()
    };
    plan_with_report_for(workload(queries), &capabilities).await
}

/// The one Planner call for `workload` on a deployment with `capabilities`:
/// the selected plan, ready to install, and Stage 3's report.
async fn plan_with_report_for(
    workload: PlanningWorkload,
    capabilities: &DeploymentCapabilities,
) -> (InstallablePlan, Option<SelectedPlanReport>) {
    // The deployment's Stage 3 prices, from its JSON file: cost model 1.
    let settings: CostInputs = serde_json::from_str(
        r#"{"cost_model": {"usage_over_time": {
              "cost_per_cpu_op": 1e-6, "cost_per_scanned_byte": 1e-7,
              "cost_per_memory_byte_second": 1.25e-7,
              "cost_per_provisioned_memory_byte_second": 1.25e-7,
              "cost_per_provisioned_cpu_core_second": 1000.0,
              "cost_per_disk_byte_second": 0.0}}}"#,
    )
    .unwrap();
    // The models Stages 1-3 read: built-in accuracy, the deployment inputs.
    let models = PlanningModels::builtin()
        .with_deployment_capabilities(capabilities)
        .with_plan_selection(&settings);
    // The one Planner call: the selected plan, ready to install.
    plan_installable(UserInput::new(
        &workload,
        FrontendInput::Promql { histograms: None },
        models,
    ))
    .await
    .unwrap()
}

/// What one data-plane evaluation of a plan file returned.
struct DataPlaneRun {
    /// Per query, in workload order, the installed plan's answers.
    answers_per_query: Vec<Vec<Answer>>,
    /// Per query, the query-time reference's answers.
    reference_answers_per_query: Vec<Vec<Answer>>,
    /// How many materialized views this evaluation refreshed (ingestion or
    /// query runs); the others it read came from the store.
    outputs_built: usize,
}

/// One summary state this test stores: its refreshed state, and how long
/// to keep it.
struct StoredState {
    /// The refreshed state, bound at every materialized view with its key.
    state: Batch,
    /// When the data it summarizes starts, in Unix milliseconds.
    start_ms: i64,
    /// How far back from the newest evaluation it is kept: the largest
    /// `retention_ms` among the views it served.
    retention_ms: u64,
}

/// This stand-in runtime's summary store, by [`state_key`].
type SummaryStore = BTreeMap<String, StoredState>;

/// The key of `view`'s state at evaluation `evaluation_ms`, as a summary
/// store keys instances: its definition id and its selection made
/// concrete (relative time made absolute). A view whose selection
/// names no time (its definition reads a window relative to the
/// evaluation, e.g. an instant selector) is keyed by the evaluation too.
fn state_key(view: &MaterializedView, evaluation_ms: i64) -> String {
    use std::ops::Bound;
    let absolute = |bound: &Bound<i64>| match bound {
        Bound::Included(at) => Bound::Included(evaluation_ms + at),
        Bound::Excluded(at) => Bound::Excluded(evaluation_ms + at),
        Bound::Unbounded => Bound::Unbounded,
    };
    let boxes = view.coverage().selection();
    let timed = boxes.iter().any(|b| b.relative_time().is_some());
    let selection: Vec<SelectionBox> = boxes
        .iter()
        .map(|selection_box| {
            SelectionBox::new(
                selection_box.columns().clone(),
                selection_box
                    .relative_time()
                    .map(|(lower, upper)| (absolute(&lower), absolute(&upper))),
            )
        })
        .collect();
    serde_json::json!({
        "definition_id": view.coverage().definition_id(),
        "selection": selection,
        "evaluation_ms": (!timed).then_some(evaluation_ms),
    })
    .to_string()
}

/// The node that answers each query of `plan`, in workload order.
fn query_roots(plan: &InstallablePlan) -> Vec<u64> {
    plan.dag.roots.iter().map(|&root| root as u64).collect()
}

/// The data plane at evaluation time `evaluation_ms`: from `plan_json`,
/// `samples` and the states earlier evaluations left in `store`, refresh each
/// materialized view the store lacks once, in the plan's order, from its
/// raw-data windows and the views it reads; answer every query; drop the
/// states past their retention; and answer the queries again with the
/// query-time reference.
fn run_data_plane(
    plan_json: &str,
    samples: &[RawDataSample],
    evaluation_ms: i64,
    store: &mut SummaryStore,
) -> DataPlaneRun {
    run_data_plane_on(
        plan_json,
        samples,
        evaluation_ms,
        store,
        IngestionBuilds::PerEvaluation,
    )
}

/// When this stand-in runtime refreshes a materialized view an ingestion
/// run refreshes from raw data samples.
#[derive(Clone, Copy)]
enum IngestionBuilds {
    /// For each evaluation, over windows ending at that evaluation.
    PerEvaluation,
    /// Tumbling, as a runtime that maintains panes as samples arrive: each
    /// once, over windows ending on the grid of its window's width anchored
    /// at the epoch (pane end ≡ 0 mod width, as the backend data plane
    /// builds them); an evaluation reads the newest one at or before it.
    Tumbling,
}

/// [`run_data_plane`] with at-ingestion views refreshed as `ingestion` says.
fn run_data_plane_on(
    plan_json: &str,
    samples: &[RawDataSample],
    evaluation_ms: i64,
    store: &mut SummaryStore,
    ingestion: IngestionBuilds,
) -> DataPlaneRun {
    // The installed plan, read back from its file and checked.
    let installed: InstallablePlan = serde_json::from_str(plan_json).unwrap();
    installed.validate().unwrap();
    // The DAG the executor runs.
    let dag = &installed.dag;
    let views = installed.materialized_views().unwrap();
    // Every input of the query run, keyed by the node it is bound to.
    let mut inputs = BTreeMap::new();
    // How many materialized views this evaluation refreshes.
    let mut outputs_built = 0;
    // Each view's state and the time span of its data at this
    // evaluation, `(start_ms, end_ms]`, by node id.
    let mut built: BTreeMap<u64, (Batch, (i64, i64))> = BTreeMap::new();
    for view in &views {
        // The evaluation its windows end at: this one, or for a tumbling
        // at-ingestion view the newest point of its grid at or before it.
        let built_for_ms = match (ingestion, view.refresh()) {
            (IngestionBuilds::Tumbling, MaterializedViewRefresh::AtIngestion) => view
                .inputs
                .raw_data_windows()
                .first()
                .map_or(evaluation_ms, |window| {
                    evaluation_ms - evaluation_ms.rem_euclid(window.width_ms())
                }),
            _ => evaluation_ms,
        };
        // The span of the data it summarizes: its windows' and the views'
        // it reads spans together.
        let span = view
            .inputs
            .raw_data_windows()
            .iter()
            .map(|window| window.bounds_ms(built_for_ms))
            .chain(
                view.inputs
                    .materialized_views
                    .iter()
                    .map(|id| built[&(*id as u64)].1),
            )
            .reduce(|(a, b), (c, d)| (a.min(c), b.max(d)))
            .expect("a materialized view reads something");
        let key = state_key(view, built_for_ms);
        if !store.contains_key(&key) {
            // The views its refresh reads; its raw data is bound at its `Scan`s.
            let mut sources = BTreeMap::new();
            for id in &view.inputs.materialized_views {
                sources.insert(*id as u64, built[&(*id as u64)].0.clone());
            }
            // The run that refreshes it: an ingestion run over its data's span
            // once its inputs are complete (each window ends at the run's
            // end), or the query run of this evaluation.
            let scope = match view.refresh() {
                MaterializedViewRefresh::AtIngestion => Scope::Ingestion {
                    window_start_ms: span.0,
                    window_end_ms: span.1,
                },
                MaterializedViewRefresh::OnFirstRead => Scope::Query {
                    evaluation_time_ms: evaluation_ms,
                },
            };
            // The batches that run produced, in one batch, as the store keeps it.
            let batches = run(dag, sources, samples, &[view.node_id as u64], scope).remove(0);
            let schema = batches[0].schema().clone();
            let state_rows = batches.iter().flat_map(|b| b.to_rows().to_vec()).collect();
            store.insert(
                key.clone(),
                StoredState {
                    state: Batch::try_new(schema, state_rows).unwrap(),
                    start_ms: span.0,
                    retention_ms: view.retention_ms(),
                },
            );
            outputs_built += 1;
        }
        // One stored state serves every view with its key, so it is kept
        // for the largest retention any of them requires.
        let stored = store.get_mut(&key).unwrap();
        stored.retention_ms = stored.retention_ms.max(view.retention_ms());
        built.insert(view.node_id as u64, (stored.state.clone(), span));
    }
    // The query run reads the views it reaches from the queries' roots
    // without passing another view; those below are read only through the
    // views refreshed from them.
    let mut pending = query_roots(&installed);
    let mut seen = std::collections::BTreeSet::new();
    while let Some(id) = pending.pop() {
        if !seen.insert(id) {
            continue;
        }
        match built.get(&id) {
            Some((state, _)) => {
                inputs.insert(id, state.clone());
            }
            None => pending.extend(
                dag.edges
                    .iter()
                    .filter(|edge| edge.consumer as u64 == id)
                    .map(|edge| edge.producer as u64),
            ),
        }
    }
    // The query evaluation, which reads the stored states.
    let query = Scope::Query {
        evaluation_time_ms: evaluation_ms,
    };
    // Each query's answers: (series labels, value) per series.
    let answers_per_query = run(
        dag,
        inputs,
        samples,
        &query_roots(&installed),
        query.clone(),
    )
    .iter()
    .map(|batches| answers(batches))
    .collect();
    // The store drops each state whose data starts more than its retention
    // before this evaluation.
    store.retain(|_, state| state.start_ms >= evaluation_ms - state.retention_ms as i64);

    // The correctness reference: nothing stored, every window read from
    // the raw samples at query time.
    let reference = asap_planner::test_support::query_time_reference(&installed).unwrap();
    // The reference's DAG.
    let reference_dag = &reference.dag;
    // The reference's answers, which the installed plan's must equal.
    let reference_answers_per_query = run(
        reference_dag,
        BTreeMap::new(),
        samples,
        &query_roots(&reference),
        query,
    )
    .iter()
    .map(|batches| answers(batches))
    .collect();
    DataPlaneRun {
        answers_per_query,
        reference_answers_per_query,
        outputs_built,
    }
}

/// #509 Example 4b, as a runtime runs it: one `plan_installable` call,
/// `plan.json`, then each pane view refreshed in an ingestion run and merged
/// in the query run. One p99 per series, read from the hour's oldest pane,
/// equal to the query-time reference; the other metric is not read.
#[tokio::test]
async fn a_runtime_installs_and_runs_the_plan_from_one_facade_call() {
    let plan_json = plan_json(&["quantile_over_time(0.99, latency_ms[60m])"]).await;
    let run = run_data_plane(
        &plan_json,
        &remote_write_samples(),
        EVALUATION_MS,
        &mut SummaryStore::new(),
    );
    let installed: InstallablePlan = serde_json::from_str(&plan_json).unwrap();
    let views = installed.materialized_views().unwrap();
    assert!(!views.is_empty());
    assert_eq!(run.outputs_built, views.len());
    // The p99 of the hour comes from its oldest pane, pane 5.
    assert_answers_from_pane(&run.answers_per_query[0], 5);
    assert_eq!(run.answers_per_query[0][0].0["instance"], "a");
    assert_eq!(run.answers_per_query, run.reference_answers_per_query);
}

/// The plan file carries Stage 3's predicted usage of the selected plan, so
/// a runtime reports it against what it measures without planning again:
/// for Example 4b's panes maintained at ingestion time, its nodes' prices
/// add up to Stage 3's selection cost and their CPU to the plan's, with
/// one latency per query, each node naming the installed nodes that run it.
#[tokio::test]
async fn the_plan_carries_stage3s_predicted_usage() {
    let (plan, selection) =
        planned_with_report(&["quantile_over_time(0.99, latency_ms[60m])"]).await;
    let views = plan.materialized_views().unwrap();
    assert!(!views.is_empty(), "a pane-only plan");
    assert!(
        plan.raw_data_retention().unwrap().is_empty(),
        "a pane-only plan"
    );
    let selection = selection.as_ref().expect("Stage 3's report");
    let usage = &selection.predicted_usage;
    let node_costs: f64 = usage
        .per_node
        .iter()
        .map(|node| node.cost_per_second.unwrap())
        .sum();
    let selection_cost = selection.cost_per_second.unwrap();
    assert!(
        (node_costs - selection_cost).abs() <= 1e-9 * selection_cost.abs(),
        "{node_costs} vs {selection_cost}"
    );
    let node_cpu: f64 = usage
        .per_node
        .iter()
        .map(|node| node.cpu_core_seconds_per_second)
        .sum();
    assert_eq!(node_cpu, usage.cpu_core_seconds_per_second);
    assert!(usage.cpu_core_seconds_per_second > 0.0);
    assert!(usage
        .per_node
        .iter()
        .any(|node| node.operator == OperatorKind::PaneChain
            && node.timing == asap_planner::ExecutionTiming::IngestionTime));
    assert_eq!(usage.per_query.len(), 1);
    assert!(usage.peak_memory_bytes > 0);
    // Each prediction names the installed nodes that run it: the pane
    // chain its selected panes, the query's root its own node.
    let installed: BTreeSet<_> = plan.dag.nodes.iter().map(|node| node.id).collect();
    for node in &usage.per_node {
        assert!(
            node.installed_node_ids
                .iter()
                .all(|id| installed.contains(id)),
            "{node:?}"
        );
    }
    let chain = usage
        .per_node
        .iter()
        .find(|node| node.operator == OperatorKind::PaneChain)
        .unwrap();
    assert_eq!(chain.installed_node_ids.len(), views.len(), "{chain:?}");
    let view_ids: BTreeSet<_> = views.iter().map(|v| v.node_id).collect();
    assert!(chain
        .installed_node_ids
        .iter()
        .all(|id| view_ids.contains(id)));
    let root = plan.dag.roots[0];
    assert!(usage
        .per_node
        .iter()
        .any(|node| node.installed_node_ids == [root]));
}

/// The query's label matcher selects series `a` only: the runtime feeds
/// every sample, and the window's selector applies the plan's `Scan`.
#[tokio::test]
async fn label_matchers_select_the_series_a_pane_reads() {
    let plan_json =
        plan_json(&[r#"quantile_over_time(0.99, latency_ms{instance="a"}[60m])"#]).await;
    let run = run_data_plane(
        &plan_json,
        &remote_write_samples(),
        EVALUATION_MS,
        &mut SummaryStore::new(),
    );
    let answered = &run.answers_per_query[0];
    assert_eq!(answered.len(), 1, "{answered:?}");
    assert_eq!(answered[0].0["instance"], "a");
    assert_answers_from_pane(answered, 5);
    assert_eq!(run.answers_per_query, run.reference_answers_per_query);
}

/// The p99 over the last hour and over the hour before the last 10
/// minutes may share selected pane windows. The runtime refreshes each distinct
/// state key once, independent of the optimizer's pane widths. Both answers
/// equal the reference.
#[tokio::test]
async fn an_hour_and_its_offset_twin_both_equal_the_reference() {
    let plan_json = plan_json(&[
        "quantile_over_time(0.99, latency_ms[60m])",
        "quantile_over_time(0.99, latency_ms[60m] offset 10m)",
    ])
    .await;
    let installed: InstallablePlan = serde_json::from_str(&plan_json).unwrap();
    let distinct_states: BTreeSet<_> = installed
        .materialized_views()
        .unwrap()
        .iter()
        .map(|view| state_key(view, EVALUATION_MS))
        .collect();
    assert!(!distinct_states.is_empty());
    let run = run_data_plane(
        &plan_json,
        &remote_write_samples(),
        EVALUATION_MS,
        &mut SummaryStore::new(),
    );
    assert_eq!(run.outputs_built, distinct_states.len());
    assert_eq!(run.answers_per_query.len(), 2);
    // The last hour's p99 comes from pane 5, the offset hour's from pane 6.
    assert_answers_from_pane(&run.answers_per_query[0], 5);
    assert_answers_from_pane(&run.answers_per_query[1], 6);
    assert_eq!(run.answers_per_query, run.reference_answers_per_query);
}

/// Two copies of `sum by (job) (sum_over_time(m[10m]))`, each every 20 s,
/// one at phase 0 and one at phase 5 s, over 6 ingesting series of `m`
/// sampled every second. Panes shared by both must end on both schedules:
/// 20 s panes end only on the first's, so the second would read
/// `(t − 605 s, t − 5 s]` instead of `(t − 600 s, t]`.
#[tokio::test]
async fn queries_at_two_phases_read_panes_on_both_schedules() {
    assert_phased_copies_answer_as_reference(&[0, 5_000]).await;
}

/// The same two copies, both at phase 5 s. A runtime anchors panes at the
/// epoch and the plan carries no pane origin, so 20 s panes would end 5 s
/// before every evaluation: the panes must also end on the phase itself.
#[tokio::test]
async fn queries_at_one_nonzero_phase_read_panes_anchored_at_the_epoch() {
    assert_phased_copies_answer_as_reference(&[5_000, 5_000]).await;
}

/// Copies of `sum by (job) (sum_over_time(m[10m]))`, each every 20 s, copy
/// `q` at phase `phases_ms[q]`, over 6 ingesting series of `m` sampled
/// every second. Run by a runtime that refreshes each ingestion pane once, on
/// the grid of its width anchored at the epoch, each answers as the
/// query-time reference at its own evaluation.
async fn assert_phased_copies_answer_as_reference(phases_ms: &[i64]) {
    let entries: Vec<String> = phases_ms
        .iter()
        .map(|phase_ms| {
            format!(
                r#"{{"query": "sum by (job) (sum_over_time(m[10m]))",
                  "demand": {{"fixed_interval_at": {{"interval": 20000, "evaluation_phase": {phase_ms}}}}},
                  "requirements": {{}},
                  "predictability": {{"predictable": {{"known_at": null}}}}}}"#
            )
        })
        .collect();
    let workload: PlanningWorkload = serde_json::from_str(&format!(
        r#"{{"query_workload": {{"language": "promql", "repeating_queries": [{}]}},
            "data_workload": {{"arrival": {{"ingesting": {{"rows_per_second": 6.0}}}},
              "sources": {{"m": {{"series_count": 6, "sample_interval": 1000}}}}}}}}"#,
        entries.join(",")
    ))
    .unwrap();
    // A deployment that keeps no raw data samples: the windows are views.
    let capabilities = DeploymentCapabilities {
        raw_data_retained: false,
        ..DeploymentCapabilities::default()
    };
    let plan_json = plan_json_for(workload, &capabilities).await;
    let plan: InstallablePlan = serde_json::from_str(&plan_json).unwrap();
    let views = plan.materialized_views().unwrap();
    assert!(
        views
            .iter()
            .any(|view| view.refresh() == MaterializedViewRefresh::AtIngestion),
        "{views:#?}"
    );
    // Samples of `m`, `job` a (instances 1-3) and b (4-6), every second
    // from 11 minutes before the first evaluation to 10 s after it; each
    // value distinct, so a window off by 5 s at either edge answers
    // differently.
    let samples: Vec<RawDataSample> = (1..=6i64)
        .flat_map(|instance| {
            (0..680i64).map(move |step| RawDataSample {
                labels: BTreeMap::from([
                    ("__name__".to_string(), "m".to_string()),
                    (
                        "job".to_string(),
                        if instance <= 3 { "a" } else { "b" }.to_string(),
                    ),
                    ("instance".to_string(), instance.to_string()),
                ]),
                timestamp_ms: EVALUATION_MS - 660_000 + step * 1_000,
                value: (instance * 1000 + step * step % 997) as f64,
            })
        })
        .collect();
    // The panes stored across every evaluation.
    let mut store = SummaryStore::new();
    // Copy `q` evaluates `phases_ms[q]` after `EVALUATION_MS` (on the
    // 20 s grid); each reads the panes the store holds by then.
    for (query, phase_ms) in phases_ms.iter().enumerate() {
        let evaluation_ms = EVALUATION_MS + phase_ms;
        let run = run_data_plane_on(
            &plan_json,
            &samples,
            evaluation_ms,
            &mut store,
            IngestionBuilds::Tumbling,
        );
        assert_eq!(run.answers_per_query[query].len(), 2, "one sum per job");
        assert_eq!(
            run.answers_per_query[query], run.reference_answers_per_query[query],
            "query {query} at {evaluation_ms}"
        );
    }
}

/// A query whose window equals its interval, with no declared phase: one
/// tumbling window per evaluation, maintained at ingestion time, answers
/// every evaluation without raw data. `quantile_over_time(0.99, m[5m])`
/// every 5 minutes at rank error 0.01 (a KLL per series) had no plan.
#[tokio::test]
async fn a_window_equal_to_its_interval_is_kept_at_ingestion_time() {
    assert_answers_as_reference_without_raw_data(
        "quantile_over_time(0.99, m[5m])",
        r#"{"fixed_interval": 300000}"#,
        RANK_ERROR_1_PERCENT,
        &[0, 300_000, 600_000],
    )
    .await;
    assert_answers_as_reference_without_raw_data(
        "sum by (job) (sum_over_time(m[20s]))",
        r#"{"fixed_interval": 20000}"#,
        "{}",
        &[0, 20_000, 40_000, 60_000],
    )
    .await;
}

/// The same 20 s window every 20 s at phase 5 s: a 20 s window on the
/// epoch grid would end 5 s before every evaluation, so the plan must not
/// read one; it still answers every evaluation as the reference.
#[tokio::test]
async fn a_window_equal_to_its_interval_at_an_offset_phase_answers_as_reference() {
    assert_answers_as_reference_without_raw_data(
        "sum by (job) (sum_over_time(m[20s]))",
        r#"{"fixed_interval_at": {"interval": 20000, "evaluation_phase": 5000}}"#,
        "{}",
        &[5_000, 25_000, 45_000, 65_000],
    )
    .await;
}

/// Rank error 0.01 with failure probability 0.01, as `requirements`.
const RANK_ERROR_1_PERCENT: &str = r#"{"accuracy": {"accuracy_metric": "rank", "max_error": 0.01,
    "failure_probability": 0.01, "accuracy_scope": "whole_result"}}"#;

/// Plans `query` with `demand` and `requirements` over 6 ingesting series of `m` sampled every
/// second, for a deployment that keeps no raw data, and runs it at
/// `EVALUATION_MS` plus each of `evaluation_offsets_ms` on a runtime that
/// refreshes each ingestion state once, on the epoch grid of its width. The
/// plan keeps state at ingestion time and reads no raw data at query time;
/// every answer equals the query-time reference.
async fn assert_answers_as_reference_without_raw_data(
    query: &str,
    demand: &str,
    requirements: &str,
    evaluation_offsets_ms: &[i64],
) {
    let workload: PlanningWorkload = serde_json::from_str(&format!(
        r#"{{"query_workload": {{"language": "promql", "repeating_queries": [
              {{"query": {query:?}, "demand": {demand}, "requirements": {requirements},
                "predictability": {{"predictable": {{"known_at": null}}}}}}]}},
            "data_workload": {{"arrival": {{"ingesting": {{"rows_per_second": 6.0}}}},
              "sources": {{"m": {{"series_count": 6, "sample_interval": 1000}}}}}}}}"#
    ))
    .unwrap();
    let capabilities = DeploymentCapabilities {
        raw_data_retained: false,
        ..DeploymentCapabilities::default()
    };
    let plan_json = plan_json_for(workload, &capabilities).await;
    let plan: InstallablePlan = serde_json::from_str(&plan_json).unwrap();
    let views = plan.materialized_views().unwrap();
    assert!(
        views
            .iter()
            .any(|view| view.refresh() == MaterializedViewRefresh::AtIngestion),
        "{views:#?}"
    );
    assert!(
        plan.raw_data_retention().unwrap().is_empty(),
        "no raw data at query time"
    );
    // Samples of `m`, `job` a (instances 1-3) and b (4-6), every second
    // from 6 minutes before `EVALUATION_MS` to past the last evaluation;
    // each value distinct, so a window off by a second answers differently.
    let last_ms = EVALUATION_MS + evaluation_offsets_ms.iter().max().unwrap();
    let steps = (last_ms - EVALUATION_MS + 360_000) / 1_000 + 10;
    let samples: Vec<RawDataSample> = (1..=6i64)
        .flat_map(|instance| {
            (0..steps).map(move |step| RawDataSample {
                labels: BTreeMap::from([
                    ("__name__".to_string(), "m".to_string()),
                    (
                        "job".to_string(),
                        if instance <= 3 { "a" } else { "b" }.to_string(),
                    ),
                    ("instance".to_string(), instance.to_string()),
                ]),
                timestamp_ms: EVALUATION_MS - 360_000 + step * 1_000,
                value: (instance * 1000 + step * step % 997) as f64,
            })
        })
        .collect();
    // The states stored across every evaluation.
    let mut store = SummaryStore::new();
    for offset_ms in evaluation_offsets_ms {
        let evaluation_ms = EVALUATION_MS + offset_ms;
        let run = run_data_plane_on(
            &plan_json,
            &samples,
            evaluation_ms,
            &mut store,
            IngestionBuilds::Tumbling,
        );
        assert!(!run.answers_per_query[0].is_empty());
        assert_eq!(
            run.answers_per_query, run.reference_answers_per_query,
            "{query} at {evaluation_ms}"
        );
    }
}

/// A plan file of another DAG wire version is refused, and so is the
/// reference of a tampered plan file, with an error rather than a panic.
#[tokio::test]
async fn a_plan_file_of_another_version_or_tampered_is_refused() {
    let plan_json = plan_json(&["quantile_over_time(0.99, latency_ms[60m])"]).await;
    let mut other_version: InstallablePlan = serde_json::from_str(&plan_json).unwrap();
    other_version.wire_version += 1;
    assert!(matches!(
        other_version.validate(),
        Err(InstallablePlanError::UnsupportedWireVersion(_))
    ));
    let mut tampered: InstallablePlan = serde_json::from_str(&plan_json).unwrap();
    tampered.query_texts.clear();
    assert!(matches!(
        asap_planner::test_support::query_time_reference(&tampered),
        Err(InstallablePlanError::QueriesDoNotMatchRoots { .. })
    ));
    let mut missing_node: InstallablePlan = serde_json::from_str(&plan_json).unwrap();
    missing_node.dag.nodes.remove(0);
    assert!(matches!(
        asap_planner::test_support::query_time_reference(&missing_node),
        Err(InstallablePlanError::InvalidDag(_))
    ));
}

/// `sum(latency_ms)` reads an instant selector: per series, only the latest
/// sample in its 1 s lookback counts. Series `a` has samples at T-900 ms (1)
/// and T-100 ms (10), series `b` at T-200 ms (100): the sum is 10 + 100.
#[tokio::test]
async fn an_instant_selector_reads_the_latest_sample_per_series() {
    let workload = workload(&["sum(latency_ms)"]);
    let plan = plan_installable(UserInput::new(
        &workload,
        FrontendInput::Promql { histograms: None },
        PlanningModels::builtin(),
    ))
    .await
    .unwrap()
    .0;
    let dag = &plan.dag;
    let [retention] = plan
        .raw_data_retention()
        .unwrap()
        .try_into()
        .unwrap_or_else(|r: Vec<_>| {
            panic!("{r:?}");
        });
    assert_eq!(retention.lookback_ms(), Some(1_000));
    let samples = [
        sample("latency_ms", "a", EVALUATION_MS - 900, 1.0),
        sample("latency_ms", "a", EVALUATION_MS - 100, 10.0),
        sample("latency_ms", "b", EVALUATION_MS - 200, 100.0),
    ];
    let query = Scope::Query {
        evaluation_time_ms: EVALUATION_MS,
    };
    let answered = answers(&run(dag, BTreeMap::new(), &samples, &query_roots(&plan), query)[0]);
    assert_eq!(answered.len(), 1);
    assert_eq!(answered[0].1, 110.0);
}

/// A deployment that keeps raw data samples and query-time results across
/// evaluations (#509 Example 4's B3), with a dashboard panel
/// (`max_over_time(latency_ms[5m])` every minute over 1M series sampled
/// every 1.5 s, answered as the last hour at 1-minute steps): Stage 3
/// keeps five 1-minute panes refreshed at query time, and keeps raw data
/// for refreshing the oldest pane at the oldest step (5 min + 1 h). The first
/// evaluation, with an empty store, refreshes all five in its query run; the
/// next, a minute later, reuses the four still in its window and refreshes
/// only the newest. Each
/// evaluation's answers (each series' newest-pane maximum) equal the
/// reference.
#[tokio::test]
async fn query_time_results_kept_across_evaluations_are_built_in_query_runs() {
    let capabilities = DeploymentCapabilities {
        query_time_results_retained: true,
        ..DeploymentCapabilities::default()
    };
    let workload: PlanningWorkload = serde_json::from_str(
        r#"{"query_workload": {"language": "promql", "repeating_queries": [{
              "query": "max_over_time(latency_ms[5m])",
              "demand": {"fixed_interval": 60000},
              "requirements": {},
              "predictability": {"predictable": {"known_at": null}},
              "range_evaluation": {"step_ms": 60000, "span_ms": 3600000}}]},
            "data_workload": {"arrival": {"ingesting": {"rows_per_second": 666666.67}},
              "sources": {"latency_ms": {"series_count": 1000000, "sample_interval": 1500}}}}"#,
    )
    .unwrap();
    let plan_json = plan_json_for(workload, &capabilities).await;
    let installed: InstallablePlan = serde_json::from_str(&plan_json).unwrap();
    let views = installed.materialized_views().unwrap();
    assert_eq!(views.len(), 5);
    assert!(views
        .iter()
        .all(|v| v.refresh() == MaterializedViewRefresh::OnFirstRead
            && v.inputs.raw_data_windows()[0].width_ms() == 60_000));
    // Raw data is kept for refreshing the oldest pane at the oldest step:
    // the five panes' 5 minutes plus the hour of steps, as long as the panes
    // themselves are kept.
    let pane_lookback_ms = views
        .iter()
        .flat_map(|v| v.inputs.raw_data_windows())
        .map(asap_types::test_support::lookback_ms)
        .max();
    assert_eq!(pane_lookback_ms, Some(300_000));
    assert!(views.iter().all(|v| v.retention_ms() == 3_900_000));
    let retention = installed.raw_data_retention().unwrap();
    assert_eq!(
        retention
            .iter()
            .map(|r| r.lookback_ms())
            .collect::<Vec<_>>(),
        vec![Some(3_900_000)],
        "{retention:?}"
    );
    // The pane cache the data plane keeps across evaluations.
    let mut store = SummaryStore::new();
    let first = run_data_plane(
        &plan_json,
        &remote_write_samples(),
        EVALUATION_MS,
        &mut store,
    );
    assert_eq!(first.outputs_built, 5);
    assert_answers_from_pane(&first.answers_per_query[0], 0);
    assert_eq!(first.answers_per_query, first.reference_answers_per_query);
    let next = run_data_plane(
        &plan_json,
        &remote_write_samples(),
        EVALUATION_MS + 60_000,
        &mut store,
    );
    assert_eq!(next.outputs_built, 1, "four panes reused from the cache");
    assert_eq!(next.answers_per_query, next.reference_answers_per_query);
}

/// An exact per-series quantile under a group average (issue #770), on a
/// deployment that keeps raw data samples: Stage 3 selects the exact plan, which
/// reads each series' raw data samples in the window at query time, and the
/// executor runs it. Series `a` (1..=10) and `b` (10, 20, ..., 100) are in
/// `east`, `c` (constantly 5) in `west`; a sample of each before the window
/// is not read. Per series, p99 interpolates between its two largest
/// samples (rank 0.99 × 9 = 8.91): 9.91 for `a`, 99.1 for `b`, 5 for `c`.
#[tokio::test]
async fn an_exact_per_series_quantile_runs_from_raw_data_samples() {
    let workload: PlanningWorkload = serde_json::from_str(
        r#"{"query_workload": {"language": "promql", "repeating_queries": [{
              "query": "avg by (region) (quantile_over_time(0.99, latency_ms[100s]))",
              "demand": {"fixed_interval": 60000},
              "requirements": {},
              "predictability": {"predictable": {"known_at": null}}}]},
            "data_workload": {"arrival": {"ingesting": {"rows_per_second": 3.0}},
              "sources": {"latency_ms": {"series_count": 3, "sample_interval": 10000}}}}"#,
    )
    .unwrap();
    let plan_json = plan_json_for(workload, &DeploymentCapabilities::default()).await;
    let installed: InstallablePlan = serde_json::from_str(&plan_json).unwrap();
    assert!(
        installed.materialized_views().unwrap().is_empty(),
        "the exact plan"
    );
    assert_eq!(installed.raw_data_retention().unwrap().len(), 1);
    // Ten samples per series in the window, one every 10 s, and one before it.
    let mut samples = Vec::new();
    for (instance, region, value) in [
        ("a", "east", (|step| step as f64) as fn(i64) -> f64),
        ("b", "east", |step| 10.0 * step as f64),
        ("c", "west", |_| 5.0),
    ] {
        for step in 0..=10 {
            let mut sample = sample(
                "latency_ms",
                instance,
                EVALUATION_MS - 105_000 + 10_000 * step,
                if step == 0 { 1e9 } else { value(step) },
            );
            sample
                .labels
                .insert("region".to_string(), region.to_string());
            samples.push(sample);
        }
    }
    let run = run_data_plane(
        &plan_json,
        &samples,
        EVALUATION_MS,
        &mut SummaryStore::new(),
    );
    let answered = &run.answers_per_query[0];
    assert_eq!(answered.len(), 2, "{answered:?}");
    for ((labels, value), (region, expected)) in answered
        .iter()
        .zip([("east", (9.91 + 99.1) / 2.0), ("west", 5.0)])
    {
        assert_eq!(labels["region"], region);
        assert!((value - expected).abs() < 1e-9, "{region}: {value}");
    }
    assert_eq!(run.answers_per_query, run.reference_answers_per_query);
}

// ── A ratio of two relative-error quantiles (issue #852) ────────────────────

/// Repeating `queries`, each every 10 minutes at error 0.05 in each
/// summary's own metric (W-RangeHeavy leaves the metric unset) and δ 0.01,
/// over 1,000 ingesting non-negative `latency_ms` gauges sampled
/// every second.
fn relative_workload(queries: &[&str]) -> PlanningWorkload {
    let entries: Vec<String> = queries
        .iter()
        .map(|query| {
            format!(
                r#"{{"query": {query:?}, "demand": {{"fixed_interval": 600000}},
                  "requirements": {{"accuracy": {{"max_error": 0.05, "failure_probability": 0.01,
                      "accuracy_scope": "whole_result"}}}},
                  "predictability": {{"predictable": {{"known_at": null}}}}}}"#
            )
        })
        .collect();
    serde_json::from_str(&format!(
        r#"{{"query_workload": {{"language": "promql", "repeating_queries": [{}]}},
            "data_workload": {{"arrival": {{"ingesting": {{"rows_per_second": 1000.0}}}},
              "sources": {{"latency_ms": {{"series_count": 1000, "sample_interval": 1000,
                "metric_type": "gauge", "values_non_negative": true}}}}}}}}"#,
        entries.join(",")
    ))
    .unwrap()
}

/// The DDSketch α of every sketch `plan` builds; panics on any other sketch.
fn ddsketch_alphas(plan: &InstallablePlan) -> Vec<f64> {
    plan.dag
        .nodes
        .iter()
        .filter_map(|node| match &node.payload {
            asap_executor::planner::ir::physical_export::PhysicalASAPOperatorPayload::ASAP(
                ASAPOp::SummaryAgg {
                    family: FieldDataType::Sketch(kind, _),
                    ..
                }
                | ASAPOp::PaneChain {
                    family: FieldDataType::Sketch(kind, _),
                    ..
                },
            ) => match kind.params() {
                SketchParams::DDSketch { alpha } => Some(*alpha),
                other => panic!("not a DDSketch: {other:?}"),
            },
            _ => None,
        })
        .collect()
}

/// PromQL `quantile_over_time(q, …)` of `values`: linear interpolation
/// between the sorted values at rank `q·(n − 1)`.
fn promql_quantile(q: f64, values: &mut [f64]) -> f64 {
    values.sort_by(f64::total_cmp);
    let rank = q * (values.len() - 1) as f64;
    let (low, high) = (rank.floor() as usize, rank.ceil() as usize);
    values[low] + (values[high] - values[low]) * (rank - low as f64)
}

/// The p90/p50 ratio of an hour (W-RangeHeavy template 8) on a deployment
/// that keeps no raw data samples: both quantiles come from DDSketches
/// sized for a share of the target (α = 1 − 1.05^{−1/2} ≈ 0.0241, so
/// `(a + b)/(1 − b) ≤ 0.05`), maintained in panes; nothing is read from
/// raw data at query time. Run on data, each series' ratio is within 0.05
/// of the exact ratio of its hour's samples.
#[tokio::test]
async fn a_ratio_of_two_quantiles_runs_from_ddsketches_within_its_relative_target() {
    let query =
        "quantile_over_time(0.9, latency_ms[60m]) / quantile_over_time(0.5, latency_ms[60m])";
    let capabilities = DeploymentCapabilities {
        raw_data_retained: false,
        ..DeploymentCapabilities::default()
    };
    let plan_json = plan_json_for(relative_workload(&[query]), &capabilities).await;
    let installed: InstallablePlan = serde_json::from_str(&plan_json).unwrap();
    assert!(
        installed.raw_data_retention().unwrap().is_empty(),
        "no raw scan"
    );
    let alphas = ddsketch_alphas(&installed);
    assert!(!alphas.is_empty(), "DDSketch states");
    let share = 1.0 - 1.05f64.powf(-0.5);
    for alpha in &alphas {
        assert!(*alpha <= share + 1e-12, "α {alpha} > {share}");
    }
    let samples = remote_write_samples();
    let run = run_data_plane(
        &plan_json,
        &samples,
        EVALUATION_MS,
        &mut SummaryStore::new(),
    );
    let answered = &run.answers_per_query[0];
    assert_eq!(answered.len(), 2, "{answered:?}");
    for (labels, value) in answered {
        let mut hour: Vec<f64> = samples
            .iter()
            .filter(|s| {
                s.labels["__name__"] == "latency_ms"
                    && s.labels["instance"] == labels["instance"]
                    && s.timestamp_ms > EVALUATION_MS - 3_600_000
                    && s.timestamp_ms <= EVALUATION_MS
            })
            .map(|s| s.value)
            .collect();
        let exact = promql_quantile(0.9, &mut hour) / promql_quantile(0.5, &mut hour);
        let error = (value - exact).abs() / exact;
        assert!(error <= 0.05, "{labels:?}: {value} vs {exact} ({error})");
    }
}

/// The ratio's numerator is also asked for on its own (W-RangeHeavy
/// templates 5 and 8): the p90 estimate is shared, so its state is sized
/// for its strictest reader, the ratio's share; the plan still reads no raw
/// data samples.
#[tokio::test]
async fn a_quantile_shared_with_a_ratio_is_sized_for_the_ratio() {
    let capabilities = DeploymentCapabilities {
        raw_data_retained: false,
        ..DeploymentCapabilities::default()
    };
    let plan_json = plan_json_for(
        relative_workload(&[
            "quantile_over_time(0.9, latency_ms[60m])",
            "quantile_over_time(0.9, latency_ms[60m]) / quantile_over_time(0.5, latency_ms[60m])",
        ]),
        &capabilities,
    )
    .await;
    let installed: InstallablePlan = serde_json::from_str(&plan_json).unwrap();
    assert!(
        installed.raw_data_retention().unwrap().is_empty(),
        "no raw scan"
    );
    let share = 1.0 - 1.05f64.powf(-0.5);
    for alpha in ddsketch_alphas(&installed) {
        assert!(alpha <= share + 1e-12, "α {alpha} > {share}");
    }
    let run = run_data_plane(
        &plan_json,
        &remote_write_samples(),
        EVALUATION_MS,
        &mut SummaryStore::new(),
    );
    assert_eq!(run.answers_per_query, run.reference_answers_per_query);
}

// ── Materialized views refreshed from materialized views (issue #781) ──────────────────

/// Stage 1 to Stage 3, then every node `store` names stored as it says,
/// the others as Stage 2 chose, and the plan timed again: how a test makes
/// the plan store a shape Stage 3 does not select today.
struct StoredAsForced {
    store: fn(&OperatorNode) -> Option<Materialization>,
}

impl OptimizationPass for StoredAsForced {
    fn name(&self) -> &'static str {
        "stored-as-forced"
    }

    fn optimize(&self, input: OptimizationInput<'_>) -> Result<PlanOutput, OptimizeError> {
        let mut output = Stage1ToStage3Pass.optimize(input)?;
        // Each node untimed once, so shared inputs stay shared, and its
        // materialization in `assignment`.
        let mut untimed: HashMap<*const OperatorNode, Rc<OperatorNode>> = HashMap::new();
        let mut assignment = MaterializationAssignment::default();
        let roots: Vec<Rc<OperatorNode>> = output
            .plans
            .iter()
            .map(|plan| untime(&plan.root, self.store, &mut untimed, &mut assignment))
            .collect();
        // A node read both at ingestion and at query time is copied, one
        // copy per timing, as Stage 2 does.
        let (roots, assignment) = split_shared_by_timing(&roots, &assignment);
        let roots: Vec<QueryRoot> = roots.into_iter().map(QueryRoot::Operator).collect();
        let timed = apply_materialization_timings_to_roots(&roots, &assignment).unwrap();
        for (plan, root) in output.plans.iter_mut().zip(timed) {
            let QueryRoot::Operator(root) = root else {
                unreachable!("operator roots in, operator roots out")
            };
            plan.root = root;
        }
        Ok(output)
    }
}

/// `node` untimed, with `store`'s materialization (or its own) recorded in
/// `assignment` for every node of its sub-DAG.
fn untime(
    node: &Rc<OperatorNode>,
    store: fn(&OperatorNode) -> Option<Materialization>,
    untimed: &mut HashMap<*const OperatorNode, Rc<OperatorNode>>,
    assignment: &mut MaterializationAssignment,
) -> Rc<OperatorNode> {
    if let Some(done) = untimed.get(&Rc::as_ptr(node)) {
        return Rc::clone(done);
    }
    let mut copy = (**node).clone();
    copy.operator = node
        .operator
        .map_children(|child| untime(child, store, untimed, assignment));
    copy.timing = None;
    copy.materialization = Materialization::NotMaterialized;
    let copy = Rc::new(copy);
    // A shared non-summary node Stage 2 built once per evaluation is not
    // kept: a build forced to ingestion time above it cannot read a
    // query-time result.
    let own = match node.asap() {
        Some(
            ASAPOp::SummaryAgg { .. } | ASAPOp::PaneChain { .. } | ASAPOp::SummaryMerge { .. },
        ) => node.materialization,
        _ => Materialization::NotMaterialized,
    };
    let materialization = store(node).unwrap_or(own);
    if materialization != Materialization::NotMaterialized {
        assignment.set(&copy, materialization);
    }
    untimed.insert(Rc::as_ptr(node), Rc::clone(&copy));
    copy
}

/// Every `SummaryAgg` built at ingestion time.
fn summaries_at_ingestion(node: &OperatorNode) -> Option<Materialization> {
    matches!(node.asap(), Some(ASAPOp::SummaryAgg { .. }))
        .then_some(Materialization::MatAtIngestion(Medium::Memory))
}

/// Every `SummaryMerge` built at query time and kept across evaluations.
fn merges_kept(node: &OperatorNode) -> Option<Materialization> {
    matches!(node.asap(), Some(ASAPOp::SummaryMerge { .. })).then_some(
        Materialization::MatAtQueryKeptAcrossEvaluations(Medium::Memory),
    )
}

/// Exact PromQL `queries`, every minute, over 6 ingesting series of `m`
/// sampled every 10 s; `tail` is appended to each entry's JSON (e.g. its
/// range evaluation).
fn exact_workload(queries: &[&str], tail: &str) -> PlanningWorkload {
    let entries: Vec<String> = queries
        .iter()
        .map(|query| {
            format!(
                r#"{{"query": {query:?}, "demand": {{"fixed_interval": 60000}},
                  "requirements": {{}},
                  "predictability": {{"predictable": {{"known_at": null}}}}{tail}}}"#
            )
        })
        .collect();
    serde_json::from_str(&format!(
        r#"{{"query_workload": {{"language": "promql", "repeating_queries": [{}]}},
            "data_workload": {{"arrival": {{"ingesting": {{"rows_per_second": 0.6}}}},
              "sources": {{"m": {{"series_count": 6, "sample_interval": 10000}}}}}}}}"#,
        entries.join(",")
    ))
    .unwrap()
}

/// The plan file of `workload` on `capabilities`, with what `store` names
/// stored.
async fn forced_plan_json(
    workload: &PlanningWorkload,
    capabilities: &DeploymentCapabilities,
    store: fn(&OperatorNode) -> Option<Materialization>,
) -> String {
    let pass = StoredAsForced { store };
    let mut input = UserInput::new(
        workload,
        FrontendInput::Promql { histograms: None },
        PlanningModels::builtin().with_deployment_capabilities(capabilities),
    );
    input.pass = Some(&pass);
    serde_json::to_string(&plan_installable(input).await.unwrap().0).unwrap()
}

/// Samples of `m` for six series, `job` a (instances 1, 2, 3) and b (4, 5,
/// 6), every 10 s over the last 10 minutes; each value distinct.
fn job_samples() -> Vec<RawDataSample> {
    let mut samples = Vec::new();
    for instance in 1..=6 {
        for step in 0..60 {
            samples.push(RawDataSample {
                labels: BTreeMap::from([
                    ("__name__".to_string(), "m".to_string()),
                    (
                        "job".to_string(),
                        if instance <= 3 { "a" } else { "b" }.to_string(),
                    ),
                    ("instance".to_string(), instance.to_string()),
                ]),
                timestamp_ms: EVALUATION_MS - 595_000 + step * 10_000,
                value: (instance * 1000 + (step * 37) % 101) as f64,
            });
        }
    }
    samples
}

/// The time selection `(from_ms, to_ms]` relative to the evaluation.
fn relative_time(from_ms: i64, to_ms: i64) -> Vec<SelectionBox> {
    vec![SelectionBox::new(
        Default::default(),
        Some((Bound::Excluded(from_ms), Bound::Included(to_ms))),
    )]
}

/// A per-group maximum and a per-group sum (`max by (job)` and
/// `sum by (job, instance)`), both refreshed at ingestion from one stored
/// per-series sum: Stage 1 shares the per-series sums the two queries read
/// (over scans that project other labels). The per-series sum is listed
/// first, then each group output, which reads only it, with its time
/// selection under a definition of its own; refreshed in that order from
/// stored inputs, they answer as the query-time reference. (A coarse sum
/// in place of the maximum rolls up from the fine sum instead:
/// [`a_coarse_sum_rolls_up_from_the_stored_fine_sum`].)
#[tokio::test]
async fn group_outputs_from_one_stored_per_series_sum_are_listed_and_built() {
    let workload = exact_workload(
        &[
            "max by (job) (sum_over_time(m[1m]))",
            "sum by (job, instance) (sum_over_time(m[1m]))",
        ],
        "",
    );
    let plan_json = forced_plan_json(
        &workload,
        &DeploymentCapabilities::default(),
        summaries_at_ingestion,
    )
    .await;
    let plan: InstallablePlan = serde_json::from_str(&plan_json).unwrap();
    let views = plan.materialized_views().unwrap();
    let [per_series, sums @ ..] = views.as_slice() else {
        panic!("{:#?}", views);
    };
    assert_eq!(sums.len(), 2, "{:#?}", views);
    assert!(per_series.inputs.materialized_views.is_empty());
    assert_eq!(per_series.inputs.raw_data_windows().len(), 1);
    for sum in sums {
        assert_eq!(sum.inputs.materialized_views, [per_series.node_id]);
        assert!(sum.inputs.raw_data_windows().is_empty());
        assert_ne!(
            sum.coverage().definition_id(),
            per_series.coverage().definition_id()
        );
    }
    assert_ne!(
        sums[0].coverage().definition_id(),
        sums[1].coverage().definition_id()
    );
    for output in &views {
        assert_eq!(output.refresh(), MaterializedViewRefresh::AtIngestion);
        assert_eq!(output.coverage().selection(), relative_time(-60_000, 0));
        assert_eq!(output.retention_ms(), 60_000);
    }
    assert!(plan.raw_data_retention().unwrap().is_empty());
    let run = run_data_plane(
        &plan_json,
        &job_samples(),
        EVALUATION_MS,
        &mut SummaryStore::new(),
    );
    assert_eq!(run.outputs_built, 3);
    assert_eq!(run.answers_per_query[0].len(), 2, "one maximum per job");
    assert_eq!(run.answers_per_query[1].len(), 6, "one sum per series");
    assert_eq!(run.answers_per_query, run.reference_answers_per_query);
}

/// A coarse and a fine sum (`sum by (job)` and `sum by (job, instance)`):
/// the fine grouping is priced at no more than the 6 series, as the
/// per-series sum is, so the coarse sum rolls up from it and the plan
/// stores two outputs, the second reading only the first. Built in that
/// order, they answer as the query-time reference.
#[tokio::test]
async fn a_coarse_sum_rolls_up_from_the_stored_fine_sum() {
    let workload = exact_workload(
        &[
            "sum by (job) (sum_over_time(m[1m]))",
            "sum by (job, instance) (sum_over_time(m[1m]))",
        ],
        "",
    );
    let plan_json = forced_plan_json(
        &workload,
        &DeploymentCapabilities::default(),
        summaries_at_ingestion,
    )
    .await;
    let plan: InstallablePlan = serde_json::from_str(&plan_json).unwrap();
    let views = plan.materialized_views().unwrap();
    let [first, rolled_up] = views.as_slice() else {
        panic!("{:#?}", views);
    };
    assert!(first.inputs.materialized_views.is_empty());
    assert_eq!(rolled_up.inputs.materialized_views, [first.node_id]);
    assert!(rolled_up.inputs.raw_data_windows().is_empty());
    let run = run_data_plane(
        &plan_json,
        &job_samples(),
        EVALUATION_MS,
        &mut SummaryStore::new(),
    );
    assert_eq!(run.outputs_built, 2);
    assert_eq!(run.answers_per_query[0].len(), 2, "one sum per job");
    assert_eq!(run.answers_per_query[1].len(), 6, "one sum per series");
    assert_eq!(run.answers_per_query, run.reference_answers_per_query);
}

/// A maximum across series (`max by (job)`) refreshed at ingestion from the
/// stored per-series maxima, which the per-series query reads too: listed
/// after them with their time selection, under a definition of its own.
#[tokio::test]
async fn a_summary_across_series_from_per_series_summaries_is_listed_and_built() {
    let workload = exact_workload(
        &[
            "max by (job) (max_over_time(m[1m]))",
            "max_over_time(m[1m])",
        ],
        "",
    );
    let plan_json = forced_plan_json(
        &workload,
        &DeploymentCapabilities::default(),
        summaries_at_ingestion,
    )
    .await;
    let plan: InstallablePlan = serde_json::from_str(&plan_json).unwrap();
    let views = plan.materialized_views().unwrap();
    let [per_series, across] = views.as_slice() else {
        panic!("{:#?}", views);
    };
    assert!(per_series.inputs.materialized_views.is_empty());
    assert_eq!(across.inputs.materialized_views, [per_series.node_id]);
    assert!(across.inputs.raw_data_windows().is_empty());
    for output in [per_series, across] {
        assert_eq!(output.refresh(), MaterializedViewRefresh::AtIngestion);
        assert_eq!(output.coverage().selection(), relative_time(-60_000, 0));
    }
    assert_ne!(
        across.coverage().definition_id(),
        per_series.coverage().definition_id()
    );
    let run = run_data_plane(
        &plan_json,
        &job_samples(),
        EVALUATION_MS,
        &mut SummaryStore::new(),
    );
    assert_eq!(run.outputs_built, 2);
    assert_eq!(run.answers_per_query[0].len(), 2, "one maximum per job");
    assert_eq!(run.answers_per_query[1].len(), 6, "one maximum per series");
    assert_eq!(run.answers_per_query, run.reference_answers_per_query);
}

/// A dashboard's 5-minute max over the last hour at 1-minute steps (#509
/// Example 4's B3) keeps five 1-minute panes; with its merge kept too, the
/// merge is listed after the panes it reads, with their definition and the
/// union of their times, refreshed in the query run and kept for its window
/// plus the hour of steps; its panes are kept at least that long. Built
/// from the stored panes it answers as the reference, and a minute later
/// only the newest pane and the new merge are refreshed.
#[tokio::test]
async fn a_kept_sliding_window_merge_is_listed_and_built() {
    let capabilities = DeploymentCapabilities {
        query_time_results_retained: true,
        ..DeploymentCapabilities::default()
    };
    let mut workload = exact_workload(
        &["max_over_time(m[5m])"],
        r#", "range_evaluation": {"step_ms": 60000, "span_ms": 3600000}"#,
    );
    // Declared as sampled every second, so that keeping the panes is
    // cheaper than rebuilding the window at every step; the samples the
    // run reads are every 10 s, and exact answers do not depend on it.
    let facts = workload
        .data_workload
        .as_mut()
        .and_then(|data| data.sources.get_mut("m"))
        .unwrap();
    facts.sample_interval = Some(asap_types::workload::DurationMs(1_000));
    let plan_json = forced_plan_json(&workload, &capabilities, merges_kept).await;
    let plan: InstallablePlan = serde_json::from_str(&plan_json).unwrap();
    let views = plan.materialized_views().unwrap();
    let (merge, panes) = views.split_last().unwrap();
    assert_eq!(panes.len(), 5);
    assert_eq!(
        merge.inputs.materialized_views,
        panes.iter().map(|pane| pane.node_id).collect::<Vec<_>>()
    );
    assert!(merge.inputs.raw_data_windows().is_empty());
    assert_eq!(merge.refresh(), MaterializedViewRefresh::OnFirstRead);
    assert_eq!(merge.coverage().selection(), relative_time(-300_000, 0));
    assert_eq!(merge.retention_ms(), 300_000 + 3_600_000);
    for (i, pane) in panes.iter().enumerate() {
        let i = i as i64;
        assert_eq!(
            pane.coverage().definition_id(),
            merge.coverage().definition_id()
        );
        assert_eq!(
            pane.coverage().selection(),
            relative_time(-(i + 1) * 60_000, -i * 60_000)
        );
        assert!(pane.retention_ms() >= merge.retention_ms());
    }
    let mut store = SummaryStore::new();
    let first = run_data_plane(&plan_json, &job_samples(), EVALUATION_MS, &mut store);
    assert_eq!(first.outputs_built, 6);
    assert_eq!(first.answers_per_query, first.reference_answers_per_query);
    let next = run_data_plane(
        &plan_json,
        &job_samples(),
        EVALUATION_MS + 60_000,
        &mut store,
    );
    assert_eq!(next.outputs_built, 2, "the newest pane and the new merge");
    assert_eq!(next.answers_per_query, next.reference_answers_per_query);
}

/// Example 4b's selected panes cover the hour and share one definition id,
/// each with its own time selection; each reads its pane of
/// raw data samples and nothing stored.
#[tokio::test]
async fn panes_of_an_hour_share_one_definition_with_distinct_times() {
    let plan: InstallablePlan =
        serde_json::from_str(&plan_json(&["quantile_over_time(0.99, latency_ms[60m])"]).await)
            .unwrap();
    let views = plan.materialized_views().unwrap();
    assert!(!views.is_empty());
    let mut panes: Vec<&MaterializedView> = views.iter().collect();
    panes.sort_by_key(|o| o.inputs.raw_data_windows()[0].end_offset_ms());
    let width_ms = panes[0].inputs.raw_data_windows()[0].width_ms();
    assert!(width_ms > 0);
    assert_eq!(
        width_ms * panes.len() as i64,
        3_600_000,
        "complete hour coverage"
    );
    for (i, pane) in panes.iter().enumerate() {
        let i = i as i64;
        let [window] = pane.inputs.raw_data_windows() else {
            panic!("{pane:?}");
        };
        assert_eq!(
            (window.width_ms(), window.end_offset_ms()),
            (width_ms, i * width_ms)
        );
        assert!(pane.inputs.materialized_views.is_empty());
        assert_eq!(
            pane.coverage().definition_id(),
            panes[0].coverage().definition_id()
        );
        assert_eq!(
            pane.coverage().selection(),
            relative_time(-(i + 1) * width_ms, -i * width_ms)
        );
    }
}

/// PromQL `topk` returns whole input series: `topk by (region) (2,
/// sum_over_time(data[1m]))` answers, per region, the two series with the
/// largest sums, each with its own full label set (region, service and
/// instance; `sum_over_time` drops the metric name), not only `region`.
/// Series `e1`..`e3` are in `east` with sums 6, 60 and 30; `w1`, `w2` in
/// `west` with sums 9 and 3.
#[tokio::test]
async fn topk_answers_carry_the_chosen_series_labels() {
    let workload: PlanningWorkload = serde_json::from_str(
        r#"{"query_workload": {"language": "promql", "repeating_queries": [{
              "query": "topk by (region) (2, sum_over_time(data[1m]))",
              "demand": {"fixed_interval": 60000},
              "requirements": {},
              "predictability": {"predictable": {"known_at": null}}}]},
            "data_workload": {"arrival": {"ingesting": {"rows_per_second": 0.5}},
              "sources": {"data": {"series_count": 5, "sample_interval": 10000}}}}"#,
    )
    .unwrap();
    let plan_json = plan_json_for(workload, &DeploymentCapabilities::default()).await;
    let samples = topk_samples();
    let run = run_data_plane(
        &plan_json,
        &samples,
        EVALUATION_MS,
        &mut SummaryStore::new(),
    );
    assert_full_series_answers(&run, 4);
    let labels = |instance: &str, region: &str, service: &str| {
        BTreeMap::from([
            ("instance".to_string(), instance.to_string()),
            ("region".to_string(), region.to_string()),
            ("service".to_string(), service.to_string()),
        ])
    };
    let mut answered = run.answers_per_query[0].clone();
    answered.sort_by(|a, b| a.0.cmp(&b.0));
    let mut expected = vec![
        (labels("e2", "east", "web"), 60.0),
        (labels("e3", "east", "db"), 30.0),
        (labels("w1", "west", "api"), 9.0),
        (labels("w2", "west", "web"), 3.0),
    ];
    expected.sort_by(|a, b| a.0.cmp(&b.0));
    assert_eq!(answered.len(), expected.len(), "{answered:?}");
    for ((labels, value), (want_labels, want)) in answered.iter().zip(&expected) {
        assert_eq!(labels, want_labels);
        assert!((value - want).abs() < 1e-9, "{labels:?}: {value}");
    }
    assert_eq!(run.answers_per_query, run.reference_answers_per_query);
}

/// Samples of `data` for `topk_answers_carry_the_chosen_series_labels`:
/// three per series inside the last minute, each a third of its sum.
fn topk_samples() -> Vec<RawDataSample> {
    let mut samples = Vec::new();
    for (instance, region, service, sum) in [
        ("e1", "east", "api", 6.0),
        ("e2", "east", "web", 60.0),
        ("e3", "east", "db", 30.0),
        ("w1", "west", "api", 9.0),
        ("w2", "west", "web", 3.0),
    ] {
        // In time order, as a counter's samples arrive.
        for step in (1..=3).rev() {
            let mut sample = sample("data", instance, EVALUATION_MS - step * 15_000, sum / 3.0);
            sample.labels.insert("region".into(), region.into());
            sample.labels.insert("service".into(), service.into());
            samples.push(sample);
        }
    }
    samples
}

/// The first query's `expected` answers, each a whole input series (it
/// carries its `instance`), and the same as the query-time reference's.
fn assert_full_series_answers(run: &DataPlaneRun, expected: usize) {
    let answered = &run.answers_per_query[0];
    assert_eq!(answered.len(), expected, "{answered:?}");
    for (labels, _) in answered {
        assert!(labels.contains_key("instance"), "{labels:?}");
        assert!(labels.contains_key("service"), "{labels:?}");
    }
    assert_eq!(run.answers_per_query, run.reference_answers_per_query);
}

/// Every top-k and bottom-k form, grouped or not, over sums, counts and
/// rates, answers whole input series: `topk(2, ...)` the two of five,
/// `by (region)` per region, `bottomk` likewise.
#[tokio::test]
async fn every_topk_form_answers_whole_series() {
    for (query, expected) in [
        ("topk(2, sum_over_time(data[1m]))", 2),
        ("topk by (region) (1, count_over_time(data[1m]))", 2),
        ("topk by (region) (1, rate(data[1m]))", 2),
        ("bottomk by (region) (1, sum_over_time(data[1m]))", 2),
        ("bottomk(3, sum_over_time(data[1m]))", 3),
    ] {
        let workload: PlanningWorkload = serde_json::from_str(&format!(
            r#"{{"query_workload": {{"language": "promql", "repeating_queries": [{{
                  "query": {query:?},
                  "demand": {{"fixed_interval": 60000}},
                  "requirements": {{}},
                  "predictability": {{"predictable": {{"known_at": null}}}}}}]}},
                "data_workload": {{"arrival": {{"ingesting": {{"rows_per_second": 0.5}}}},
                  "sources": {{"data": {{"series_count": 5, "sample_interval": 10000}}}}}}}}"#
        ))
        .unwrap();
        let plan_json = plan_json_for(workload, &DeploymentCapabilities::default()).await;
        let run = run_data_plane(
            &plan_json,
            &topk_samples(),
            EVALUATION_MS,
            &mut SummaryStore::new(),
        );
        assert_full_series_answers(&run, expected);
    }
}

/// The MVP benchmark's p99 query: the per-series p99 of a gauge over 100 s,
/// averaged per (region, service, job), every second, within 0.05 at
/// failure probability 0.3, over 1,000 `data` series sampled every 100 ms,
/// with `data` declared non-negative or not.
fn p99_avg_workload(values_non_negative: bool) -> PlanningWorkload {
    serde_json::from_str(&format!(
        r#"{{"query_workload": {{"language": "promql", "repeating_queries": [{{
              "query": "avg by (region, service, job) (quantile_over_time(0.99, data[100s]))",
              "demand": {{"fixed_interval": 1000}},
              "requirements": {{"accuracy": {{"max_error": 0.05, "failure_probability": 0.3,
                  "accuracy_scope": "whole_result"}}}},
              "predictability": {{"predictable": {{"known_at": null}}}}}}]}},
            "data_workload": {{"arrival": {{"ingesting": {{"rows_per_second": 10000.0}}}},
              "sources": {{"data": {{"series_count": 1000, "metric_type": "gauge",
                  "sample_interval": 100, "values_non_negative": {values_non_negative}}}}}}}}}"#
    ))
    .unwrap()
}

/// [`p99_avg_workload`] planned for a deployment that keeps no raw data samples.
async fn plan_p99_avg(
    values_non_negative: bool,
) -> Result<(InstallablePlan, Option<SelectedPlanReport>), String> {
    let capabilities = DeploymentCapabilities {
        raw_data_retained: false,
        ..DeploymentCapabilities::default()
    };
    plan_installable(UserInput::new(
        &p99_avg_workload(values_non_negative),
        FrontendInput::Promql { histograms: None },
        PlanningModels::builtin().with_deployment_capabilities(&capabilities),
    ))
    .await
    .map_err(|error| error.to_string())
}

/// Without raw data samples, the only valid plan of the p99 query keeps one
/// DDSketch per series in one-second panes built at ingestion and merges
/// the window's panes at query time. A quantile
/// of merged panes is still one of the samples, so a declared non-negative
/// gauge carries the sketch's relative bound through `avg`: that plan is
/// selected, its answers equal the query-time reference's and are within
/// 5% of the exact averages.
#[tokio::test]
async fn a_non_negative_gauges_merged_ddsketch_panes_carry_the_relative_bound_through_avg() {
    let (plan, selection) = plan_p99_avg(true).await.expect("a valid plan");
    let selection = selection.as_ref().expect("Stage 3's report");
    let views = plan.materialized_views().unwrap();
    assert!(selection.cost_per_second.is_some());
    assert!(!views.is_empty(), "panes built at ingestion");
    assert_eq!(
        stored_definitions(&plan, "DDSketch").len(),
        1,
        "one DDSketch"
    );
    assert!(
        plan.raw_data_retention().unwrap().is_empty(),
        "no raw samples read"
    );
    // The query's guarantee, with `data`'s declared sign and its 1,000
    // series bound as Stage 3 binds them: a relative error of at most 0.05
    // at failure probability at most 0.3 (DDSketch's bound is
    // deterministic).
    let root = plan.dag.roots[0];
    let guarantee = plan
        .dag
        .nodes
        .iter()
        .find(|node| node.id == root)
        .and_then(|node| node.guarantee.clone())
        .expect("the query's guarantee");
    let sign = asap_accuracy::test_support::raw_data_samples_sign_statistic("data");
    let bound = guarantee.bind(&|statistic| match statistic {
        "input_row_count" => Some(1_000.0),
        _ => (statistic == sign).then_some(1.0),
    });
    assert_eq!(
        bound.spec.accuracy_metric,
        asap_types::ir::properties::AccuracyMetric::Relative
    );
    let error = bound.spec.error.evaluate().expect("a numeric bound");
    assert!(error <= 0.05, "{error}");
    let failure = bound
        .spec
        .failure_probability
        .evaluate()
        .expect("a numeric failure probability");
    assert!(failure <= 0.3, "{failure}");

    // Eight non-negative series, one sample every 100 ms over the last
    // 110 s: two per (region, service, job) group.
    let evaluation_ms = EVALUATION_MS;
    let mut samples = Vec::new();
    let mut per_group: BTreeMap<(String, String, String), Vec<Vec<f64>>> = BTreeMap::new();
    for series in 0..8 {
        let region = format!("r{}", series % 2);
        let service = format!("s{}", series / 4);
        let job = "j".to_string();
        let mut window = Vec::new();
        for step in 0..1_100i64 {
            let timestamp_ms = evaluation_ms - 110_000 + 50 + step * 100;
            let value = 10.0 * (series + 1) as f64 + ((step * 37 + series * 11) % 997) as f64;
            samples.push(RawDataSample {
                labels: BTreeMap::from([
                    ("__name__".to_string(), "data".to_string()),
                    ("region".to_string(), region.clone()),
                    ("service".to_string(), service.clone()),
                    ("job".to_string(), job.clone()),
                    ("instance".to_string(), format!("i{series}")),
                ]),
                timestamp_ms,
                value,
            });
            if timestamp_ms > evaluation_ms - 100_000 {
                window.push(value);
            }
        }
        per_group
            .entry((region, service, job))
            .or_default()
            .push(window);
    }
    let run = run_data_plane(
        &serde_json::to_string(&plan).unwrap(),
        &samples,
        evaluation_ms,
        &mut SummaryStore::new(),
    );
    assert_eq!(run.answers_per_query, run.reference_answers_per_query);
    let answers = &run.answers_per_query[0];
    assert_eq!(answers.len(), per_group.len());
    for (labels, value) in answers {
        let key = (
            labels["region"].clone(),
            labels["service"].clone(),
            labels["job"].clone(),
        );
        // The exact answer: the mean of the group's per-series p99s, each
        // PromQL's interpolated quantile of the series' window.
        let exact = per_group[&key]
            .iter()
            .map(|window| {
                let mut sorted = window.clone();
                sorted.sort_by(f64::total_cmp);
                let rank = 0.99 * (sorted.len() - 1) as f64;
                let (low, high) = (rank.floor() as usize, rank.ceil() as usize);
                sorted[low] + (sorted[high] - sorted[low]) * (rank - low as f64)
            })
            .sum::<f64>()
            / per_group[&key].len() as f64;
        assert!(
            (value - exact).abs() <= 0.05 * exact,
            "{labels:?}: {value} vs exact {exact}"
        );
    }
}

/// Without a non-negative declaration, relative sketch error cannot propagate
/// through the average, so this no-raw deployment has no valid plan: plan
/// selection selects none. Pass 1 prunes the quantile sketches for that
/// reason (`NoErrorBoundForQueryTarget`) and Stage 2 the exact candidates,
/// which scan raw data at query time.
#[tokio::test]
async fn an_undeclared_gauges_merged_ddsketch_panes_get_no_relative_bound_through_avg() {
    let error = plan_p99_avg(false).await.expect_err("no valid plan");
    assert_plan_selection_selected_none(&error);
    // Pass 1 prunes both quantile sketches for that reason: DDSketch's
    // relative bound does not pass through the average of an undeclared
    // gauge, and no rule carries KLL's rank bound through it.
    let capabilities = DeploymentCapabilities {
        raw_data_retained: false,
        ..DeploymentCapabilities::default()
    };
    let models = PlanningModels::builtin().with_deployment_capabilities(&capabilities);
    let no_bound = pass1_pruned_for_no_error_bound(&p99_avg_workload(false), models);
    assert!(
        no_bound.contains(&"DDSketch".to_string()) && no_bound.contains(&"Kll".to_string()),
        "{no_bound:?}"
    );
    // Declared non-negative, DDSketch's bound passes through the average.
    let no_bound = pass1_pruned_for_no_error_bound(&p99_avg_workload(true), models);
    assert!(!no_bound.contains(&"DDSketch".to_string()), "{no_bound:?}");
}

/// The summaries Pass 1 pruned planning `workload` (PromQL) with `models`
/// because no query reading them can meet its target
/// ([`PruneReason::NoErrorBoundForQueryTarget`]), as the facade's planning
/// run reports them (`MemoSteps::pass1_pruned_candidates`): each family's
/// name (e.g. `"Kll"`).
fn pass1_pruned_for_no_error_bound(
    workload: &PlanningWorkload,
    models: PlanningModels<'_>,
) -> Vec<String> {
    let roots = asap_frontend_promql::lower_promql_query_workload(workload)
        .unwrap()
        .into_iter()
        .map(|root| match root {
            QueryRoot::Operator(node) => {
                QueryRoot::Operator(with_promql_series_identity(&node).unwrap_or(node))
            }
            scalar => scalar,
        })
        .collect();
    let entries: Vec<_> = workload.query_workload.entries().collect();
    let data = workload.data_workload.clone().unwrap_or_default();
    let run = run_planning(
        roots,
        &entries,
        &data,
        models,
        PlanSearchSettings {
            time_limit_ms: Some(5_000),
        },
    )
    .unwrap();
    run.per_computation_candidates
        .expect("Pass 1's output")
        .pruned_candidates
        .iter()
        .filter(|pruned| pruned.reason == PruneReason::NoErrorBoundForQueryTarget)
        .filter_map(|pruned| match &pruned.candidate {
            HowToComputeAggregateMeasure::Summary(family) => Some(format!("{family:?}")),
            _ => None,
        })
        .collect()
}

/// `error` says plan selection found no valid plan: no candidate is valid,
/// or none was validated by the deadline. Not a solver, realizer or input
/// failure.
fn assert_plan_selection_selected_none(error: &str) {
    assert!(
        error.contains("plan selection: no valid candidate")
            || error.contains("plan selection: no plan by the deadline")
            || error.contains("the planning budget ended before a plan was selected"),
        "{error}"
    );
}

/// `avg by (g) (quantile_over_time(0.99, x[100s]))` every 10 s at rank
/// error 0.01 and δ 0.01, over 100 ingesting `x` series sampled every
/// second: ASAPQuery's random mixed workload query. Stage 1 has no
/// analytical rule for an average of quantile estimates, so every sketch
/// candidate has no guarantee.
fn average_of_p99s_workload() -> PlanningWorkload {
    serde_json::from_str(
        r#"{"query_workload": {"language": "promql", "repeating_queries": [{
              "query": "avg by (g) (quantile_over_time(0.99, x[100s]))",
              "demand": {"fixed_interval": 10000},
              "requirements": {"accuracy": {"accuracy_metric": "rank", "max_error": 0.01,
                  "failure_probability": 0.01, "accuracy_scope": "per_value"}},
              "predictability": {"predictable": {"known_at": null}}}]},
            "data_workload": {"arrival": {"ingesting": {"rows_per_second": 100.0}},
              "sources": {"x": {"series_count": 100, "sample_interval": 1000}}}}"#,
    )
    .unwrap()
}

/// Raw `x` samples of series `a` and `b` in group `g="1"` and `c` in group
/// `g="2"`, one every second from 200 s before `EVALUATION_MS` to it, off
/// the second boundaries: series `i`'s value is `100 × i + 1..=101`, all
/// positive, which DDSketch needs at run time (nothing declares it here).
fn grouped_samples() -> Vec<RawDataSample> {
    let mut samples = Vec::new();
    for (i, (instance, g)) in [("a", "1"), ("b", "1"), ("c", "2")].iter().enumerate() {
        for step in 0..200 {
            samples.push(RawDataSample {
                labels: BTreeMap::from([
                    ("__name__".to_string(), "x".to_string()),
                    ("instance".to_string(), instance.to_string()),
                    ("g".to_string(), g.to_string()),
                ]),
                timestamp_ms: EVALUATION_MS - 199_500 + step * 1_000,
                value: (100 * i) as f64 + 1.0 + ((step * 37 + 11 * i as i64) % 101) as f64,
            });
        }
    }
    samples
}

/// Quantile queries over one metric and window with different `by` labels
/// (ASAPQuery's seed-862 quantile queries) share one per-series DDSketch:
/// planned jointly on a deployment that keeps no raw data samples, every stored
/// DDSketch has one definition, and each query answers as its query-time
/// reference. Series of `data` carry `region`, `service` and `job`; values
/// are positive.
#[tokio::test]
async fn quantile_queries_with_different_groupings_share_one_per_series_sketch() {
    let plan_json = jointly_planned(
        &[
            "avg by (region) (quantile_over_time(0.99, data[100s]))",
            "avg by (region, service, job) (quantile_over_time(0.9, data[100s]))",
            "avg by (service) (quantile_over_time(0.5, data[100s]))",
        ],
        r#"{"accuracy": {"accuracy_metric": "relative", "max_error": 0.05,
            "failure_probability": 0.01, "accuracy_scope": "per_value"}}"#,
    )
    .await;
    let plan: InstallablePlan = serde_json::from_str(&plan_json).unwrap();
    let sketches = stored_definitions(&plan, "DDSketch");
    assert_eq!(sketches.len(), 1, "{sketches:?}");
    let run = run_data_plane(
        &plan_json,
        &grouped_series_samples(),
        EVALUATION_MS,
        &mut SummaryStore::new(),
    );
    assert_eq!(run.answers_per_query[0].len(), 2, "one answer per region");
    assert_eq!(run.answers_per_query[1].len(), 6);
    assert_eq!(run.answers_per_query[2].len(), 3, "one answer per service");
    assert_eq!(run.answers_per_query, run.reference_answers_per_query);
}

/// The six series of `data` the sharing tests read: `region` east/west,
/// `service` api/web/db, `job` batch/online; values positive, one every
/// 10 s over the last two minutes, in time order.
fn grouped_series_samples() -> Vec<RawDataSample> {
    let mut samples = Vec::new();
    for (instance, region, service, job) in [
        ("s1", "east", "api", "online"),
        ("s2", "east", "web", "online"),
        ("s3", "east", "db", "batch"),
        ("s4", "west", "api", "online"),
        ("s5", "west", "web", "batch"),
        ("s6", "west", "db", "batch"),
    ] {
        let base: f64 = instance[1..].parse().unwrap();
        for step in (0..12).rev() {
            let mut sample = sample(
                "data",
                instance,
                EVALUATION_MS - 5_000 - step * 10_000,
                base * 10.0 + ((step * 37) % 11) as f64,
            );
            sample.labels.insert("region".into(), region.into());
            sample.labels.insert("service".into(), service.into());
            sample.labels.insert("job".into(), job.into());
            samples.push(sample);
        }
    }
    samples
}

/// The exact answer of `avg by (g) (quantile_over_time(0.99, x[100s]))` at
/// `EVALUATION_MS` over [`grouped_samples`]: PromQL's p99 (linear
/// interpolation at rank `0.99 × (n − 1)`) of each series' samples in
/// `(EVALUATION_MS − 100 s, EVALUATION_MS]`, averaged per group.
fn exact_average_of_p99s(samples: &[RawDataSample]) -> BTreeMap<String, f64> {
    let mut per_series: BTreeMap<(String, String), Vec<f64>> = BTreeMap::new();
    for s in samples
        .iter()
        .filter(|s| EVALUATION_MS - 100_000 < s.timestamp_ms && s.timestamp_ms <= EVALUATION_MS)
    {
        per_series
            .entry((s.labels["g"].clone(), s.labels["instance"].clone()))
            .or_default()
            .push(s.value);
    }
    let mut per_group: BTreeMap<String, Vec<f64>> = BTreeMap::new();
    for ((g, _), mut values) in per_series {
        values.sort_by(f64::total_cmp);
        let rank = 0.99 * (values.len() - 1) as f64;
        let (low, high) = (rank.floor() as usize, rank.ceil() as usize);
        let p99 = values[low] + (values[high] - values[low]) * (rank - low as f64);
        per_group.entry(g).or_default().push(p99);
    }
    per_group
        .into_iter()
        .map(|(g, p99s)| (g, p99s.iter().sum::<f64>() / p99s.len() as f64))
        .collect()
}

/// Issue #774: without raw data samples retained, every candidate of an average
/// of p99s fails a check (the exact ones scan raw data samples at query time,
/// the sketch ones have no guarantee), so planning fails. With
/// `PlanningModels::ignore_accuracy_guarantees`, the cheapest
/// sketch plan is selected, the report marks its output as accepted without
/// a guarantee, and it runs: equal to its query-time reference and within a
/// loose tolerance of the exact answer.
#[tokio::test]
async fn ignoring_accuracy_guarantees_selects_a_sketch_plan_that_runs() {
    let workload = average_of_p99s_workload();
    let capabilities = DeploymentCapabilities {
        raw_data_retained: false,
        ..DeploymentCapabilities::default()
    };
    let plan = |ignore: bool| {
        let workload = workload.clone();
        let capabilities = capabilities.clone();
        async move {
            plan_installable(UserInput::new(
                &workload,
                FrontendInput::Promql { histograms: None },
                PlanningModels::builtin()
                    .with_deployment_capabilities(&capabilities)
                    .with_ignore_accuracy_guarantees(ignore),
            ))
            .await
        }
    };
    // Without the setting, no candidate is valid.
    let error = plan(false)
        .await
        .expect_err("no candidate is valid without the setting");
    assert_plan_selection_selected_none(&error.to_string());
    // Pass 1 prunes every quantile sketch for it: no rule carries a rank
    // bound through the average, and a relative one does not meet a rank
    // target. With the setting, Stage 3 may accept an output without its
    // guarantee, so Pass 1 prunes none.
    let models = PlanningModels::builtin().with_deployment_capabilities(&capabilities);
    let no_bound = pass1_pruned_for_no_error_bound(&workload, models);
    assert!(
        no_bound.contains(&"DDSketch".to_string()) && no_bound.contains(&"Kll".to_string()),
        "{no_bound:?}"
    );
    assert_eq!(
        pass1_pruned_for_no_error_bound(&workload, models.with_ignore_accuracy_guarantees(true)),
        Vec::<String>::new()
    );
    // With it.
    let (installed, report) = plan(true).await.expect("a sketch plan is selected");
    // A per-series KLL or DDSketch plan.
    let families: Vec<String> = installed
        .dag
        .nodes
        .iter()
        .filter_map(|node| match &node.payload {
            asap_executor::planner::ir::physical_export::PhysicalASAPOperatorPayload::ASAP(
                ASAPOp::SummaryAgg { family, .. } | ASAPOp::PaneChain { family, .. },
            ) => Some(format!("{family:?}")),
            _ => None,
        })
        .collect();
    // Its sketches (beside the average's exact sum and count) are KLL or
    // DDSketch.
    let sketches: Vec<&String> = families
        .iter()
        .filter(|f| f.starts_with("Sketch"))
        .collect();
    assert!(
        !sketches.is_empty()
            && sketches
                .iter()
                .all(|f| f.contains("Kll") || f.contains("DDSketch")),
        "{families:?}"
    );
    // The report marks the query's output as accepted without a guarantee.
    let report = report.as_ref().expect("Stage 3's report");
    assert_eq!(report.queries_accepted_without_guarantee, vec![0]);
    // It runs, equal to its query-time reference ...
    let plan_json = serde_json::to_string(&installed).unwrap();
    let samples = grouped_samples();
    let run = run_data_plane(
        &plan_json,
        &samples,
        EVALUATION_MS,
        &mut SummaryStore::new(),
    );
    assert_eq!(run.answers_per_query, run.reference_answers_per_query);
    // ... and within 5 (of values spanning 100 per series) of the exact
    // average of p99s, per group.
    let exact = exact_average_of_p99s(&samples);
    let answered: BTreeMap<String, f64> = run.answers_per_query[0]
        .iter()
        .map(|(labels, value)| (labels["g"].clone(), *value))
        .collect();
    assert_eq!(
        answered.keys().collect::<Vec<_>>(),
        exact.keys().collect::<Vec<_>>()
    );
    for (g, value) in &answered {
        assert!(
            (value - exact[g]).abs() <= 5.0,
            "g={g}: {value} vs exact {}",
            exact[g]
        );
    }
}

/// The MVP benchmark's p99 query over an undeclared gauge, which has no
/// valid plan with the built-in model, gets one with
/// `ignore_accuracy_guarantees`, and the report marks its output as
/// accepted without a guarantee.
#[tokio::test]
async fn ignoring_accuracy_guarantees_plans_the_undeclared_p99_query() {
    let capabilities = DeploymentCapabilities {
        raw_data_retained: false,
        ..DeploymentCapabilities::default()
    };
    let settings = CostInputs::default();
    let (plan, report) = plan_installable(UserInput::new(
        &p99_avg_workload(false),
        FrontendInput::Promql { histograms: None },
        PlanningModels::builtin()
            .with_deployment_capabilities(&capabilities)
            .with_plan_selection(&settings)
            .with_ignore_accuracy_guarantees(true),
    ))
    .await
    .expect("a plan is selected");
    let report = report.as_ref().expect("Stage 3's report");
    assert_eq!(report.queries_accepted_without_guarantee, vec![0]);
    assert!(
        plan.raw_data_retention().unwrap().is_empty(),
        "no raw samples read"
    );
}

/// [`p99_avg_workload`]'s query with `outer` in place of `avg`, planned
/// for a deployment that keeps no raw data samples.
async fn plan_p99(
    outer: &str,
    values_non_negative: bool,
    ignore_accuracy: bool,
) -> (InstallablePlan, Option<SelectedPlanReport>) {
    let mut json = serde_json::to_value(p99_avg_workload(values_non_negative)).unwrap();
    json["query_workload"]["repeating_queries"][0]["query"] =
        format!("{outer} by (region, service, job) (quantile_over_time(0.99, data[100s]))").into();
    let workload: PlanningWorkload = serde_json::from_value(json).unwrap();
    let capabilities = DeploymentCapabilities {
        raw_data_retained: false,
        // A DDSketch kernel fixture, not a comparison of sketch families.
        supported_summaries: ignore_accuracy
            .then(|| only_summary_family(SketchAlgorithm::DDSketch)),
        ..DeploymentCapabilities::default()
    };
    let settings = CostInputs::default();
    let models = PlanningModels::builtin()
        .with_deployment_capabilities(&capabilities)
        .with_plan_selection(&settings)
        .with_ignore_accuracy_guarantees(ignore_accuracy);
    plan_installable(UserInput::new(
        &workload,
        FrontendInput::Promql { histograms: None },
        models,
    ))
    .await
    .expect("a valid plan")
}

/// `data` samples of series `0..series`, `value(series, step)` every
/// `interval_ms` over the `steps` steps ending just before `EVALUATION_MS`,
/// two series per (region, service, job) group; and each group's series'
/// windows `(EVALUATION_MS - 100 s, EVALUATION_MS]`.
#[allow(clippy::type_complexity)]
fn data_samples(
    series: usize,
    steps: i64,
    interval_ms: i64,
    value: impl Fn(usize, i64) -> f64,
) -> (
    Vec<RawDataSample>,
    BTreeMap<(String, String, String), Vec<Vec<f64>>>,
) {
    let mut samples = Vec::new();
    let mut per_group: BTreeMap<(String, String, String), Vec<Vec<f64>>> = BTreeMap::new();
    for s in 0..series {
        let (region, service, job) = (
            format!("r{}", s % 2),
            format!("s{}", s / 4),
            "j".to_string(),
        );
        let mut window = Vec::new();
        for step in 0..steps {
            let timestamp_ms = EVALUATION_MS - steps * interval_ms + 50 + step * interval_ms;
            let v = value(s, step);
            samples.push(RawDataSample {
                labels: BTreeMap::from([
                    ("__name__".to_string(), "data".to_string()),
                    ("region".to_string(), region.clone()),
                    ("service".to_string(), service.clone()),
                    ("job".to_string(), job.clone()),
                    ("instance".to_string(), format!("i{s}")),
                ]),
                timestamp_ms,
                value: v,
            });
            if timestamp_ms > EVALUATION_MS - 100_000 {
                window.push(v);
            }
        }
        per_group
            .entry((region, service, job))
            .or_default()
            .push(window);
    }
    (samples, per_group)
}

/// PromQL's interpolated `quantile_over_time(q, ...)` of `window`.
fn interpolated_quantile(q: f64, window: &[f64]) -> f64 {
    let mut sorted = window.to_vec();
    sorted.sort_by(f64::total_cmp);
    let rank = q * (sorted.len() - 1) as f64;
    let (low, high) = (rank.floor() as usize, rank.ceil() as usize);
    sorted[low] + (sorted[high] - sorted[low]) * (rank - low as f64)
}

/// Runs `plan` over `samples` and checks every group's answer against
/// `combine` of its series' exact p99s, within the relative error 0.05, and
/// against the query-time reference.
fn assert_p99s_within_alpha(
    plan: &InstallablePlan,
    samples: &[RawDataSample],
    per_group: &BTreeMap<(String, String, String), Vec<Vec<f64>>>,
    combine: impl Fn(&[f64]) -> f64,
) {
    let run = run_data_plane(
        &serde_json::to_string(plan).unwrap(),
        samples,
        EVALUATION_MS,
        &mut SummaryStore::new(),
    );
    assert_eq!(run.answers_per_query, run.reference_answers_per_query);
    let answers = &run.answers_per_query[0];
    assert_eq!(answers.len(), per_group.len());
    for (labels, value) in answers {
        let key = (
            labels["region"].clone(),
            labels["service"].clone(),
            labels["job"].clone(),
        );
        let p99s: Vec<f64> = per_group[&key]
            .iter()
            .map(|window| interpolated_quantile(0.99, window))
            .collect();
        let exact = combine(&p99s);
        assert!(
            (value - exact).abs() <= 0.05 * exact.abs(),
            "{labels:?}: {value} vs exact {exact}"
        );
    }
}

/// A non-negative gauge that reports exactly 0 is still summarized by the
/// selected DDSketch panes (zero has its own bucket): each series' window
/// holds the samples 0, 0, 3 and 5 scaled by the series, and every answer
/// equals the query-time reference's and is within the relative error 0.05
/// of the exact average of PromQL's interpolated p99s.
#[tokio::test]
async fn a_ddsketch_over_a_gauge_reporting_zero_runs_within_alpha() {
    let (plan, _) = plan_p99("avg", true, false).await;
    let views = plan.materialized_views().unwrap();
    assert!(!views.is_empty(), "panes built at ingestion");
    let (samples, per_group) = data_samples(8, 4, 20_000, |s, step| {
        (s + 1) as f64 * [0.0, 0.0, 3.0, 5.0][step as usize]
    });
    assert!(per_group.values().flatten().all(|window| window.len() == 4));
    let average = |p99s: &[f64]| p99s.iter().sum::<f64>() / p99s.len() as f64;
    assert_p99s_within_alpha(&plan, &samples, &per_group, average);
    // Many samples, a third of them zero.
    let (samples, per_group) = data_samples(8, 1_100, 100, |s, step| {
        (s + 1) as f64 * [0.0, 3.0, 5.0][(step % 3) as usize]
    });
    assert_p99s_within_alpha(&plan, &samples, &per_group, average);
}

/// Signed samples (negative, zero and positive) are summarized by the
/// selected DDSketch panes, through their negative store and zero bucket:
/// Signed interpolation has no general relative guarantee, so this kernel
/// fixture explicitly bypasses accuracy admission. Every answer equals the
/// query-time reference's and is
/// within 0.05 of the exact value, including series whose samples are all
/// negative.
#[tokio::test]
async fn a_ddsketch_over_signed_samples_runs_within_alpha() {
    let (plan, report) = plan_p99("max", false, true).await;
    let views = plan.materialized_views().unwrap();
    assert!(
        !stored_definitions(&plan, "DDSketch").is_empty(),
        "exercise signed DDS state"
    );
    assert_eq!(
        report.as_ref().unwrap().queries_accepted_without_guarantee,
        vec![0]
    );
    assert!(!views.is_empty(), "panes built at ingestion");
    let max = |p99s: &[f64]| p99s.iter().copied().fold(f64::NEG_INFINITY, f64::max);
    // Series 0..4 cross zero; series 4..8 are all negative.
    let (samples, per_group) = data_samples(8, 1_100, 100, |s, step| {
        let scale = (s + 1) as f64;
        if s < 4 {
            scale * [-7.0, -3.0, 0.0, 2.0, 4.0][(step % 5) as usize]
        } else {
            scale * [-9.0, -8.0, -5.0, -1.0][(step % 4) as usize]
        }
    });
    assert_p99s_within_alpha(&plan, &samples, &per_group, max);
}

/// The ASAPQuery workloads' counter top-k query `topk(10, sum by (job)
/// (<f>(data_total[5m])))` for `f` `increase` or `rate`, every minute
/// within 0.05 at failure probability 0.3, over 24 `data_total` counter
/// series sampled every 10 s.
fn counter_topk_workload(function: &str) -> PlanningWorkload {
    let query = format!("topk(10, sum by (job) ({function}(data_total[5m])))");
    serde_json::from_str(&format!(
        r#"{{"query_workload": {{"language": "promql", "repeating_queries": [{{
              "query": {query:?},
              "demand": {{"fixed_interval": 60000}},
              "requirements": {{"accuracy": {{"max_error": 0.05, "failure_probability": 0.3}}}},
              "predictability": {{"predictable": {{"known_at": null}}}}}}]}},
            "data_workload": {{"arrival": {{"ingesting": {{"rows_per_second": 100000.0}}}},
              "sources": {{"data_total": {{"series_count": 10000, "metric_type": "counter",
                  "sample_interval": 100, "distinct_values_per_label": {{"job": 12}}}}}}}}}}"#
    ))
    .unwrap()
}

/// The per-second rate of series `instance` (1 or 2) of job `job` (1..=12)
/// in [`counter_raw_data_samples`]: distinct for every series and every job's sum.
fn counter_rate(job: i64, instance: i64) -> f64 {
    job as f64 + 0.1 * instance as f64
}

/// `data_total` samples of 12 jobs with 2 series each, every 10 s from
/// 595 s before `EVALUATION_MS`: each a counter growing linearly at
/// [`counter_rate`], so its 5-minute `increase` is 300 × its rate (the
/// samples 5 s inside each window end extrapolate to them exactly).
fn counter_raw_data_samples() -> Vec<RawDataSample> {
    let mut samples = Vec::new();
    for job in 1..=12 {
        for instance in 1..=2 {
            for step in 0..60 {
                let mut s = sample(
                    "data_total",
                    &format!("{job}-{instance}"),
                    EVALUATION_MS - 595_000 + step * 10_000,
                    counter_rate(job, instance) * (5 + step * 10) as f64,
                );
                s.labels.insert("job".into(), format!("j{job:02}"));
                samples.push(s);
            }
        }
    }
    samples
}

/// Issue: with guarantees ignored and no raw data samples, the counter top-k
/// queries selected exact per-series panes followed by a query-time
/// CountSketchWithHeap top-k the executor cannot compile ("invalid keyed
/// evaluation shape or capacity"). The selected plan must be one the
/// executor compiles; it runs, equal to its query-time reference and to the
/// exact top 10 jobs (jobs 3..=12, each the sum of its two series).
#[tokio::test]
async fn ignoring_guarantees_counter_topk_plans_compile_and_answer_exactly() {
    let capabilities = DeploymentCapabilities {
        raw_data_retained: false,
        ..DeploymentCapabilities::default()
    };
    let settings = CostInputs::default();
    for (function, seconds) in [("increase", 300.0), ("rate", 1.0)] {
        let plan = plan_installable(UserInput::new(
            &counter_topk_workload(function),
            FrontendInput::Promql { histograms: None },
            PlanningModels::builtin()
                .with_deployment_capabilities(&capabilities)
                .with_plan_selection(&settings)
                .with_ignore_accuracy_guarantees(true),
        ))
        .await
        .unwrap_or_else(|error| panic!("{function}: {error}"))
        .0;
        // An exact plan: no heap over the per-job sums, which have one row
        // per job.
        assert!(
            !plan.dag.nodes.iter().any(|node| matches!(
                &node.payload,
                asap_executor::planner::ir::physical_export::PhysicalASAPOperatorPayload::ASAP(
                    ASAPOp::SummaryAgg {
                        family: asap_executor::planner::ir::schema::FieldDataType::Sketch(..),
                        ..
                    }
                )
            )),
            "{function}"
        );
        // The plan compiles as a runtime compiles it.
        plan.check_compiles().unwrap();
        let plan_json = serde_json::to_string(&plan).unwrap();
        let run = run_data_plane(
            &plan_json,
            &counter_raw_data_samples(),
            EVALUATION_MS,
            &mut SummaryStore::new(),
        );
        assert_eq!(
            run.answers_per_query, run.reference_answers_per_query,
            "{function}"
        );
        let expected: Vec<(String, f64)> = (3..=12)
            .map(|job| {
                (
                    format!("j{job:02}"),
                    (counter_rate(job, 1) + counter_rate(job, 2)) * seconds,
                )
            })
            .collect();
        let mut answered: Vec<(String, f64)> = run.answers_per_query[0]
            .iter()
            .map(|(labels, value)| (labels["job"].clone(), *value))
            .collect();
        answered.sort_by(|a, b| a.0.cmp(&b.0));
        assert_eq!(answered.len(), expected.len(), "{function}: {answered:?}");
        for ((job, value), (want_job, want)) in answered.iter().zip(&expected) {
            assert_eq!(job, want_job, "{function}");
            assert!(
                (value - want).abs() <= 1e-9 * want,
                "{function} {job}: {value} vs {want}"
            );
        }
    }
}

/// `InstallablePlan::check_compiles` reports what the Planner's executor
/// refuses: the counter top-k plan with its answer column's type changed
/// from Float64 to Utf8 does not compile, with the executor's reason.
#[tokio::test]
async fn check_compiles_reports_the_executors_error() {
    let capabilities = DeploymentCapabilities {
        raw_data_retained: false,
        ..DeploymentCapabilities::default()
    };
    let (mut plan, _) = plan_installable(UserInput::new(
        &counter_topk_workload("rate"),
        FrontendInput::Promql { histograms: None },
        PlanningModels::builtin().with_deployment_capabilities(&capabilities),
    ))
    .await
    .unwrap();
    plan.check_compiles().unwrap();
    let root = plan.dag.roots[0];
    let node = plan
        .dag
        .nodes
        .iter_mut()
        .find(|node| node.id == root)
        .unwrap();
    let answer = node
        .output_schema
        .fields
        .iter_mut()
        .find(|field| field.name == "topk_10")
        .unwrap();
    answer.dtype = asap_executor::planner::ir::schema::FieldDataType::Plain(
        asap_executor::planner::ir::schema::DataType::Utf8,
    );
    let error = plan.check_compiles().unwrap_err();
    assert!(
        error.starts_with("the executor cannot compile the plan's query run: "),
        "{error}"
    );
}

/// `queries` every 10 s at `accuracy` (JSON) over six ingesting series of
/// `data`, a non-negative gauge, planned jointly on a deployment that keeps
/// no raw data samples: the plan file.
async fn jointly_planned(queries: &[&str], accuracy: &str) -> String {
    let entries: Vec<String> = queries
        .iter()
        .map(|query| {
            format!(
                r#"{{"query": {query:?}, "demand": {{"fixed_interval": 10000}},
                  "requirements": {accuracy},
                  "predictability": {{"predictable": {{"known_at": null}}}}}}"#
            )
        })
        .collect();
    let workload: PlanningWorkload = serde_json::from_str(&format!(
        r#"{{"query_workload": {{"language": "promql", "repeating_queries": [{}]}},
            "data_workload": {{"arrival": {{"ingesting": {{"rows_per_second": 0.6}}}},
              "sources": {{"data": {{"series_count": 6, "sample_interval": 10000,
                "metric_type": "gauge", "values_non_negative": true}}}}}}}}"#,
        entries.join(",")
    ))
    .unwrap();
    let capabilities = DeploymentCapabilities {
        raw_data_retained: false,
        ..DeploymentCapabilities::default()
    };
    plan_json_for(workload, &capabilities).await
}

/// The definition ids of `plan`'s materialized views whose node's payload
/// mentions `family` (e.g. `"DDSketch"`, `"Sum"`).
fn stored_definitions(plan: &InstallablePlan, family: &str) -> BTreeSet<String> {
    plan.materialized_views()
        .unwrap()
        .iter()
        .filter(|view| {
            let node = plan
                .dag
                .nodes
                .iter()
                .find(|n| n.id == view.node_id)
                .unwrap();
            serde_json::to_string(&node.payload)
                .unwrap()
                .contains(family)
        })
        .map(|view| view.coverage().definition_id().to_owned())
        .collect()
}

/// A fine and a coarse sum over one metric and window (ASAPQuery's
/// sum-fine and sum-coarse) store one Sum: both group sums are computed
/// from one stored per-series Sum (Stage 1 shares the per-series sums the
/// two queries' scans read), so every stored Sum has one definition, and
/// both answer as the query-time reference.
#[tokio::test]
async fn fine_and_coarse_sums_store_one_sum() {
    let plan_json = jointly_planned(
        &[
            "sum by (region, service, job) (sum_over_time(data[100s]))",
            "sum by (region) (sum_over_time(data[100s]))",
        ],
        "{}",
    )
    .await;
    let plan: InstallablePlan = serde_json::from_str(&plan_json).unwrap();
    let sums = stored_definitions(&plan, "\"Sum\"");
    assert_eq!(sums.len(), 1, "{sums:?}");
    let run = run_data_plane(
        &plan_json,
        &grouped_series_samples(),
        EVALUATION_MS,
        &mut SummaryStore::new(),
    );
    assert_eq!(run.answers_per_query[0].len(), 6);
    assert_eq!(run.answers_per_query[1].len(), 2, "one sum per region");
    assert_eq!(run.answers_per_query, run.reference_answers_per_query);
}

/// Per-series answers planned beside a grouping of the same per-series
/// values (over a scan with other labels) answer as their query-time
/// reference, and the two share one stored per-series state: a top-k of
/// per-series sums keeps each chosen series' labels, and per-series
/// DDSketch quantiles keep every series' labels.
#[tokio::test]
async fn per_series_answers_beside_a_grouping_share_one_state_and_answer_as_the_reference() {
    for (queries, family, accuracy, answers) in [
        (
            [
                "topk(2, sum_over_time(data[100s]))",
                "sum by (region) (sum_over_time(data[100s]))",
            ],
            "\"Sum\"",
            "{}",
            2,
        ),
        (
            [
                "quantile_over_time(0.9, data[100s])",
                "avg by (region) (quantile_over_time(0.99, data[100s]))",
            ],
            "DDSketch",
            r#"{"accuracy": {"accuracy_metric": "relative", "max_error": 0.05,
                "failure_probability": 0.01, "accuracy_scope": "per_value"}}"#,
            6,
        ),
    ] {
        let plan_json = jointly_planned(&queries, accuracy).await;
        let plan: InstallablePlan = serde_json::from_str(&plan_json).unwrap();
        assert_eq!(stored_definitions(&plan, family).len(), 1, "{queries:?}");
        let run = run_data_plane(
            &plan_json,
            &grouped_series_samples(),
            EVALUATION_MS,
            &mut SummaryStore::new(),
        );
        assert_eq!(run.answers_per_query[0].len(), answers, "{queries:?}");
        assert_eq!(run.answers_per_query[1].len(), 2, "{queries:?}: per region");
        assert_eq!(
            run.answers_per_query, run.reference_answers_per_query,
            "{queries:?}"
        );
    }
}

/// A heap top-k query `query` (the ASAPQuery workloads' top-k-sum or
/// counter top-k shape) every 10 s, within 0.05 at failure probability 0.3,
/// over 10,000 series of `metric` sampled every 100 ms, with `facts` (its
/// type and sign, as JSON fields).
fn heap_topk_workload(query: &str, metric: &str, facts: &str) -> PlanningWorkload {
    serde_json::from_str(&format!(
        r#"{{"query_workload": {{"language": "promql", "repeating_queries": [{{
              "query": {query:?},
              "demand": {{"fixed_interval": 10000}},
              "requirements": {{"accuracy": {{"max_error": 0.05, "failure_probability": 0.3}}}},
              "predictability": {{"predictable": {{"known_at": null}}}}}}]}},
            "data_workload": {{"arrival": {{"ingesting": {{"rows_per_second": 100000.0}}}},
              "sources": {{"{metric}": {{"series_count": 10000, "sample_interval": 100,
                  {facts}}}}}}}}}"#
    ))
    .unwrap()
}

/// The executor's support for `family` alone: a deployment whose only
/// sketch family is `family` (exact plans stay allowed).
fn only_summary_family(family: SketchAlgorithm) -> Vec<asap_planner::SupportedSummary> {
    asap_executor::supported_summary_families()
        .into_iter()
        .filter(|supported| supported.family == family)
        .collect()
}

/// A deployment's accuracy model that accepts, whatever its guarantee, only
/// a query output that reads a heap of this algorithm, and no other output:
/// it forces the cheapest plan with that heap, as a deployment that requires
/// the heap would.
struct HeapPlansOnly(SketchAlgorithm);

impl asap_accuracy::AccuracyModel for HeapPlansOnly {
    fn satisfies(
        &self,
        _guarantee: &asap_types::ir::properties::AccuracyGuarantee,
        _target: &asap_types::ir::properties::AccuracyTarget,
    ) -> bool {
        false
    }

    fn accepts_output_regardless_of_guarantee(
        &self,
        output: &QueryRoot,
        _target: &asap_types::ir::properties::AccuracyTarget,
    ) -> bool {
        let QueryRoot::Operator(root) = output else {
            return false;
        };
        OperatorNode::reachable(root).iter().any(|node| {
            matches!(node.asap(), Some(ASAPOp::SummaryAgg { family: FieldDataType::Sketch(kind, _), .. }
                | ASAPOp::PaneChain { family: FieldDataType::Sketch(kind, _), .. })
                if *kind.algorithm() == self.0)
        })
    }
}

/// `workload` planned with accuracy guarantees ignored (a top-k estimate
/// has none) on a deployment that keeps no raw samples; with `heap`, the
/// deployment's accuracy model accepts only outputs that read a heap of that
/// algorithm ([`HeapPlansOnly`]).
async fn plan_ignoring_guarantees(
    workload: &PlanningWorkload,
    heap: Option<SketchAlgorithm>,
) -> Result<InstallablePlan, String> {
    let capabilities = DeploymentCapabilities {
        raw_data_retained: false,
        ..DeploymentCapabilities::default()
    };
    // Ignoring guarantees replaces the accuracy model, so a forced heap goes
    // through its own accuracy model instead.
    let settings = CostInputs::default();
    let heap_only = heap.map(HeapPlansOnly);
    let models = PlanningModels::builtin()
        .with_deployment_capabilities(&capabilities)
        .with_plan_selection(&settings)
        .with_ignore_accuracy_guarantees(heap.is_none());
    let models = match &heap_only {
        Some(model) => models.with_accuracy(model),
        None => models,
    };
    plan_installable(UserInput::new(
        workload,
        FrontendInput::Promql { histograms: None },
        models,
    ))
    .await
    .map(|(plan, _)| plan)
    .map_err(|e| e.to_string())
}

/// The heap sketch `plan` builds: its algorithm and width.
fn heap_of(plan: &InstallablePlan) -> Option<(SketchAlgorithm, u32)> {
    plan.dag.nodes.iter().find_map(|node| match &node.payload {
        asap_executor::planner::ir::physical_export::PhysicalASAPOperatorPayload::ASAP(
            ASAPOp::SummaryAgg { family, .. } | ASAPOp::PaneChain { family, .. },
        ) => match family {
            FieldDataType::Sketch(kind, _) => match kind.params() {
                SketchParams::CmsWithHeap { width, .. }
                | SketchParams::CountSketchWithHeap { width, .. } => {
                    Some((*kind.algorithm(), *width))
                }
                _ => None,
            },
            _ => None,
        },
        _ => None,
    })
}

/// The heap of `algorithm`'s point-error bound on an item of a partition
/// whose items' true values are `values`: `ε·L1` for Count-Min (ε = e/w,
/// an overestimate), `ε·L2` for Count Sketch (ε = √(3/w)), which is at most
/// `ε·L1`.
fn heap_bound(algorithm: &SketchAlgorithm, width: u32, values: &[f64]) -> f64 {
    let width = f64::from(width);
    match algorithm {
        SketchAlgorithm::CmsWithHeap => {
            std::f64::consts::E / width * values.iter().map(|v| v.abs()).sum::<f64>()
        }
        SketchAlgorithm::CountSketchWithHeap => {
            (3.0 / width).sqrt() * values.iter().map(|v| v * v).sum::<f64>().sqrt()
        }
        other => panic!("not a heap: {other:?}"),
    }
}

/// Checks a heap top-k plan's `answers` against the `truth` (each item's
/// exact value, by partition): per partition, `k` answers, the true top `k`
/// recalled, each item named by its labels (`item_label`, and `partition`
/// when partitioned), and each estimate within the heap's bound.
fn assert_heap_topk_answers(
    answers: &[Answer],
    truth: &BTreeMap<String, BTreeMap<String, f64>>,
    (partition, item_label, k): (Option<&str>, &str, usize),
    (algorithm, width): (&SketchAlgorithm, u32),
) {
    let mut answered: BTreeMap<String, Vec<(String, f64)>> = BTreeMap::new();
    for (labels, value) in answers {
        // A range function's result drops the metric name, as the exact
        // top-k's does.
        assert!(!labels.contains_key("__name__"), "{labels:?}");
        let group = partition.map_or(String::new(), |p| labels[p].clone());
        let item = labels
            .get(item_label)
            .unwrap_or_else(|| panic!("{item_label} kept: {labels:?}"));
        answered
            .entry(group)
            .or_default()
            .push((item.clone(), *value));
    }
    assert_eq!(
        answered.keys().collect::<Vec<_>>(),
        truth.keys().collect::<Vec<_>>()
    );
    for (group, items) in &answered {
        let exact = &truth[group];
        let bound = heap_bound(
            algorithm,
            width,
            &exact.values().copied().collect::<Vec<_>>(),
        );
        let mut ranked: Vec<(&String, &f64)> = exact.iter().collect();
        ranked.sort_by(|a, b| b.1.total_cmp(a.1));
        let top: BTreeSet<&String> = ranked.iter().take(k).map(|(item, _)| *item).collect();
        assert_eq!(items.len(), k, "{group}: {items:?}");
        let recalled = items.iter().filter(|(item, _)| top.contains(item)).count();
        assert_eq!(recalled, k, "{group}: recall of {top:?} in {items:?}");
        for (item, estimate) in items {
            let error = estimate - exact[item];
            assert!(
                error.abs() <= bound + 1e-9 * exact[item].abs(),
                "{group}/{item}: {estimate} vs exact {}, bound {bound}",
                exact[item]
            );
            if *algorithm == SketchAlgorithm::CmsWithHeap {
                assert!(
                    error >= -1e-9 * exact[item].abs(),
                    "Count-Min overestimates"
                );
            }
        }
    }
}

/// `data` samples of 2 regions with 40 series each, six inside the last
/// minute per series: series `r` (rank 1..=40) of a region sums to
/// `1000 / r^1.1` (west at half that), a skewed ranking.
fn skewed_series_samples() -> (Vec<RawDataSample>, BTreeMap<String, BTreeMap<String, f64>>) {
    let mut samples = Vec::new();
    let mut truth: BTreeMap<String, BTreeMap<String, f64>> = BTreeMap::new();
    for (region, scale) in [("east", 1.0), ("west", 0.5)] {
        for rank in 1..=40 {
            let instance = format!("{region}-{rank:02}");
            let sum = scale * 1000.0 / f64::from(rank).powf(1.1);
            for step in 0..6 {
                let mut s = sample(
                    "data",
                    &instance,
                    EVALUATION_MS - 55_000 + step * 10_000,
                    sum / 6.0,
                );
                s.labels.insert("region".into(), region.into());
                samples.push(s);
            }
            truth
                .entry(region.to_string())
                .or_default()
                .insert(instance, sum);
        }
    }
    (samples, truth)
}

/// `workload`'s plan with only `algorithm`'s heap plans accepted
/// ([`HeapPlansOnly`] forces the heap; guarantees are not ignored): it builds that heap, compiles as a runtime compiles it, and
/// both it and its query-time reference answer `samples` with the true top
/// `k` per partition, each named by its labels and within the heap's bound
/// of `truth` ([`assert_heap_topk_answers`]).
async fn assert_forced_heap_answers(
    workload: &PlanningWorkload,
    algorithm: SketchAlgorithm,
    (samples, truth): (&[RawDataSample], &BTreeMap<String, BTreeMap<String, f64>>),
    labels: (Option<&str>, &str, usize),
) {
    let plan = plan_ignoring_guarantees(workload, Some(algorithm))
        .await
        .unwrap_or_else(|error| panic!("{algorithm:?}: {error}"));
    let (built, width) = heap_of(&plan).expect("a heap plan");
    assert_eq!(built, algorithm);
    plan.check_compiles().unwrap();
    let plan_json = serde_json::to_string(&plan).unwrap();
    let run = run_data_plane(&plan_json, samples, EVALUATION_MS, &mut SummaryStore::new());
    for answers in [
        &run.answers_per_query[0],
        &run.reference_answers_per_query[0],
    ] {
        assert_heap_topk_answers(answers, truth, labels, (&algorithm, width));
    }
}

/// The ASAPQuery workloads' top-k-sum shape over a gauge,
/// `topk by (region) (3, sum_over_time(data[1m]))`: the fused
/// whole-expression Count Sketch heap (one per region over the raw
/// samples, ranking series identities), forced with guarantees ignored,
/// compiles, and answers each region's true top 3 series, with their full
/// labels, within its bound of their exact sums. (A Count-Min heap needs
/// the weights proven non-negative.)
#[tokio::test]
async fn a_forced_heap_topk_sum_plan_compiles_and_answers_within_its_bound() {
    let workload = heap_topk_workload(
        "topk by (region) (3, sum_over_time(data[1m]))",
        "data",
        r#""metric_type": "gauge""#,
    );
    let (samples, truth) = skewed_series_samples();
    assert_forced_heap_answers(
        &workload,
        SketchAlgorithm::CountSketchWithHeap,
        (&samples, &truth),
        (Some("region"), "instance", 3),
    )
    .await;
}

/// `data` samples of 2 regions with 40 series each inside the last minute:
/// series `r` (rank 1..=40) has `round(200 / r^1.1)` samples (at least 1),
/// a skewed count ranking.
fn skewed_count_samples() -> (Vec<RawDataSample>, BTreeMap<String, BTreeMap<String, f64>>) {
    let mut samples = Vec::new();
    let mut truth: BTreeMap<String, BTreeMap<String, f64>> = BTreeMap::new();
    for region in ["east", "west"] {
        for rank in 1..=40 {
            let instance = format!("{region}-{rank:02}");
            let count = (200.0 / f64::from(rank).powf(1.1)).round().max(1.0) as i64;
            for step in 0..count {
                // Spread over (EVALUATION_MS - 59 s, EVALUATION_MS - 1 s].
                let at = EVALUATION_MS - 1_000 - step * 58_000 / count;
                let mut s = sample("data", &instance, at, 1.0 + step as f64);
                s.labels.insert("region".into(), region.into());
                samples.push(s);
            }
            truth
                .entry(region.to_string())
                .or_default()
                .insert(instance, count as f64);
        }
    }
    samples.sort_by_key(|s| s.timestamp_ms);
    (samples, truth)
}

/// `topk by (region) (3, count_over_time(data[1m]))`: each forced fused
/// heap (each sample counts 1) compiles and answers each region's true top
/// 3 series, with their labels, within its bound of their counts.
#[tokio::test]
async fn a_forced_heap_topk_count_plan_compiles_and_answers_within_its_bound() {
    let workload = heap_topk_workload(
        "topk by (region) (3, count_over_time(data[1m]))",
        "data",
        r#""metric_type": "gauge""#,
    );
    let (samples, truth) = skewed_count_samples();
    for algorithm in [
        SketchAlgorithm::CmsWithHeap,
        SketchAlgorithm::CountSketchWithHeap,
    ] {
        assert_forced_heap_answers(
            &workload,
            algorithm,
            (&samples, &truth),
            (Some("region"), "instance", 3),
        )
        .await;
    }
}

/// The per-second rate of series `instance` (1 or 2) of job `job` (1..=12)
/// in [`skewed_counter_raw_data_samples`]: `50 / job^1.2`, plus a little per
/// instance, a skewed ranking of the jobs' sums.
fn skewed_counter_rate(job: i64, instance: i64) -> f64 {
    50.0 / (job as f64).powf(1.2) + 0.01 * instance as f64
}

/// `data_total` samples of 12 jobs with 2 series each, every 10 s from 595 s
/// before `EVALUATION_MS`, each a counter growing linearly at
/// [`skewed_counter_rate`], as [`counter_raw_data_samples`].
fn skewed_counter_raw_data_samples() -> Vec<RawDataSample> {
    let mut samples = Vec::new();
    for job in 1..=12 {
        for instance in 1..=2 {
            for step in 0..60 {
                let mut s = sample(
                    "data_total",
                    &format!("{job}-{instance}"),
                    EVALUATION_MS - 595_000 + step * 10_000,
                    skewed_counter_rate(job, instance) * (5 + step * 10) as f64,
                );
                s.labels.insert("job".into(), format!("j{job:02}"));
                samples.push(s);
            }
        }
    }
    samples
}

/// The ASAPQuery workloads' counter top-k, `topk(3, sum by (job)
/// (<f>(data_total[5m])))` for `f` `increase` or `rate`: each forced fused
/// heap (one over the per-series values, ranking jobs) compiles and answers
/// the true top 3 jobs, with their `job` labels, within its bound of their
/// exact sums.
#[tokio::test]
async fn a_forced_heap_counter_topk_plan_compiles_and_answers_within_its_bound() {
    let samples = skewed_counter_raw_data_samples();
    for (function, seconds) in [("increase", 300.0), ("rate", 1.0)] {
        let workload = heap_topk_workload(
            &format!("topk(3, sum by (job) ({function}(data_total[5m])))"),
            "data_total",
            r#""metric_type": "counter""#,
        );
        // Each job's exact sum over its two series.
        let truth = BTreeMap::from([(
            String::new(),
            (1..=12)
                .map(|job| {
                    let sum = skewed_counter_rate(job, 1) + skewed_counter_rate(job, 2);
                    (format!("j{job:02}"), sum * seconds)
                })
                .collect::<BTreeMap<_, _>>(),
        )]);
        for algorithm in [
            SketchAlgorithm::CmsWithHeap,
            SketchAlgorithm::CountSketchWithHeap,
        ] {
            assert_forced_heap_answers(&workload, algorithm, (&samples, &truth), (None, "job", 3))
                .await;
        }
    }
}

/// `data` samples of 40 regions with 3 series each, inside the last
/// minute: region `r` (rank 1..=40) sums to `1000 / r^1.1` over
/// its series (a skewed ranking of the regions) and holds `3 ·
/// round(200 / r^1.1)` samples (a skewed count ranking), returned with each
/// region's exact sum and exact count.
fn skewed_region_raw_data_samples() -> (
    Vec<RawDataSample>,
    BTreeMap<String, f64>,
    BTreeMap<String, f64>,
) {
    let mut samples = Vec::new();
    let mut sums = BTreeMap::new();
    let mut counts = BTreeMap::new();
    for rank in 1..=40i64 {
        let region = format!("r{rank:02}");
        let sum = 1000.0 / (rank as f64).powf(1.1);
        let per_series_steps = (200.0 / (rank as f64).powf(1.1)).round().max(1.0) as i64;
        for series in 0..3 {
            let instance = format!("{region}-{series}");
            for step in 0..per_series_steps {
                // Spread over (EVALUATION_MS - 59 s, EVALUATION_MS - 1 s].
                let at = EVALUATION_MS - 1_000 - step * 58_000 / per_series_steps;
                let mut s = sample("data", &instance, at, sum / 3.0 / per_series_steps as f64);
                s.labels.insert("region".into(), region.clone());
                samples.push(s);
            }
        }
        sums.insert(region.clone(), sum);
        counts.insert(region, (3 * per_series_steps) as f64);
    }
    samples.sort_by_key(|s| s.timestamp_ms);
    (samples, sums, counts)
}

/// The ASAPQuery workloads' grouped top-k-sum shape,
/// `topk(3, sum by (region) (<f>(data[1m])))` for `f` `sum_over_time` or
/// `count_over_time`: each forced fused heap (one over the raw data samples,
/// keyed by `region`, weighted by the sample value or by one) compiles and
/// answers the true top 3 regions within its bound of their exact sums or
/// counts, each named by its `region` label alone, as the exact top-k's.
/// A Count-Min heap over sample values needs the gauge declared
/// non-negative; a Count Sketch heap takes any gauge.
#[tokio::test]
async fn a_forced_heap_grouped_topk_sum_plan_compiles_and_answers_within_its_bound() {
    let (samples, sums, counts) = skewed_region_raw_data_samples();
    let gauge = r#""metric_type": "gauge""#;
    let non_negative = r#""metric_type": "gauge", "values_non_negative": true"#;
    for (function, truth, facts, algorithm) in [
        (
            "sum_over_time",
            &sums,
            non_negative,
            SketchAlgorithm::CmsWithHeap,
        ),
        (
            "sum_over_time",
            &sums,
            gauge,
            SketchAlgorithm::CountSketchWithHeap,
        ),
        (
            "count_over_time",
            &counts,
            gauge,
            SketchAlgorithm::CmsWithHeap,
        ),
        (
            "count_over_time",
            &counts,
            gauge,
            SketchAlgorithm::CountSketchWithHeap,
        ),
    ] {
        let query = format!("topk(3, sum by (region) ({function}(data[1m])))");
        let workload = heap_topk_workload(&query, "data", facts);
        let truth = BTreeMap::from([(String::new(), truth.clone())]);
        let plan = plan_ignoring_guarantees(&workload, Some(algorithm))
            .await
            .unwrap_or_else(|error| panic!("{query} {algorithm:?}: {error}"));
        let (built, width) = heap_of(&plan).expect("a heap plan");
        assert_eq!(built, algorithm, "{query}");
        plan.check_compiles().unwrap();
        let plan_json = serde_json::to_string(&plan).unwrap();
        let run = run_data_plane(
            &plan_json,
            &samples,
            EVALUATION_MS,
            &mut SummaryStore::new(),
        );
        for answers in [
            &run.answers_per_query[0],
            &run.reference_answers_per_query[0],
        ] {
            assert_heap_topk_answers(answers, &truth, (None, "region", 3), (&algorithm, width));
            for (labels, _) in answers.iter() {
                assert_eq!(
                    labels.keys().collect::<Vec<_>>(),
                    ["region"],
                    "{query} {algorithm:?}: the group label alone"
                );
            }
        }
        // The installed plan (stored states) answers as its query-time
        // reference does, within the heap's bound.
        let bound = heap_bound(
            &algorithm,
            width,
            &truth[""].values().copied().collect::<Vec<_>>(),
        );
        let reference: BTreeMap<_, _> = run.reference_answers_per_query[0]
            .iter()
            .map(|(labels, value)| (labels.clone(), *value))
            .collect();
        for (labels, value) in &run.answers_per_query[0] {
            let expected = reference
                .get(labels)
                .unwrap_or_else(|| panic!("{query} {algorithm:?}: {labels:?} in the reference"));
            assert!(
                (value - expected).abs() <= bound,
                "{query} {algorithm:?}: {value} vs {expected}"
            );
        }
    }
}

// ── Classic-histogram `histogram_quantile` (issue #890) ─────────────────────

/// The classic bucket metric of the histogram tests.
const BUCKET_METRIC: &str = "http_request_duration_seconds_bucket";

/// Exact PromQL `queries`, every minute, over ingesting classic-bucket
/// series of [`BUCKET_METRIC`] sampled every 10 s.
fn exact_bucket_workload(queries: &[&str]) -> PlanningWorkload {
    let entries: Vec<String> = queries
        .iter()
        .map(|query| {
            format!(
                r#"{{"query": {query:?}, "demand": {{"fixed_interval": 60000}},
                  "requirements": {{"accuracy": {{"max_error": 0.0, "failure_probability": 0.0}}}},
                  "predictability": {{"predictable": {{"known_at": null}}}}}}"#
            )
        })
        .collect();
    serde_json::from_str(&format!(
        r#"{{"query_workload": {{"language": "promql", "repeating_queries": [{}]}},
            "data_workload": {{"arrival": {{"ingesting": {{"rows_per_second": 2.4}}}},
              "sources": {{"{BUCKET_METRIC}": {{"series_count": 24, "sample_interval": 10000}}}}}}}}"#,
        entries.join(",")
    ))
    .unwrap()
}

/// Cumulative bucket counters of [`BUCKET_METRIC`], one sample every 10 s
/// over the 5 minutes before `EVALUATION_MS` (first 295 s before it, last
/// 5 s before it), each rising at a constant per-second rate from 1000, so
/// `rate(…[5m])` of each is its slope. Per `(service, job)`, the slopes of
/// buckets `le` 0.1, 0.5, 1 and `+Inf`, each on instances `1` and `2`:
/// - `orders`/`api`: 0.5, 1.5, 2, 2 (summed over instances: 1, 3, 4, 4);
/// - `batch`/`worker`: 0.5, 0.5, 0.5, 2.5 (most observations above 1, in
///   `+Inf` alone; summed: 1, 1, 1, 5);
/// - `idle`/`idle`: 0 everywhere (no observations).
fn bucket_samples() -> Vec<RawDataSample> {
    let mut samples = Vec::new();
    for (service, job, slopes) in [
        ("orders", "api", [0.5, 1.5, 2.0, 2.0]),
        ("batch", "worker", [0.5, 0.5, 0.5, 2.5]),
        ("idle", "idle", [0.0; 4]),
    ] {
        for (le, slope) in ["0.1", "0.5", "1", "+Inf"].into_iter().zip(slopes) {
            for instance in ["1", "2"] {
                for step in 0..30 {
                    samples.push(RawDataSample {
                        labels: BTreeMap::from([
                            ("__name__".to_string(), BUCKET_METRIC.to_string()),
                            ("service".to_string(), service.to_string()),
                            ("job".to_string(), job.to_string()),
                            ("instance".to_string(), instance.to_string()),
                            ("le".to_string(), le.to_string()),
                        ]),
                        timestamp_ms: EVALUATION_MS - 295_000 + step * 10_000,
                        value: 1000.0 + slope * 10.0 * step as f64,
                    });
                }
            }
        }
    }
    samples
}

/// The plan file of exact `queries` over [`bucket_samples`]' metric, on a
/// deployment that keeps raw data samples.
async fn bucket_plan_json(queries: &[&str]) -> String {
    plan_json_for(
        exact_bucket_workload(queries),
        &DeploymentCapabilities::default(),
    )
    .await
}

/// `histogram_quantile` over classic buckets as the issue's dashboard
/// writes it: p50, p95 and p99 of the summed bucket rates of one service,
/// in one workload. Stage 3 selects an exact plan the executor compiles,
/// and it answers with Prometheus' cumulative-bucket interpolation: over
/// summed rates 1, 3, 4, 4 at `le` 0.1, 0.5, 1, `+Inf`, rank 0.5 · 4 = 2
/// lies in the 0.5 bucket, 0.1 + 0.4 · (2 − 1) / (3 − 1) = 0.3; rank 3.8
/// and 3.96 in the 1 bucket, 0.5 + 0.5 · 0.8 = 0.9 and 0.5 + 0.5 · 0.96 =
/// 0.98. The result has no labels: `sum by (le)` keeps only `le`, which
/// the quantile drops.
#[tokio::test]
async fn classic_histogram_quantiles_run_with_prometheus_interpolation() {
    let selector = format!(r#"rate({BUCKET_METRIC}{{service="orders"}}[5m])"#);
    let queries: Vec<String> = [0.5, 0.95, 0.99]
        .iter()
        .map(|q| format!("histogram_quantile({q}, sum by (le) ({selector}))"))
        .collect();
    let queries: Vec<&str> = queries.iter().map(String::as_str).collect();
    let plan_json = bucket_plan_json(&queries).await;
    let run = run_data_plane(
        &plan_json,
        &bucket_samples(),
        EVALUATION_MS,
        &mut SummaryStore::new(),
    );
    assert_eq!(run.answers_per_query.len(), 3);
    for (answers, expected) in run.answers_per_query.iter().zip([0.3, 0.9, 0.98]) {
        assert_eq!(answers.len(), 1, "{answers:?}");
        let (labels, value) = &answers[0];
        assert!(labels.is_empty(), "le is dropped: {labels:?}");
        assert!((value - expected).abs() < 1e-9, "{value} vs {expected}");
    }
    assert_eq!(run.answers_per_query, run.reference_answers_per_query);
}

/// One p99 per `job` (`sum by (job, le)`): the result keeps `job` and
/// drops `le`. `worker`'s rank 0.99 · 5 = 4.95 lies past every finite
/// bucket (most observations are in `+Inf` alone), so Prometheus answers
/// the largest finite bound, 1; `idle` has no observations, so NaN. The
/// p99 alert (`> 0.5`) keeps `api` (0.98) and `worker` (1) and drops
/// `idle` (NaN compares false). A selector that matches no series gives an
/// empty result. Over one series' buckets, without `sum`, the result keeps
/// that series' labels but `le` (and the name `rate` dropped).
#[tokio::test]
async fn a_classic_histogram_quantile_keeps_labels_and_handles_inf_and_empty_input() {
    let by_job = format!("histogram_quantile(0.99, sum by (job, le) (rate({BUCKET_METRIC}[5m])))");
    let alert = format!("{by_job} > 0.5");
    let empty = format!(
        r#"histogram_quantile(0.5, sum by (le) (rate({BUCKET_METRIC}{{service="missing"}}[5m])))"#
    );
    // Without `sum`, each series is its own histogram: `instance` 1's
    // buckets alone (rates 0.5, 1.5, 2, 2), keeping its labels but `le`.
    let per_series = format!(
        r#"histogram_quantile(0.99, rate({BUCKET_METRIC}{{service="orders",instance="1"}}[5m]))"#
    );
    let plan_json = bucket_plan_json(&[&by_job, &alert, &empty, &per_series]).await;
    let run = run_data_plane(
        &plan_json,
        &bucket_samples(),
        EVALUATION_MS,
        &mut SummaryStore::new(),
    );
    let job = |answers: &[Answer]| -> Vec<(String, f64)> {
        answers
            .iter()
            .map(|(labels, value)| {
                assert_eq!(labels.keys().collect::<Vec<_>>(), ["job"], "{labels:?}");
                (labels["job"].clone(), *value)
            })
            .collect()
    };
    for answers in [&run.answers_per_query, &run.reference_answers_per_query] {
        let per_job = job(&answers[0]);
        assert_eq!(per_job.len(), 3, "{per_job:?}");
        assert_eq!(per_job[0].0, "api");
        assert!((per_job[0].1 - 0.98).abs() < 1e-9, "{per_job:?}");
        assert_eq!(per_job[1].0, "idle");
        assert!(per_job[1].1.is_nan(), "{per_job:?}");
        assert_eq!(per_job[2], ("worker".to_string(), 1.0));
        let alerting = job(&answers[1]);
        assert_eq!(alerting.len(), 2, "{alerting:?}");
        assert_eq!(alerting[0].0, "api");
        assert_eq!(alerting[1], ("worker".to_string(), 1.0));
        assert!(answers[2].is_empty(), "{:?}", answers[2]);
        let [(labels, value)] = answers[3].as_slice() else {
            panic!("{:?}", answers[3]);
        };
        assert_eq!(
            labels.keys().collect::<Vec<_>>(),
            ["instance", "job", "service"],
            "{labels:?}"
        );
        assert!((value - 0.98).abs() < 1e-9, "{value}");
    }
}

// ── PromQL absent() (issue #895) ─────────────────────────────────────────────

/// The Prometheus stale marker, which Remote Write carries when a series ends.
const STALE_MARKER: f64 = f64::from_bits(0x7ff0_0000_0000_0002);

/// The answers of the exact plan of PromQL `query` over `samples` of `m`
/// (6 series, every 10 s), with the query-time reference's.
async fn exact_answers(query: &str, samples: &[RawDataSample]) -> Vec<Answer> {
    let plan_json = plan_json_for(
        exact_workload(&[query], ""),
        &DeploymentCapabilities::default(),
    )
    .await;
    let run = run_data_plane(&plan_json, samples, EVALUATION_MS, &mut SummaryStore::new());
    assert_eq!(run.answers_per_query, run.reference_answers_per_query);
    run.answers_per_query[0].clone()
}

/// A sample of `m` at `ago_ms` before `EVALUATION_MS` with `labels`.
fn m_sample(labels: &[(&str, &str)], ago_ms: i64, value: f64) -> RawDataSample {
    let mut sample = sample("m", "1", EVALUATION_MS - ago_ms, value);
    for (name, label) in labels {
        sample.labels.insert(name.to_string(), label.to_string());
    }
    sample
}

/// `absent(v)` answers as Prometheus' `funcAbsent`: one sample of value 1
/// when `v` selects no series at the evaluation time, labelled with the
/// selector's equality matchers (`job`, not the negated `instance` and
/// `env`); no sample when some series matches, whatever its value
/// (0 included). A series whose newest sample is the stale marker has
/// ended, so `absent` answers 1 again.
#[tokio::test]
async fn absent_answers_as_prometheus_through_the_installed_plan() {
    let query = r#"absent(m{job="a",instance!="3",env!="prod"})"#;
    let one = vec![(BTreeMap::from([("job".to_string(), "a".to_string())]), 1.0)];
    // No sample at all.
    assert_eq!(exact_answers(query, &[]).await, one);
    // Only series the matchers reject: job b, env prod, instance 3.
    let rejected = [
        m_sample(&[("job", "b")], 1_000, 1.0),
        m_sample(&[("job", "a"), ("env", "prod")], 1_000, 1.0),
        {
            let mut other = m_sample(&[("job", "a")], 1_000, 1.0);
            other.labels.insert("instance".into(), "3".into());
            other
        },
    ];
    assert_eq!(exact_answers(query, &rejected).await, one);
    // A matching series, valued 0 or not: no sample.
    for value in [0.0, 5.0] {
        let samples = [m_sample(&[("job", "a")], 1_000, value)];
        assert_eq!(exact_answers(query, &samples).await, vec![], "{value}");
    }
    // The last matching series ended (its newest sample is stale).
    let ended = [
        m_sample(&[("job", "a")], 5_000, 5.0),
        m_sample(&[("job", "a")], 1_000, STALE_MARKER),
    ];
    assert_eq!(exact_answers(query, &ended).await, one);
}

/// `absent(m)` (no matcher) synthesizes a label-less sample; the missing-job
/// alert shape (several equality matchers) keeps every matcher's label.
#[tokio::test]
async fn absent_labels_come_from_the_equality_matchers_only() {
    assert_eq!(
        exact_answers("absent(m)", &[]).await,
        vec![(BTreeMap::new(), 1.0)]
    );
    let alert = r#"absent(m{kubernetes_namespace="mw-content-history-reconcile-enrich",release="production",job_name="mw_content_history_reconcile_enrich"})"#;
    let labels: BTreeMap<String, String> = [
        (
            "kubernetes_namespace",
            "mw-content-history-reconcile-enrich",
        ),
        ("release", "production"),
        ("job_name", "mw_content_history_reconcile_enrich"),
    ]
    .into_iter()
    .map(|(name, value)| (name.to_string(), value.to_string()))
    .collect();
    assert_eq!(exact_answers(alert, &[]).await, vec![(labels.clone(), 1.0)]);
    let running = [m_sample(
        &labels
            .iter()
            .map(|(name, value)| (name.as_str(), value.as_str()))
            .collect::<Vec<_>>(),
        1_000,
        3600.0,
    )];
    assert_eq!(exact_answers(alert, &running).await, vec![]);
}

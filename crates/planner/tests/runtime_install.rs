//! A deployment runtime installs and runs the Planner's plan with public
//! APIs only: `asap_planner` to plan, `asap_executor` to execute (issue
//! #749). The executor links here without its `test-support` feature, so
//! the compiler checks that no internal is used (CI runs
//! `cargo check -p asap-planner --tests --locked` on its own for that).

use std::collections::{BTreeMap, BTreeSet};

use asap_executor::operators::Operator as ExecutorOperator;
use asap_executor::physical_planner::{compile, InputContract, Source};
use asap_executor::runtime::{Limits, RunContext, Scope};
use asap_executor::values::{Batch, Value};
use asap_executor::{series_labels_of_output_row, RawSampleSelector, Sample};
use asap_planner::{
    plan_installable, DeploymentCapabilities, ExecutorCompileCheck, FrontendInput, InstallablePlan,
    InstallablePlanError, OperatorKind, OptimizationInput, OptimizationPass, OptimizeError,
    PhysicalASAPDAG, PlanOutput, PlanSelectionSettings, PlannerExecutorCompileCheck,
    PlanningModels, PlanningWorkload, SelectionBox, Stage1ToStage3Pass, StoredOutput,
    StoredOutputBuild, UserInput,
};
use asap_types::ir::schema::{FieldDataType, SketchAlgorithm, SketchParams};
use asap_types::ir::{
    apply_materialization_timings_to_roots, split_shared_by_timing, ASAPOp,
    MaterializationAssignment, OperatorNode, QueryRoot,
};
use asap_types::physical::{Materialization, Medium};
use futures::{executor::block_on, StreamExt};
use std::collections::HashMap;
use std::ops::Bound;
use std::rc::Rc;

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
fn sample(metric: &str, instance: &str, timestamp_ms: i64, value: f64) -> Sample {
    Sample {
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
fn remote_write_samples() -> Vec<Sample> {
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

/// Compile `ids` of `dag` over `inputs` and run them in `scope`, as a
/// runtime does with the executor: each id's output batches.
fn run(
    dag: &PhysicalASAPDAG,
    inputs: BTreeMap<u64, Batch>,
    ids: &[u64],
    scope: Scope,
) -> Vec<Vec<Batch>> {
    // What each bound input holds: its rows' schema.
    let contracts = inputs
        .iter()
        .map(|(&id, batch)| (id, InputContract::bounded(batch.schema().clone())))
        .collect();
    // The executor's plan for `ids`, cut at the bound inputs.
    let compiled = compile(dag, contracts, ids).expect("the executor compiles the plan");
    // Each bound input as an in-memory source of its rows.
    let sources = inputs
        .into_iter()
        .map(|(id, batch)| {
            let source = ExecutorOperator::source(batch.schema().clone(), vec![batch]).unwrap();
            (id, Box::new(source) as Source<'_>)
        })
        .collect();
    // The plan bound to its sources, ready to run.
    let instance = compiled.instantiate(sources).unwrap();
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
                let labels = series_labels_of_output_row(batch.schema(), &row).unwrap();
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
    // What the deployment keeps: no raw samples, no query-time results
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
    // The deployment's Stage 3 prices, from its JSON file: cost model 1.
    let settings: PlanSelectionSettings = serde_json::from_str(
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
    let plan = plan_installable(UserInput::new(
        &workload,
        FrontendInput::Promql { histograms: None },
        models,
    ))
    .await
    .unwrap();
    // `plan.json`, as the control plane writes it.
    serde_json::to_string(&plan).unwrap()
}

/// What one data-plane evaluation of a plan file returned.
struct DataPlaneRun {
    /// Per query, in workload order, the installed plan's answers.
    answers_per_query: Vec<Vec<Answer>>,
    /// Per query, the query-time reference's answers.
    reference_answers_per_query: Vec<Vec<Answer>>,
    /// How many stored outputs this evaluation built (ingestion or query
    /// runs); the others it read came from the store.
    outputs_built: usize,
}

/// One summary state this test stores: its built state, and how long to
/// keep it.
struct StoredState {
    /// The built state, bound at every stored output with its key.
    state: Batch,
    /// When the data it summarizes starts, in Unix milliseconds.
    start_ms: i64,
    /// How far back from the newest evaluation it is kept: the largest
    /// `retention_ms` among the stored outputs it served.
    retention_ms: u64,
}

/// This stand-in runtime's summary store, by [`state_key`].
type SummaryStore = BTreeMap<String, StoredState>;

/// The key of `output`'s state at evaluation `evaluation_ms`, as a summary
/// store keys instances: its definition id and its selection made
/// concrete (relative time made absolute). An output whose selection
/// names no time (its definition reads a window relative to the
/// evaluation, e.g. an instant selector) is keyed by the evaluation too.
fn state_key(output: &StoredOutput, evaluation_ms: i64) -> String {
    use std::ops::Bound;
    let absolute = |bound: &Bound<i64>| match bound {
        Bound::Included(at) => Bound::Included(evaluation_ms + at),
        Bound::Excluded(at) => Bound::Excluded(evaluation_ms + at),
        Bound::Unbounded => Bound::Unbounded,
    };
    let mut selection = output.coverage.selection.clone();
    let timed = selection.iter().any(|b| b.relative_time.is_some());
    for selection_box in &mut selection {
        selection_box.relative_time = selection_box
            .relative_time
            .as_ref()
            .map(|(lower, upper)| (absolute(lower), absolute(upper)));
    }
    serde_json::json!({
        "definition_id": output.coverage.definition_id,
        "selection": selection,
        "evaluation_ms": (!timed).then_some(evaluation_ms),
    })
    .to_string()
}

/// The node that answers each query of `plan`, in workload order.
fn query_roots(plan: &InstallablePlan) -> Vec<u64> {
    plan.queries
        .iter()
        .map(|query| query.root_node_id as u64)
        .collect()
}

/// The data plane at evaluation time `evaluation_ms`: from `plan_json`,
/// `samples` and the states earlier evaluations left in `store`, build each
/// stored output the store lacks once, in the plan's order, from its
/// raw-sample windows and stored inputs; answer every query; drop the
/// states past their retention; and answer the queries again with the
/// query-time reference.
fn run_data_plane(
    plan_json: &str,
    samples: &[Sample],
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

/// When this stand-in runtime builds a stored output an ingestion run
/// builds from raw samples.
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

/// [`run_data_plane`] with ingestion-run outputs built as `ingestion` says.
fn run_data_plane_on(
    plan_json: &str,
    samples: &[Sample],
    evaluation_ms: i64,
    store: &mut SummaryStore,
    ingestion: IngestionBuilds,
) -> DataPlaneRun {
    // The installed plan, read back from its file and checked.
    let installed: InstallablePlan = serde_json::from_str(plan_json).unwrap();
    installed.validate().unwrap();
    // The DAG the executor runs.
    let dag = &installed.dag.dag;
    // Every input of the query run, keyed by the node it is bound to.
    let mut inputs = BTreeMap::new();
    // How many stored outputs this evaluation builds.
    let mut outputs_built = 0;
    // Each stored output's state and the time span of its data at this
    // evaluation, `(start_ms, end_ms]`, by node id.
    let mut built: BTreeMap<u64, (Batch, (i64, i64))> = BTreeMap::new();
    for output in &installed.stored_outputs {
        // The evaluation its windows end at: this one, or for a tumbling
        // ingestion build the newest point of its grid at or before it.
        let built_for_ms = match (ingestion, output.built_at) {
            (IngestionBuilds::Tumbling, StoredOutputBuild::IngestionRun) => output
                .inputs
                .raw_sample_windows
                .first()
                .map_or(evaluation_ms, |window| {
                    evaluation_ms - evaluation_ms.rem_euclid(window.width_ms)
                }),
            _ => evaluation_ms,
        };
        // The span of the data it summarizes: its windows' and stored
        // inputs' spans together.
        let span = output
            .inputs
            .raw_sample_windows
            .iter()
            .map(|window| window.bounds_ms(built_for_ms))
            .chain(
                output
                    .inputs
                    .stored_outputs
                    .iter()
                    .map(|id| built[&(*id as u64)].1),
            )
            .reduce(|(a, b), (c, d)| (a.min(c), b.max(d)))
            .expect("a stored output reads something");
        let key = state_key(output, built_for_ms);
        if !store.contains_key(&key) {
            // The build's sources: each raw-sample window's rows, and each
            // stored input's state.
            let mut sources = BTreeMap::new();
            for window in &output.inputs.raw_sample_windows {
                // Prepared once per window: routes by metric, applies the Scan.
                let selector = RawSampleSelector::new(dag, window).unwrap();
                // Only the samples of the selector's metric reach it.
                let routed: Vec<Sample> = samples
                    .iter()
                    .filter(|s| s.labels["__name__"] == selector.metric())
                    .cloned()
                    .collect();
                let (_, end_ms) = window.bounds_ms(built_for_ms);
                sources.insert(
                    window.time_range_node_id as u64,
                    selector.window_batch(end_ms, &routed).unwrap(),
                );
            }
            for id in &output.inputs.stored_outputs {
                sources.insert(*id as u64, built[&(*id as u64)].0.clone());
            }
            // The run that builds it: an ingestion run over its data's span
            // once its inputs are complete, or the query run of this
            // evaluation.
            let scope = match output.built_at {
                StoredOutputBuild::IngestionRun => Scope::Ingestion {
                    window_start_ms: span.0,
                    window_end_ms: span.1,
                    revision: 1,
                },
                StoredOutputBuild::QueryRun => Scope::Query {
                    evaluation_time_ms: evaluation_ms,
                    revision: 1,
                },
            };
            // The batches that run built, in one batch, as the store keeps it.
            let batches = run(dag, sources, &[output.node_id as u64], scope).remove(0);
            let schema = batches[0].schema().clone();
            let state_rows = batches.iter().flat_map(|b| b.to_rows().to_vec()).collect();
            store.insert(
                key.clone(),
                StoredState {
                    state: Batch::try_new(schema, state_rows).unwrap(),
                    start_ms: span.0,
                    retention_ms: output.retention_ms,
                },
            );
            outputs_built += 1;
        }
        // One stored state serves every output with its key, so it is kept
        // for the largest retention any of them requires.
        let stored = store.get_mut(&key).unwrap();
        stored.retention_ms = stored.retention_ms.max(output.retention_ms);
        built.insert(output.node_id as u64, (stored.state.clone(), span));
    }
    // The query run reads the stored outputs it reaches from the queries'
    // roots without passing another stored output; those below are read
    // only through the outputs built from them.
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
    // Raw samples read at query time, other than through stored outputs.
    for window in &installed.query_time_raw_windows {
        // The window's selector and its end at this evaluation.
        let selector = RawSampleSelector::new(dag, window).unwrap();
        let (_, end_ms) = window.bounds_ms(evaluation_ms);
        inputs.insert(
            window.time_range_node_id as u64,
            selector.window_batch(end_ms, samples).unwrap(),
        );
    }
    // The query evaluation, which reads the stored states.
    let query = Scope::Query {
        evaluation_time_ms: evaluation_ms,
        revision: 1,
    };
    // Each query's answers: (series labels, value) per series.
    let answers_per_query = run(dag, inputs, &query_roots(&installed), query.clone())
        .iter()
        .map(|batches| answers(batches))
        .collect();
    // The store drops each state whose data starts more than its retention
    // before this evaluation.
    store.retain(|_, state| state.start_ms >= evaluation_ms - state.retention_ms as i64);

    // The correctness reference: nothing stored, every window fed raw
    // samples at query time.
    let reference = installed.query_time_reference().unwrap();
    // The reference's DAG.
    let reference_dag = &reference.dag.dag;
    // Each query-time window's input rows, keyed by its TimeRange.
    let raw: BTreeMap<u64, Batch> = reference
        .query_time_raw_windows
        .iter()
        .map(|window| {
            // The window's selector and its end at this evaluation.
            let selector = RawSampleSelector::new(reference_dag, window).unwrap();
            let (_, end_ms) = window.bounds_ms(evaluation_ms);
            (
                window.time_range_node_id as u64,
                selector.window_batch(end_ms, samples).unwrap(),
            )
        })
        .collect();
    // The reference's answers, which the installed plan's must equal.
    let reference_answers_per_query = run(reference_dag, raw, &query_roots(&reference), query)
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
/// `plan.json`, then each stored pane built in an ingestion run and merged
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
    assert_eq!(run.outputs_built, 6, "six ten-minute panes");
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
    let plan: InstallablePlan =
        serde_json::from_str(&plan_json(&["quantile_over_time(0.99, latency_ms[60m])"]).await)
            .unwrap();
    assert!(!plan.stored_outputs.is_empty(), "a pane-only plan");
    assert!(plan.query_time_raw_windows.is_empty(), "a pane-only plan");
    let selection = plan.selection.as_ref().expect("Stage 3's report");
    let usage = selection.predicted_usage.as_ref().expect("predicted usage");
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
    // chain its six ten-minute panes, the query's root its own node.
    let installed: BTreeSet<_> = plan.dag.dag.nodes.iter().map(|node| node.id).collect();
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
    assert_eq!(chain.installed_node_ids.len(), 6, "{chain:?}");
    let stored: BTreeSet<_> = plan.stored_outputs.iter().map(|o| o.node_id).collect();
    assert!(chain
        .installed_node_ids
        .iter()
        .all(|id| stored.contains(id)));
    let root = plan.queries[0].root_node_id;
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
/// minutes store six panes each; five of them cover the same windows
/// (offsets 10..50 min), which this runtime's own cache key finds, so it
/// builds 7. Both answers equal the reference.
#[tokio::test]
async fn an_hour_and_its_offset_twin_both_equal_the_reference() {
    let plan_json = plan_json(&[
        "quantile_over_time(0.99, latency_ms[60m])",
        "quantile_over_time(0.99, latency_ms[60m] offset 10m)",
    ])
    .await;
    let installed: InstallablePlan = serde_json::from_str(&plan_json).unwrap();
    assert_eq!(installed.stored_outputs.len(), 12);
    let run = run_data_plane(
        &plan_json,
        &remote_write_samples(),
        EVALUATION_MS,
        &mut SummaryStore::new(),
    );
    assert_eq!(run.outputs_built, 7);
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
/// every second. Run by a runtime that builds each ingestion pane once, on
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
    // A deployment that keeps no raw samples: the windows are stored.
    let capabilities = DeploymentCapabilities {
        raw_data_retained: false,
        ..DeploymentCapabilities::default()
    };
    let plan_json = plan_json_for(workload, &capabilities).await;
    let plan: InstallablePlan = serde_json::from_str(&plan_json).unwrap();
    assert!(
        plan.stored_outputs
            .iter()
            .any(|output| output.built_at == StoredOutputBuild::IngestionRun),
        "{:#?}",
        plan.stored_outputs
    );
    // Samples of `m`, `job` a (instances 1-3) and b (4-6), every second
    // from 11 minutes before the first evaluation to 10 s after it; each
    // value distinct, so a window off by 5 s at either edge answers
    // differently.
    let samples: Vec<Sample> = (1..=6i64)
        .flat_map(|instance| {
            (0..680i64).map(move |step| Sample {
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

/// A plan file of another DAG wire version is refused, and so is the
/// reference of a tampered plan file, with an error rather than a panic.
#[tokio::test]
async fn a_plan_file_of_another_version_or_tampered_is_refused() {
    let plan_json = plan_json(&["quantile_over_time(0.99, latency_ms[60m])"]).await;
    let mut other_version: InstallablePlan = serde_json::from_str(&plan_json).unwrap();
    other_version.dag.schema_version += 1;
    assert!(matches!(
        other_version.validate(),
        Err(InstallablePlanError::InvalidDag(_))
    ));
    let mut tampered: InstallablePlan = serde_json::from_str(&plan_json).unwrap();
    tampered.queries.clear();
    assert!(matches!(
        tampered.query_time_reference(),
        Err(InstallablePlanError::QueriesDoNotMatchRoots { .. })
    ));
    let mut missing_node: InstallablePlan = serde_json::from_str(&plan_json).unwrap();
    missing_node.dag.dag.nodes.remove(0);
    assert!(matches!(
        missing_node.query_time_reference(),
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
    .unwrap();
    let dag = &plan.dag.dag;
    let [window] = plan.query_time_raw_windows.as_slice() else {
        panic!("{:?}", plan.query_time_raw_windows);
    };
    assert_eq!(window.width_ms, 1_000);
    let samples = [
        sample("latency_ms", "a", EVALUATION_MS - 900, 1.0),
        sample("latency_ms", "a", EVALUATION_MS - 100, 10.0),
        sample("latency_ms", "b", EVALUATION_MS - 200, 100.0),
    ];
    let selector = RawSampleSelector::new(dag, window).unwrap();
    let rows = selector.window_batch(EVALUATION_MS, &samples).unwrap();
    let query = Scope::Query {
        evaluation_time_ms: EVALUATION_MS,
        revision: 1,
    };
    let inputs = BTreeMap::from([(window.time_range_node_id as u64, rows)]);
    let answered = answers(&run(dag, inputs, &query_roots(&plan), query)[0]);
    assert_eq!(answered.len(), 1);
    assert_eq!(answered[0].1, 110.0);
}

/// A deployment that keeps raw samples and query-time results across
/// evaluations (#509 Example 4's B3), with a dashboard panel
/// (`max_over_time(latency_ms[5m])` every minute over 1M series sampled
/// every 1.5 s, answered as the last hour at 1-minute steps): Stage 3
/// keeps five 1-minute panes built at query time, and feeds their ranges
/// only through those builds, so no window is listed twice. The first evaluation, with an empty
/// store, builds all five in its query run; the next, a minute later,
/// reuses the four still in its window and builds only the newest. Each
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
    assert_eq!(installed.stored_outputs.len(), 5);
    assert!(installed
        .stored_outputs
        .iter()
        .all(|o| o.built_at == StoredOutputBuild::QueryRun
            && o.inputs.raw_sample_windows[0].width_ms == 60_000));
    let pane_ranges: BTreeSet<_> = installed
        .stored_outputs
        .iter()
        .map(|o| o.inputs.raw_sample_windows[0].time_range_node_id)
        .collect();
    assert!(installed
        .query_time_raw_windows
        .iter()
        .all(|w| !pane_ranges.contains(&w.time_range_node_id)));
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
/// deployment that keeps raw samples: Stage 3 selects the exact plan, which
/// reads each series' raw samples in the window at query time, and the
/// executor runs it. Series `a` (1..=10) and `b` (10, 20, ..., 100) are in
/// `east`, `c` (constantly 5) in `west`; a sample of each before the window
/// is not read. Per series, p99 interpolates between its two largest
/// samples (rank 0.99 × 9 = 8.91): 9.91 for `a`, 99.1 for `b`, 5 for `c`.
#[tokio::test]
async fn an_exact_per_series_quantile_runs_from_raw_samples() {
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
    assert!(installed.stored_outputs.is_empty(), "the exact plan");
    assert_eq!(installed.query_time_raw_windows.len(), 1);
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

// ── Stored outputs built from stored outputs (issue #781) ──────────────────

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
    let materialization = store(node).unwrap_or(node.materialization);
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
    serde_json::to_string(&plan_installable(input).await.unwrap()).unwrap()
}

/// Samples of `m` for six series, `job` a (instances 1, 2, 3) and b (4, 5,
/// 6), every 10 s over the last 10 minutes; each value distinct.
fn job_samples() -> Vec<Sample> {
    let mut samples = Vec::new();
    for instance in 1..=6 {
        for step in 0..60 {
            samples.push(Sample {
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
    vec![SelectionBox {
        relative_time: Some((Bound::Excluded(from_ms), Bound::Included(to_ms))),
        ..SelectionBox::default()
    }]
}

/// A fine and a coarse sum (`sum by (job, instance)` and `sum by (job)`),
/// both built at ingestion from one stored per-series sum: Stage 1 shares
/// the per-series sums the two queries read (over scans that project other
/// labels). The per-series sum is listed first, then each group sum, which
/// reads only it, with its time selection under a definition of its own;
/// built in that order from stored inputs, they answer as the query-time
/// reference.
#[tokio::test]
async fn group_sums_from_one_stored_per_series_sum_are_listed_and_built() {
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
    let [per_series, sums @ ..] = plan.stored_outputs.as_slice() else {
        panic!("{:#?}", plan.stored_outputs);
    };
    assert_eq!(sums.len(), 2, "{:#?}", plan.stored_outputs);
    assert!(per_series.inputs.stored_outputs.is_empty());
    assert_eq!(per_series.inputs.raw_sample_windows.len(), 1);
    for sum in sums {
        assert_eq!(sum.inputs.stored_outputs, [per_series.node_id]);
        assert!(sum.inputs.raw_sample_windows.is_empty());
        assert_ne!(
            sum.coverage.definition_id,
            per_series.coverage.definition_id
        );
    }
    assert_ne!(
        sums[0].coverage.definition_id,
        sums[1].coverage.definition_id
    );
    for output in &plan.stored_outputs {
        assert_eq!(output.built_at, StoredOutputBuild::IngestionRun);
        assert_eq!(output.coverage.selection, relative_time(-60_000, 0));
        assert_eq!(output.retention_ms, 60_000);
    }
    assert!(plan.query_time_raw_windows.is_empty());
    let run = run_data_plane(
        &plan_json,
        &job_samples(),
        EVALUATION_MS,
        &mut SummaryStore::new(),
    );
    assert_eq!(run.outputs_built, 3);
    assert_eq!(run.answers_per_query[0].len(), 2, "one sum per job");
    assert_eq!(run.answers_per_query[1].len(), 6, "one sum per series");
    assert_eq!(run.answers_per_query, run.reference_answers_per_query);
}

/// A maximum across series (`max by (job)`) built at ingestion from the
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
    let [per_series, across] = plan.stored_outputs.as_slice() else {
        panic!("{:#?}", plan.stored_outputs);
    };
    assert!(per_series.inputs.stored_outputs.is_empty());
    assert_eq!(across.inputs.stored_outputs, [per_series.node_id]);
    assert!(across.inputs.raw_sample_windows.is_empty());
    for output in [per_series, across] {
        assert_eq!(output.built_at, StoredOutputBuild::IngestionRun);
        assert_eq!(output.coverage.selection, relative_time(-60_000, 0));
    }
    assert_ne!(
        across.coverage.definition_id,
        per_series.coverage.definition_id
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
/// union of their times, built in the query run and kept for its window
/// plus the hour of steps; its panes are kept at least that long. Built
/// from the stored panes it answers as the reference, and a minute later
/// only the newest pane and the new merge are built.
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
    let (merge, panes) = plan.stored_outputs.split_last().unwrap();
    assert_eq!(panes.len(), 5);
    assert_eq!(
        merge.inputs.stored_outputs,
        panes.iter().map(|pane| pane.node_id).collect::<Vec<_>>()
    );
    assert!(merge.inputs.raw_sample_windows.is_empty());
    assert_eq!(merge.built_at, StoredOutputBuild::QueryRun);
    assert_eq!(merge.coverage.selection, relative_time(-300_000, 0));
    assert_eq!(merge.retention_ms, 300_000 + 3_600_000);
    for (i, pane) in panes.iter().enumerate() {
        let i = i as i64;
        assert_eq!(pane.coverage.definition_id, merge.coverage.definition_id);
        assert_eq!(
            pane.coverage.selection,
            relative_time(-(i + 1) * 60_000, -i * 60_000)
        );
        assert!(pane.retention_ms >= merge.retention_ms);
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

/// Example 4b's six 10-minute panes at offsets 0 to 50 minutes share one
/// definition id, each with its own time selection; each reads its pane of
/// raw samples and nothing stored.
#[tokio::test]
async fn panes_of_an_hour_share_one_definition_with_distinct_times() {
    let plan: InstallablePlan =
        serde_json::from_str(&plan_json(&["quantile_over_time(0.99, latency_ms[60m])"]).await)
            .unwrap();
    assert_eq!(plan.stored_outputs.len(), 6);
    let mut panes: Vec<&StoredOutput> = plan.stored_outputs.iter().collect();
    panes.sort_by_key(|o| o.inputs.raw_sample_windows[0].end_offset_ms);
    for (i, pane) in panes.iter().enumerate() {
        let i = i as i64;
        let [window] = pane.inputs.raw_sample_windows.as_slice() else {
            panic!("{pane:?}");
        };
        assert_eq!(
            (window.width_ms, window.end_offset_ms),
            (600_000, i * 600_000)
        );
        assert!(pane.inputs.stored_outputs.is_empty());
        assert_eq!(pane.coverage.definition_id, panes[0].coverage.definition_id);
        assert_eq!(
            pane.coverage.selection,
            relative_time(-(i + 1) * 600_000, -i * 600_000)
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
fn topk_samples() -> Vec<Sample> {
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

/// [`p99_avg_workload`] planned for a deployment that keeps no raw samples.
async fn plan_p99_avg(values_non_negative: bool) -> Result<InstallablePlan, String> {
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

/// Without raw samples, the only valid plan of the p99 query keeps one
/// DDSketch per series in one-second panes built at ingestion and merges
/// the window's panes at query time. A quantile
/// of merged panes is still one of the samples, so a declared non-negative
/// gauge carries the sketch's relative bound through `avg`: that plan is
/// selected, its answers equal the query-time reference's and are within
/// 5% of the exact averages.
#[tokio::test]
async fn a_non_negative_gauges_merged_ddsketch_panes_carry_the_relative_bound_through_avg() {
    let plan = plan_p99_avg(true).await.expect("a valid plan");
    let selection = plan.selection.as_ref().expect("Stage 3's report");
    // Its id depends on what the search enumerates: without raw data the
    // search builds only the plans that scan nothing at query time.
    assert!(!selection.selected_candidate_id.is_empty());
    assert!(!plan.stored_outputs.is_empty(), "panes built at ingestion");
    assert_eq!(
        stored_definitions(&plan, "DDSketch").len(),
        1,
        "one DDSketch"
    );
    assert!(
        plan.query_time_raw_windows.is_empty(),
        "no raw samples read"
    );
    // The query's guarantee, with `data`'s declared sign and its 1,000
    // series bound as Stage 3 binds them: a relative error of at most 0.05
    // at failure probability at most 0.3 (DDSketch's bound is
    // deterministic).
    let root = plan.queries[0].root_node_id;
    let guarantee = plan
        .dag
        .dag
        .nodes
        .iter()
        .find(|node| node.id == root)
        .and_then(|node| node.guarantee.clone())
        .expect("the query's guarantee");
    let sign =
        asap_logical_asap_optimization::accuracy::composition::samples_sign_statistic("data");
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
            samples.push(Sample {
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

/// Without the declaration nothing proves `data`'s samples non-negative, so
/// the merged panes' relative bound does not carry through `avg`: no
/// merged-pane DDSketch candidate is accepted, and with no raw samples no
/// plan is valid. The joint search discards the sketch candidates before
/// composition (they have no bound to meet the target), and the plans left
/// all scan raw samples at query time, so its error reports that. The
/// exhaustive search keeps every candidate and shows each merged-pane
/// DDSketch candidate rejected for missing the target with no relative
/// bound.
#[tokio::test]
async fn an_undeclared_gauges_merged_ddsketch_panes_get_no_relative_bound_through_avg() {
    let error = plan_p99_avg(false).await.expect_err("no valid plan");
    assert!(error.contains("no valid candidate"), "{error}");
    assert!(error.contains("cause: RawDataNotRetained"), "{error}");

    // The exhaustive search keeps every candidate in Stage 3, so its report
    // shows why the merged-pane DDSketch candidates fail: their output has
    // no relative bound, which misses the 5% target.
    let workload = p99_avg_workload(false);
    let capabilities = DeploymentCapabilities {
        raw_data_retained: false,
        ..DeploymentCapabilities::default()
    };
    let roots = asap_frontend_promql::lower_promql_query_workload(&workload).expect("lowers");
    let run = asap_plan_selection::run_stage1_to_stage3_with(
        roots.into_iter().enumerate().collect(),
        &workload.query_workload.entries().collect::<Vec<_>>(),
        workload.data_workload.as_ref().unwrap(),
        PlanningModels::builtin().with_deployment_capabilities(&capabilities),
        asap_plan_selection::Stage1ToStage3Search::exhaustive(),
    )
    .expect("Stage 1 accepts the workload");
    // A candidate merges DDSketch panes when it keeps a DDSketch pane chain
    // (its panes are merged at query time).
    let merges_ddsketch_panes = |root: &QueryRoot| {
        let QueryRoot::Operator(root) = root else {
            return false;
        };
        OperatorNode::reachable(root).iter().any(|node| {
            matches!(
                node.asap(),
                Some(ASAPOp::PaneChain {
                    family: asap_types::ir::schema::FieldDataType::Sketch(kind, _),
                    ..
                }) if *kind.algorithm() == asap_types::ir::schema::SketchAlgorithm::DDSketch
            )
        })
    };
    let merged_pane_ids: BTreeSet<&str> = run
        .stage2_physical_asap_optimization
        .iter()
        .filter(|candidate| candidate.roots.iter().any(merges_ddsketch_panes))
        .map(|candidate| candidate.id.as_str())
        .collect();
    assert!(
        !merged_pane_ids.is_empty(),
        "a merged-pane DDSketch candidate"
    );
    let Err(asap_plan_selection::SelectionError::NoValidCandidate(rejections)) =
        run.stage3_plan_selection
    else {
        panic!("no valid candidate");
    };
    for id in merged_pane_ids {
        let rejection = rejections
            .iter()
            .find(|rejection| rejection.id == id)
            .unwrap_or_else(|| panic!("{id} rejected: {rejections:?}"));
        assert_eq!(
            rejection.cause,
            asap_plan_selection::RejectionCause::AccuracyTargetMissed,
            "{rejection:?}"
        );
        assert!(
            rejection.reason.contains("Relative bound None"),
            "{rejection:?}"
        );
    }
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
fn grouped_samples() -> Vec<Sample> {
    let mut samples = Vec::new();
    for (i, (instance, g)) in [("a", "1"), ("b", "1"), ("c", "2")].iter().enumerate() {
        for step in 0..200 {
            samples.push(Sample {
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
/// planned jointly on a deployment that keeps no raw samples, every stored
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
fn grouped_series_samples() -> Vec<Sample> {
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
fn exact_average_of_p99s(samples: &[Sample]) -> BTreeMap<String, f64> {
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

/// Issue #774: without raw samples retained, every candidate of an average
/// of p99s fails a check (the exact ones scan raw samples at query time,
/// the sketch ones have no guarantee), so planning fails. With
/// `ignore_accuracy_guarantees` loaded from the settings JSON, the cheapest
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
    let plan = |settings: PlanSelectionSettings| {
        let workload = workload.clone();
        let capabilities = capabilities.clone();
        async move {
            plan_installable(UserInput::new(
                &workload,
                FrontendInput::Promql { histograms: None },
                PlanningModels::builtin()
                    .with_deployment_capabilities(&capabilities)
                    .with_plan_selection(&settings),
            ))
            .await
        }
    };
    // Without the setting, no candidate is valid.
    let error = plan(PlanSelectionSettings::default())
        .await
        .expect_err("no candidate is valid without the setting");
    assert!(error.to_string().contains("no valid candidate"), "{error}");
    // With it, loaded from JSON as a deployment passes it.
    let settings: PlanSelectionSettings =
        serde_json::from_str(r#"{"ignore_accuracy_guarantees": true}"#).unwrap();
    assert!(settings.ignore_accuracy_guarantees);
    let installed = plan(settings).await.expect("a sketch plan is selected");
    // A per-series KLL or DDSketch plan.
    let families: Vec<String> = installed
        .dag
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
    let report = installed.selection.as_ref().expect("Stage 3's report");
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
    let settings = PlanSelectionSettings {
        ignore_accuracy_guarantees: true,
        ..PlanSelectionSettings::default()
    };
    let plan = plan_installable(UserInput::new(
        &p99_avg_workload(false),
        FrontendInput::Promql { histograms: None },
        PlanningModels::builtin()
            .with_deployment_capabilities(&capabilities)
            .with_plan_selection(&settings),
    ))
    .await
    .expect("a plan is selected");
    let report = plan.selection.as_ref().expect("Stage 3's report");
    assert_eq!(report.queries_accepted_without_guarantee, vec![0]);
    assert!(
        plan.query_time_raw_windows.is_empty(),
        "no raw samples read"
    );
}

/// [`p99_avg_workload`]'s query with `outer` in place of `avg`, planned
/// for a deployment that keeps no raw samples.
async fn plan_p99(outer: &str, values_non_negative: bool) -> InstallablePlan {
    let mut json = serde_json::to_value(p99_avg_workload(values_non_negative)).unwrap();
    json["query_workload"]["repeating_queries"][0]["query"] =
        format!("{outer} by (region, service, job) (quantile_over_time(0.99, data[100s]))").into();
    let workload: PlanningWorkload = serde_json::from_value(json).unwrap();
    let capabilities = DeploymentCapabilities {
        raw_data_retained: false,
        ..DeploymentCapabilities::default()
    };
    plan_installable(UserInput::new(
        &workload,
        FrontendInput::Promql { histograms: None },
        PlanningModels::builtin().with_deployment_capabilities(&capabilities),
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
    Vec<Sample>,
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
            samples.push(Sample {
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
    samples: &[Sample],
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
    let plan = plan_p99("avg", true).await;
    assert!(!plan.stored_outputs.is_empty(), "panes built at ingestion");
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
/// `max` of the per-series p99s carries the relative bound whatever the
/// samples' sign, and every answer equals the query-time reference's and is
/// within 0.05 of the exact value, including series whose samples are all
/// negative.
#[tokio::test]
async fn a_ddsketch_over_signed_samples_runs_within_alpha() {
    let plan = plan_p99("max", false).await;
    assert!(!plan.stored_outputs.is_empty(), "panes built at ingestion");
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
                  "sample_interval": 100}}}}}}}}"#
    ))
    .unwrap()
}

/// The per-second rate of series `instance` (1 or 2) of job `job` (1..=12)
/// in [`counter_samples`]: distinct for every series and every job's sum.
fn counter_rate(job: i64, instance: i64) -> f64 {
    job as f64 + 0.1 * instance as f64
}

/// `data_total` samples of 12 jobs with 2 series each, every 10 s from
/// 595 s before `EVALUATION_MS`: each a counter growing linearly at
/// [`counter_rate`], so its 5-minute `increase` is 300 × its rate (the
/// samples 5 s inside each window end extrapolate to them exactly).
fn counter_samples() -> Vec<Sample> {
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

/// Issue: with guarantees ignored and no raw samples, the counter top-k
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
    let settings = PlanSelectionSettings {
        ignore_accuracy_guarantees: true,
        ..PlanSelectionSettings::default()
    };
    for (function, seconds) in [("increase", 300.0), ("rate", 1.0)] {
        let plan = plan_installable(UserInput::new(
            &counter_topk_workload(function),
            FrontendInput::Promql { histograms: None },
            PlanningModels::builtin()
                .with_deployment_capabilities(&capabilities)
                .with_plan_selection(&settings),
        ))
        .await
        .unwrap_or_else(|error| panic!("{function}: {error}"));
        // An exact plan: no heap over the per-job sums, which have one row
        // per job.
        assert!(
            !plan.dag.dag.nodes.iter().any(|node| matches!(
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
            &counter_samples(),
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
    let mut plan = plan_installable(UserInput::new(
        &counter_topk_workload("rate"),
        FrontendInput::Promql { histograms: None },
        PlanningModels::builtin().with_deployment_capabilities(&capabilities),
    ))
    .await
    .unwrap();
    plan.check_compiles().unwrap();
    let root = plan.queries[0].root_node_id;
    let node = plan
        .dag
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
    assert!(error.starts_with("query run: "), "{error}");
}

/// `queries` every 10 s at `accuracy` (JSON) over six ingesting series of
/// `data`, a non-negative gauge, planned jointly on a deployment that keeps
/// no raw samples: the plan file.
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

/// The definition ids of `plan`'s stored outputs whose node's payload
/// mentions `family` (e.g. `"DDSketch"`, `"Sum"`).
fn stored_definitions<'a>(plan: &'a InstallablePlan, family: &str) -> BTreeSet<&'a str> {
    plan.stored_outputs
        .iter()
        .filter(|output| {
            let node = plan
                .dag
                .dag
                .nodes
                .iter()
                .find(|n| n.id == output.node_id)
                .unwrap();
            serde_json::to_string(&node.payload)
                .unwrap()
                .contains(family)
        })
        .map(|output| output.coverage.definition_id.as_str())
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

/// Stage 3's executor check for a deployment that runs only top-k plans
/// with a heap of this algorithm: any other plan is refused, and such a
/// plan must compile with the Planner's executor
/// ([`PlannerExecutorCompileCheck`]). It forces the cheapest such plan that
/// compiles.
struct HeapPlansOnly(SketchAlgorithm);

impl ExecutorCompileCheck for HeapPlansOnly {
    fn compiles(&self, roots: &[QueryRoot]) -> Result<(), String> {
        let heap = roots
            .iter()
            .filter_map(|root| match root {
                QueryRoot::Operator(node) => Some(node),
                QueryRoot::Scalar(_) => None,
            })
            .flat_map(OperatorNode::reachable)
            .any(|node| {
                matches!(node.asap(), Some(ASAPOp::SummaryAgg { family: FieldDataType::Sketch(kind, _), .. }
                    | ASAPOp::PaneChain { family: FieldDataType::Sketch(kind, _), .. })
                    if *kind.algorithm() == self.0)
            });
        if !heap {
            return Err(format!(
                "this deployment runs only {:?} top-k plans",
                self.0
            ));
        }
        PlannerExecutorCompileCheck.compiles(roots)
    }
}

/// `workload` planned with accuracy guarantees ignored (a top-k estimate
/// has none) on a deployment that keeps no raw samples; with `heap`, Stage
/// 3 keeps only plans with a heap of that algorithm ([`HeapPlansOnly`]).
async fn plan_ignoring_guarantees(
    workload: &PlanningWorkload,
    heap: Option<SketchAlgorithm>,
) -> Result<InstallablePlan, String> {
    let capabilities = DeploymentCapabilities {
        raw_data_retained: false,
        ..DeploymentCapabilities::default()
    };
    let settings = PlanSelectionSettings {
        ignore_accuracy_guarantees: true,
        ..PlanSelectionSettings::default()
    };
    let models = PlanningModels::builtin()
        .with_deployment_capabilities(&capabilities)
        .with_plan_selection(&settings);
    let check = heap.map(HeapPlansOnly);
    let models = match &check {
        Some(check) => models.with_executor_compile_check(check),
        None => models,
    };
    plan_installable(UserInput::new(
        workload,
        FrontendInput::Promql { histograms: None },
        models,
    ))
    .await
    .map_err(|e| e.to_string())
}

/// The heap sketch `plan` builds: its algorithm and width.
fn heap_of(plan: &InstallablePlan) -> Option<(SketchAlgorithm, u32)> {
    plan.dag
        .dag
        .nodes
        .iter()
        .find_map(|node| match &node.payload {
            asap_executor::planner::ir::physical_export::PhysicalASAPOperatorPayload::ASAP(
                ASAPOp::SummaryAgg { family, .. } | ASAPOp::PaneChain { family, .. },
            ) => match family {
                FieldDataType::Sketch(kind, _) => match kind.params() {
                    SketchParams::CmsWithHeap { width, .. }
                    | SketchParams::CountSketchWithHeap { width, .. } => {
                        Some((kind.algorithm().clone(), *width))
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
fn skewed_series_samples() -> (Vec<Sample>, BTreeMap<String, BTreeMap<String, f64>>) {
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

/// `workload`'s plan with only `algorithm`'s heap plans allowed (guarantees
/// ignored): it builds that heap, compiles as a runtime compiles it, and
/// both it and its query-time reference answer `samples` with the true top
/// `k` per partition, each named by its labels and within the heap's bound
/// of `truth` ([`assert_heap_topk_answers`]).
async fn assert_forced_heap_answers(
    workload: &PlanningWorkload,
    algorithm: SketchAlgorithm,
    (samples, truth): (&[Sample], &BTreeMap<String, BTreeMap<String, f64>>),
    labels: (Option<&str>, &str, usize),
) {
    let plan = plan_ignoring_guarantees(workload, Some(algorithm.clone()))
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
fn skewed_count_samples() -> (Vec<Sample>, BTreeMap<String, BTreeMap<String, f64>>) {
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
/// in [`skewed_counter_samples`]: `50 / job^1.2`, plus a little per
/// instance, a skewed ranking of the jobs' sums.
fn skewed_counter_rate(job: i64, instance: i64) -> f64 {
    50.0 / (job as f64).powf(1.2) + 0.01 * instance as f64
}

/// `data_total` samples of 12 jobs with 2 series each, every 10 s from 595 s
/// before `EVALUATION_MS`, each a counter growing linearly at
/// [`skewed_counter_rate`], as [`counter_samples`].
fn skewed_counter_samples() -> Vec<Sample> {
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
    let samples = skewed_counter_samples();
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

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
    plan_installable, DeploymentCapabilities, FrontendInput, InstallablePlan, InstallablePlanError,
    OptimizationInput, OptimizationPass, OptimizeError, PhysicalASAPDAG, PlanOutput,
    PlanSelectionSettings, PlanningModels, PlanningWorkload, SelectionBox, Stage1ToStage3Pass,
    StoredOutput, StoredOutputBuild, UserInput,
};
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
        // The span of the data it summarizes: its windows' and stored
        // inputs' spans together.
        let span = output
            .inputs
            .raw_sample_windows
            .iter()
            .map(|window| window.bounds_ms(evaluation_ms))
            .chain(
                output
                    .inputs
                    .stored_outputs
                    .iter()
                    .map(|id| built[&(*id as u64)].1),
            )
            .reduce(|(a, b), (c, d)| (a.min(c), b.max(d)))
            .expect("a stored output reads something");
        let key = state_key(output, evaluation_ms);
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
                let (_, end_ms) = window.bounds_ms(evaluation_ms);
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
/// (`max_over_time(latency_ms[5m])` every minute over 1M series, answered
/// as the last hour at 1-minute steps): Stage 3 keeps five 1-minute panes
/// built at query time, and feeds their ranges only through those builds,
/// so no window is listed twice. The first evaluation, with an empty
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
            "data_workload": {"arrival": {"ingesting": {"rows_per_second": 66666.67}},
              "sources": {"latency_ms": {"series_count": 1000000, "sample_interval": 15000}}}}"#,
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

/// A coarse rollup (`sum by (job)`) built at ingestion from the finer
/// stored sums (`sum by (job, instance)`): both are listed, the rollup
/// after the sums it reads, with the sums' time selection under a
/// definition of its own; built in that order from stored inputs, they
/// answer as the query-time reference.
#[tokio::test]
async fn a_rollup_from_finer_stored_sums_is_listed_and_built() {
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
    let [finer, coarse] = plan.stored_outputs.as_slice() else {
        panic!("{:#?}", plan.stored_outputs);
    };
    assert!(finer.inputs.stored_outputs.is_empty());
    assert_eq!(finer.inputs.raw_sample_windows.len(), 1);
    assert_eq!(coarse.inputs.stored_outputs, [finer.node_id]);
    assert!(coarse.inputs.raw_sample_windows.is_empty());
    for output in [finer, coarse] {
        assert_eq!(output.built_at, StoredOutputBuild::IngestionRun);
        assert_eq!(output.coverage.selection, relative_time(-60_000, 0));
        assert_eq!(output.retention_ms, 60_000);
    }
    assert_ne!(coarse.coverage.definition_id, finer.coverage.definition_id);
    assert!(plan.query_time_raw_windows.is_empty());
    let run = run_data_plane(
        &plan_json,
        &job_samples(),
        EVALUATION_MS,
        &mut SummaryStore::new(),
    );
    assert_eq!(run.outputs_built, 2);
    assert_eq!(run.answers_per_query[0].len(), 2, "one sum per job");
    assert_eq!(run.answers_per_query, run.reference_answers_per_query);
}

/// A maximum across series (`max by (job)`) built at ingestion from the
/// stored per-series maxima: listed after them with their time selection,
/// under a definition of its own.
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
    let [per_series, across, other_per_series] = plan.stored_outputs.as_slice() else {
        panic!("{:#?}", plan.stored_outputs);
    };
    assert_eq!(across.inputs.stored_outputs, [per_series.node_id]);
    assert!(across.inputs.raw_sample_windows.is_empty());
    assert!(other_per_series.inputs.stored_outputs.is_empty());
    for output in [per_series, across, other_per_series] {
        assert_eq!(output.built_at, StoredOutputBuild::IngestionRun);
        assert_eq!(output.coverage.selection, relative_time(-60_000, 0));
    }
    // The second query's per-series maxima read no `job` column, so they
    // are another state than the first query's.
    assert_ne!(
        per_series.coverage.definition_id,
        other_per_series.coverage.definition_id
    );
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
    assert_eq!(run.outputs_built, 3);
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
    let workload = exact_workload(
        &["max_over_time(m[5m])"],
        r#", "range_evaluation": {"step_ms": 60000, "span_ms": 3600000}"#,
    );
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

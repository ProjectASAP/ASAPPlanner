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
    PhysicalASAPDAG, PlanSelectionSettings, PlanningModels, PlanningWorkload, StoredOutputBuild,
    UserInput,
};
use futures::{executor::block_on, StreamExt};

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
            batch.rows().iter().map(|row| {
                let labels = series_labels_of_output_row(batch.schema(), row).unwrap();
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
    /// How many panes this evaluation built (ingestion or query runs);
    /// the others it read came from the store.
    panes_built: usize,
}

/// One pane the data plane stores: its built state, and how long to keep it.
struct StoredPane {
    /// The built state, bound at each stored output of its computation.
    state: Batch,
    /// When the pane's window starts, in Unix milliseconds.
    start_ms: i64,
    /// How far back from the newest evaluation the pane is kept.
    retention_ms: u64,
}

/// The data plane's pane store, by computation and pane end: stored
/// outputs with the same key are the same state, across queries and
/// evaluations.
type PaneStore = BTreeMap<(String, i64), StoredPane>;

/// The node that answers each query of `plan`, in workload order.
fn query_roots(plan: &InstallablePlan) -> Vec<u64> {
    plan.queries
        .iter()
        .map(|query| query.root_node_id as u64)
        .collect()
}

/// The data plane at evaluation time `evaluation_ms`: from `plan_json`,
/// `samples` and the panes earlier evaluations left in `store`, build each
/// distinct stored pane the store lacks once, answer every query, drop the
/// panes past their retention, and answer the queries again with the
/// query-time reference.
fn run_data_plane(
    plan_json: &str,
    samples: &[Sample],
    evaluation_ms: i64,
    store: &mut PaneStore,
) -> DataPlaneRun {
    // The installed plan, read back from its file and checked.
    let installed: InstallablePlan = serde_json::from_str(plan_json).unwrap();
    installed.validate().unwrap();
    // The DAG the executor runs.
    let dag = &installed.dag.dag;
    // Every input of the query run, keyed by the node it is bound to.
    let mut inputs = BTreeMap::new();
    // How many panes this evaluation builds.
    let mut panes_built = 0;
    for output in &installed.stored_outputs {
        // The pane's window at this evaluation, `(start_ms, end_ms]`.
        let (start_ms, end_ms) = output.pane.bounds_ms(evaluation_ms);
        // The pane's identity: its computation and where it ends.
        let pane_key = (output.computation_id.clone(), end_ms);
        if !store.contains_key(&pane_key) {
            // Prepared once per window: routes by metric, applies the Scan.
            let selector = RawSampleSelector::new(dag, &output.pane).unwrap();
            // Only the samples of the selector's metric reach it.
            let routed: Vec<Sample> = samples
                .iter()
                .filter(|s| s.labels["__name__"] == selector.metric())
                .cloned()
                .collect();
            // The pane's input rows: the samples its Scan selects in its window.
            let rows = selector.rows(end_ms, &routed).unwrap();
            // The run that builds the pane: an ingestion run over its
            // window, or, for a query-time result kept across evaluations,
            // the query run of this evaluation.
            let scope = match output.built_at {
                StoredOutputBuild::IngestionRun => Scope::Ingestion {
                    window_start_ms: start_ms,
                    window_end_ms: end_ms,
                    revision: 1,
                },
                StoredOutputBuild::QueryRun => Scope::Query {
                    evaluation_time_ms: evaluation_ms,
                    revision: 1,
                },
            };
            // The rows bound to the pane's TimeRange.
            let pane_input = BTreeMap::from([(output.pane.time_range_node_id as u64, rows)]);
            // The batches that run built.
            let built = run(dag, pane_input, &[output.node_id as u64], scope).remove(0);
            // The pane's state, as the store keeps it, in one batch.
            let schema = built[0].schema().clone();
            let state_rows = built.iter().flat_map(|b| b.rows().to_vec()).collect();
            let state = Batch::try_new(schema, state_rows).unwrap();
            store.insert(
                pane_key.clone(),
                StoredPane {
                    state,
                    start_ms,
                    retention_ms: output.retention_ms,
                },
            );
            panes_built += 1;
        }
        inputs.insert(output.node_id as u64, store[&pane_key].state.clone());
    }
    // Raw samples read at query time, other than stored outputs' panes.
    for window in &installed.query_time_raw_windows {
        // The window's selector and its end at this evaluation.
        let selector = RawSampleSelector::new(dag, window).unwrap();
        let (_, end_ms) = window.bounds_ms(evaluation_ms);
        inputs.insert(
            window.time_range_node_id as u64,
            selector.rows(end_ms, samples).unwrap(),
        );
    }
    // The query evaluation, which merges the stored panes.
    let query = Scope::Query {
        evaluation_time_ms: evaluation_ms,
        revision: 1,
    };
    // Each query's answers: (series labels, p99) per series.
    let answers_per_query = run(dag, inputs, &query_roots(&installed), query.clone())
        .iter()
        .map(|batches| answers(batches))
        .collect();
    // The store drops each pane whose window starts more than its
    // retention before this evaluation.
    store.retain(|_, pane| pane.start_ms >= evaluation_ms - pane.retention_ms as i64);

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
                selector.rows(end_ms, samples).unwrap(),
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
        panes_built,
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
        &mut PaneStore::new(),
    );
    assert_eq!(run.panes_built, 6, "six ten-minute panes");
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
        &mut PaneStore::new(),
    );
    let answered = &run.answers_per_query[0];
    assert_eq!(answered.len(), 1, "{answered:?}");
    assert_eq!(answered[0].0["instance"], "a");
    assert_answers_from_pane(answered, 5);
    assert_eq!(run.answers_per_query, run.reference_answers_per_query);
}

/// The p99 over the last hour and over the hour before the last 10
/// minutes store 12 panes of one computation; five panes are the same
/// (offsets 10..50 min), so the data plane builds 7, and both answers equal
/// the reference.
#[tokio::test]
async fn panes_of_one_computation_build_once_across_queries() {
    let plan_json = plan_json(&[
        "quantile_over_time(0.99, latency_ms[60m])",
        "quantile_over_time(0.99, latency_ms[60m] offset 10m)",
    ])
    .await;
    let installed: InstallablePlan = serde_json::from_str(&plan_json).unwrap();
    assert_eq!(installed.stored_outputs.len(), 12);
    let computations: BTreeSet<&str> = installed
        .stored_outputs
        .iter()
        .map(|o| o.computation_id.as_str())
        .collect();
    assert_eq!(computations.len(), 1);
    let run = run_data_plane(
        &plan_json,
        &remote_write_samples(),
        EVALUATION_MS,
        &mut PaneStore::new(),
    );
    assert_eq!(run.panes_built, 7);
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
    let rows = selector.rows(EVALUATION_MS, &samples).unwrap();
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
        .all(|o| o.built_at == StoredOutputBuild::QueryRun && o.pane.width_ms == 60_000));
    let pane_ranges: BTreeSet<_> = installed
        .stored_outputs
        .iter()
        .map(|o| o.pane.time_range_node_id)
        .collect();
    assert!(installed
        .query_time_raw_windows
        .iter()
        .all(|w| !pane_ranges.contains(&w.time_range_node_id)));
    // The store the data plane keeps across evaluations.
    let mut store = PaneStore::new();
    let first = run_data_plane(
        &plan_json,
        &remote_write_samples(),
        EVALUATION_MS,
        &mut store,
    );
    assert_eq!(first.panes_built, 5);
    assert_answers_from_pane(&first.answers_per_query[0], 0);
    assert_eq!(first.answers_per_query, first.reference_answers_per_query);
    let next = run_data_plane(
        &plan_json,
        &remote_write_samples(),
        EVALUATION_MS + 60_000,
        &mut store,
    );
    assert_eq!(next.panes_built, 1, "four panes reused from the store");
    assert_eq!(next.answers_per_query, next.reference_answers_per_query);
}

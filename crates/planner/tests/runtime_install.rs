//! A deployment runtime installs and runs the Planner's plan with public
//! APIs only: `asap_planner` to plan, `asap_executor` to execute (issue
//! #749). The executor links here without its `test-support` feature, so
//! the compiler checks that no internal is used.

use std::collections::BTreeMap;

use asap_executor::operators::Operator as ExecutorOperator;
use asap_executor::physical_planner::{compile, InputContract, Source};
use asap_executor::runtime::{Limits, RunContext, Scope};
use asap_executor::values::{Batch, Value};
use asap_executor::{input_rows_from_samples, series_labels_of_output_row, Sample};
use asap_planner::{
    plan_installable, DeploymentCapabilities, FrontendInput, InstallablePlan, InstallablePlanError,
    InstalledQueryRoot, PhysicalASAPDAG, PlanSelectionSettings, PlanningModels, PlanningWorkload,
    UserInput,
};
use futures::{executor::block_on, StreamExt};

/// Example 4b's evaluation time: the end of the 60-minute window.
const EVALUATION_MS: i64 = 3_600_000;

/// One repeating PromQL `query` every 10 minutes, rank error 0.01 at δ 0.01,
/// over 1,000 ingesting `latency_ms` series sampled every second: #509
/// Example 4b when `query` is its p99.
fn workload(query: &str) -> PlanningWorkload {
    serde_json::from_str(&format!(
        r#"{{"query_workload": {{"language": "promql", "repeating_queries": [{{
              "query": {query:?}, "demand": {{"fixed_interval": 600000}},
              "requirements": {{"accuracy": {{"accuracy_metric": "rank", "max_error": 0.01,
                  "failure_probability": 0.01, "accuracy_scope": "whole_result"}},
                  "max_latency_ms": 200.0}},
              "predictability": {{"predictable": {{"known_at": null}}}}}}]}},
            "data_workload": {{"arrival": {{"ingesting": {{"rows_per_second": 1000.0}}}},
              "sources": {{"latency_ms": {{"series_count": 1000, "sample_interval": 1000}}}}}}}}"#
    ))
    .unwrap()
}

/// Raw `latency_ms` samples of series `a` (100..200) and `b` (300..400),
/// one every 10 s over the hour, off the 10-minute pane boundaries, and one
/// sample of another metric the plan never reads.
fn remote_write_samples() -> Vec<Sample> {
    let series = |metric: &str, instance: &str| {
        BTreeMap::from([
            ("__name__".to_string(), metric.to_string()),
            ("instance".to_string(), instance.to_string()),
        ])
    };
    let mut samples = Vec::new();
    for (instance, base) in [("a", 100.0), ("b", 300.0)] {
        for step in 0..360 {
            samples.push(Sample {
                labels: series("latency_ms", instance),
                timestamp_ms: 5_000 + step * 10_000,
                value: base + (step * 37 % 101) as f64,
            });
        }
    }
    samples.push(Sample {
        labels: series("cpu_seconds", "a"),
        timestamp_ms: 5_000,
        value: 1e9,
    });
    samples
}

/// Compile `ids` of `dag` over `inputs` and run them in `scope`, as a
/// runtime does with the executor: one batch list per id.
fn run(
    dag: &PhysicalASAPDAG,
    inputs: BTreeMap<u64, Batch>,
    ids: &[u64],
    scope: Scope,
) -> Vec<Batch> {
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
        // The output stream of the first id.
        let mut output = instance
            .execute(compiled.roots(), context)
            .unwrap()
            .remove(0);
        // Its batches, collected.
        let mut batches = Vec::new();
        while let Some(batch) = output.next().await {
            batches.push((*batch.unwrap()).clone());
        }
        batches
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

/// Plan `query` for a deployment that keeps no raw samples, with its cost
/// model read from JSON, and write the plan file.
async fn plan_json(query: &str) -> String {
    // The workload the runtime received.
    let workload = workload(query);
    // What the deployment keeps: no raw samples, no query-time results
    // across evaluations. Supported summaries are left unrestricted, so the
    // Planner offers only what its own executor runs.
    let capabilities = DeploymentCapabilities {
        raw_data_retained: false,
        ..DeploymentCapabilities::default()
    };
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
        .with_deployment_capabilities(&capabilities)
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

/// What the data plane answers for the plan in `plan_json` at
/// `EVALUATION_MS` from `samples`, and what the same plan answers with
/// every node at query time (its correctness reference).
fn run_installed_and_reference(plan_json: &str, samples: &[Sample]) -> (Vec<Answer>, Vec<Answer>) {
    // The installed plan, read back from its file and checked.
    let installed: InstallablePlan = serde_json::from_str(plan_json).unwrap();
    installed.validate().unwrap();
    // The DAG the executor runs.
    let dag = &installed.dag.dag;
    // The node that answers the one query.
    let InstalledQueryRoot::Operator { node_id: root } = installed.queries[0].root else {
        panic!("an operator query");
    };
    // Each stored output's built state, keyed by its node, for the query run.
    let mut stored = BTreeMap::new();
    for output in &installed.stored_outputs {
        // The pane's window at this evaluation, `(start_ms, end_ms]`: it
        // ends `window_end_offset_ms` before `EVALUATION_MS`.
        let (start_ms, end_ms) = output.pane.bounds_ms(EVALUATION_MS);
        // The pane's input rows: the samples its Scan selects in its window.
        let rows = input_rows_from_samples(dag, &output.pane, EVALUATION_MS, samples).unwrap();
        // One ingestion run over the pane's window builds the pane.
        let scope = Scope::Ingestion {
            window_start_ms: start_ms,
            window_end_ms: end_ms,
            revision: 1,
        };
        // The rows bound to the pane's TimeRange.
        let input = BTreeMap::from([(output.pane.time_range_node_id as u64, rows)]);
        // The batches that ingestion run built.
        let built = run(dag, input, &[output.node_id as u64], scope);
        // The pane's state, as the store keeps it.
        let schema = built[0].schema().clone();
        // Its rows, in one batch.
        let built_rows = built.iter().flat_map(|b| b.rows().to_vec()).collect();
        stored.insert(
            output.node_id as u64,
            Batch::try_new(schema, built_rows).unwrap(),
        );
    }
    // Inputs read at query time: none here, as no raw samples are kept.
    for window in &installed.query_time_raw_windows {
        // The window's rows: the samples its Scan selects in its window.
        let rows = input_rows_from_samples(dag, window, EVALUATION_MS, samples).unwrap();
        stored.insert(window.time_range_node_id as u64, rows);
    }
    // The query evaluation at the end of the window, which merges the
    // stored panes.
    let query = Scope::Query {
        evaluation_time_ms: EVALUATION_MS,
        revision: 1,
    };
    // The installed plan's answers: (series labels, p99) per series.
    let answered = answers(&run(dag, stored, &[root as u64], query.clone()));

    // The correctness reference: nothing stored, every window fed raw
    // samples at query time.
    let reference = installed.with_every_node_at_query_time().unwrap();
    // The reference's DAG.
    let reference_dag = &reference.dag.dag;
    // The node that answers the query in it.
    let InstalledQueryRoot::Operator {
        node_id: reference_root,
    } = reference.queries[0].root
    else {
        panic!("an operator query");
    };
    // Each query-time window's input rows, keyed by its TimeRange.
    let raw: BTreeMap<u64, Batch> = reference
        .query_time_raw_windows
        .iter()
        .map(|window| {
            // The window's rows at query time.
            let rows =
                input_rows_from_samples(reference_dag, window, EVALUATION_MS, samples).unwrap();
            (window.time_range_node_id as u64, rows)
        })
        .collect();
    // The reference's answers, which the installed plan's must equal.
    let expected = answers(&run(reference_dag, raw, &[reference_root as u64], query));
    (answered, expected)
}

/// #509 Example 4b, as a runtime runs it: one `plan_installable` call,
/// `plan.json`, then each stored pane built in an ingestion run and merged
/// in the query run. One p99 per series (100 + 100 for `a`, 300 + 100 for
/// `b`), equal to the query-time reference; the other metric is not read.
#[tokio::test]
async fn a_runtime_installs_and_runs_the_plan_from_one_facade_call() {
    let plan_json = plan_json("quantile_over_time(0.99, latency_ms[60m])").await;
    let installed: InstallablePlan = serde_json::from_str(&plan_json).unwrap();
    assert_eq!(installed.stored_outputs.len(), 6, "six ten-minute panes");
    let (answered, expected) = run_installed_and_reference(&plan_json, &remote_write_samples());
    let values: Vec<f64> = answered.iter().map(|(_, v)| *v).collect();
    assert_eq!(values, [200.0, 400.0]);
    assert_eq!(answered[0].0["instance"], "a");
    assert_eq!(answered, expected);
}

/// The query's label matcher selects series `a` only: the runtime feeds
/// every sample, and the plan's `Scan` drops series `b`.
#[tokio::test]
async fn label_matchers_select_the_series_a_pane_reads() {
    let plan_json = plan_json(r#"quantile_over_time(0.99, latency_ms{instance="a"}[60m])"#).await;
    let (answered, expected) = run_installed_and_reference(&plan_json, &remote_write_samples());
    assert_eq!(answered.len(), 1, "{answered:?}");
    assert_eq!(answered[0].0["instance"], "a");
    assert_eq!(answered[0].1, 200.0);
    assert_eq!(answered, expected);
}

/// A plan file of another DAG wire version is refused, and so is the
/// reference of a tampered plan file, with an error rather than a panic.
#[tokio::test]
async fn a_plan_file_of_another_version_or_tampered_is_refused() {
    let plan_json = plan_json("quantile_over_time(0.99, latency_ms[60m])").await;
    let mut other_version: InstallablePlan = serde_json::from_str(&plan_json).unwrap();
    other_version.dag.schema_version += 1;
    assert!(matches!(
        other_version.validate(),
        Err(InstallablePlanError::InvalidDag(_))
    ));
    let mut tampered: InstallablePlan = serde_json::from_str(&plan_json).unwrap();
    tampered.queries.clear();
    assert!(matches!(
        tampered.with_every_node_at_query_time(),
        Err(InstallablePlanError::QueriesDoNotMatchRoots { .. })
    ));
    let mut no_roots: InstallablePlan = serde_json::from_str(&plan_json).unwrap();
    no_roots.dag.dag.roots.clear();
    assert!(no_roots.with_every_node_at_query_time().is_err());
}

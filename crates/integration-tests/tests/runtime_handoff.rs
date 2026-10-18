//! A deployment runtime calls the facade once (`plan_installable`), writes
//! the returned plan as `plan.json`, and executes it with the executor
//! alone (issue #749).
mod physical_common;

use std::collections::BTreeMap;
use std::sync::Arc;

use asap_executor::physical_planner::{compile, frontier_from_timing, InputContract};
use asap_executor::runtime::Scope;
use asap_executor::values::{Batch, Value};
use asap_executor::{input_rows_from_samples, series_labels_of_output_row, Sample};
use asap_logical_asap_optimization::DeploymentCapabilities;
use asap_plan_selection::PlanSelectionSettings;
use asap_planner::{
    e2e_plan, plan_installable, FrontendInput, InstallablePlan, InstalledQueryRoot, PlanOutput,
    PlanningModels, RawSampleInput, UserInput,
};
use asap_types::ir::physical_export::{
    compile_physical_asap_workload, PhysicalASAPDAG, PhysicalASAPDAGNode, PhysicalASAPNodeId,
};
use asap_types::ir::properties::ExecutionTiming;
use asap_types::ir::{ASAPOp, NonASAPOp, Operator};
use asap_types::physical::Materialization;
use asap_types::workload::PlanningWorkload;

/// #509 Example 4b: one repeating `quantile_over_time` over a 60-minute
/// window, evaluated every 10 minutes, over ingesting data.
fn example4b() -> PlanningWorkload {
    serde_json::from_str(
        r#"{
          "query_workload": {
            "language": "promql",
            "repeating_queries": [{
              "query": "quantile_over_time(0.99, latency_ms[60m])",
              "demand": {"fixed_interval": 600000},
              "requirements": {
                "accuracy": {"accuracy_metric": "rank", "max_error": 0.01,
                             "failure_probability": 0.01, "accuracy_scope": "whole_result"},
                "max_latency_ms": 200.0
              },
              "predictability": {"predictable": {"known_at": null}}
            }]
          },
          "data_workload": {
            "arrival": {"ingesting": {"rows_per_second": 1000.0}},
            "sources": {"latency_ms": {"series_count": 1000, "sample_interval": 1000}}
          }
        }"#,
    )
    .expect("Example 4b parses")
}

/// A deployment that keeps no raw samples: every summary must be built at
/// ingestion time.
fn without_raw_data() -> DeploymentCapabilities {
    DeploymentCapabilities {
        raw_data_retained: false,
        ..DeploymentCapabilities::default()
    }
}

async fn plan(workload: &PlanningWorkload, capabilities: &DeploymentCapabilities) -> PlanOutput {
    let models = PlanningModels::builtin().with_deployment_capabilities(capabilities);
    e2e_plan(UserInput::new(
        workload,
        FrontendInput::Promql { histograms: None },
        models,
    ))
    .await
    .expect("Example 4b plans")
}

fn materialized_count(dag: &PhysicalASAPDAG) -> usize {
    dag.nodes
        .iter()
        .filter(|n| n.materialization != Materialization::NotMaterialized)
        .count()
}

/// Without raw data Stage 3 selects ingestion-time panes; the facade's
/// export carries that selection rather than resetting every node to
/// query time.
#[tokio::test]
async fn export_carries_the_selected_materialization() {
    let capabilities = without_raw_data();
    let output = plan(&example4b(), &capabilities).await;
    let dag = output.physical_asap_dag().expect("exports");
    assert!(
        materialized_count(&dag) > 0,
        "the selected panes are stored"
    );
    assert!(dag
        .nodes
        .iter()
        .any(|n| n.output_state.timing == ExecutionTiming::IngestionTime));
    // Exactly Stage 3's selected plan, as Stage 3 timed it.
    let selection = output.selection.as_ref().expect("Stage 3 selected");
    assert_eq!(
        dag,
        compile_physical_asap_workload(&selection.plan).expect("the selected plan exports")
    );
}

/// What a runtime that keeps no raw samples gets from one facade call.
async fn installable(workload: &PlanningWorkload) -> InstallablePlan {
    let capabilities = without_raw_data();
    let models = PlanningModels::builtin().with_deployment_capabilities(&capabilities);
    plan_installable(UserInput::new(
        workload,
        FrontendInput::Promql { histograms: None },
        models,
    ))
    .await
    .expect("Example 4b plans")
}

/// The deployment's raw input contract for each raw `TimeRange` of `dag`:
/// the rows a runtime binds to each range it scans.
fn raw_range_inputs(dag: &PhysicalASAPDAG) -> BTreeMap<u64, InputContract> {
    dag.nodes
        .iter()
        .filter(|n| matches!(n.payload, Operator::NonASAP(NonASAPOp::TimeRange { .. })))
        .map(|n| {
            (
                n.id as u64,
                InputContract::bounded(Arc::new(n.output_schema.clone())),
            )
        })
        .collect()
}

fn pane_chain_count(dag: &PhysicalASAPDAG) -> usize {
    dag.nodes
        .iter()
        .filter(|n| matches!(n.payload, Operator::ASAP(ASAPOp::PaneChain { .. })))
        .count()
}

/// The installable plan has the selected pane chain expanded into the six
/// ten-minute panes the executor runs, each stored at ingestion time, and
/// the executor compiles it. The stage export keeps the chain for costing
/// and display. Its one query is answered by the DAG's one root.
#[tokio::test]
async fn installable_plan_has_its_pane_chains_expanded() {
    let workload = example4b();
    let plan = installable(&workload).await;
    plan.validate().unwrap();
    assert_eq!(pane_chain_count(&plan.dag), 0);
    assert_eq!(materialized_count(&plan.dag), 6, "six ten-minute panes");
    let roots: Vec<u64> = plan.dag.roots.iter().map(|&r| r as u64).collect();
    compile(&plan.dag, raw_range_inputs(&plan.dag), &roots).expect("the executor compiles it");
    assert_eq!(plan.queries.len(), 1);
    assert_eq!(
        plan.queries[0].query,
        "quantile_over_time(0.99, latency_ms[60m])"
    );
    assert_eq!(
        plan.queries[0].root,
        InstalledQueryRoot::Operator {
            node_id: plan.dag.roots[0]
        }
    );
    // The stored outputs are the ingestion frontier: the six panes, each
    // ten minutes wide, ending 0, 10, ..., 50 minutes before evaluation.
    let stored: Vec<u64> = plan
        .stored_outputs
        .iter()
        .map(|o| o.node_id as u64)
        .collect();
    assert_eq!(stored, frontier_from_timing(&plan.dag).unwrap());
    let mut offsets: Vec<i64> = plan
        .stored_outputs
        .iter()
        .map(|o| {
            let pane = o.pane.as_ref().unwrap();
            assert_eq!(pane.window_width_ms, 600_000);
            pane.window_end_offset_ms
        })
        .collect();
    offsets.sort();
    assert_eq!(offsets, (0..6).map(|i| i * 600_000).collect::<Vec<_>>());
    let report = plan.selection.as_ref().expect("Stage 3 selected");
    assert_eq!(report.accepted_accuracy_per_query.len(), 1);

    let staged = plan_for_stages(&workload).await;
    assert_eq!(
        pane_chain_count(&staged),
        1,
        "the stage export keeps the chain"
    );
}

async fn plan_for_stages(workload: &PlanningWorkload) -> PhysicalASAPDAG {
    plan(workload, &without_raw_data())
        .await
        .physical_asap_dag()
        .unwrap()
}

/// Each summary build of `dag` (`SummaryAgg` or `PaneChain`) as its JSON
/// payload, sorted: which families with which parameters it builds.
fn summary_builds(dag: &PhysicalASAPDAG) -> Vec<String> {
    let mut builds: Vec<String> = dag
        .nodes
        .iter()
        .filter(|n| {
            matches!(
                n.payload,
                Operator::ASAP(ASAPOp::SummaryAgg { .. } | ASAPOp::PaneChain { .. })
            )
        })
        .map(|n| serde_json::to_string(&n.payload).unwrap())
        .collect();
    builds.sort();
    builds
}

/// The correctness reference of the installable plan: the same summaries
/// with the same parameters, every node at query time and nothing stored,
/// with the same queries bound to its roots.
#[tokio::test]
async fn reference_plan_runs_the_installable_plan_at_query_time() {
    let plan = installable(&example4b()).await;
    let reference = plan.with_every_node_at_query_time().expect("re-times");
    reference.validate().unwrap();
    assert_eq!(materialized_count(&reference.dag), 0);
    assert!(reference
        .dag
        .nodes
        .iter()
        .all(|n| n.output_state.timing == ExecutionTiming::QueryTime));
    assert_eq!(summary_builds(&reference.dag), summary_builds(&plan.dag));
    assert_eq!(
        reference.queries[0].root,
        InstalledQueryRoot::Operator {
            node_id: reference.dag.roots[0]
        }
    );
}

// ── End to end: a runtime that holds only `plan.json` ───────────────────

/// Example 4b's evaluation time: the end of the 60-minute window.
const EVALUATION_MS: i64 = 3_600_000;

/// Raw `latency_ms` samples of two series, one every 10 s over the
/// window, off the 10-minute pane boundaries.
fn latency_samples() -> Vec<Sample> {
    let mut samples = Vec::new();
    for (series, base) in [("a", 100.0), ("b", 300.0)] {
        let labels = BTreeMap::from([
            ("__name__".to_string(), "latency_ms".to_string()),
            ("instance".to_string(), series.to_string()),
        ]);
        for step in 0..360 {
            samples.push(Sample {
                labels: labels.clone(),
                timestamp_ms: 5_000 + step * 10_000,
                value: base + (step * 37 % 101) as f64,
            });
        }
    }
    samples
}

/// Compile `dag`'s `ids` over `inputs` and run them in `scope`: what a
/// runtime does with the executor.
fn run(
    dag: &PhysicalASAPDAG,
    inputs: BTreeMap<u64, Batch>,
    ids: &[u64],
    scope: Scope,
) -> Vec<Batch> {
    let contracts = inputs
        .iter()
        .map(|(&id, batch)| (id, InputContract::bounded(batch.schema().clone())))
        .collect();
    let plan = compile(dag, contracts, ids).expect("the executor compiles the plan");
    physical_common::execute(&plan, inputs, scope).remove(0)
}

fn node(dag: &PhysicalASAPDAG, id: PhysicalASAPNodeId) -> &PhysicalASAPDAGNode {
    dag.nodes.iter().find(|n| n.id == id).unwrap()
}

/// `(start, end]` of `input`'s window at `EVALUATION_MS`.
fn window(input: &RawSampleInput) -> (i64, i64) {
    let end = EVALUATION_MS - input.window_end_offset_ms;
    (end - input.window_width_ms, end)
}

/// The input rows of `input`: the samples of its window.
fn input_rows(dag: &PhysicalASAPDAG, input: &RawSampleInput, samples: &[Sample]) -> Batch {
    let (start, end) = window(input);
    let in_window: Vec<Sample> = samples
        .iter()
        .filter(|s| start < s.timestamp_ms && s.timestamp_ms <= end)
        .cloned()
        .collect();
    input_rows_from_samples(node(dag, input.time_range_node_id), &in_window).unwrap()
}

/// Each answer row's series labels and value, sorted by labels.
fn answers(batches: &[Batch]) -> Vec<(BTreeMap<String, String>, f64)> {
    let mut answers: Vec<_> = batches
        .iter()
        .flat_map(|batch| {
            batch.rows().iter().map(|row| {
                let labels = series_labels_of_output_row(batch.schema(), row).unwrap();
                let value = row
                    .iter()
                    .find_map(|v| match v {
                        Value::Float64(v) => Some(*v),
                        _ => None,
                    })
                    .unwrap();
                (labels, value)
            })
        })
        .collect();
    answers.sort_by(|a, b| a.0.cmp(&b.0));
    answers
}

/// #509 Example 4b, as a runtime runs it: it builds the workload and its
/// deployment inputs, calls `plan_installable` once, writes the result as
/// `plan.json`, and from that file alone builds each stored pane in an
/// ingestion run and merges them in the query run, with the executor and
/// no other Planner call. Its answers equal the executor running the same
/// plan entirely at query time over raw samples.
#[tokio::test]
async fn a_runtime_installs_and_runs_the_plan_from_one_facade_call() {
    // ── Runtime, planning: the workload and the deployment inputs ──
    // The workload: one p99 over 60 minutes, every 10 minutes.
    let workload = example4b();
    // What the deployment keeps: no raw samples, no query-time results
    // across evaluations. Supported summaries are left unrestricted, so the
    // Planner offers only what its own executor runs.
    let capabilities = without_raw_data();
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
    let plan_json = serde_json::to_string(&plan).unwrap();

    // ── Runtime, executing: only `plan_json` and the executor ──
    let installed: InstallablePlan = serde_json::from_str(&plan_json).unwrap();
    installed.validate().unwrap();
    let dag = &installed.dag;
    // The query's root, run at query time.
    let InstalledQueryRoot::Operator { node_id: root } = installed.queries[0].root else {
        panic!("an operator query");
    };
    // The raw samples Remote Write delivered.
    let samples = latency_samples();
    // Each stored output: build it in an ingestion run over its pane's
    // samples, fed to its pane's `TimeRange`, and keep its output.
    let mut stored = BTreeMap::new();
    for output in &installed.stored_outputs {
        assert!(matches!(
            output.materialization,
            Materialization::MatAtIngestion(_)
        ));
        let pane = output
            .pane
            .as_ref()
            .expect("each stored output reads one pane");
        let (start, end) = window(pane);
        let input = BTreeMap::from([(
            pane.time_range_node_id as u64,
            input_rows(dag, pane, &samples),
        )]);
        let scope = Scope::Ingestion {
            window_start_ms: start,
            window_end_ms: end,
            revision: 1,
        };
        let rows: Vec<_> = run(dag, input, &[output.node_id as u64], scope)
            .iter()
            .flat_map(|b| b.rows().to_vec())
            .collect();
        let schema = Arc::new(node(dag, output.node_id).output_schema.clone());
        stored.insert(output.node_id as u64, Batch::try_new(schema, rows).unwrap());
    }
    assert_eq!(stored.len(), 6, "six ten-minute panes");
    // Panes that differ only in their offset compute the same thing.
    let computations: std::collections::BTreeSet<_> = installed
        .stored_outputs
        .iter()
        .map(|o| o.computation_id.clone())
        .collect();
    assert_eq!(computations.len(), 1);
    // The query run at the end of the window merges the stored panes.
    let query = Scope::Query {
        evaluation_time_ms: EVALUATION_MS,
        revision: 1,
    };
    let answered = answers(&run(dag, stored, &[root as u64], query.clone()));
    // One p99 per series: 100 + 100 for `a`, 300 + 100 for `b`.
    let values: Vec<f64> = answered.iter().map(|(_, v)| *v).collect();
    assert_eq!(values, [200.0, 400.0]);
    assert_eq!(answered[0].0["instance"], "a");

    assert!(
        installed.query_time_raw_inputs.is_empty(),
        "no raw data kept"
    );

    // ── Runtime, correctness test: the same plan at query time ──
    // Nothing is stored; each pane's `TimeRange` is fed its window's raw
    // samples at query time.
    let reference = installed.with_every_node_at_query_time().unwrap();
    assert!(reference.stored_outputs.is_empty());
    assert_eq!(reference.query_time_raw_inputs.len(), 6);
    let InstalledQueryRoot::Operator {
        node_id: reference_root,
    } = reference.queries[0].root
    else {
        panic!("an operator query");
    };
    let raw: BTreeMap<u64, Batch> = reference
        .query_time_raw_inputs
        .iter()
        .map(|input| {
            let rows = input_rows(&reference.dag, input, &samples);
            (input.time_range_node_id as u64, rows)
        })
        .collect();
    let expected = answers(&run(&reference.dag, raw, &[reference_root as u64], query));
    assert_eq!(answered, expected);
}

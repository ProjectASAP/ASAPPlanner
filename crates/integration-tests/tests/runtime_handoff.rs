//! A deployment runtime consumes Stage 3's selected plan through the
//! facade's exported `PhysicalASAPDAG` document alone (issue #749).
mod physical_common;

use std::collections::BTreeMap;
use std::sync::Arc;

use asap_executor::physical_planner::{compile, promql_rows, InputContract};
use asap_executor::runtime::Scope;
use asap_executor::supported_summary_families;
use asap_executor::values::{Batch, Value};
use asap_logical_asap_optimization::DeploymentCapabilities;
use asap_plan_selection::PlanSelectionSettings;
use asap_planner::{e2e_plan, FrontendInput, PlanOutput, PlanningModels, UserInput};
use asap_types::ir::physical_export::{
    compile_physical_asap_workload, PhysicalASAPDAG, PhysicalASAPDAGDocument,
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

/// A runtime holding only the exported document (as JSON) expands its
/// pane chains into the panes the executor runs, each stored as the chain
/// was, and the executor compiles the result.
#[tokio::test]
async fn pane_chains_expand_on_the_exported_document() {
    let output = plan(&example4b(), &without_raw_data()).await;
    let json = serde_json::to_string(&PhysicalASAPDAGDocument::new(
        output.physical_asap_dag().expect("exports"),
    ))
    .unwrap();
    let document: PhysicalASAPDAGDocument = serde_json::from_str(&json).unwrap();
    document.validate().unwrap();
    assert_eq!(pane_chain_count(&document.dag), 1, "one chain of six panes");
    let roots: Vec<u64> = document.dag.roots.iter().map(|&r| r as u64).collect();
    let unexpanded = compile(&document.dag, raw_range_inputs(&document.dag), &roots);
    assert!(
        unexpanded.is_err(),
        "the executor runs a chain as its panes"
    );

    let expanded = document.dag.with_pane_chains_expanded().expect("expands");
    assert_eq!(pane_chain_count(&expanded), 0);
    assert_eq!(materialized_count(&expanded), 6, "six ten-minute panes");
    compile(&expanded, raw_range_inputs(&expanded), &roots).expect("the executor compiles it");
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

/// The correctness reference of the selected plan, from the exported
/// document alone: the same summaries with the same parameters, every node
/// at query time and nothing stored.
#[tokio::test]
async fn reference_plan_runs_the_selected_plan_at_query_time() {
    let output = plan(&example4b(), &without_raw_data()).await;
    let selected = output.physical_asap_dag().expect("exports");
    let reference = selected.with_every_node_at_query_time().expect("re-times");
    assert_eq!(materialized_count(&reference), 0);
    assert!(reference
        .nodes
        .iter()
        .all(|n| n.output_state.timing == ExecutionTiming::QueryTime));
    assert_eq!(summary_builds(&reference), summary_builds(&selected));
    assert_eq!(reference.roots.len(), selected.roots.len());
}

// ── End to end: a runtime that holds only the exported document ─────────

/// Example 4b's evaluation time: the end of the 60-minute window.
const EVALUATION_MS: i64 = 3_600_000;

/// Raw `latency_ms` samples of two series, one every 10 s over the
/// window, off the 10-minute pane boundaries.
fn latency_samples() -> Vec<(BTreeMap<String, String>, i64, f64)> {
    let mut samples = Vec::new();
    for (series, base) in [("a", 100.0), ("b", 300.0)] {
        let labels = BTreeMap::from([
            ("__name__".to_string(), "latency_ms".to_string()),
            ("instance".to_string(), series.to_string()),
        ]);
        for step in 0..360 {
            let ts = 5_000 + step * 10_000;
            samples.push((labels.clone(), ts, base + (step * 37 % 101) as f64));
        }
    }
    samples
}

/// The window `[start, end)` of evaluation time a `TimeRange` node reads:
/// its range, ending its `TimeShift` child's offset before `EVALUATION_MS`.
fn range_window(dag: &PhysicalASAPDAG, range: u64) -> (i64, i64) {
    let node = |id: u64| dag.nodes.iter().find(|n| n.id as u64 == id).unwrap();
    let Operator::NonASAP(NonASAPOp::TimeRange {
        range: width,
        child,
        ..
    }) = &node(range).payload
    else {
        panic!("not a range");
    };
    let back = match &node(*child as u64).payload {
        Operator::NonASAP(NonASAPOp::TimeShift { shift, .. }) => shift.offset_ms,
        _ => 0,
    };
    let end = EVALUATION_MS - back;
    (end - width.as_millis() as i64, end)
}

/// The raw rows the runtime binds to `range`: the samples in its window.
fn raw_rows(dag: &PhysicalASAPDAG, range: u64) -> Batch {
    let schema = Arc::new(
        dag.nodes
            .iter()
            .find(|n| n.id as u64 == range)
            .unwrap()
            .output_schema
            .clone(),
    );
    let (start, end) = range_window(dag, range);
    let rows = latency_samples()
        .into_iter()
        .filter(|(_, ts, _)| (start..end).contains(ts))
        .map(|(labels, ts, value)| promql_rows::series_row(&schema, &labels, ts, value).unwrap())
        .collect();
    Batch::try_new(schema, rows).unwrap()
}

/// Run `inputs` through `ids` of `dag` compiled over those inputs.
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

/// The `Float64` value of each output row, ascending.
fn p99s(batches: &[Batch]) -> Vec<f64> {
    let mut values: Vec<f64> = batches
        .iter()
        .flat_map(|b| b.rows().iter())
        .flat_map(|row| {
            row.iter().filter_map(|v| match v {
                Value::Float64(v) => Some(*v),
                _ => None,
            })
        })
        .collect();
    values.sort_by(f64::total_cmp);
    values
}

/// Each output row as text, sorted: `Value` has no equality.
fn rendered(batches: &[Batch]) -> Vec<String> {
    let mut rows: Vec<String> = batches
        .iter()
        .flat_map(|b| b.rows().iter().map(|row| format!("{row:?}")))
        .collect();
    rows.sort();
    rows
}

/// The selected plan of #509 Example 4b, consumed from its exported JSON
/// document alone: the runtime expands the pane chain, builds the six
/// ten-minute KLL panes in ingestion runs, merges them in the query run,
/// and answers what the executor answers running the same plan entirely
/// at query time over raw data.
#[tokio::test]
async fn a_runtime_runs_the_selected_plan_from_the_exported_document_alone() {
    // ── Planner side ──
    // What the deployment's executor builds (item 3) and keeps: no raw
    // samples, so summaries must be built at ingestion time.
    let capabilities = DeploymentCapabilities {
        supported_summaries: Some(supported_summary_families()),
        raw_data_retained: false,
        query_time_results_retained: false,
    };
    // The deployment's Stage 3 prices, from a file (item 4): cost model 1.
    let settings: PlanSelectionSettings = serde_json::from_str(
        r#"{"cost_model": {"usage_over_time": {
              "cost_per_cpu_op": 1e-6, "cost_per_scanned_byte": 1e-7,
              "cost_per_memory_byte_second": 1.25e-7,
              "cost_per_provisioned_memory_byte_second": 1.25e-7,
              "cost_per_provisioned_cpu_core_second": 1000.0,
              "cost_per_disk_byte_second": 0.0}}}"#,
    )
    .unwrap();
    let workload = example4b();
    let models = PlanningModels::builtin()
        .with_deployment_capabilities(&capabilities)
        .with_plan_selection(&settings);
    let output = e2e_plan(UserInput::new(
        &workload,
        FrontendInput::Promql { histograms: None },
        models,
    ))
    .await
    .unwrap();
    // `plan.json`: Stage 3's selected plan with its timings and
    // materialization (item 1).
    let plan_json = serde_json::to_string(&PhysicalASAPDAGDocument::new(
        output.physical_asap_dag().unwrap(),
    ))
    .unwrap();

    // ── Runtime side: only `plan_json` ──
    let document: PhysicalASAPDAGDocument = serde_json::from_str(&plan_json).unwrap();
    document.validate().unwrap();
    // The panes the executor runs, each stored as its chain was (item 2).
    let installed = document.dag.with_pane_chains_expanded().unwrap();
    let roots: Vec<u64> = installed.roots.iter().map(|&r| r as u64).collect();
    let mut stored = BTreeMap::new();
    for pane in installed
        .nodes
        .iter()
        .filter(|n| n.materialization != Materialization::NotMaterialized)
    {
        let Operator::ASAP(ASAPOp::SummaryAgg { child: range, .. }) = pane.payload else {
            panic!("only panes are stored");
        };
        let (start, end) = range_window(&installed, range as u64);
        let input = BTreeMap::from([(range as u64, raw_rows(&installed, range as u64))]);
        let scope = Scope::Ingestion {
            window_start_ms: start,
            window_end_ms: end,
            revision: 1,
        };
        let rows: Vec<_> = run(&installed, input, &[pane.id as u64], scope)
            .iter()
            .flat_map(|b| b.rows().to_vec())
            .collect();
        let schema = Arc::new(pane.output_schema.clone());
        stored.insert(pane.id as u64, Batch::try_new(schema, rows).unwrap());
    }
    assert_eq!(stored.len(), 6, "six ten-minute panes");
    let query = Scope::Query {
        evaluation_time_ms: EVALUATION_MS,
        revision: 1,
    };
    let answered = run(&installed, stored, &roots, query.clone());
    // One p99 per series: 100 + 100 for `a`, 300 + 100 for `b`.
    assert_eq!(p99s(&answered), [200.0, 400.0]);
    let answers = rendered(&answered);

    // The correctness reference (item 5): the same plan with nothing
    // materialized, every pane built at query time from raw rows.
    let reference = document
        .dag
        .with_every_node_at_query_time()
        .unwrap()
        .with_pane_chains_expanded()
        .unwrap();
    let raw: BTreeMap<u64, Batch> = reference
        .nodes
        .iter()
        .filter(|n| matches!(n.payload, Operator::NonASAP(NonASAPOp::TimeRange { .. })))
        .map(|n| (n.id as u64, raw_rows(&reference, n.id as u64)))
        .collect();
    let expected = rendered(&run(&reference, raw, &roots, query));
    assert_eq!(answers, expected);
}

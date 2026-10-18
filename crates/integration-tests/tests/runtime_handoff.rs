//! A deployment runtime consumes Stage 3's selected plan through the
//! facade's exported `PhysicalASAPDAG` document alone (issue #749).

use std::collections::BTreeMap;
use std::sync::Arc;

use asap_executor::physical_planner::{compile, InputContract};
use asap_logical_asap_optimization::DeploymentCapabilities;
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

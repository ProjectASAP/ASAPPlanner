//! A deployment runtime consumes Stage 3's selected plan through the
//! facade's exported `PhysicalASAPDAG` document alone (issue #749).

use asap_logical_asap_optimization::DeploymentCapabilities;
use asap_planner::{e2e_plan, FrontendInput, PlanOutput, PlanningModels, UserInput};
use asap_types::ir::physical_export::{compile_physical_asap_workload, PhysicalASAPDAG};
use asap_types::ir::properties::ExecutionTiming;
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

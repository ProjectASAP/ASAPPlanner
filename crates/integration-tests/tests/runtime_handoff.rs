//! What `plan_installable` returns for a deployment runtime (issue #749),
//! checked against the Planner's internals; the runtime's own end-to-end
//! run, with public APIs only, is `crates/planner/tests/runtime_install.rs`.

use std::collections::BTreeMap;
use std::sync::Arc;

use asap_executor::physical_planner::{compile, frontier_from_timing, InputContract};
use asap_logical_asap_optimization::DeploymentCapabilities;
use asap_planner::{
    e2e_plan, plan_installable, FrontendInput, InstallablePlan, InstallablePlanError,
    InstalledQueryRoot, PlanError, PlanOutput, PlanningModels, UserInput,
};
use asap_types::ir::physical_export::{compile_physical_asap_workload, PhysicalASAPDAG};
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
    let installed = installable(&workload).await;
    installed.validate().unwrap();
    let plan = &installed;
    let dag = &installed.dag.dag;
    assert_eq!(pane_chain_count(dag), 0);
    assert_eq!(materialized_count(dag), 6, "six ten-minute panes");
    let roots: Vec<u64> = dag.roots.iter().map(|&r| r as u64).collect();
    compile(dag, raw_range_inputs(dag), &roots).expect("the executor compiles it");
    assert_eq!(plan.queries.len(), 1);
    assert_eq!(
        plan.queries[0].query,
        "quantile_over_time(0.99, latency_ms[60m])"
    );
    assert_eq!(
        plan.queries[0].root,
        InstalledQueryRoot::Operator {
            node_id: dag.roots[0]
        }
    );
    // The stored outputs are the ingestion frontier: the six panes, each
    // ten minutes wide, ending 0, 10, ..., 50 minutes before evaluation.
    let stored: Vec<u64> = plan
        .stored_outputs
        .iter()
        .map(|o| o.node_id as u64)
        .collect();
    assert_eq!(stored, frontier_from_timing(dag).unwrap());
    let mut offsets: Vec<i64> = plan
        .stored_outputs
        .iter()
        .map(|o| {
            assert_eq!(o.pane.width_ms, 600_000);
            o.pane.end_offset_ms
        })
        .collect();
    offsets.sort();
    assert_eq!(offsets, (0..6).map(|i| i * 600_000).collect::<Vec<_>>());
    assert!(plan.query_time_raw_windows.is_empty(), "no raw data kept");
    let report = plan.selection.as_ref().expect("Stage 3 selected");
    assert!(report.cost_per_second.is_some());

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
    let reference = plan.query_time_reference().expect("re-times");
    reference.validate().unwrap();
    assert_eq!(materialized_count(&reference.dag.dag), 0);
    assert!(reference.stored_outputs.is_empty());
    assert_eq!(reference.query_time_raw_windows.len(), 6);
    assert!(reference
        .dag
        .dag
        .nodes
        .iter()
        .all(|n| n.output_state.timing == ExecutionTiming::QueryTime));
    assert_eq!(
        summary_builds(&reference.dag.dag),
        summary_builds(&plan.dag.dag)
    );
    assert_eq!(
        reference.queries[0].root,
        InstalledQueryRoot::Operator {
            node_id: reference.dag.dag.roots[0]
        }
    );
}

/// A repeating PromQL workload over ingesting `latency_ms`: `queries`, each
/// every 10 minutes at rank error 0.01, δ 0.01.
fn repeating(queries: &[&str]) -> PlanningWorkload {
    let entries: Vec<String> = queries
        .iter()
        .map(|q| {
            format!(
                r#"{{"query": {q:?}, "demand": {{"fixed_interval": 600000}},
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

/// p99 and p50 over the same 60 minutes share one chain of panes: the
/// installable plan stores six panes, not one set per query.
#[tokio::test]
async fn two_queries_on_one_chain_store_its_six_panes_once() {
    let plan = installable(&repeating(&[
        "quantile_over_time(0.99, latency_ms[60m])",
        "quantile_over_time(0.5, latency_ms[60m])",
    ]))
    .await;
    assert_eq!(plan.stored_outputs.len(), 6);
    assert_eq!(plan.queries.len(), 2);
}

/// Planned with raw data kept (everything at query time), so each query's
/// range is a query-time raw-sample window.
async fn installable_at_query_time(query: &str) -> Result<InstallablePlan, PlanError> {
    let workload = repeating(&[query]);
    plan_installable(UserInput::new(
        &workload,
        FrontendInput::Promql { histograms: None },
        PlanningModels::builtin(),
    ))
    .await
}

/// A raw-sample window whose position is not a fixed offset from the
/// evaluation time (a subquery or an offset above the range, an `@`
/// anchor below it) is refused with its reason, not passed on with a
/// wrong window or dropped; a plain range is listed.
#[tokio::test]
async fn raw_sample_windows_of_unsupported_shapes_are_refused() {
    for (query, reason) in [
        (
            "max_over_time(rate(latency_ms[5m])[1h:1m])",
            "a subquery or a TimeShift above the range",
        ),
        (
            "max_over_time(latency_ms[10m] @ 1000)",
            "an @ anchor below the range",
        ),
    ] {
        match installable_at_query_time(query).await {
            Err(PlanError::Installable(InstallablePlanError::UnsupportedRawSampleWindow {
                reason: refused,
                ..
            })) => assert_eq!(refused, reason, "{query}"),
            other => panic!("{query}: {:?}", other.map(|p| p.query_time_raw_windows)),
        }
    }
    let plain = installable_at_query_time("max_over_time(latency_ms[10m] offset 5m)")
        .await
        .unwrap();
    let [window] = plain.query_time_raw_windows.as_slice() else {
        panic!("{:?}", plain.query_time_raw_windows);
    };
    assert_eq!((window.width_ms, window.end_offset_ms), (600_000, 300_000));
}

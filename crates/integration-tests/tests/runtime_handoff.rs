//! What `plan_installable` returns for a deployment runtime (issue #749),
//! checked against the Planner's internals; the runtime's own end-to-end
//! run, with public APIs only, is `crates/planner/tests/runtime_install.rs`.

use std::collections::BTreeSet;

use asap_executor::physical_planner::compile;
use asap_logical_asap_optimization::DeploymentCapabilities;
use asap_planner::{
    e2e_plan, plan_installable, test_support::query_time_reference, FrontendInput,
    InstallableDagError, InstallablePlan, InstallablePlanError, PlanError, PlanOutput,
    PlanningModels, SelectedPlanReport, UserInput,
};
use asap_types::ir::physical_export::{
    compile_physical_asap_workload, PhysicalASAPDAG, PhysicalASAPNodeId,
};
use asap_types::ir::properties::ExecutionTiming;
use asap_types::ir::{ASAPOp, Operator};
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

/// A deployment that keeps no raw data samples: every summary must be built at
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

/// What a runtime that keeps no raw data samples gets from one facade call:
/// the plan and Stage 3's report of the selection.
async fn installable_with_report(
    workload: &PlanningWorkload,
) -> (InstallablePlan, Option<SelectedPlanReport>) {
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

async fn installable(workload: &PlanningWorkload) -> InstallablePlan {
    installable_with_report(workload).await.0
}

fn pane_chain_count(dag: &PhysicalASAPDAG) -> usize {
    dag.nodes
        .iter()
        .filter(|n| matches!(n.payload, Operator::ASAP(ASAPOp::PaneChain { .. })))
        .count()
}

/// The installable plan expands its selected pane width into contiguous
/// panes, each a materialized view refreshed at ingestion time, and
/// the executor compiles it. The stage export keeps the chain for costing
/// and display. Its one query is answered by the DAG's one root.
#[tokio::test]
async fn installable_plan_has_its_pane_chains_expanded() {
    let workload = example4b();
    let (installed, report) = installable_with_report(&workload).await;
    installed.validate().unwrap();
    let plan = &installed;
    let dag = &installed.dag;
    let views = plan.materialized_views().unwrap();
    assert_eq!(pane_chain_count(dag), 0);
    assert_eq!(materialized_count(dag), views.len());
    let roots: Vec<u64> = dag.roots.iter().map(|&r| r as u64).collect();
    let compiled = compile(dag, &views, &roots).expect("the executor compiles it");
    assert_eq!(plan.query_texts.len(), 1);
    assert_eq!(
        plan.query_texts[0],
        "quantile_over_time(0.99, latency_ms[60m])"
    );
    // The query reads exactly the materialized views, and no raw data.
    let refreshed: BTreeSet<u64> = views.iter().map(|v| v.node_id as u64).collect();
    let slots: BTreeSet<u64> = compiled.input_slots().map(|(id, _)| id).collect();
    assert_eq!(slots, refreshed);
    assert_pane_coverage(plan, dag.roots[0], 0, 3_600_000);
    assert!(
        plan.raw_data_retention().unwrap().is_empty(),
        "no raw data kept"
    );
    let report = report.as_ref().expect("Stage 3 selected");
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
/// with the same parameters, every node at query time and nothing materialized,
/// with the same queries bound to its roots.
#[tokio::test]
async fn reference_plan_runs_the_installable_plan_at_query_time() {
    let plan = installable(&example4b()).await;
    let reference = query_time_reference(&plan).expect("re-times");
    reference.validate().unwrap();
    assert_eq!(materialized_count(&reference.dag), 0);
    assert!(reference.materialized_views().unwrap().is_empty());
    // The reference reads the raw data the plan's views were refreshed from.
    let [retention] = reference.raw_data_retention().unwrap().try_into().unwrap();
    assert_eq!(retention.lookback_ms, Some(3_600_000));
    assert!(reference
        .dag
        .nodes
        .iter()
        .all(|n| n.output_state.timing == ExecutionTiming::QueryTime));
    assert_eq!(summary_builds(&reference.dag), summary_builds(&plan.dag));
    assert_eq!(reference.query_texts, plan.query_texts);
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
/// installable plan materializes one shared set of panes.
#[tokio::test]
async fn two_queries_on_one_chain_store_its_panes_once() {
    let plan = installable(&repeating(&[
        "quantile_over_time(0.99, latency_ms[60m])",
        "quantile_over_time(0.5, latency_ms[60m])",
    ]))
    .await;
    assert_eq!(plan.query_texts.len(), 2);
    let views = plan.materialized_views().unwrap();
    let views_below = |root| {
        let below = nodes_below(&plan.dag, root);
        views
            .iter()
            .filter(|view| below.contains(&view.node_id))
            .map(|view| view.node_id)
            .collect::<BTreeSet<_>>()
    };
    assert_eq!(
        views_below(plan.dag.roots[0]),
        views_below(plan.dag.roots[1])
    );
    assert_eq!(views_below(plan.dag.roots[0]).len(), views.len());
    assert_pane_coverage(&plan, plan.dag.roots[0], 0, 3_600_000);
}

/// Planned with raw data kept (everything at query time), so each query's
/// range is a query-time raw-data window.
async fn installable_at_query_time(query: &str) -> Result<InstallablePlan, PlanError> {
    let workload = repeating(&[query]);
    plan_installable(UserInput::new(
        &workload,
        FrontendInput::Promql { histograms: None },
        PlanningModels::builtin(),
    ))
    .await
    .map(|(plan, _)| plan)
}

/// A raw-data window whose position is not a fixed offset from the
/// evaluation time (a subquery or an offset above the range, an `@`
/// anchor below it), or that ends after the evaluation (a negative offset,
/// which no ingestion run can build in time), is refused with its reason,
/// not passed on with a wrong window or dropped; a plain range is listed.
#[tokio::test]
async fn raw_data_windows_of_unsupported_shapes_are_refused() {
    for (query, reason) in [
        (
            "max_over_time(rate(latency_ms[5m])[1h:1m])",
            "a subquery or a TimeShift above the range",
        ),
        (
            "max_over_time(latency_ms[10m] @ 1000)",
            "an @ anchor below the range",
        ),
        (
            "max_over_time(latency_ms[60m] offset -10m)",
            "a negative offset below the range",
        ),
    ] {
        match installable_at_query_time(query).await {
            Err(PlanError::Installable(InstallablePlanError::Uninstallable(
                InstallableDagError::UnsupportedRawDataWindow {
                    reason: refused, ..
                },
            ))) => assert_eq!(refused, reason, "{query}"),
            Err(error) => {
                // When planning fails before installation, its no-plan
                // diagnostic must keep the concrete refusal reason.
                let diagnostic = error.to_string();
                assert!(diagnostic.contains(reason), "{query}: {diagnostic}");
                assert!(diagnostic.contains("raw-data window"), "{diagnostic}");
            }
            Ok(plan) => panic!(
                "{query}: unexpectedly installed {:?}",
                plan.raw_data_retention().unwrap()
            ),
        }
    }
    let plain = installable_at_query_time("max_over_time(latency_ms[10m] offset 5m)")
        .await
        .unwrap();
    // 10 minutes of data ending 5 minutes back reaches 15 minutes back.
    let [retention] = plain.raw_data_retention().unwrap().try_into().unwrap();
    assert_eq!(retention.lookback_ms, Some(900_000));
}

/// Example 4b answered as the last hour at 10-minute steps (a range
/// evaluation): each selected pane view is kept for the hour its window reads
/// plus the hour the range spans, 7,200,000 ms, as Stage 3 records for the
/// chain (`Selection::pane_retention_ms`); without the range, for the hour.
#[tokio::test]
async fn stored_panes_carry_stage3_pane_retention() {
    let mut workload = example4b();
    let plain = installable(&workload).await;
    assert!(plain
        .materialized_views()
        .unwrap()
        .iter()
        .all(|v| v.retention_ms == 3_600_000));
    let mut json = serde_json::to_value(&workload).unwrap();
    json["query_workload"]["repeating_queries"][0]["range_evaluation"] =
        serde_json::json!({"step_ms": 600000, "span_ms": 3600000});
    workload = serde_json::from_value(json).unwrap();
    let ranged = installable(&workload).await;
    assert_pane_coverage(&ranged, ranged.dag.roots[0], 0, 3_600_000);
    assert!(ranged
        .materialized_views()
        .unwrap()
        .iter()
        .all(|v| v.retention_ms == 7_200_000));
    let selection = plan(&workload, &without_raw_data())
        .await
        .selection
        .unwrap();
    assert_eq!(
        selection
            .pane_retention_ms
            .values()
            .copied()
            .collect::<Vec<_>>(),
        [7_200_000]
    );
    // A plan file that keeps a pane for less than it reads is refused.
    let mut short = ranged.clone();
    let view = ranged.materialized_views().unwrap()[0].node_id;
    short
        .dag
        .nodes
        .iter_mut()
        .find(|n| n.id == view)
        .unwrap()
        .retention_ms = Some(0);
    assert!(matches!(
        short.validate(),
        Err(InstallablePlanError::Uninstallable(
            InstallableDagError::ViewRetentionTooShort { .. }
        ))
    ));
}

/// A chain over an offset input keeps its panes for its window plus the
/// offset: `[60m] offset 10m` reaches 70 minutes back (4,200,000 ms), the
/// unshifted query's chain 60 minutes (3,600,000 ms). Each materialized view
/// carries its own node's requirement; a runtime that keeps the twins'
/// identical panes once keeps them for the larger one.
#[tokio::test]
async fn an_offset_chain_keeps_its_panes_for_its_window_plus_the_offset() {
    let plan = installable(&repeating(&[
        "quantile_over_time(0.99, latency_ms[60m])",
        "quantile_over_time(0.99, latency_ms[60m] offset 10m)",
    ]))
    .await;
    assert_pane_coverage(&plan, plan.dag.roots[0], 0, 3_600_000);
    assert_pane_coverage(&plan, plan.dag.roots[1], 600_000, 4_200_000);
    let below: Vec<_> = plan
        .dag
        .roots
        .iter()
        .map(|&root| nodes_below(&plan.dag, root))
        .collect();
    for view in &plan.materialized_views().unwrap() {
        let required = [3_600_000, 4_200_000]
            .into_iter()
            .zip(&below)
            .filter(|(_, nodes)| nodes.contains(&view.node_id))
            .map(|(retention, _)| retention)
            .max()
            .expect("materialized pane is consumed");
        assert_eq!(view.retention_ms, required);
    }
    plan.validate().unwrap();
}

/// Any selected legal pane width must cover the requested interval exactly.
fn assert_pane_coverage(plan: &InstallablePlan, root: PhysicalASAPNodeId, start: i64, end: i64) {
    let below = nodes_below(&plan.dag, root);
    let mut intervals: Vec<_> = plan
        .materialized_views()
        .unwrap()
        .iter()
        .filter(|view| below.contains(&view.node_id))
        .map(|view| {
            let [window] = view.inputs.raw_data_windows.as_slice() else {
                panic!("pane reads one raw interval: {:?}", view.inputs);
            };
            assert!(window.width_ms > 0 && 600_000 % window.width_ms == 0);
            (window.end_offset_ms, window.end_offset_ms + window.width_ms)
        })
        .collect();
    intervals.sort_unstable();
    assert!(!intervals.is_empty());
    assert_eq!(intervals[0].0, start);
    assert_eq!(intervals.last().unwrap().1, end);
    for pair in intervals.windows(2) {
        assert_eq!(pair[0].1, pair[1].0);
    }
}

/// Every node `root` reads, directly or through other nodes.
fn nodes_below(dag: &PhysicalASAPDAG, root: PhysicalASAPNodeId) -> BTreeSet<PhysicalASAPNodeId> {
    let mut below = BTreeSet::new();
    let mut pending = vec![root];
    while let Some(node) = pending.pop() {
        for edge in dag.edges.iter().filter(|edge| edge.consumer == node) {
            if below.insert(edge.producer) {
                pending.push(edge.producer);
            }
        }
    }
    below
}

/// A standalone scalar query (`scalar(sum(latency_ms))`) has no DAG node
/// to answer it through the public API, so it is refused with its entry.
#[tokio::test]
async fn a_scalar_query_is_refused() {
    match installable_at_query_time("scalar(sum(latency_ms))").await {
        Err(PlanError::Installable(InstallablePlanError::UnsupportedQueryRoot {
            entry_index: 0,
        })) => {}
        other => panic!("{:?}", other.map(|p| p.query_texts)),
    }
}

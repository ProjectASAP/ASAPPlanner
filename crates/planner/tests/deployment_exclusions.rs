//! The deployment's exclusions reach Stage 2 through the facade: a caller
//! of `plan_installable` or `e2e_plan` passes them in
//! `PlanningModels::deployment_capabilities`, beside the executor's
//! summaries the facade fills in.

use asap_planner::{
    e2e_plan, plan_installable, CandidateExclusion, DeploymentCapabilities, FrontendInput,
    PhysicalASAPDAG, PlanningModels, PlanningWorkload, SketchParameterRange, SketchParams,
    UserInput,
};
use asap_types::ir::{ASAPOp, Operator};

/// `quantile_over_time(0.99, m[1h])` every 60 s over one series at 10 Hz,
/// with a per-value Rank target of 0.005 at δ = 0.01: planned with KLL at
/// k = 547 by default.
fn kll_workload() -> PlanningWorkload {
    serde_json::from_str(
        r#"{"query_workload": {"language": "promql", "repeating_queries": [
              {"query": "quantile_over_time(0.99, m[1h])", "demand": {"fixed_interval_at": {"interval": 60000, "evaluation_phase": 0}},
               "requirements": {"accuracy": {"accuracy_metric": "rank", "max_error": 0.005, "failure_probability": 0.01, "accuracy_scope": "per_value"}},
               "predictability": {"predictable": {"known_at": null}}}]},
            "data_workload": {"arrival": {"ingesting": {"rows_per_second": 10.0}},
              "sources": {"m": {"series_count": 1, "metric_type": "gauge", "sample_interval": 100}}}}"#,
    )
    .unwrap()
}

/// The θ of each `SummaryAgg` of `dag`.
fn summary_agg_parameters(dag: &PhysicalASAPDAG) -> Vec<Option<SketchParams>> {
    dag.nodes
        .iter()
        .filter(|node| matches!(node.payload, Operator::ASAP(ASAPOp::SummaryAgg { .. })))
        .map(|node| node.parameters.clone())
        .collect()
}

/// Excluding KLL at k ≤ 1,403 moves the installable plan's KLL from
/// k = 547 to k = 1,404, and `e2e_plan` with the same models selects the
/// same; without the exclusion it stays at k = 547.
#[tokio::test]
async fn exclusions_reach_stage2_through_plan_installable() {
    let workload = kll_workload();
    let capabilities = DeploymentCapabilities {
        excluded_candidates: vec![CandidateExclusion {
            sketch_parameters: Some(SketchParameterRange::KllK {
                at_least: None,
                at_most: Some(1403),
            }),
            ..Default::default()
        }],
        ..Default::default()
    };
    let restricted = PlanningModels::builtin().with_deployment_capabilities(&capabilities);
    let (installable, _) =
        plan_installable(UserInput::new(&workload, FrontendInput::Promql, restricted))
            .await
            .expect("plans");
    assert_eq!(
        summary_agg_parameters(&installable.dag),
        vec![Some(SketchParams::Kll { k: 1404 })]
    );
    let output = e2e_plan(UserInput::new(&workload, FrontendInput::Promql, restricted))
        .await
        .expect("plans");
    assert_eq!(
        summary_agg_parameters(&output.plan),
        vec![Some(SketchParams::Kll { k: 1404 })]
    );

    let (unrestricted, _) = plan_installable(UserInput::new(
        &workload,
        FrontendInput::Promql,
        PlanningModels::builtin(),
    ))
    .await
    .expect("plans");
    assert_eq!(
        summary_agg_parameters(&unrestricted.dag),
        vec![Some(SketchParams::Kll { k: 547 })]
    );
}

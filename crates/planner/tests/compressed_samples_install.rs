//! I1 of `compressed-sample-windows.md`, section 7: a plan with a
//! compressed-samples chain and its decode, in both read modes, is a plan
//! file a deployment can install. Written before the implementation; it
//! names the planned interfaces and fails until they exist.

use asap_planner::{
    plan_installable, DeploymentCapabilities, FrontendInput, InstallablePlan, PlanningModels,
    PlanningWorkload, UserInput,
};
use asap_types::ir::schema::FieldDataType;
use asap_types::ir::{ASAPOp, Operator};

/// `quantile_over_time(0.99, x[1h])` exactly (no accuracy given) every
/// 10 s over 100 series of the gauge `x`, one sample per second.
fn exact_quantile_workload() -> PlanningWorkload {
    serde_json::from_str(
        r#"{"query_workload": {"language": "promql", "repeating_queries": [{
              "query": "quantile_over_time(0.99, x[1h])",
              "demand": {"fixed_interval_at": {"interval": 10000, "evaluation_phase": 0}},
              "requirements": {},
              "predictability": {"predictable": {"known_at": null}}}]},
            "data_workload": {"arrival": {"ingesting": {"rows_per_second": 100.0}},
              "sources": {"x": {"series_count": 100, "metric_type": "gauge", "sample_interval": 1000}}}}"#,
    )
    .unwrap()
}

/// The plan for the exact quantile with no raw data kept, the runtime
/// reading unsealed windows or not.
async fn planned(unsealed_window_reads: bool) -> InstallablePlan {
    let capabilities = DeploymentCapabilities {
        raw_data_retained: false,
        unsealed_window_reads,
        ..DeploymentCapabilities::default()
    };
    plan_installable(UserInput::new(
        &exact_quantile_workload(),
        FrontendInput::Promql,
        PlanningModels::builtin().with_deployment_capabilities(&capabilities),
    ))
    .await
    .unwrap()
    .0
}

/// I1. In both read modes the plan (with no raw data, an exact quantile
/// has only the decode) holds a compressed-samples chain and a decode in
/// that mode; its plan file reads back as written and the Planner's
/// executor compiles it. A wrong implementation fails it by a serde form
/// that does not round-trip (a missing `reads_unsealed_window`, a renamed
/// variant), or by an executor lowering that refuses either operator or
/// either mode.
#[tokio::test]
async fn installable_plan_round_trips_compressed_samples() {
    for unsealed_window_reads in [true, false] {
        let plan = planned(unsealed_window_reads).await;
        assert!(plan.dag.nodes.iter().any(|node| matches!(
            &node.payload,
            Operator::ASAP(ASAPOp::SummaryTumblingWindowChain {
                summary_type: FieldDataType::CompressedSamples,
                ..
            })
        )));
        let modes: Vec<bool> = plan
            .dag
            .nodes
            .iter()
            .filter_map(|node| match &node.payload {
                Operator::ASAP(ASAPOp::DecodeSamplesInRange {
                    reads_unsealed_window,
                    ..
                }) => Some(*reads_unsealed_window),
                _ => None,
            })
            .collect();
        assert_eq!(modes, [unsealed_window_reads]);
        let text = serde_json::to_string(&plan).unwrap();
        let read = InstallablePlan::from_json(&text).unwrap();
        assert_eq!(serde_json::to_string(&read).unwrap(), text);
        assert!(text.contains("DecodeSamplesInRange") && text.contains("CompressedSamples"));
        read.check_compiles().unwrap();
    }
}

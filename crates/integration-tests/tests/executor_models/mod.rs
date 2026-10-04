//! The deployment input these tests plan with: the built-in models and the
//! reference executor's capabilities, as a deployment running the plans on
//! that executor would pass them (C2).
use std::sync::LazyLock;

use asap_plan_selection::{DeploymentCapabilities, PlanningModels};

static EXECUTOR: LazyLock<DeploymentCapabilities> = LazyLock::new(asap_executor::capabilities);

pub fn executor_models() -> PlanningModels<'static> {
    PlanningModels::builtin().with_capabilities(&EXECUTOR)
}

/// The logical DAG the #509 stage pipeline selects for the single query
/// `root` at `accuracy`, run once over a continuously ingested source, with
/// [`executor_models`].
#[allow(dead_code)]
pub fn selected_dag(
    root: std::rc::Rc<asap_types::ir::OperatorNode>,
    accuracy: asap_types::types::AccuracyTarget,
) -> std::rc::Rc<asap_types::ir::OperatorNode> {
    use asap_types::ir::QueryRoot;
    use asap_types::workload::{
        DataArrival, DataWorkload, Evidence, EvidenceSource, Predictability, QueryRecurrence, Rate,
        RootDemand,
    };
    let demand = [RootDemand {
        accuracy: Some(accuracy),
        recurrence: QueryRecurrence::OneTime {
            invocations: 1,
            execute_at: None,
        },
        predictability: Predictability::default(),
        latency_ms: None,
    }];
    let data = DataWorkload {
        arrival: DataArrival::ContinuouslyIngesting,
        ingestion_rate: Evidence {
            value: Some(Rate(1_000.0)),
            source: EvidenceSource::Declared,
            ..Default::default()
        },
        ..Default::default()
    };
    let run = asap_plan_selection::plan_stages(
        vec![(0, QueryRoot::Operator(root))],
        &demand,
        &data,
        executor_models(),
        0,
    )
    .unwrap();
    let QueryRoot::Operator(root) = &run.plan.logical[0].1 else {
        panic!("operator root")
    };
    root.clone()
}

#![allow(dead_code)]
use planner_types::ir::export::PhysicalASAPDAG;
use planner_types::ir::{
    apply_materialization_timings, MaterializationAssignment, OperatorNode, TimingMemo,
};
use std::rc::Rc;

pub fn compile_physical_asap_dag(
    root: &Rc<OperatorNode>,
) -> Result<PhysicalASAPDAG, Box<dyn std::error::Error>> {
    let root = apply_materialization_timings(
        root,
        &MaterializationAssignment::default(),
        &mut TimingMemo::default(),
    )?;
    Ok(planner_types::ir::export::compile_physical_asap_dag(&root)?)
}

/// The logical DAG the #509 stage pipeline selects for the single query
/// `root` at `accuracy`, run once over a continuously ingested source, with
/// the built-in models and this executor's capabilities.
pub fn selected_dag(
    root: Rc<OperatorNode>,
    accuracy: planner_types::types::AccuracyTarget,
) -> Rc<OperatorNode> {
    use planner_types::ir::QueryRoot;
    use planner_types::workload::{
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
    let capabilities = asap_executor::capabilities();
    let models = asap_plan_selection::PlanningModels::builtin().with_capabilities(&capabilities);
    let run = asap_plan_selection::plan_stages(
        vec![(0, QueryRoot::Operator(root))],
        &demand,
        &data,
        models,
        0,
    )
    .unwrap();
    let QueryRoot::Operator(root) = &run.plan.logical[0].1 else {
        panic!("operator root")
    };
    root.clone()
}

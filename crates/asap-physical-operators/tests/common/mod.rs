#![allow(dead_code)]
use planner_types::ir::physical_export::PhysicalASAPDAG;
use planner_types::ir::{
    apply_materialization_timings, MaterializationAssignment, OperatorNode, QueryRoot, TimingMemo,
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
    Ok(planner_types::ir::physical_export::compile_physical_asap_dag(&root)?)
}

/// The logical DAG the planner pipeline (`plan_stages`) selects for the
/// single query `root` at `accuracy`, over a continuously ingested source,
/// with the built-in models.
pub fn selected_dag(
    root: Rc<OperatorNode>,
    accuracy: planner_types::types::AccuracyTarget,
) -> Rc<OperatorNode> {
    let run = plan_stages(root, accuracy);
    let [QueryRoot::Operator(selected)] = run.selected_logical().expect("a plan is selected")
    else {
        panic!("one operator root");
    };
    Rc::clone(selected)
}

/// Every logical candidate (a composed CandidateLogicalASAPDAG) of the single
/// query `root` at `accuracy`, in enumeration order; a choice that does not
/// compose is skipped.
pub fn logical_candidates(
    root: Rc<OperatorNode>,
    accuracy: planner_types::types::AccuracyTarget,
) -> Vec<Rc<OperatorNode>> {
    plan_stages(root, accuracy)
        .logical
        .into_iter()
        .filter_map(|candidate| match candidate.roots.ok()?.remove(0) {
            QueryRoot::Operator(node) => Some(node),
            QueryRoot::Scalar(_) => panic!("operator root"),
        })
        .collect()
}

fn plan_stages(
    root: Rc<OperatorNode>,
    accuracy: planner_types::types::AccuracyTarget,
) -> asap_aware_mapping::StagePipelineRun<usize> {
    use planner_types::workload::{DataArrival, DataWorkload, Evidence, EvidenceSource, Rate};
    let data = DataWorkload {
        arrival: DataArrival::ContinuouslyIngesting,
        ingestion_rate: Evidence {
            value: Some(Rate(1_000.0)),
            source: EvidenceSource::Declared,
            ..Default::default()
        },
        ..Default::default()
    };
    asap_aware_mapping::plan_stages(
        vec![(0, QueryRoot::Operator(root))],
        &[Some(accuracy)],
        &data,
        asap_aware_mapping::PlanningModels::builtin(),
        asap_aware_mapping::pass::MAX_ENUMERATED_CANDIDATES,
    )
    .expect("Pass 1 accepts the query")
}

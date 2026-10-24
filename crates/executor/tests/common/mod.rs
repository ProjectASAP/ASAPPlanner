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

/// The logical DAG the planner pipeline (`run_stage1_to_stage3`) selects for the
/// single query `root` at `accuracy`, over a continuously ingested source,
/// with the built-in models.
pub fn selected_dag(
    root: Rc<OperatorNode>,
    accuracy: planner_types::ir::properties::AccuracyTarget,
) -> Rc<OperatorNode> {
    let run = run_stage1_to_stage3(root, accuracy);
    let [QueryRoot::Operator(selected)] = run.selected_logical().expect("a plan is selected")
    else {
        panic!("one operator root");
    };
    Rc::clone(selected)
}

/// Every logical candidate (a composed CandidateLogicalASAPDAG) of the single
/// query `root` at `accuracy`, in enumeration order; a candidate that does not
/// compose panics, so none is silently skipped.
pub fn logical_candidates(
    root: Rc<OperatorNode>,
    accuracy: planner_types::ir::properties::AccuracyTarget,
) -> Vec<Rc<OperatorNode>> {
    require_composed_operator_candidates(
        run_stage1_to_stage3(root, accuracy).stage1_logical_asap_optimization,
    )
}

/// Fail on rejected composition rather than silently skipping a candidate.
pub fn require_composed_operator_candidates(
    candidates: Vec<asap_plan_selection::search::LogicalCandidate>,
) -> Vec<Rc<OperatorNode>> {
    candidates
        .into_iter()
        .map(|candidate| {
            match candidate
                .roots
                .unwrap_or_else(|e| panic!("{} does not compose: {e}", candidate.id))
                .remove(0)
            {
                QueryRoot::Operator(node) => node,
                QueryRoot::Scalar(_) => panic!("operator root"),
            }
        })
        .collect()
}

fn run_stage1_to_stage3(
    root: Rc<OperatorNode>,
    accuracy: planner_types::ir::properties::AccuracyTarget,
) -> asap_plan_selection::Stage1ToStage3Run<usize> {
    use planner_types::workload::{DataArrival, DataWorkload};
    let data = DataWorkload {
        arrival: DataArrival::Ingesting {
            rows_per_second: 1_000.0,
        },
        ..Default::default()
    };
    asap_plan_selection::run_stage1_to_stage3_with(
        vec![(0, QueryRoot::Operator(root))],
        &[planner_types::workload::QueryWorkloadEntry {
            query: Default::default(),
            requirements: planner_types::workload::QueryRequirements {
                accuracy,
                max_latency_ms: None,
            },
            predictability: Default::default(),
            recurrence: Default::default(),
            range_evaluation: None,
        }],
        &data,
        asap_plan_selection::PlanningModels::builtin(),
        asap_plan_selection::Stage1ToStage3Search::exhaustive(),
    )
    .expect("Pass 1 accepts the query")
}

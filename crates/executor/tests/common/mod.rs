#![allow(dead_code)]
use planner_types::ir::physical_export::PhysicalASAPDAG;
use planner_types::ir::{
    apply_materialization_timings, MaterializationAssignment, NodeToTimedNodeCache, OperatorNode,
    QueryRoot,
};
use std::rc::Rc;

pub fn compile_physical_asap_dag(
    root: &Rc<OperatorNode>,
) -> Result<PhysicalASAPDAG, Box<dyn std::error::Error>> {
    let root = apply_materialization_timings(
        root,
        &MaterializationAssignment::default(),
        &mut NodeToTimedNodeCache::default(),
    )?;
    Ok(planner_types::ir::physical_export::compile_physical_asap_dag(&root)?)
}

/// The logical DAG the planner pipeline (`asap_planner::run_planning`) selects for the
/// single query `root` at `accuracy`, over a continuously ingested source,
/// with the built-in models.
pub fn selected_dag(
    root: Rc<OperatorNode>,
    accuracy: planner_types::ir::properties::AccuracyTarget,
) -> Rc<OperatorNode> {
    let run = run_stage1_to_stage3(root, accuracy);
    let [QueryRoot::Operator(selected)] = run
        .selected_plan
        .as_deref()
        .unwrap_or_else(|| panic!("a plan is selected: {:?}", run.selection))
    else {
        panic!("one operator root");
    };
    Rc::clone(selected)
}

/// Exercise each local implementation rule directly, keeping the other
/// computations at their default choice. A bounded solver archive need not
/// contain every algorithm that executor binding tests need to exercise.
pub fn computation_candidates(root: Rc<OperatorNode>) -> Vec<Rc<OperatorNode>> {
    use asap_logical_asap_optimization::test_support::{
        compose_logical_candidate, generate_per_computation_candidates,
    };
    let inventory = generate_per_computation_candidates(
        &asap_logical_asap_optimization::DagOfLogicalMemoGroups::from_roots(&[
            QueryRoot::Operator(root),
        ]),
    )
    .expect("Pass 1 accepts the query");
    let mut choices = vec![vec![0; inventory.targets.len()]];
    for (index, target) in inventory.targets.iter().enumerate() {
        for choice in 1..target.choice_count() {
            let mut selected = vec![0; inventory.targets.len()];
            selected[index] = choice;
            choices.push(selected);
        }
    }
    choices
        .into_iter()
        .map(|choice| {
            let mut roots = compose_logical_candidate(&inventory, &choice)
                .expect("a generated computation candidate composes");
            match roots.remove(0) {
                QueryRoot::Operator(root) => root,
                QueryRoot::Scalar(_) => panic!("one operator root"),
            }
        })
        .collect()
}

fn run_stage1_to_stage3(
    root: Rc<OperatorNode>,
    accuracy: planner_types::ir::properties::AccuracyTarget,
) -> asap_planner::PlanningRun {
    use planner_types::workload::{DataArrival, DataWorkload};
    let data = DataWorkload {
        arrival: DataArrival::Ingesting {
            rows_per_second: 1_000.0,
        },
        ..Default::default()
    };
    asap_planner::run_planning(
        vec![QueryRoot::Operator(root)],
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
        asap_planner::PlanningModels::builtin(),
        asap_planner::PlanSearchSettings::default(),
    )
    .expect("Pass 1 accepts the query")
}

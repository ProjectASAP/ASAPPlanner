//! Planner-facade helpers for this crate's tests. Kept out of `support.rs`,
//! which the private corpus also compiles without depending on asap-planner.
use std::rc::Rc;

use asap_types::ir::properties::AccuracyTarget;
use asap_types::ir::OperatorNode;

use crate::support::declared_data;

/// Every planning stage's output for the single query `root` at `accuracy`,
/// with the built-in models.
pub fn run_stage1_to_stage3(
    root: Rc<OperatorNode>,
    accuracy: AccuracyTarget,
) -> asap_planner::PlanningRun {
    let data = declared_data(&root);
    asap_planner::run_planning(
        vec![asap_types::ir::QueryRoot::Operator(root)],
        &[asap_types::workload::QueryWorkloadEntry {
            query: Default::default(),
            requirements: asap_types::workload::QueryRequirements {
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

/// The logical DAG the planner pipeline selects for the single query `root`
/// at `accuracy`.
pub fn selected_dag(root: Rc<OperatorNode>, accuracy: AccuracyTarget) -> Rc<OperatorNode> {
    let run = run_stage1_to_stage3(root, accuracy);
    match run.selected_plan.as_deref().expect("a plan is selected") {
        [asap_types::ir::QueryRoot::Operator(node)] => Rc::clone(node),
        other => panic!("one operator root: {other:?}"),
    }
}

#![allow(dead_code)]

use planner_types::ir::{OperatorNode, QueryRoot};
use std::rc::Rc;

/// The logical DAG the planner pipeline (`asap_planner::run_planning`) selects for the
/// single query `root` at `accuracy`, over a continuously ingested source,
/// with the built-in models.
pub fn selected_dag(
    root: Rc<OperatorNode>,
    accuracy: planner_types::accuracy::AccuracyTarget,
) -> Rc<OperatorNode> {
    let run = run_all_stages(root, accuracy);
    let roots = match &run.selection {
        Ok((plan, _)) => plan.query_roots().unwrap(),
        Err(error) => panic!("a plan is selected: {error}"),
    };
    let [QueryRoot::Operator(selected)] = roots.as_slice() else {
        panic!("one operator root");
    };
    Rc::clone(selected)
}

fn run_all_stages(
    root: Rc<OperatorNode>,
    accuracy: planner_types::accuracy::AccuracyTarget,
) -> asap_planner::PlanningRun {
    use planner_types::workload::{DataArrival, DataWorkload};
    let data = DataWorkload {
        arrival: DataArrival::Ingesting {
            rows_per_second: 1_000.0,
            max_out_of_order_ms: None,
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

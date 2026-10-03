//! Complete selection considers locally expensive choices and executes its winner.
mod physical_common;

use asap_aware_mapping::{
    cost_model::{Cost, CostModel, DefaultCostModel},
    pass::{optimize, CompletePass, LifecycleInput, OptimizationInput, PlanningModels},
    CostRate, SummaryMaintenanceLifecycleCostInputs, SummaryMaintenanceLifecyclePlan,
};
use asap_physical_operators::values::Value;
use asap_types::{
    ir::{
        operator_properties::{Reduction, Source},
        NonASAPOp, Operator, OperatorNode,
    },
    parsed_workload::ParsedWorkload,
    pre_asap::{AggIntent, DataType, Field, Schema},
    workload::{
        BatchEntry, PlanningWorkload, Predictability, Query, QueryLanguage, QueryWorkload,
        SqlDialect,
    },
};
use std::rc::Rc;

struct InteractingCosts;
impl CostModel for InteractingCosts {
    fn rank_candidates(
        &self,
        intent: &AggIntent,
        candidates: &[asap_types::post_asap::SketchAlgorithm],
    ) -> Vec<asap_types::post_asap::SketchAlgorithm> {
        DefaultCostModel.rank_candidates(intent, candidates)
    }
    fn raw_query_recompute_cost(&self, _: &OperatorNode) -> Option<Cost> {
        Some(Cost(1.0))
    }
    fn summary_maintenance_lifecycle_cost_inputs(
        &self,
        _: &OperatorNode,
    ) -> SummaryMaintenanceLifecycleCostInputs {
        SummaryMaintenanceLifecycleCostInputs {
            build_cost: Some(Cost(10.0)),
            maintenance_cost_per_update: Some(Cost::ZERO),
            summary_read_cost: Some(Cost::ZERO),
            retention_cost_rate: Some(CostRate(0.0)),
            retirement_cost: Some(Cost::ZERO),
        }
    }
    fn complete_workload_candidate_cost(
        &self,
        plans: &[SummaryMaintenanceLifecyclePlan],
        _: &[(usize, asap_types::ir::ScalarExpr)],
    ) -> Option<Cost> {
        // A deployment's complete implementation has a discount only when
        // both summary consumers use it. Per-site ranking cannot see this.
        Some(Cost(
            if plans.iter().all(|plan| !plan.selected_raw_recompute) {
                0.25
            } else {
                2.0
            },
        ))
    }
}

fn fixture() -> ParsedWorkload {
    let scan = OperatorNode::new_shared(Operator::NonASAP(NonASAPOp::Scan {
        source: Source::Table {
            table_ref: "events".into(),
        },
        predicates: vec![],
        schema: Schema::new(vec![Field::plain("value", DataType::Float64, false)]),
    }))
    .unwrap();
    let roots = [
        AggIntent::Sum { col: Some(0) },
        AggIntent::Max { col: Some(0) },
    ]
    .into_iter()
    .map(|intent| {
        OperatorNode::new_shared(Operator::NonASAP(NonASAPOp::Aggregate {
            reduction: Reduction::by(vec![]),
            measures: vec![intent],
            output_names: vec![],
            filters: vec![],
            having: None,
            child: Rc::clone(&scan),
        }))
        .unwrap()
    })
    .collect();
    let workload = PlanningWorkload {
        query_workload: QueryWorkload {
            language: QueryLanguage::SQL(SqlDialect::DataFusionSQL),
            repeating_queries: None,
            query_batch: Some(
                [
                    "SELECT SUM(value) FROM events",
                    "SELECT MAX(value) FROM events",
                ]
                .into_iter()
                .map(|query| BatchEntry {
                    query: Query(query.into()),
                    requirements: Default::default(),
                    predictability: Predictability::AdHoc,
                    invocations: 1,
                    execute_at: None,
                    time_selection: Default::default(),
                })
                .collect(),
            ),
        },
        data_workload: None,
    };
    ParsedWorkload::new(workload, roots).unwrap()
}

/// Both summaries are locally more expensive than raw; their complete implementation wins.
#[test]
fn globally_cheapest_assignment_executes_both_queries() {
    let workload = fixture();
    let input = OptimizationInput::new(
        &workload,
        PlanningModels::builtin().with_cost(&InteractingCosts),
        LifecycleInput::new(0),
    );
    let pass = CompletePass {
        max_candidates: 4096,
    };
    let inventory = pass.enumerate(input).unwrap();
    assert!(inventory.candidates.iter().any(|candidate| candidate
        .plans
        .iter()
        .all(|plan| plan.plan.selected_raw_recompute)));
    let output = optimize(&pass, input).unwrap();
    assert_eq!(output.workload_total_cost, Some(Cost(0.25)));
    assert!(output
        .plans
        .iter()
        .all(|plan| !plan.plan.selected_raw_recompute));
    for (plan, expected) in output.plans.iter().zip([3.0, 2.0]) {
        let result = physical_common::execute_raw_rows(
            &plan.plan.root,
            vec![vec![Value::Float64(1.0)], vec![Value::Float64(2.0)]],
        );
        assert_eq!(result.len(), 1);
        assert!(matches!(result[0].as_slice(), [Value::Float64(value)] if *value == expected));
    }
}

/// Exhaustion and unknown complete costs never yield a heuristic or partially searched winner.
#[test]
fn incomplete_search_is_an_explicit_error() {
    let workload = fixture();
    let input = OptimizationInput::new(
        &workload,
        PlanningModels::builtin().with_cost(&InteractingCosts),
        LifecycleInput::new(0),
    );
    assert!(CompletePass { max_candidates: 1 }
        .enumerate(input)
        .unwrap_err()
        .to_string()
        .contains("budget"));
    let unknown =
        OptimizationInput::new(&workload, PlanningModels::builtin(), LifecycleInput::new(0));
    assert!(optimize(&CompletePass::default(), unknown)
        .unwrap_err()
        .to_string()
        .contains("no feasible priced"));
}

struct PartialSharingCosts;
impl CostModel for PartialSharingCosts {
    fn rank_candidates(
        &self,
        intent: &AggIntent,
        candidates: &[asap_types::post_asap::SketchAlgorithm],
    ) -> Vec<asap_types::post_asap::SketchAlgorithm> {
        DefaultCostModel.rank_candidates(intent, candidates)
    }
    fn raw_query_recompute_cost(&self, _: &OperatorNode) -> Option<Cost> {
        Some(Cost(1000.0))
    }
    fn summary_maintenance_lifecycle_cost_inputs(
        &self,
        node: &OperatorNode,
    ) -> SummaryMaintenanceLifecycleCostInputs {
        InteractingCosts.summary_maintenance_lifecycle_cost_inputs(node)
    }
    fn complete_workload_candidate_cost(
        &self,
        plans: &[SummaryMaintenanceLifecyclePlan],
        _: &[(usize, asap_types::ir::ScalarExpr)],
    ) -> Option<Cost> {
        if plans.iter().any(|plan| plan.deployments.is_empty()) {
            return Some(Cost(1000.0));
        }
        let states: Vec<_> = plans
            .iter()
            .map(|plan| &plan.deployments[0].summary)
            .collect();
        Some(Cost(
            if Rc::ptr_eq(states[0], states[1]) && !Rc::ptr_eq(states[0], states[2]) {
                0.1
            } else {
                10.0
            },
        ))
    }
}

/// A partial sharing partition can beat both all-shared and all-independent choices.
#[test]
fn three_consumers_can_share_only_a_subset() {
    let two = fixture();
    let root = Rc::clone(two.entries().next().unwrap().1);
    let mut workload = two.planning_workload().clone();
    let entry = workload.query_workload.query_batch.as_ref().unwrap()[0].clone();
    workload.query_workload.query_batch = Some(vec![entry.clone(), entry.clone(), entry]);
    let workload = ParsedWorkload::new(workload, vec![root.clone(), root.clone(), root]).unwrap();
    let input = OptimizationInput::new(
        &workload,
        PlanningModels::builtin().with_cost(&PartialSharingCosts),
        LifecycleInput::new(0),
    );
    let output = optimize(
        &CompletePass {
            max_candidates: 65_536,
        },
        input,
    )
    .unwrap();
    assert_eq!(output.workload_total_cost, Some(Cost(0.1)));
    let states: Vec<_> = output
        .plans
        .iter()
        .map(|plan| &plan.plan.deployments[0].summary)
        .collect();
    assert!(Rc::ptr_eq(states[0], states[1]));
    assert!(!Rc::ptr_eq(states[0], states[2]));
}

struct AdditiveCosts;
impl CostModel for AdditiveCosts {
    fn rank_candidates(
        &self,
        intent: &AggIntent,
        candidates: &[asap_types::post_asap::SketchAlgorithm],
    ) -> Vec<asap_types::post_asap::SketchAlgorithm> {
        DefaultCostModel.rank_candidates(intent, candidates)
    }
    fn raw_query_recompute_cost(&self, _: &OperatorNode) -> Option<Cost> {
        Some(Cost(1000.0))
    }
    fn summary_maintenance_lifecycle_cost_inputs(
        &self,
        node: &OperatorNode,
    ) -> SummaryMaintenanceLifecycleCostInputs {
        InteractingCosts.summary_maintenance_lifecycle_cost_inputs(node)
    }
}

/// Union demand permits retention, and the additive workload model charges the shared producer once.
#[test]
fn shared_producer_cost_is_not_multiplied_by_consumer_count() {
    let two = fixture();
    let root = Rc::clone(two.entries().next().unwrap().1);
    let mut workload = two.planning_workload().clone();
    workload.data_workload = Some(asap_types::workload::DataWorkload {
        arrival: asap_types::workload::DataArrival::AtRest,
        ..Default::default()
    });
    let entry = workload.query_workload.query_batch.as_ref().unwrap()[0].clone();
    workload.query_workload.query_batch = Some(vec![entry.clone(), entry]);
    let workload = ParsedWorkload::new(workload, vec![root.clone(), root]).unwrap();
    let input = OptimizationInput::new(
        &workload,
        PlanningModels::builtin().with_cost(&AdditiveCosts),
        LifecycleInput::new(0).with_horizon(asap_aware_mapping::Horizon(100.0)),
    );
    let output = optimize(
        &CompletePass {
            max_candidates: 4096,
        },
        input,
    )
    .unwrap();
    assert_eq!(output.workload_total_cost, Some(Cost(10.0)));
    let left = &output.plans[0].plan.deployments[0];
    let right = &output.plans[1].plan.deployments[0];
    assert!(Rc::ptr_eq(&left.summary, &right.summary));
    assert_eq!(
        left.summary_maintenance_lifecycle_guarantee,
        right.summary_maintenance_lifecycle_guarantee
    );
    assert!(matches!(
        left.summary_maintenance_lifecycle_guarantee
            .as_ref()
            .unwrap()
            .summary_maintenance_lifecycle,
        asap_types::post_asap::SummaryMaintenanceLifecycle::Shared { .. }
    ));
}

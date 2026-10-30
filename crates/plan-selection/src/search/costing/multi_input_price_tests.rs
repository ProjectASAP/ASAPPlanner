//! End-to-end pricing uses the evaluator's binary operators and shared DAGs.
use super::*;
use asap_logical_asap_optimization::test_support::lower_promql;
use asap_types::ir::{properties::AccuracyTarget, OperatorNode};

use crate::search::candidates::dag::*;
use asap_types::workload::{
    DataWorkload, Predictability, Query, QueryRequirements, QueryWorkloadEntry, RepetitionInterval,
};
use std::rc::Rc;

fn append(
    node: &Rc<OperatorNode>,
    inventory: &mut PhysicalCandidatesWithCosts,
) -> PhysicalCandidateId {
    // Exact fixture expressions can be interned safely; repeated operands must
    // share one producer while keeping their two ordered input positions.
    if let Some(id) = (0..inventory.physical.candidates.len())
        .position(|known| inventory.node(known).as_ref() == node.as_ref())
    {
        return PhysicalCandidateId(id);
    }
    let mut inputs = Vec::new();
    node.operator
        .map_children(|child| inputs.push(append(child, inventory)));
    inventory.push_for_tests(
        node.clone(),
        ExecutionTiming::QueryTime,
        Materialization::NotMaterialized,
        0.0,
        inputs
            .into_iter()
            .map(|producer| InputProducerCandidates {
                candidates: vec![producer].into(),
            })
            .collect(),
    )
}

#[test]
fn binary_prices_match_evaluator_with_distinct_and_shared_inputs() {
    for query in ["m + n", "m + m", "m or n"] {
        let logical = lower_promql(query, AccuracyTarget::exact());
        let mut configured = PhysicalCandidatesWithCosts::empty_for_tests();
        let root = append(&logical, &mut configured);
        configured.dag.query_roots.push(QueryOutputCandidates {
            candidates: vec![root],
        });
        configured.sync_physical_for_tests();
        assert!(configured
            .dag
            .candidates
            .iter()
            .any(|candidate| candidate.producer_candidates_per_input.len() == 2));
        let roots = vec![QueryRoot::Operator(logical)];
        let entries = vec![QueryWorkloadEntry {
            query: Query(query.into()),
            requirements: QueryRequirements {
                accuracy: AccuracyTarget::exact(),
                max_latency_ms: None,
            },
            predictability: Predictability::Predictable { known_at: None },
            recurrence: QueryRecurrence::Repeated(RepeatedDemand::FixedInterval(
                RepetitionInterval(60_000),
            )),
            range_evaluation: None,
        }];
        let data: DataWorkload = serde_json::from_value(serde_json::json!({
            "arrival":{"ingesting":{"rows_per_second":100.0}},
            "sources":{"m":{"rows_held":10,"series_count":1},"n":{"rows_held":100,"series_count":1}}
        }))
        .unwrap();
        let context = CandidatePlanningContext {
            deadline: None,
            refine_materialization: false,
            seed_separate_plans: false,
            original_roots: &roots,
            entries: &entries,
            data: &data,
            models: crate::PlanningModels::builtin(),
            calibration: asap_cost::CalibrationFit::default(),
        };
        let profile = extract_price_options(&configured, &context)
            .unwrap()
            .unwrap();
        assert!(
            !profile.candidates[root.0].is_empty(),
            "unsupported binary fixture {query}"
        );
        let proposal =
            crate::search::milp::direct_price_options::solve_joint_price_proposal_with_metrics(
                &configured.dag,
                &[],
                None,
                &mut Default::default(),
                &profile,
            )
            .unwrap()
            .unwrap();
        let mut rejections =
            crate::search::refinement::planner::CandidateRejectionSummary::default();
        let evaluated = crate::search::refinement::planner::evaluate_configured_candidate(
            &configured,
            &context,
            &proposal.selected,
            0,
            &mut rejections,
            &mut Vec::new(),
        )
        .unwrap()
        .unwrap_or_else(|| panic!("{query}: {rejections:?}"));
        let actual = evaluated.0.costs[&evaluated.0.selected]
            .cost_per_second
            .unwrap();
        let direct = proposal.direct_modeled_cost.unwrap();
        assert!(
            (actual - direct).abs() <= 1e-9 * actual.max(1e-15),
            "{query}: actual={actual}, direct={direct}"
        );
        if query == "m + m" {
            let binary = configured
                .dag
                .candidates
                .iter()
                .enumerate()
                .find(|(_, candidate)| candidate.producer_candidates_per_input.len() == 2)
                .unwrap()
                .0;
            let edges: Vec<_> = proposal
                .selected
                .connections
                .iter()
                .filter(|edge| edge.consumer.0 == binary)
                .collect();
            assert_eq!(edges.len(), 2);
            assert_eq!(edges[0].producer, edges[1].producer);
            assert_ne!(edges[0].requirement, edges[1].requirement);
        }
    }
}

// Two independently approximate inputs must retain their divided confidence
// budgets; pricing may not silently resize either state after selection.
#[test]
fn two_sketch_binary_prices_match_evaluator_after_budget_sizing() {
    use asap_logical_asap_optimization::pass1::per_computation_candidates::{
        compose_logical_candidate, generate_per_computation_candidates,
        HowToComputeAggregateMeasure,
    };
    use asap_types::ir::properties::{AccuracyMetric, AccuracyScope};
    use asap_types::ir::schema::SketchAlgorithm;
    let query = "quantile_over_time(0.99, m[5m]) / quantile_over_time(0.5, n[5m])";
    let target = AccuracyTarget::max_error(0.05)
        .accuracy_metric(AccuracyMetric::Relative)
        .failure_probability(0.01)
        .accuracy_scope(AccuracyScope::PerValue);
    let planned = AccuracyTarget::max_error(0.01)
        .accuracy_metric(AccuracyMetric::Relative)
        .failure_probability(0.01)
        .accuracy_scope(AccuracyScope::PerValue);
    let original = lower_promql(query, planned);
    let generated = generate_per_computation_candidates(
        &asap_logical_asap_optimization::pass2::memo::DagOfLogicalMemoGroups::from_roots(&[(
            0,
            QueryRoot::Operator(original.clone()),
        )]),
    )
    .unwrap();
    let choices: Vec<_> = generated
        .targets
        .iter()
        .map(|target| {
            target
                .ways_to_compute
                .iter()
                .position(|choice| {
                    matches!(
                        choice,
                        HowToComputeAggregateMeasure::Summary(SketchAlgorithm::DDSketch)
                    )
                })
                .unwrap()
        })
        .collect();
    assert_eq!(choices.len(), 2);
    let roots: Vec<_> = compose_logical_candidate(&generated, &choices).unwrap();
    let data: DataWorkload = serde_json::from_value(serde_json::json!({
        "arrival":{"ingesting":{"rows_per_second":100.0}},
        "sources":{"m":{"rows_held":3000,"series_count":1,"values_non_negative":true},
                   "n":{"rows_held":3000,"series_count":1,"values_non_negative":true}}
    }))
    .unwrap();
    let sized = crate::sized_as_pass1(&roots, std::slice::from_ref(&target), &data);
    let QueryRoot::Operator(root) = &sized[0] else {
        panic!("operator root")
    };
    let mut configured = PhysicalCandidatesWithCosts::empty_for_tests();
    let id = append(root, &mut configured);
    configured.dag.query_roots.push(QueryOutputCandidates {
        candidates: vec![id],
    });
    configured.sync_physical_for_tests();
    let original = vec![QueryRoot::Operator(original)];
    let entries = vec![QueryWorkloadEntry {
        query: Query(query.into()),
        requirements: QueryRequirements {
            accuracy: target,
            max_latency_ms: None,
        },
        predictability: Predictability::Predictable { known_at: None },
        recurrence: QueryRecurrence::Repeated(RepeatedDemand::FixedInterval(RepetitionInterval(
            60_000,
        ))),
        range_evaluation: None,
    }];
    let context = CandidatePlanningContext {
        deadline: None,
        refine_materialization: false,
        seed_separate_plans: false,
        original_roots: &original,
        entries: &entries,
        data: &data,
        models: crate::PlanningModels::builtin(),
        calibration: asap_cost::CalibrationFit::default(),
    };
    let profile = extract_price_options(&configured, &context)
        .unwrap()
        .unwrap();
    assert!(
        !profile.candidates[id.0].is_empty(),
        "two certified sketches must reach root"
    );
    let proposal =
        crate::search::milp::direct_price_options::solve_joint_price_proposal_with_metrics(
            &configured.dag,
            &[],
            None,
            &mut Default::default(),
            &profile,
        )
        .unwrap()
        .unwrap();
    let mut rejections = crate::search::refinement::planner::CandidateRejectionSummary::default();
    let evaluated = crate::search::refinement::planner::evaluate_configured_candidate(
        &configured,
        &context,
        &proposal.selected,
        0,
        &mut rejections,
        &mut Vec::new(),
    )
    .unwrap()
    .unwrap_or_else(|| panic!("two-sketch plan rejected: {rejections:?}"));
    let actual = evaluated.0.costs[&evaluated.0.selected]
        .cost_per_second
        .unwrap();
    assert!((actual - proposal.direct_modeled_cost.unwrap()).abs() <= 1e-9 * actual.max(1e-15));
}

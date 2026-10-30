//! Whole-evaluator parity for selected consumer range-retention tiers.
use super::*;
use asap_logical_asap_optimization::pass1::per_computation_candidates::{
    compose_logical_candidate, generate_per_computation_candidates, HowToComputeAggregateMeasure,
};
use asap_logical_asap_optimization::test_support::lower_promql;
use asap_types::ir::properties::{AccuracyScope, AccuracyTarget};
use asap_types::ir::schema::SketchAlgorithm;
use asap_types::ir::OperatorNode;

use crate::search::candidates::dag::{InputProducerCandidates, QueryOutputCandidates};
use asap_types::workload::{
    DataWorkload, Predictability, Query, QueryRequirements, QueryWorkloadEntry, RangeEvaluation,
    RepetitionInterval,
};
use std::rc::Rc;

#[test]
fn shared_pane_range_tiers_match_evaluator_for_ingestion_and_kept_refresh() {
    fn append(
        node: Rc<OperatorNode>,
        configured: &mut PhysicalCandidatesWithCosts,
        ingestion: bool,
        mode: Materialization,
    ) -> PhysicalCandidateId {
        let ingestion = ingestion
            || (matches!(node.asap(), Some(ASAPOp::PaneChain { .. }))
                && mode == Materialization::MatAtIngestion(Medium::Memory));
        let pane = matches!(node.asap(), Some(ASAPOp::PaneChain { .. }));
        let mut inputs = Vec::new();
        node.operator
            .map_children(|child| inputs.push(append(child.clone(), configured, ingestion, mode)));
        let id = PhysicalCandidateId(configured.physical.candidates.len());
        configured.push_for_tests(
            node,
            if ingestion {
                ExecutionTiming::IngestionTime
            } else {
                ExecutionTiming::QueryTime
            },
            if pane {
                mode
            } else {
                Materialization::NotMaterialized
            },
            0.0,
            inputs
                .into_iter()
                .map(|supplier| InputProducerCandidates {
                    candidates: vec![supplier].into(),
                })
                .collect(),
        );
        id
    }

    for mode in [
        Materialization::MatAtIngestion(Medium::Memory),
        Materialization::MatAtQueryKeptAcrossEvaluations(Medium::Memory),
    ] {
        let target = AccuracyTarget::max_error(0.05)
            .failure_probability(0.01)
            .accuracy_scope(AccuracyScope::PerValue);
        let original = lower_promql("quantile_over_time(0.95, latency[5m])", target.clone());
        let generated =
            generate_per_computation_candidates(vec![(0, QueryRoot::Operator(original.clone()))])
                .unwrap();
        let choice = generated.targets[0]
            .ways_to_compute
            .iter()
            .position(|candidate| {
                matches!(
                    candidate,
                    HowToComputeAggregateMeasure::Summary(SketchAlgorithm::Kll)
                )
            })
            .unwrap();
        let roots: Vec<_> = compose_logical_candidate(&generated, &[choice])
            .unwrap()
            .into_iter()
            .map(|(_, root)| root)
            .collect();
        let rules = asap_logical_asap_optimization::pass2::asap_aware_cse::pane_candidates(
            &roots,
            &[Some(10_000)],
        );
        let roots =
            asap_logical_asap_optimization::pass2::asap_aware_cse::window_composition_pass2_candidates(
                &roots, &rules,
            )
            .unwrap()
            .pop()
            .unwrap()
            .roots
            .unwrap();
        let data: DataWorkload = serde_json::from_value(serde_json::json!({
            "arrival":{"ingesting":{"rows_per_second":100.0}},
            "sources":{"latency":{"series_count":2,"rows_held":30000,"values_non_negative":true}}
        }))
        .unwrap();
        let sized = crate::sized_as_pass1(&roots, std::slice::from_ref(&target), &data);
        let mut configured = PhysicalCandidatesWithCosts::empty_for_tests();
        let QueryRoot::Operator(root) = &sized[0] else {
            panic!("operator")
        };
        let root_id = append(root.clone(), &mut configured, false, mode);
        configured.dag.query_roots = vec![
            QueryOutputCandidates {
                candidates: vec![root_id]
            };
            2
        ];
        configured.sync_physical_for_tests();
        let originals = vec![
            QueryRoot::Operator(original.clone()),
            QueryRoot::Operator(original),
        ];
        let entries: Vec<_> = [(60_000, 120_000, 10_000), (90_000, 600_000, 30_000)]
            .into_iter()
            .map(|(interval, span_ms, step_ms)| QueryWorkloadEntry {
                query: Query("quantile_over_time(0.95, latency[5m])".into()),
                requirements: QueryRequirements {
                    accuracy: target.clone(),
                    max_latency_ms: None,
                },
                predictability: Predictability::Predictable { known_at: None },
                recurrence: QueryRecurrence::Repeated(RepeatedDemand::FixedInterval(
                    RepetitionInterval(
                        if mode == Materialization::MatAtQueryKeptAcrossEvaluations(Medium::Memory)
                        {
                            10_000
                        } else {
                            interval
                        },
                    ),
                )),
                range_evaluation: Some(RangeEvaluation { span_ms, step_ms }),
            })
            .collect();
        let mut settings = asap_cost::CostInputs::default();
        settings.resource_speed.ms_per_cpu_op *= 1000.0;
        settings.resource_speed.ms_per_scanned_byte *= 1000.0;
        settings.measured_operation_costs.ns_per_evaluation = 1_000_000.0;
        let deployment = asap_logical_asap_optimization::DeploymentCapabilities {
            query_time_results_retained: true,
            ..Default::default()
        };
        crate::search::candidates::configurations::fill_may_keep_across_evaluations_per_sole_reader_query(
            &mut configured,
            &entries,
            &data,
            deployment.query_time_results_retained,
        );
        let context = CandidatePlanningContext {
            deadline: None,
            refine_materialization: false,
            seed_separate_plans: false,
            original_roots: &originals,
            entries: &entries,
            data: &data,
            models: crate::PlanningModels {
                plan_selection: &settings,
                deployment_capabilities: &deployment,
                ..crate::PlanningModels::builtin()
            },
            calibration: asap_cost::CalibrationFit::default(),
        };
        let profile = extract_price_options(&configured, &context)
            .unwrap()
            .unwrap();
        let chain = (0..configured.physical.candidates.len())
            .position(|id| matches!(configured.node(id).asap(), Some(ASAPOp::PaneChain { .. })))
            .unwrap();
        let tiers: Vec<_> = profile.candidates[chain]
            .iter()
            .map(|option| option.required_range_span_ms)
            .collect();
        assert!(
            tiers.contains(&Some(120_000)) && tiers.contains(&Some(600_000)),
            "{mode:?}: {tiers:?}"
        );
        let general = profile.schedules.as_ref().unwrap();
        let kept = mode == Materialization::MatAtQueryKeptAcrossEvaluations(Medium::Memory);
        assert!(
            (general.union_rate([true, true].into_iter()) - if kept { 2.9 } else { 0.45 }).abs()
                < 1e-12
        );
        assert!(
            (general.request_union_rate([true, true].into_iter())
                - if kept { 0.1 } else { 1.0 / 60.0 + 1.0 / 90.0 })
            .abs()
                < 1e-12
        );
        assert!(profile.candidates[chain]
            .iter()
            .all(|option| option.profile.per_request
                == (mode == Materialization::MatAtQueryKeptAcrossEvaluations(Medium::Memory))));
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
        // Keep option indices/bindings unchanged, but deny every option a long
        // enough retention witness. Both outputs must then be infeasible.
        let mut insufficient = profile.clone();
        for option in &mut insufficient.candidates[chain] {
            option.required_range_span_ms = Some(120_000);
        }
        assert!(
            crate::search::milp::direct_price_options::solve_joint_price_proposal_with_metrics(
                &configured.dag,
                &[],
                None,
                &mut Default::default(),
                &insufficient
            )
            .unwrap()
            .is_none()
        );
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
        .unwrap_or_else(|| panic!("{mode:?}: {rejections:?}"));
        let actual = evaluated.0.costs[&evaluated.0.selected]
            .cost_per_second
            .unwrap();
        let modeled = proposal.direct_modeled_cost.unwrap();
        assert!(
            (actual - modeled).abs() <= 1e-9 * actual.max(1e-15),
            "{mode:?}: direct {modeled}, evaluator {actual}"
        );
    }
}

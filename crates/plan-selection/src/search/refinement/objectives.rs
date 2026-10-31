//! Exact evaluator ranking over a bounded traversal of the candidate graph.
//! Traversal priorities are not an objective scalarization: PlanSelector owns
//! cluster lexicographic priorities, Pareto dominance, constraints and tie policy.
use std::collections::BTreeMap;
use std::time::Instant;

use asap_types::physical::PhysicalWorkloadPlan;

use crate::search::candidates::dag::CandidateSelectionError;
use crate::search::candidates::dag::PhysicalCandidatesWithCosts;
use crate::search::candidates::graph::SelectedCandidateGraph;
use crate::search::costing::calibration::CandidateCalibration;
use crate::search::milp::model::{
    solve_candidate_proposal_with_metrics, CandidateSolveMetrics, ObservedAssignment,
};
use crate::search::refinement::planner::{
    accuracy_baseline_weights, reconstruct_configured_candidate, CandidatePlanningContext,
    CandidateRejectionSummary,
};
use crate::search::refinement::search::{CandidateSearchLimits, CandidateSearchStatus};
use crate::{ErrorSelectingPlan, PlanSelector, WholePlanSelection};
use asap_cost::CalibrationFit;

// `calibration` and `calibration_scope` are read by the engine's tests; #1095 replaces the engine.
#[cfg_attr(not(test), allow(dead_code))]
pub(in crate::search) struct ObjectivePlanningResult {
    pub status: CandidateSearchStatus,
    pub solve_metrics: CandidateSolveMetrics,
    pub validation_seconds: f64,
    pub calibration: CalibrationFit,
    pub calibration_scope: &'static str,
    pub selected_graph: Option<SelectedCandidateGraph>,
    pub best: Option<(WholePlanSelection, PhysicalWorkloadPlan)>,
    /// When no plan is valid, the first rejection of each cause.
    pub evaluation_rejections: Vec<crate::Rejection>,
}

/// Enumerate connection assignments without replacing a lexicographic
/// objective by a weighted sum. Exhaustion proves optimality for this
/// catalog; a budget stop returns the incumbent over evaluated candidates
/// only. Complete-catalog calibration must finish before any calibrated
/// plan is returned.
pub(in crate::search) fn optimize_objective_candidates(
    configured: &PhysicalCandidatesWithCosts,
    context: &CandidatePlanningContext<'_>,
    limits: CandidateSearchLimits,
    calibration: CandidateCalibration,
) -> Result<ObjectivePlanningResult, String> {
    let calibration = match context.models.cost_inputs.frozen_calibration_factor {
        Some(factor) => CandidateCalibration::Frozen(
            CalibrationFit::from_factor(factor).map_err(|error| error.to_string())?,
        ),
        None => calibration,
    };
    let started = Instant::now();
    let context = &CandidatePlanningContext {
        deadline: crate::search::refinement::planner::earliest_deadline(
            context.deadline,
            started.checked_add(limits.wall_time),
        ),
        ..*context
    };
    let original = &configured.dag;
    original.validate().map_err(|error| error.to_string())?;
    if configured.physical.candidates.len() != original.candidates.len()
        || context.query_roots.len() != context.entries.len()
    {
        return Err("objective search configuration and query dimensions disagree".into());
    }
    let mut selector = PlanSelector::new(context.entries, context.data, context.models, false)
        .map_err(|error| error.to_string())?;
    let mut fit = match calibration {
        CandidateCalibration::Frozen(fit) => {
            if !fit.factor().is_finite() || fit.factor() <= 0.0 {
                return Err(
                    "objective search requires a positive finite frozen calibration".into(),
                );
            }
            selector.set_calibration(fit);
            fit
        }
        CandidateCalibration::CompleteCatalog => CalibrationFit::default(),
    };
    let mut traversal = original.clone();
    // Prefer an accuracy-safe first proposal when the contract requires one.
    // Ignored guarantees must not delay a legal cheaper implementation.
    if !context.models.accuracy.may_accept_regardless_of_guarantee() {
        for (candidate, weight) in traversal
            .candidates
            .iter_mut()
            .zip(accuracy_baseline_weights(configured, context)?)
        {
            candidate.local_cost = weight;
        }
    }
    let mut observations = Vec::new();
    let mut physical_catalog = Vec::new();
    let mut selected_by_id = BTreeMap::new();
    let mut metrics = CandidateSolveMetrics::default();
    let mut rejections = CandidateRejectionSummary::default();
    let mut validation_seconds = 0.0;
    let mut refinements = 0;
    let mut catalog_exhausted = false;
    let mut status = loop {
        let remaining = limits.wall_time.saturating_sub(started.elapsed());
        if remaining.is_zero() {
            break CandidateSearchStatus::TimeLimit;
        }
        if refinements >= limits.max_refinements {
            break CandidateSearchStatus::RefinementLimit;
        }
        let proposal = match solve_candidate_proposal_with_metrics(
            &traversal,
            &observations,
            Some(remaining.as_secs_f64()),
            &mut metrics,
        ) {
            Ok(Some(proposal)) => proposal,
            Ok(None) => {
                catalog_exhausted = true;
                break CandidateSearchStatus::Optimal;
            }
            Err(CandidateSelectionError::Solver(_)) if started.elapsed() >= limits.wall_time => {
                break CandidateSearchStatus::TimeLimit
            }
            Err(error) => break CandidateSearchStatus::SolverStopped(error.to_string()),
        };
        let mut selected = proposal.selected.clone();
        selected.modeled_cost = selected
            .selected_candidates
            .iter()
            .map(|id| original.candidates[id.0].local_cost)
            .sum();
        refinements += 1;
        let validation_started = Instant::now();
        let mut rejected_queries = Vec::new();
        let reconstructed = reconstruct_configured_candidate(
            configured,
            context,
            &selected,
            refinements,
            &mut rejections,
            &mut rejected_queries,
        );
        let evaluation = match reconstructed {
            Ok(Some(physical)) => {
                selected_by_id.insert(physical.id.clone(), selected);
                match calibration {
                    CandidateCalibration::CompleteCatalog => {
                        match selector.calibration_fit(&physical) {
                            Ok(candidate_fit) => {
                                fit.add(candidate_fit);
                                physical_catalog.push(physical);
                                Ok(())
                            }
                            Err(error) => Err(error),
                        }
                    }
                    CandidateCalibration::Frozen(_) => {
                        selector.offer_before(&physical, context.deadline)
                    }
                }
            }
            Ok(None) => Ok(()),
            Err(error) => Err(ErrorSelectingPlan::Solver(error)),
        };
        validation_seconds += validation_started.elapsed().as_secs_f64();
        if let Err(error) = evaluation {
            break match error {
                ErrorSelectingPlan::EvaluationDeadlineExceeded => CandidateSearchStatus::TimeLimit,
                error => CandidateSearchStatus::EvaluatorStopped(error.to_string()),
            };
        }
        // The seed preference is not an enumeration order for the entire
        // catalog: that would exhaust exact variants before visiting summaries.
        // Costs only order proposals here; no-good cuts retain every other plan
        // and PlanSelector still owns the configured objective/Pareto ranking.
        if refinements == 1 {
            for (candidate, original) in traversal.candidates.iter_mut().zip(&original.candidates) {
                candidate.local_cost = original.local_cost;
            }
        }
        // These are traversal skip decisions, not claims that a validated plan
        // is infeasible. The selector retains its incumbent/Pareto archive.
        let mut observed = proposal.selected;
        observed.modeled_cost = observed
            .selected_candidates
            .iter()
            .map(|id| traversal.candidates[id.0].local_cost)
            .sum();
        observations.push(ObservedAssignment {
            selected: observed,
            exact_cost: None,
            rejected_queries,
        });
    };
    let mut evaluation_complete = catalog_exhausted;
    let calibration_scope = match calibration {
        CandidateCalibration::CompleteCatalog => {
            if !catalog_exhausted {
                return Err(format!("complete-catalog calibration is incomplete after {refinements} assignments ({status:?}); no partially calibrated plan is returned; supply frozen_calibration_factor to use bounded planning"));
            }
            if !fit.factor().is_finite() || fit.factor() <= 0.0 {
                return Err(
                    "complete-catalog calibration produced a nonfinite or nonpositive factor"
                        .into(),
                );
            }
            let validation_started = Instant::now();
            selector.set_calibration(fit);
            for (index, physical) in physical_catalog.iter().enumerate() {
                if started.elapsed() >= limits.wall_time {
                    status = CandidateSearchStatus::TimeLimit;
                    evaluation_complete = false;
                    break;
                }
                match selector.offer_before(physical, context.deadline) {
                    Ok(()) => {}
                    Err(ErrorSelectingPlan::EvaluationDeadlineExceeded) => {
                        status = CandidateSearchStatus::TimeLimit;
                        evaluation_complete = false;
                        break;
                    }
                    Err(error) => return Err(error.to_string()),
                }
                // Evaluators are cooperative; retain the validated incumbent
                // from an overrun, but never begin another unbudgeted offer.
                if started.elapsed() >= limits.wall_time {
                    status = CandidateSearchStatus::TimeLimit;
                    evaluation_complete = index + 1 == physical_catalog.len();
                    break;
                }
            }
            validation_seconds += validation_started.elapsed().as_secs_f64();
            "complete_catalog"
        }
        CandidateCalibration::Frozen(_) if selector.needs_calibration() => "provided_frozen",
        CandidateCalibration::Frozen(_)
            if context
                .models
                .cost_inputs
                .frozen_calibration_factor
                .is_some() =>
        {
            "provided_frozen"
        }
        CandidateCalibration::Frozen(_) => "not_required",
    };
    let mut evaluation_rejections = Vec::new();
    let best = match selector.finish() {
        Ok(best) => {
            for rejection in best
                .0
                .rejected
                .iter()
                .filter(|rejection| rejection.cause.failed_a_check())
            {
                let category = serde_json::to_value(rejection.cause)
                    .ok()
                    .and_then(|value| value.as_str().map(str::to_owned))
                    .unwrap_or_else(|| format!("{:?}", rejection.cause));
                rejections.record(category, &rejection.reason);
            }
            Some(best)
        }
        Err(ErrorSelectingPlan::NoValidCandidate(rejected)) => {
            evaluation_rejections = rejected.clone();
            for rejection in rejected {
                let category = serde_json::to_value(rejection.cause)
                    .ok()
                    .and_then(|value| value.as_str().map(str::to_owned))
                    .unwrap_or_else(|| format!("{:?}", rejection.cause));
                rejections.record(category, &rejection.reason);
            }
            if catalog_exhausted && evaluation_complete && status == CandidateSearchStatus::Optimal
            {
                status = CandidateSearchStatus::Infeasible;
            }
            None
        }
        Err(error) => return Err(error.to_string()),
    };
    let selected_graph = best
        .as_ref()
        .map(|(selection, _)| {
            selected_by_id
                .remove(&selection.selected)
                .ok_or("selected physical plan is missing its connection assignment")
        })
        .transpose()?;
    Ok(ObjectivePlanningResult {
        status,
        solve_metrics: metrics,
        validation_seconds,
        calibration: fit,
        calibration_scope,
        selected_graph,
        best,
        evaluation_rejections,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::search::candidates::configurations::enumerate_candidate_configurations;
    use crate::search::candidates::inventory::build_candidate_inventory;
    use crate::search::milp::model::solve_candidate_proposal;
    use crate::SelectionModels;
    use asap_cost::CostInputs;
    use asap_logical_asap_optimization::generate_dag_of_logical_memo_groups;
    use asap_logical_asap_optimization::test_support::generate_per_computation_candidates;
    use asap_logical_asap_optimization::test_support::try_populate_pass1;
    use asap_logical_asap_optimization::test_support::{entry_with, lower_promql};
    use asap_types::ir::{properties::AccuracyTarget, QueryRoot};
    use asap_types::workload::{
        DataArrival, DataWorkload, Predictability, QueryRecurrence, QueryWorkloadEntry,
        RepeatedDemand, RepetitionInterval, SourceFacts,
    };
    use std::time::Duration;

    fn fixture() -> (
        PhysicalCandidatesWithCosts,
        Vec<QueryRoot>,
        Vec<QueryWorkloadEntry>,
        DataWorkload,
    ) {
        let root = QueryRoot::Operator(lower_promql(
            "sum_over_time(latency[1m])",
            AccuracyTarget::exact(),
        ));
        let roots = vec![root];
        let query_roots = vec![roots[0].clone()];
        let initial = generate_dag_of_logical_memo_groups(&query_roots);
        let output = *initial.query_roots[0].as_operator().unwrap();
        let generated =
            generate_per_computation_candidates(&generate_dag_of_logical_memo_groups(&query_roots))
                .unwrap();
        let memo = try_populate_pass1(initial, &generated).unwrap();
        let inventory = build_candidate_inventory(&memo, &[output], |_| Ok(0.0)).unwrap();
        let mut entry = entry_with(
            AccuracyTarget::exact(),
            QueryRecurrence::Repeated(RepeatedDemand::FixedInterval(RepetitionInterval(60_000))),
        );
        entry.predictability = Predictability::Predictable { known_at: None };
        let entries = vec![entry];
        let data = DataWorkload {
            arrival: DataArrival::Ingesting {
                rows_per_second: 100.0,
            },
            sources: [(
                "latency".into(),
                SourceFacts {
                    series_count: Some(1),
                    ..Default::default()
                },
            )]
            .into_iter()
            .collect(),
        };
        let configured = enumerate_candidate_configurations(
            &inventory,
            &[0],
            &entries,
            &data,
            &asap_logical_asap_optimization::DeploymentCapabilities::default(),
        )
        .unwrap();
        (configured, roots, entries, data)
    }

    fn limits(n: usize) -> CandidateSearchLimits {
        CandidateSearchLimits {
            max_refinements: n,
            wall_time: Duration::from_secs(30),
        }
    }

    // Independently enumerate using zero costs, then use the pre-existing
    // batch selector as the ranking and calibration reference.
    fn reference_catalog(
        configured: &PhysicalCandidatesWithCosts,
        context: &CandidatePlanningContext<'_>,
    ) -> Vec<PhysicalWorkloadPlan> {
        let graph = &configured.dag;
        let mut observations = Vec::new();
        let mut catalog = Vec::new();
        for index in 1..=256 {
            let Some(proposal) = solve_candidate_proposal(graph, &observations, None).unwrap()
            else {
                return catalog;
            };
            if let Some(physical) = reconstruct_configured_candidate(
                configured,
                context,
                &proposal.selected,
                index,
                &mut CandidateRejectionSummary::default(),
                &mut Vec::new(),
            )
            .unwrap()
            {
                catalog.push(physical);
            }
            observations.push(ObservedAssignment {
                selected: proposal.selected,
                exact_cost: None,
                rejected_queries: Vec::new(),
            });
        }
        panic!("reference fixture must exhaust its small catalog");
    }

    fn measured_settings() -> CostInputs {
        serde_json::from_value(serde_json::json!({"measured_operation_costs":{"operators":[{"operator":"Scan", "ns_per_unit":100.0}]}})).unwrap()
    }

    // Complete-catalog fitting agrees with select_plan; a prefix is not a fit.
    #[test]
    fn measured_calibration_requires_complete_catalog_or_frozen_factor() {
        let (configured, roots, entries, data) = fixture();
        let settings = measured_settings();
        let models = SelectionModels {
            cost_inputs: &settings,
            ..SelectionModels::builtin()
        };
        let context = CandidatePlanningContext {
            deadline: None,
            refine_materialization: false,
            query_roots: &roots,
            realizer: &crate::search::test_realizer(&roots, &entries, &data),
            entries: &entries,
            data: &data,
            models,
            calibration: CalibrationFit::default(),
        };
        assert!(matches!(
            CandidateCalibration::for_models(models).unwrap(),
            CandidateCalibration::CompleteCatalog
        ));
        let error = optimize_objective_candidates(
            &configured,
            &context,
            limits(1),
            CandidateCalibration::CompleteCatalog,
        )
        .err()
        .unwrap();
        assert!(error.contains("calibration is incomplete"));
        let catalog = reference_catalog(&configured, &context);
        let expected = crate::select_among_plans(&catalog, &entries, &data, models).unwrap();
        let result = optimize_objective_candidates(
            &configured,
            &context,
            limits(256),
            CandidateCalibration::CompleteCatalog,
        )
        .unwrap();
        assert_eq!(result.calibration_scope, "complete_catalog");
        assert!(result.calibration.factor() > 1.0);
        let (selection, _) = result.best.unwrap();
        let actual_cost = selection.costs[&selection.selected]
            .cost_per_second
            .unwrap();
        let expected_cost = expected.costs[&expected.selected].cost_per_second.unwrap();
        assert!((actual_cost - expected_cost).abs() <= expected_cost * 1e-9);
    }

    // An explicit fit is stable across bounded discovery and selector calls.
    #[test]
    fn frozen_factor_is_validated_and_overrides_local_fit() {
        for invalid in [0.0, -1.0, f64::INFINITY, f64::NAN] {
            assert!(CalibrationFit::from_factor(invalid).is_err());
        }
        let (configured, roots, entries, data) = fixture();
        let settings = CostInputs {
            frozen_calibration_factor: Some(3.0),
            ..measured_settings()
        };
        let models = SelectionModels {
            cost_inputs: &settings,
            ..SelectionModels::builtin()
        };
        let mut selector = PlanSelector::new(&entries, &data, models, false).unwrap();
        assert!(!selector.needs_calibration());
        selector.set_calibration(CalibrationFit::from_factor(99.0).unwrap());
        assert_eq!(selector.formula_scale, 3.0);
        let policy = CandidateCalibration::for_models(models).unwrap();
        let context = CandidatePlanningContext {
            deadline: None,
            refine_materialization: false,
            query_roots: &roots,
            realizer: &crate::search::test_realizer(&roots, &entries, &data),
            entries: &entries,
            data: &data,
            models,
            calibration: CalibrationFit::from_factor(3.0).unwrap(),
        };
        let mismatched = CandidatePlanningContext {
            deadline: None,
            calibration: CalibrationFit::default(),
            ..context
        };
        let error = crate::search::refinement::planner::optimize_configured_candidates(
            &configured,
            &mismatched,
            limits(1),
        )
        .err()
        .unwrap();
        assert!(error.contains("same frozen calibration factor"));
        let result =
            optimize_objective_candidates(&configured, &context, limits(1), policy).unwrap();
        assert_eq!(result.calibration.factor(), 3.0);
        assert_eq!(result.calibration_scope, "provided_frozen");
        assert!(result.best.is_some());
    }
}

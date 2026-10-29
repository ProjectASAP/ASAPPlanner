//! Exact evaluator ranking over a bounded traversal of the candidate graph.
//! Traversal priorities are not an objective scalarization: PlanSelector owns
//! cluster lexicographic priorities, Pareto dominance, constraints and tie policy.
use std::collections::{BTreeMap, BTreeSet};
use std::time::Instant;

use asap_physical_asap_optimization::PhysicalWorkloadPlan;

use crate::search::candidates::configurations::CandidateConfigurationInventory;
use crate::search::candidates::graph::{CandidateSelectionError, SelectedCandidateGraph};
use crate::search::costing::calibration::CandidateCalibration;
use crate::search::milp::model::{
    solve_candidate_proposal_with_metrics, CandidateSolveMetrics, ObservedAssignment,
};
use crate::search::refinement::planner::{
    accuracy_baseline_weights, materialize_configured_candidate_with_diagnostics,
    reconstruct_configured_candidate, CandidatePlanningContext, CandidateRejectionSummary,
    MaterializationOutcome,
};
use crate::search::refinement::search::{CandidateSearchLimits, CandidateSearchStatus};
use crate::{PlanSelector, Selection, SelectionError};
use asap_cost::CalibrationFit;

#[derive(Debug, Clone, Copy, Default)]
pub(in crate::search) struct ObjectiveSearchOptions {
    pub keep_every_candidate: bool,
    pub exhaustive: bool,
}

pub(in crate::search) struct Stage2RejectedAssignment {
    pub from_logical: String,
    pub selected_graph: SelectedCandidateGraph,
    pub reason: String,
}

pub(in crate::search) struct ObjectivePlanningResult {
    pub status: CandidateSearchStatus,
    pub catalog_exhausted: bool,
    /// The complete generated catalog has finished final calibrated evaluation.
    pub evaluation_complete: bool,
    pub refinements: usize,
    pub solve_metrics: CandidateSolveMetrics,
    pub validation_seconds: f64,
    pub certified_query_cuts: usize,
    pub calibration: CalibrationFit,
    pub calibration_scope: &'static str,
    pub rejections: CandidateRejectionSummary,
    pub selected_graph: Option<SelectedCandidateGraph>,
    pub best: Option<(Selection, PhysicalWorkloadPlan)>,
    /// Populated only when requested; includes accuracy/capacity-rejected plans.
    pub evaluated_plans: Vec<(SelectedCandidateGraph, PhysicalWorkloadPlan)>,
    /// Full per-plan rejection records, including an entirely infeasible archive.
    pub evaluation_rejections: Vec<crate::Rejection>,
    pub stage2_rejected_assignments: Vec<Stage2RejectedAssignment>,
}

/// Enumerate connection assignments without replacing a lexicographic/Pareto
/// objective by a weighted sum. Exhaustion proves optimality for this catalog;
/// a budget stop returns an incumbent/front over evaluated candidates only.
/// Complete-catalog calibration must finish before any calibrated plan is returned.
#[cfg(test)]
pub(in crate::search) fn optimize_objective_candidates(
    configured: &CandidateConfigurationInventory,
    context: &CandidatePlanningContext<'_>,
    limits: CandidateSearchLimits,
    calibration: CandidateCalibration,
) -> Result<ObjectivePlanningResult, String> {
    optimize_objective_candidates_with_options(
        configured,
        context,
        limits,
        calibration,
        ObjectiveSearchOptions::default(),
    )
}

pub(in crate::search) fn optimize_objective_candidates_with_options(
    configured: &CandidateConfigurationInventory,
    context: &CandidatePlanningContext<'_>,
    limits: CandidateSearchLimits,
    calibration: CandidateCalibration,
    options: ObjectiveSearchOptions,
) -> Result<ObjectivePlanningResult, String> {
    let calibration = match context.models.plan_selection.frozen_calibration_factor {
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
    let original = &configured.inventory.graph;
    original.validate().map_err(|error| error.to_string())?;
    if configured.configurations.len() != original.candidates.len()
        || context.original_roots.len() != context.entries.len()
    {
        return Err("objective search configuration and query dimensions disagree".into());
    }
    let mut selector = PlanSelector::new(
        context.entries,
        context.data,
        context.models,
        options.keep_every_candidate,
    )
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
    if !context.models.plan_selection.ignore_accuracy_guarantees {
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
    let mut evaluated_plans = Vec::new();
    let mut completed_offers = BTreeSet::new();
    let mut stage2_rejected_assignments = Vec::new();
    let mut selected_by_id = BTreeMap::new();
    let mut metrics = CandidateSolveMetrics::default();
    let mut rejections = CandidateRejectionSummary::default();
    let mut validation_seconds = 0.0;
    let mut refinements = 0;
    let mut certified_query_cuts = 0;
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
        let reconstructed = if options.keep_every_candidate || options.exhaustive {
            match materialize_configured_candidate_with_diagnostics(
                configured,
                context,
                &selected,
                refinements,
                &mut rejections,
            ) {
                Ok(MaterializationOutcome::Implemented(physical)) => Ok(Some(physical)),
                Ok(MaterializationOutcome::RejectedAssignment(reason)) => {
                    if options.keep_every_candidate {
                        stage2_rejected_assignments.push(Stage2RejectedAssignment {
                            from_logical: format!("joint-logical-{refinements}"),
                            selected_graph: selected.clone(),
                            reason,
                        });
                    }
                    Ok(None)
                }
                Err(error) => Err(error),
            }
        } else {
            reconstruct_configured_candidate(
                configured,
                context,
                &selected,
                refinements,
                &mut rejections,
                &mut rejected_queries,
            )
        };
        let evaluation = match reconstructed {
            Ok(Some(physical)) => {
                if options.keep_every_candidate {
                    evaluated_plans.push((selected.clone(), physical.clone()));
                }
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
                    CandidateCalibration::Frozen(_) => selector
                        .offer_before(&physical, context.deadline)
                        .map(|()| {
                            completed_offers.insert(physical.id.clone());
                        }),
                }
            }
            Ok(None) => Ok(()),
            Err(error) => Err(SelectionError::Solver(error)),
        };
        validation_seconds += validation_started.elapsed().as_secs_f64();
        if let Err(error) = evaluation {
            break match error {
                SelectionError::EvaluationDeadlineExceeded => CandidateSearchStatus::TimeLimit,
                error => CandidateSearchStatus::EvaluatorStopped(error.to_string()),
            };
        }
        certified_query_cuts += rejected_queries.len();
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
                    Ok(()) => {
                        completed_offers.insert(physical.id.clone());
                    }
                    Err(SelectionError::EvaluationDeadlineExceeded) => {
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
                .plan_selection
                .frozen_calibration_factor
                .is_some() =>
        {
            "provided_frozen"
        }
        CandidateCalibration::Frozen(_) => "not_required",
    };
    // Reconstruction and calibration do not complete evaluation. In particular,
    // a compiler deadline must not leave an archived plan with neither a cost
    // nor a rejection. Keep every completed offer, including rejected plans.
    evaluated_plans.retain(|(_, physical)| completed_offers.contains(&physical.id));
    let mut evaluation_rejections = Vec::new();
    let best = match selector.finish() {
        Ok(best) => {
            if options.keep_every_candidate {
                evaluation_rejections = best.0.rejected.clone();
            }
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
        Err(SelectionError::NoValidCandidate(rejected)) => {
            if options.keep_every_candidate {
                evaluation_rejections = rejected.clone();
            }
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
        catalog_exhausted,
        evaluation_complete,
        refinements,
        solve_metrics: metrics,
        validation_seconds,
        certified_query_cuts,
        calibration: fit,
        calibration_scope,
        rejections,
        selected_graph,
        best,
        evaluated_plans,
        evaluation_rejections,
        stage2_rejected_assignments,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::search::candidates::configurations::enumerate_candidate_configurations;
    use crate::search::candidates::inventory::build_candidate_inventory;
    use crate::search::milp::model::solve_candidate_proposal;
    use crate::PlanningModels;
    use asap_cost::{ClusterLimits, ClusterObjective, CostInputs, CostModel};
    use asap_logical_asap_optimization::pass1::per_computation_candidates::generate_per_computation_candidates;
    use asap_logical_asap_optimization::pass2::memo::candidates::try_populate_pass1;
    use asap_logical_asap_optimization::pass2::memo::{DagOfLogicalMemoGroups, MemoGroupId};
    use asap_logical_asap_optimization::test_support::{entry_with, lower_promql};
    use asap_types::ir::{properties::AccuracyTarget, QueryRoot};
    use asap_types::workload::{
        DataArrival, DataWorkload, Predictability, QueryRecurrence, QueryWorkloadEntry,
        RepeatedDemand, RepetitionInterval, SourceFacts,
    };
    use std::time::Duration;

    fn fixture() -> (
        CandidateConfigurationInventory,
        Vec<QueryRoot>,
        Vec<QueryWorkloadEntry>,
        DataWorkload,
    ) {
        let root = QueryRoot::Operator(lower_promql(
            "sum_over_time(latency[1m])",
            AccuracyTarget::exact(),
        ));
        let roots = vec![root];
        let identified = vec![(0usize, roots[0].clone())];
        let initial = DagOfLogicalMemoGroups::from_roots(&identified);
        let output = MemoGroupId(
            initial
                .groups
                .iter()
                .position(|group| group.roots_of.contains(&0))
                .unwrap() as u32,
        );
        let generated = generate_per_computation_candidates(identified).unwrap();
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
            PlanningModels::builtin().deployment_capabilities,
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
        configured: &CandidateConfigurationInventory,
        context: &CandidatePlanningContext<'_>,
    ) -> Vec<PhysicalWorkloadPlan> {
        let graph = &configured.inventory.graph;
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

    fn usage(selection: &Selection, id: &str) -> (u64, u64, u64) {
        let usage = &selection.costs[id].usage;
        (
            usage.peak_memory_bytes,
            usage.disk_bytes,
            crate::worst_latency_ms(usage).to_bits(),
        )
    }

    // All cluster priorities and the Pareto set use the original evaluator policy.
    #[test]
    fn cluster_priorities_and_pareto_match_batch_selector() {
        let (configured, roots, entries, data) = fixture();
        for objective in [
            ClusterObjective::LowestLatency,
            ClusterObjective::LeastMemory,
            ClusterObjective::ParetoFront,
        ] {
            let settings = CostInputs {
                cost_model: CostModel::ClusterCapacity(ClusterLimits {
                    memory_bytes: u64::MAX,
                    disk_bytes: u64::MAX,
                    cpu_cores: None,
                    objective,
                }),
                ..Default::default()
            };
            let models = PlanningModels {
                plan_selection: &settings,
                ..PlanningModels::builtin()
            };
            let context = CandidatePlanningContext {
                deadline: None,
                refine_materialization: false,
                seed_separate_plans: false,
                original_roots: &roots,
                entries: &entries,
                data: &data,
                models,
                calibration: CalibrationFit::default(),
            };
            let catalog = reference_catalog(&configured, &context);
            let expected = crate::select_plan(&catalog, &entries, &data, models).unwrap();
            let actual = optimize_objective_candidates(
                &configured,
                &context,
                limits(256),
                CandidateCalibration::Frozen(CalibrationFit::default()),
            )
            .unwrap();
            assert!(actual.catalog_exhausted);
            assert_eq!(actual.status, CandidateSearchStatus::Optimal);
            let (selected, physical) = actual.best.unwrap();
            assert_eq!(
                usage(&selected, &selected.selected),
                usage(&expected, &expected.selected)
            );
            assert_eq!(physical.id, selected.selected);
            assert!(actual.selected_graph.is_some());
            let expected_front: std::collections::BTreeSet<_> = expected
                .front
                .iter()
                .map(|id| usage(&expected, id))
                .collect();
            let actual_front: std::collections::BTreeSet<_> = selected
                .front
                .iter()
                .map(|id| usage(&selected, id))
                .collect();
            assert_eq!(actual_front, expected_front);
        }
    }

    // A timeout/refinement prefix reports only its evaluated Pareto archive.
    #[test]
    fn partial_cluster_front_has_valid_selected_physical_plan() {
        let (configured, roots, entries, data) = fixture();
        let settings = CostInputs {
            cost_model: CostModel::ClusterCapacity(ClusterLimits {
                memory_bytes: u64::MAX,
                disk_bytes: u64::MAX,
                cpu_cores: None,
                objective: ClusterObjective::ParetoFront,
            }),
            ..Default::default()
        };
        let models = PlanningModels {
            plan_selection: &settings,
            ..PlanningModels::builtin()
        };
        let context = CandidatePlanningContext {
            deadline: None,
            refine_materialization: false,
            seed_separate_plans: false,
            original_roots: &roots,
            entries: &entries,
            data: &data,
            models,
            calibration: CalibrationFit::default(),
        };
        let result = optimize_objective_candidates(
            &configured,
            &context,
            limits(1),
            CandidateCalibration::Frozen(CalibrationFit::default()),
        )
        .unwrap();
        assert_eq!(result.status, CandidateSearchStatus::RefinementLimit);
        assert!(!result.catalog_exhausted);
        let (selection, physical) = result.best.unwrap();
        assert!(selection.front.contains(&selection.selected));
        let independently_checked =
            crate::select_plan(&[physical], &entries, &data, models).unwrap();
        assert_eq!(
            usage(&selection, &selection.selected),
            usage(&independently_checked, &independently_checked.selected)
        );
    }

    fn measured_settings() -> CostInputs {
        serde_json::from_value(serde_json::json!({"measured_operation_costs":{"operators":[{"operator":"Scan", "ns_per_unit":100.0}]}})).unwrap()
    }

    // Complete-catalog fitting agrees with select_plan; a prefix is not a fit.
    #[test]
    fn measured_calibration_requires_complete_catalog_or_frozen_factor() {
        let (configured, roots, entries, data) = fixture();
        let settings = measured_settings();
        let models = PlanningModels {
            plan_selection: &settings,
            ..PlanningModels::builtin()
        };
        let context = CandidatePlanningContext {
            deadline: None,
            refine_materialization: false,
            seed_separate_plans: false,
            original_roots: &roots,
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
        let expected = crate::select_plan(&catalog, &entries, &data, models).unwrap();
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

    // Keep-all preserves real candidate DAGs and every original selector result.
    #[test]
    fn exhaustive_archive_keeps_every_reconstructed_candidate() {
        let (configured, roots, entries, data) = fixture();
        let models = PlanningModels::builtin();
        let context = CandidatePlanningContext {
            deadline: None,
            refine_materialization: false,
            seed_separate_plans: false,
            original_roots: &roots,
            entries: &entries,
            data: &data,
            models,
            calibration: CalibrationFit::default(),
        };
        let result = optimize_objective_candidates_with_options(
            &configured,
            &context,
            limits(256),
            CandidateCalibration::Frozen(CalibrationFit::default()),
            ObjectiveSearchOptions {
                keep_every_candidate: true,
                exhaustive: true,
            },
        )
        .unwrap();
        assert!(result.catalog_exhausted);
        assert_eq!(result.certified_query_cuts, 0);
        assert!(result.evaluated_plans.len() > 1);
        let catalog: Vec<_> = result
            .evaluated_plans
            .iter()
            .map(|(_, physical)| physical.clone())
            .collect();
        let expected = crate::select_plan(&catalog, &entries, &data, models).unwrap();
        let actual = result.best.unwrap().0;
        assert_eq!(actual.selected, expected.selected);
        assert_eq!(actual.costs, expected.costs);
        for (graph, physical) in &result.evaluated_plans {
            assert!(!graph.query_roots.is_empty());
            assert_eq!(physical.roots.len(), entries.len());
        }
    }

    // Keep-all retains every rejected physical even when no candidate fits.
    #[test]
    fn infeasible_archive_preserves_capacity_rejections_and_unique_logical_ids() {
        let (configured, roots, entries, data) = fixture();
        let settings = CostInputs {
            cost_model: CostModel::ClusterCapacity(ClusterLimits {
                memory_bytes: 0,
                disk_bytes: 0,
                cpu_cores: None,
                objective: ClusterObjective::LowestLatency,
            }),
            ..Default::default()
        };
        let models = PlanningModels {
            plan_selection: &settings,
            ..PlanningModels::builtin()
        };
        let context = CandidatePlanningContext {
            deadline: None,
            refine_materialization: false,
            seed_separate_plans: false,
            original_roots: &roots,
            entries: &entries,
            data: &data,
            models,
            calibration: CalibrationFit::default(),
        };
        let result = optimize_objective_candidates_with_options(
            &configured,
            &context,
            limits(256),
            CandidateCalibration::Frozen(CalibrationFit::default()),
            ObjectiveSearchOptions {
                keep_every_candidate: true,
                exhaustive: true,
            },
        )
        .unwrap();
        assert!(result.catalog_exhausted);
        assert_eq!(result.status, CandidateSearchStatus::Infeasible);
        assert!(result.best.is_none());
        assert!(!result.evaluated_plans.is_empty());
        assert_eq!(
            result.evaluation_rejections.len(),
            result.evaluated_plans.len()
        );
        assert!(result
            .evaluation_rejections
            .iter()
            .all(|rejection| rejection.cause == crate::RejectionCause::OverClusterCapacity));
        let ids: std::collections::BTreeSet<_> = result
            .evaluated_plans
            .iter()
            .map(|(_, plan)| &plan.from_logical)
            .collect();
        assert_eq!(ids.len(), result.evaluated_plans.len());
    }

    // Stage 2 rejection is an archived logical assignment, not a missing entry.
    #[test]
    fn keep_all_archives_stage2_rejections_with_exact_reason_and_graph() {
        let (configured, roots, mut entries, data) = fixture();
        entries[0].predictability = Predictability::AdHoc;
        let context = CandidatePlanningContext {
            deadline: None,
            refine_materialization: false,
            seed_separate_plans: false,
            original_roots: &roots,
            entries: &entries,
            data: &data,
            models: PlanningModels::builtin(),
            calibration: CalibrationFit::default(),
        };
        let result = optimize_objective_candidates_with_options(
            &configured,
            &context,
            limits(256),
            CandidateCalibration::Frozen(CalibrationFit::default()),
            ObjectiveSearchOptions {
                keep_every_candidate: true,
                exhaustive: true,
            },
        )
        .unwrap();
        assert!(result.catalog_exhausted);
        assert!(result.best.is_some());
        assert!(!result.stage2_rejected_assignments.is_empty());
        assert_eq!(
            result.refinements,
            result.evaluated_plans.len() + result.stage2_rejected_assignments.len()
        );
        for rejected in &result.stage2_rejected_assignments {
            assert!(rejected.from_logical.starts_with("joint-logical-"));
            let rebuilt =
                crate::search::candidates::reconstruction::reconstruct_selected_candidates(
                    &configured.inventory.graph,
                    &rejected.selected_graph,
                    &configured.inventory.templates,
                )
                .unwrap();
            assert_eq!(rebuilt.outputs.len(), roots.len());
            let outcome = materialize_configured_candidate_with_diagnostics(
                &configured,
                &context,
                &rejected.selected_graph,
                0,
                &mut CandidateRejectionSummary::default(),
            )
            .unwrap();
            let MaterializationOutcome::RejectedAssignment(reason) = outcome else {
                panic!("archived assignment must reproduce the Stage 2 rejection")
            };
            assert_eq!(reason, rejected.reason);
        }
    }

    // Adapter failures remain stop diagnostics, never infeasible archives/cuts.
    #[test]
    fn keep_all_does_not_archive_adapter_errors_as_stage2_rejections() {
        let (configured, mut roots, entries, data) = fixture();
        let QueryRoot::Operator(root) = &mut roots[0] else {
            unreachable!()
        };
        std::rc::Rc::make_mut(root).schema.fields[0]
            .name
            .push_str("_changed");
        let context = CandidatePlanningContext {
            deadline: None,
            refine_materialization: false,
            seed_separate_plans: false,
            original_roots: &roots,
            entries: &entries,
            data: &data,
            models: PlanningModels::builtin(),
            calibration: CalibrationFit::default(),
        };
        let result = optimize_objective_candidates_with_options(
            &configured,
            &context,
            limits(256),
            CandidateCalibration::Frozen(CalibrationFit::default()),
            ObjectiveSearchOptions {
                keep_every_candidate: true,
                exhaustive: true,
            },
        )
        .unwrap();
        assert!(matches!(
            result.status,
            CandidateSearchStatus::EvaluatorStopped(_)
        ));
        assert_eq!(result.refinements, 1);
        assert!(!result.catalog_exhausted);
        assert!(result.stage2_rejected_assignments.is_empty());
        assert!(result.evaluated_plans.is_empty());
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
        let models = PlanningModels {
            plan_selection: &settings,
            ..PlanningModels::builtin()
        };
        let mut selector = PlanSelector::new(&entries, &data, models, false).unwrap();
        assert!(!selector.needs_calibration());
        selector.set_calibration(CalibrationFit::from_factor(99.0).unwrap());
        assert_eq!(selector.formula_scale, 3.0);
        let policy = CandidateCalibration::for_models(models).unwrap();
        let context = CandidatePlanningContext {
            deadline: None,
            refine_materialization: false,
            seed_separate_plans: false,
            original_roots: &roots,
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

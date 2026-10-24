//! Driver integration for the explicit, bounded candidate-connection search.
use crate::{CostModel, PlanningModels, SelectionError, Stage1ToStage3Search};
use asap_logical_asap_optimization::memo::LogicalMemo;
use asap_logical_asap_optimization::per_computation_candidates::PerComputationCandidates;
use asap_types::ir::{OperatorNode, QueryRoot};
use asap_types::workload::{DataWorkload, QueryWorkloadEntry};
use std::rc::Rc;

/// Immutable workload and evaluator inputs shared by seed and complete searches.
pub(crate) struct CandidateSearchContext<'a, Id> {
    pub inventory: &'a PerComputationCandidates<Id>,
    pub entries: &'a [QueryWorkloadEntry],
    pub data: &'a DataWorkload,
    pub models: PlanningModels<'a>,
}

#[derive(Default)]
pub(crate) struct CandidateArchive {
    pub logical: Vec<crate::search::pipeline::LogicalCandidate>,
    pub physical: Vec<asap_physical_asap_optimization::PhysicalWorkloadPlan>,
    pub pruned: Vec<crate::search::pipeline::PrunedCandidate>,
}

// The driver reports price search and lexicographic/Pareto search uniformly,
// while only price search supplies a scalar objective bound.
struct RouteOutcome {
    independent_components: usize,
    catalog_exhausted: Option<bool>,
    evaluation_complete: Option<bool>,
    status: crate::search::refinement::search::CandidateSearchStatus,
    refinements: usize,
    metrics: crate::search::milp::model::CandidateSolveMetrics,
    validation_seconds: f64,
    query_cuts: usize,
    rejections: crate::search::refinement::planner::CandidateRejectionSummary,
    selected: Option<crate::search::candidates::graph::SelectedCandidateGraph>,
    best: Option<(
        crate::Selection,
        asap_physical_asap_optimization::PhysicalWorkloadPlan,
    )>,
    lower_bound: Option<f64>,
    relative_gap: Option<f64>,
    calibration: crate::CalibrationFit,
    calibration_scope: &'static str,
}

#[derive(Default)]
struct DomainProgress {
    budget_stopped: bool,
    infeasible: bool,
    catalog_completed: bool,
    refinements: usize,
    report: Option<crate::CandidateConnectionReport>,
}

/// A small catalog obtains a validated incumbent before full-width expansion.
/// Both phases use the same planner and frozen objective; only the complete
/// phase may establish a bound for the full declared domain.
pub(crate) fn candidate_connection_search<Id: Clone>(
    initial: LogicalMemo,
    context: &CandidateSearchContext<'_, Id>,
    search: Stage1ToStage3Search,
    local_candidate_generation_ms: Option<f64>,
    archive: &mut CandidateArchive,
) -> crate::search::pipeline::PlanSearchResult {
    let models = context.models;
    use crate::search::costing::calibration::CandidateCalibration;
    use asap_logical_asap_optimization::memo::state_configurations::PaneWidthDomain;
    use std::time::Instant;

    let started = Instant::now();
    let total_ms = search.milp_time_limit_ms.unwrap_or(30_000);
    let prior_ms = local_candidate_generation_ms.unwrap_or(0.0);
    let calibration = CandidateCalibration::for_models(models).map_err(SelectionError::Solver)?;
    // Exhaustive archives and complete-catalog calibration require one domain.
    let seed_enabled = !search.exhaustive
        && !search.keep_every_candidate
        && matches!(calibration, CandidateCalibration::Frozen(_))
        && prior_ms.is_finite()
        && prior_ms >= 0.0
        && total_ms as f64 - prior_ms >= 1_000.0
        && search.max_evaluations.unwrap_or(128) >= 4;
    if !seed_enabled {
        return search_domain(
            initial,
            context,
            search,
            local_candidate_generation_ms,
            archive,
            PaneWidthDomain::Complete,
            &mut DomainProgress::default(),
        );
    }
    let seed_ms = (((total_ms as f64 - prior_ms) / 4.0) as u64).min(5_000);
    let mut seed_progress = DomainProgress::default();
    let mut seed_archive = CandidateArchive::default();
    let seed = search_domain(
        initial.clone(),
        context,
        Stage1ToStage3Search {
            milp_time_limit_ms: Some(seed_ms),
            max_evaluations: Some((search.max_evaluations.unwrap_or(128) / 4).max(1)),
            ..search
        },
        Some(0.0),
        &mut seed_archive,
        PaneWidthDomain::SeedGrids,
        &mut seed_progress,
    );
    let seed_elapsed = started.elapsed();
    if seed.is_err() && !seed_progress.budget_stopped && !seed_progress.infeasible {
        return seed;
    }
    let seed_report = seed_progress.report.clone();
    let full_started = Instant::now();
    let mut full_progress = DomainProgress::default();
    let full = search_domain(
        initial,
        context,
        Stage1ToStage3Search {
            max_evaluations: Some(
                search
                    .max_evaluations
                    .unwrap_or(128)
                    .saturating_sub(seed_progress.refinements),
            ),
            ..search
        },
        Some(prior_ms + seed_elapsed.as_secs_f64() * 1_000.0),
        archive,
        PaneWidthDomain::Complete,
        &mut full_progress,
    );
    let full_planning_ms = full_started.elapsed().as_secs_f64() * 1_000.0;
    let mut seed_selected = false;
    let mut fallback_reason = None;
    let mut result = match (seed, full) {
        (
            Ok((seed_selection, mut seed_plan, seed_logical)),
            Ok((full_selection, full_plan, full_logical)),
        ) => {
            seed_plan.id = format!("seed-{}", seed_plan.id);
            seed_plan.from_logical = format!("seed-{}", seed_plan.from_logical);
            let ranking = crate::Ranking::of(&models.plan_selection.cost_model);
            // Both plans were validated under the same frozen calibration. Use
            // PlanSelector's ranking without another unbudgeted evaluator call.
            seed_selected = ranking.better(
                &seed_selection.costs[&seed_selection.selected],
                &full_selection.costs[&full_selection.selected],
            );
            let full_search = full_selection.search.clone();
            let mut seed_selection = seed_selection;
            seed_selection.selected = seed_plan.id.clone();
            seed_selection.costs = seed_selection
                .costs
                .into_iter()
                .map(|(id, cost)| (format!("seed-{id}"), cost))
                .collect();
            seed_selection.accepted = seed_selection
                .accepted
                .into_iter()
                .map(|(id, accuracy)| (format!("seed-{id}"), accuracy))
                .collect();
            for rejection in &mut seed_selection.rejected {
                rejection.id = format!("seed-{}", rejection.id);
            }
            let (mut selection, other, physical, logical) = if seed_selected {
                (seed_selection, full_selection, seed_plan, seed_logical)
            } else {
                (full_selection, seed_selection, full_plan, full_logical)
            };
            // Preserve the validated Pareto front from both searches.
            selection.costs.extend(other.costs);
            selection.accepted.extend(other.accepted);
            let ids = selection.costs.keys().cloned().collect::<Vec<_>>();
            let costs = ids
                .iter()
                .map(|id| &selection.costs[id])
                .collect::<Vec<_>>();
            let (_, front) = crate::choose(
                crate::Ranking::of(&models.plan_selection.cost_model),
                &costs,
            )
            .expect("two validated selections have costs");
            selection.front = front.into_iter().map(|index| ids[index].clone()).collect();
            selection.search = full_search;
            // A phase-local runner-up need not be second across both catalogs.
            selection.search.runner_up_cost_per_second = None;
            selection.search.incumbent_cost = selection
                .costs
                .get(&physical.id)
                .and_then(|cost| cost.cost_per_second);
            (selection, physical, logical)
        }
        (Ok(seed), Err(error)) => {
            fallback_reason = Some(error.to_string());
            let seed = validated_seed_after_stop(seed, error, &full_progress)?;
            seed_selected = true;
            seed
        }
        (_, Err(error)) => return Err(error),
        (Err(_), Ok(full)) => full,
    };
    let fallback_with_full_report = fallback_reason.is_some() && full_progress.report.is_some();
    if fallback_with_full_report {
        result.0.search.candidate_connections = full_progress.report.take();
    }
    if let Some(report) = result.0.search.candidate_connections.as_mut() {
        // A fallback before catalog completion has no full MILP metrics. Its
        // separate full_planning_ms records the interrupted generation effort.
        if fallback_reason.is_none() || fallback_with_full_report {
            if let Some(seed) = seed_report {
                report.memo_expansion_ms += seed.memo_expansion_ms;
                report.inventory_construction_ms += seed.inventory_construction_ms;
                report.search_and_validation_ms += seed.search_and_validation_ms;
                report.model_construction_ms += seed.model_construction_ms;
                report.solver_ms += seed.solver_ms;
                report.validation_ms += seed.validation_ms;
                report.solve_calls += seed.solve_calls;
                report.refinements += seed.refinements;
                report.peak_milp_variables =
                    report.peak_milp_variables.max(seed.peak_milp_variables);
                report.peak_milp_constraints =
                    report.peak_milp_constraints.max(seed.peak_milp_constraints);
            }
        }
        report.full_planning_ms = full_planning_ms;
        report.seed_planning_ms = seed_elapsed.as_secs_f64() * 1_000.0;
        report.seed_selected = seed_selected;
        report.full_catalog_completed = full_progress.catalog_completed;
        report.seed_fallback_reason = fallback_reason;
        report.time_limit_ms = total_ms;
        report.local_candidate_generation_ms = local_candidate_generation_ms;
        report.max_refinements = search.max_evaluations.unwrap_or(128);
        if report.seed_fallback_reason.is_some() {
            // The restricted seed's optimum is not a certificate for all widths.
            report.status = "time_or_evaluation_limit_with_validated_seed".into();
            report.lower_bound = None;
            report.relative_gap = None;
            report.catalog_exhausted = Some(false);
            report.evaluation_complete = Some(false);
        } else if seed_selected {
            // A full-domain lower bound remains valid; avoid a stale incumbent gap.
            report.relative_gap =
                report
                    .lower_bound
                    .zip(result.0.search.incumbent_cost)
                    .map(|(bound, cost)| {
                        if cost == 0.0 {
                            0.0
                        } else {
                            ((cost - bound).max(0.0) / cost.abs()).min(1.0)
                        }
                    });
        }
    }
    result.0.search.candidates_checked =
        (seed_progress.refinements + full_progress.refinements) as u64;
    Ok(result)
}

/// A timeout may retain a validated seed; an internal failure must remain visible.
fn validated_seed_after_stop(
    seed: (
        crate::Selection,
        asap_physical_asap_optimization::PhysicalWorkloadPlan,
        Vec<QueryRoot>,
    ),
    error: SelectionError,
    progress: &DomainProgress,
) -> crate::search::pipeline::PlanSearchResult {
    if progress.budget_stopped {
        Ok(seed)
    } else {
        Err(error)
    }
}

fn search_domain<Id: Clone>(
    initial: LogicalMemo,
    context: &CandidateSearchContext<'_, Id>,
    search: Stage1ToStage3Search,
    local_candidate_generation_ms: Option<f64>,
    archive: &mut CandidateArchive,
    domain: asap_logical_asap_optimization::memo::state_configurations::PaneWidthDomain,
    progress: &mut DomainProgress,
) -> crate::search::pipeline::PlanSearchResult {
    let inventory = context.inventory;
    let entries = context.entries;
    let data = context.data;
    let models = context.models;
    use crate::search::candidates::configurations::{
        enumerate_candidate_configurations_before, ConfigurationError,
    };
    use crate::search::candidates::inventory::{
        build_candidate_inventory_before, CandidateInventoryError,
    };
    use crate::search::costing::calibration::CandidateCalibration;
    use crate::search::refinement::planner::CandidatePlanningContext;
    use crate::search::refinement::search::{CandidateSearchLimits, CandidateSearchStatus};
    use asap_logical_asap_optimization::memo::candidates::try_populate_pass1;
    use asap_logical_asap_optimization::memo::state_groups::{
        expand_concrete_summary_state_groups_before, StateGroupExpansionError,
    };
    use std::time::{Duration, Instant};
    let unsupported = |reason: &str| {
        SelectionError::Solver(format!(
        "candidate-connections: {reason}; no validated plan was returned for the generated finite candidate catalog."
    ))
    };
    let planning_started = Instant::now();
    let total_budget = Duration::from_millis(search.milp_time_limit_ms.unwrap_or(30_000));
    let generation_ms = local_candidate_generation_ms.unwrap_or(0.0);
    if !generation_ms.is_finite() || generation_ms < 0.0 {
        return Err(unsupported("invalid candidate generation duration"));
    }
    let generation_elapsed = Duration::try_from_secs_f64(generation_ms / 1000.0)
        .map_err(|_| unsupported("candidate generation duration exceeds supported range"))?;
    let remaining = || {
        total_budget
            .saturating_sub(generation_elapsed)
            .saturating_sub(planning_started.elapsed())
    };
    let check_budget = |phase: &str, progress: &mut DomainProgress| {
        if remaining().is_zero() {
            progress.budget_stopped = true;
            Err(unsupported(&format!(
                "planning budget exhausted after {phase}; no unvalidated plan is returned"
            )))
        } else {
            Ok(())
        }
    };
    check_budget("local candidate generation", progress)?;
    crate::PlanSelector::new(entries, data, models, false)?;
    let calibration_policy =
        CandidateCalibration::for_models(models).map_err(|error| unsupported(&error))?;
    let roots = inventory
        .roots
        .iter()
        .map(|(_, root)| root.clone())
        .collect::<Vec<_>>();
    let mut output_groups = Vec::new();
    let mut output_queries = Vec::new();
    let mut missing_group = false;
    for (query, root) in roots.iter().enumerate() {
        let mut reference = |node: &Rc<OperatorNode>| {
            if let Some(group) = initial.group_for_node(node) {
                output_groups.push(group);
                output_queries.push(query);
            } else {
                missing_group = true;
            }
            Rc::clone(node)
        };
        match root {
            QueryRoot::Operator(node) => {
                reference(node);
            }
            QueryRoot::Scalar(expression) => {
                expression.map_operator_refs(&mut reference);
            }
        }
    }
    if missing_group {
        return Err(unsupported(
            "an output reference is absent from the initial memo",
        ));
    }
    let started = Instant::now();
    let pruned = crate::search::query_pruning::prune_query_candidates(
        inventory,
        entries,
        data,
        models,
        if search.exhaustive { 0 } else { 64 },
    )
    .map_err(|error| unsupported(&error))?;
    let early_query_pruning_ms = started.elapsed().as_secs_f64() * 1000.0;
    check_budget("early query pruning", progress)?;
    let pruning = pruned.report;
    let early_query_pruning_counts = [
        ("choices_checked".into(), pruning.choices_checked),
        ("choices_removed".into(), pruning.removed.len()),
        ("queries_skipped".into(), pruning.queries_skipped),
        (
            "final_choices_retained".into(),
            pruning.final_choices_retained,
        ),
        (
            "shared_producer_choices_retained".into(),
            pruning.shared_producer_choices_retained,
        ),
        (
            "non_summary_choices_retained".into(),
            pruning.non_summary_choices_retained,
        ),
    ]
    .into_iter()
    .collect();
    let early_query_pruning_examples = pruning
        .removed
        .iter()
        .take(8)
        .map(|choice| {
            format!(
                "target {} choice {}: {:?}: {}",
                choice.target,
                choice.choice,
                choice.cause,
                choice.reason.chars().take(512).collect::<String>()
            )
        })
        .collect();
    let started = Instant::now();
    let mut memo = try_populate_pass1(initial, &pruned.inventory)
        .map_err(|error| unsupported(&error.to_string()))?;
    let deadline = Instant::now().checked_add(remaining());
    asap_logical_asap_optimization::memo::window_candidates::try_add_local_window_composition_in_domain(
        &mut memo, entries, domain, deadline,
    ).map_err(|error| {
        progress.budget_stopped = true;
        unsupported(&error.to_string())
    })?;
    let memo = expand_concrete_summary_state_groups_before(memo, domain, deadline)
        .map_err(|error| {
            progress.budget_stopped =
                matches!(error, StateGroupExpansionError::DeadlineExceeded(_));
            unsupported(&error.to_string())
        })?
        .memo;
    let memo_expansion_ms = started.elapsed().as_secs_f64() * 1000.0;
    check_budget("memo rule expansion", progress)?;
    let groups = memo.groups.len();
    let logical_candidates = memo.candidates.iter().map(Vec::len).sum();
    let started = Instant::now();
    let source = build_candidate_inventory_before(&memo, &output_groups, |_| Ok(0.0), deadline)
        .map_err(|error| {
            progress.budget_stopped = matches!(error, CandidateInventoryError::DeadlineExceeded(_));
            unsupported(&error.to_string())
        })?;
    let immediate_candidates = source.graph.candidates.len();

    let mut configured = enumerate_candidate_configurations_before(
        &source,
        &output_queries,
        entries,
        data,
        models.deployment_capabilities,
        deadline,
    )
    .map_err(|error| {
        progress.budget_stopped = matches!(error, ConfigurationError::DeadlineExceeded(_));
        unsupported(&error.to_string())
    })?;
    let structural_pruning = crate::search::refinement::pruning::prune(&mut configured)
        .map_err(|error| unsupported(&error))?;
    check_budget("physical candidate generation", progress)?;
    let context = CandidatePlanningContext {
        deadline,
        original_roots: &roots,
        entries,
        data,
        models,
        calibration: match calibration_policy {
            CandidateCalibration::Frozen(fit) => fit,
            CandidateCalibration::CompleteCatalog => crate::CalibrationFit::default(),
        },
    };
    crate::search::costing::lower_bounds::apply(&mut configured, &context)
        .map_err(|error| unsupported(&error))?;
    let inventory_construction_ms = started.elapsed().as_secs_f64() * 1000.0;
    progress.catalog_completed = true;
    check_budget("cost bound construction", progress)?;
    let started = Instant::now();
    let limits = CandidateSearchLimits {
        max_refinements: search.max_evaluations.unwrap_or(128),
        wall_time: remaining(),
    };
    let result = if matches!(
        models.plan_selection.cost_model,
        CostModel::ClusterCapacity(_)
    ) || matches!(calibration_policy, CandidateCalibration::CompleteCatalog)
        || search.exhaustive
        || search.keep_every_candidate
    {
        let result =
            crate::search::refinement::objectives::optimize_objective_candidates_with_options(
                &configured,
                &context,
                limits,
                calibration_policy,
                crate::search::refinement::objectives::ObjectiveSearchOptions {
                    exhaustive: search.exhaustive,
                    keep_every_candidate: search.keep_every_candidate,
                },
            )
            .map_err(|error| unsupported(&error))?;
        for (selected, physical) in result.evaluated_plans {
            let logical = selected_logical_roots(&configured, &selected, &roots)
                .map_err(|error| unsupported(&error))?;
            archive
                .logical
                .push(crate::search::pipeline::LogicalCandidate {
                    id: physical.from_logical.clone(),
                    choice: Vec::new(),
                    roots: Ok(logical),
                    pass2: None,
                });
            archive.physical.push(physical);
        }
        for rejected in result.stage2_rejected_assignments {
            let logical = selected_logical_roots(&configured, &rejected.selected_graph, &roots)
                .map_err(|error| unsupported(&error))?;
            archive
                .logical
                .push(crate::search::pipeline::LogicalCandidate {
                    id: rejected.from_logical.clone(),
                    choice: Vec::new(),
                    roots: Ok(logical),
                    pass2: None,
                });
            archive
                .pruned
                .push(crate::search::pipeline::PrunedCandidate {
                    from_logical: rejected.from_logical,
                    reason: rejected.reason,
                });
        }
        archive.logical.sort_by_key(|candidate| {
            candidate
                .id
                .strip_prefix("joint-logical-")
                .and_then(|number| number.parse::<usize>().ok())
                .unwrap_or(usize::MAX)
        });
        progress.refinements = result.refinements;
        if result.status == CandidateSearchStatus::Infeasible
            && result.best.is_none()
            && !result.evaluation_rejections.is_empty()
        {
            progress.infeasible = true;
            return Err(SelectionError::NoValidCandidate(
                result.evaluation_rejections,
            ));
        }
        RouteOutcome {
            independent_components: 1,
            catalog_exhausted: Some(result.catalog_exhausted),
            evaluation_complete: Some(result.evaluation_complete),
            status: result.status,
            refinements: result.refinements,
            metrics: result.solve_metrics,
            validation_seconds: result.validation_seconds,
            query_cuts: result.certified_query_cuts,
            rejections: result.rejections,
            selected: result.selected_graph,
            best: result.best,
            lower_bound: None,
            relative_gap: None,
            calibration: result.calibration,
            calibration_scope: result.calibration_scope,
        }
    } else {
        let result = crate::search::refinement::components::optimize_independent_components(
            &configured,
            &context,
            limits,
        )
        .map_err(|error| unsupported(&error))?;
        RouteOutcome {
            independent_components: result.independent_components,
            catalog_exhausted: None,
            evaluation_complete: None,
            status: result.search.status,
            refinements: result.search.refinements,
            metrics: result.search.solve_metrics,
            validation_seconds: result.search.validation_seconds,
            query_cuts: result.search.certified_query_cuts,
            rejections: result.rejections,
            selected: result.search.incumbent.map(|incumbent| incumbent.selected),
            best: result.best,
            lower_bound: Some(result.search.lower_bound),
            relative_gap: result.search.relative_gap,
            calibration: context.calibration,
            calibration_scope: if models.plan_selection.frozen_calibration_factor.is_some() {
                "provided_frozen"
            } else {
                "not_required"
            },
        }
    };
    let search_and_validation_ms = started.elapsed().as_secs_f64() * 1000.0;
    let status = match &result.status {
        CandidateSearchStatus::Optimal => "optimal".into(),
        CandidateSearchStatus::Infeasible => "infeasible_under_configured_model".into(),
        CandidateSearchStatus::RefinementLimit => "refinement_limit".into(),
        CandidateSearchStatus::TimeLimit => "time_limit".into(),
        CandidateSearchStatus::SolverStopped(reason) => format!("solver_stopped: {reason}"),
        CandidateSearchStatus::EvaluatorStopped(reason) => format!("evaluator_stopped: {reason}"),
        CandidateSearchStatus::NumericalStall => "numerical_stall".into(),
    };
    progress.refinements = result.refinements;
    progress.infeasible = result.status == CandidateSearchStatus::Infeasible;
    progress.budget_stopped = matches!(
        result.status,
        CandidateSearchStatus::TimeLimit | CandidateSearchStatus::RefinementLimit
    );
    let report = crate::CandidateConnectionReport {
        seed_planning_ms: 0.0,
        full_planning_ms: planning_started.elapsed().as_secs_f64() * 1_000.0,
        seed_selected: false,
        full_catalog_completed: domain == asap_logical_asap_optimization::memo::state_configurations::PaneWidthDomain::Complete,
        seed_fallback_reason: None,
        independent_components: result.independent_components,
        catalog_exhausted: result.catalog_exhausted,
        evaluation_complete: result.evaluation_complete,
        catalog_scope: "Pass 1, local panes, shared summary/pane states, exact and sketch rollups, compatible TopK ranking reuse, and scan widening; finite capacity, pane-width, horizon, materialization and execution-scope configurations".into(),
        status, groups, logical_candidates, immediate_candidates,
        physical_configurations: configured.configurations.len(),
        compatible_connections: configured.inventory.graph.candidates.iter().flat_map(|candidate| &candidate.producer_requirements).map(|input| input.compatible_producers.len()).sum(),
        output_references: output_groups.len(), refinements: result.refinements,
        max_refinements: search.max_evaluations.unwrap_or(128), time_limit_ms: search.milp_time_limit_ms.unwrap_or(30_000),
        lower_bound: result.lower_bound, relative_gap: result.relative_gap,
        objective: match &models.plan_selection.cost_model {
            CostModel::ClusterCapacity(settings) => format!("{:?}", settings.objective),
            _ => "monetary_cost".into(),
        },
        calibration_scope: result.calibration_scope.into(), calibration_factor: result.calibration.factor(),
        memo_expansion_ms, local_candidate_generation_ms, inventory_construction_ms, search_and_validation_ms,
        model_construction_ms: result.metrics.model_construction_seconds * 1000.0,
        solver_ms: result.metrics.solver_seconds * 1000.0,
        validation_ms: result.validation_seconds * 1000.0,
        solve_calls: result.metrics.solve_calls,
        peak_milp_variables: result.metrics.peak_variables,
        peak_milp_constraints: result.metrics.peak_constraints,
        validation_rejection_counts: result.rejections.counts,
        certified_query_cuts: result.query_cuts,
        early_query_pruning_ms, early_query_pruning_counts, early_query_pruning_examples,
        structurally_removed_candidates: structural_pruning.removed_candidates,
        structurally_removed_connections: structural_pruning.removed_connections,
        validation_rejection_examples: result.rejections.examples,
    };
    progress.report = Some(report.clone());
    let Some((mut selection, physical)) = result.best else {
        let metrics =
            serde_json::to_string(&report).map_err(|error| unsupported(&error.to_string()))?;
        let reason = match result.status {
            CandidateSearchStatus::Infeasible => {
                "generated candidate catalog is infeasible under the configured model"
            }
            CandidateSearchStatus::TimeLimit | CandidateSearchStatus::RefinementLimit => {
                "planning budget ended before a validated incumbent was found"
            }
            _ => "search stopped without a validated incumbent; inspect the termination diagnostic",
        };
        return Err(unsupported(&format!("{reason}. Search report: {metrics}")));
    };
    let selected = result
        .selected
        .as_ref()
        .ok_or_else(|| unsupported("validated plan has no selected graph"))?;
    let logical = selected_logical_roots(&configured, selected, &roots)
        .map_err(|error| unsupported(&error))?;
    selection.search.candidates_checked = result.refinements as u64;
    selection.search.candidate_connections = Some(report);
    Ok((selection, physical, logical))
}

fn selected_logical_roots(
    configured: &crate::search::candidates::configurations::CandidateConfigurationInventory,
    selected: &crate::search::candidates::graph::SelectedCandidateGraph,
    roots: &[QueryRoot],
) -> Result<Vec<QueryRoot>, String> {
    let outputs = crate::search::candidates::reconstruction::reconstruct_selected_candidates(
        &configured.inventory.graph,
        selected,
        &configured.inventory.templates,
    )?
    .outputs;
    let mut next = outputs.iter();
    let logical = roots
        .iter()
        .map(|root| match root {
            QueryRoot::Operator(_) => {
                QueryRoot::Operator(Rc::clone(next.next().expect("validated output mapping")))
            }
            QueryRoot::Scalar(expression) => {
                QueryRoot::Scalar(expression.map_operator_refs(&mut |_| {
                    Rc::clone(next.next().expect("validated output mapping"))
                }))
            }
        })
        .collect();
    Ok(logical)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{run_stage1_to_stage3_with, SearchAlgorithm};
    use asap_logical_asap_optimization::test_support::{entry_with, lower_promql};
    use asap_types::ir::properties::AccuracyTarget;
    use asap_types::workload::{
        DataArrival, Predictability, QueryRecurrence, RepeatedDemand, RepetitionInterval,
        SourceFacts,
    };

    // Earlier candidate generation consumes the same budget as solver work.
    #[test]
    fn generation_time_is_charged_before_search_starts() {
        let (roots, entries, data) = workload();
        let initial = LogicalMemo::from_roots(&roots);
        let inventory = asap_logical_asap_optimization::per_computation_candidates::generate_per_computation_candidates(roots).unwrap();
        let error = candidate_connection_search(
            initial,
            &CandidateSearchContext {
                inventory: &inventory,
                entries: &entries,
                data: &data,
                models: PlanningModels::builtin(),
            },
            Stage1ToStage3Search {
                search: SearchAlgorithm::CandidateConnections,
                milp_time_limit_ms: Some(1),
                ..Default::default()
            },
            Some(2.0),
            &mut CandidateArchive::default(),
        )
        .unwrap_err();
        assert!(error
            .to_string()
            .contains("planning budget exhausted after local candidate generation"));
    }

    // Only an explicitly identified budget stop can use an already validated plan.
    #[test]
    fn validated_seed_survives_budget_stop_but_not_internal_failure() {
        use asap_logical_asap_optimization::memo::state_configurations::PaneWidthDomain;
        let (_, entries, data) = workload();
        let roots = vec![(
            0,
            QueryRoot::Scalar(asap_types::ir::ScalarExpr::Literal(
                asap_types::ir::scalar::ScalarValue::Float64(1.0),
            )),
        )];
        let initial = LogicalMemo::from_roots(&roots);
        let inventory = asap_logical_asap_optimization::per_computation_candidates::generate_per_computation_candidates(roots).unwrap();
        let seed = search_domain(
            initial.clone(),
            &CandidateSearchContext {
                inventory: &inventory,
                entries: &entries[..1],
                data: &data,
                models: PlanningModels::builtin(),
            },
            Stage1ToStage3Search::default(),
            Some(0.0),
            &mut CandidateArchive::default(),
            PaneWidthDomain::SeedGrids,
            &mut DomainProgress::default(),
        )
        .unwrap();
        let selected = seed.0.selected.clone();
        let mut full_progress = DomainProgress::default();
        let error = search_domain(
            initial,
            &CandidateSearchContext {
                inventory: &inventory,
                entries: &entries[..1],
                data: &data,
                models: PlanningModels::builtin(),
            },
            Stage1ToStage3Search {
                milp_time_limit_ms: Some(0),
                ..Default::default()
            },
            Some(0.0),
            &mut CandidateArchive::default(),
            PaneWidthDomain::Complete,
            &mut full_progress,
        )
        .unwrap_err();
        assert!(full_progress.budget_stopped);
        assert!(!full_progress.catalog_completed);
        let kept = validated_seed_after_stop(seed, error, &full_progress).unwrap();
        assert_eq!(kept.0.selected, selected);
        kept.1.dag.validate().unwrap();
        let error = validated_seed_after_stop(
            kept,
            SelectionError::Solver("internal reconstruction failure".into()),
            &DomainProgress::default(),
        )
        .unwrap_err();
        assert!(error
            .to_string()
            .contains("internal reconstruction failure"));
    }

    // Invalid upstream timing cannot silently become a fresh seed budget.
    #[test]
    fn invalid_generation_durations_are_rejected_before_seed_search() {
        let (roots, entries, data) = workload();
        let initial = LogicalMemo::from_roots(&roots);
        let inventory = asap_logical_asap_optimization::per_computation_candidates::generate_per_computation_candidates(roots).unwrap();
        for invalid in [-1.0, f64::INFINITY, f64::NAN] {
            let error = candidate_connection_search(
                initial.clone(),
                &CandidateSearchContext {
                    inventory: &inventory,
                    entries: &entries,
                    data: &data,
                    models: PlanningModels::builtin(),
                },
                Stage1ToStage3Search::default(),
                Some(invalid),
                &mut CandidateArchive::default(),
            )
            .unwrap_err();
            assert!(error
                .to_string()
                .contains("invalid candidate generation duration"));
        }
    }

    // Calibration over all assignments must never be fitted to a restricted seed.
    #[test]
    fn complete_catalog_calibration_skips_seed_domain() {
        let settings: crate::PlanSelectionSettings = serde_json::from_value(serde_json::json!({
            "measured_operation_costs": {"operators": [{"operator":"Scan", "ns_per_unit":100.0}]}
        }))
        .unwrap();
        let run = scalar_run(&settings, Stage1ToStage3Search::default());
        let selection = run.stage3_plan_selection.unwrap();
        let report = selection.search.candidate_connections.unwrap();
        assert_eq!(report.seed_planning_ms, 0.0);
        assert!(!report.seed_selected);
        assert!(report.full_catalog_completed);
        assert_eq!(report.calibration_scope, "complete_catalog");
        assert_eq!(report.catalog_exhausted, Some(true));
    }

    // Joint phases retain valid frontier metadata under the exact configured ranking.
    #[test]
    fn seed_and_complete_pareto_fronts_keep_distinct_ids_and_frozen_fit() {
        let settings = crate::PlanSelectionSettings {
            frozen_calibration_factor: Some(3.0),
            cost_model: CostModel::ClusterCapacity(crate::ClusterLimits {
                memory_bytes: 1_000_000,
                disk_bytes: 1_000_000,
                cpu_cores: Some(8),
                objective: crate::ClusterObjective::ParetoFront,
            }),
            ..Default::default()
        };
        let run = scalar_run(&settings, Stage1ToStage3Search::default());
        let selection = run.stage3_plan_selection.unwrap();
        let report = selection.search.candidate_connections.as_ref().unwrap();
        assert!(report.seed_planning_ms > 0.0);
        assert!(report.full_catalog_completed);
        assert_eq!(report.calibration_factor, 3.0);
        assert!(selection.front.contains(&selection.selected));
        assert!(selection.front.iter().any(|id| id.starts_with("seed-")));
        for id in &selection.front {
            assert!(selection.costs.contains_key(id));
            assert!(selection.accepted.contains_key(id));
        }
        let bounded = scalar_run(
            &settings,
            Stage1ToStage3Search {
                max_evaluations: Some(1),
                ..Default::default()
            },
        );
        assert_eq!(
            bounded
                .stage3_plan_selection
                .unwrap()
                .search
                .candidate_connections
                .unwrap()
                .seed_planning_ms,
            0.0
        );
    }

    fn scalar_run(
        settings: &crate::PlanSelectionSettings,
        search: Stage1ToStage3Search,
    ) -> crate::Stage1ToStage3Run<usize> {
        let (_, entries, data) = workload();
        let roots = vec![(
            0,
            QueryRoot::Scalar(asap_types::ir::ScalarExpr::Literal(
                asap_types::ir::scalar::ScalarValue::Float64(1.0),
            )),
        )];
        run_stage1_to_stage3_with(
            roots,
            &entries[..1],
            &data,
            PlanningModels {
                plan_selection: settings,
                ..PlanningModels::builtin()
            },
            search,
        )
        .unwrap()
    }

    fn workload() -> (
        Vec<(usize, QueryRoot)>,
        Vec<QueryWorkloadEntry>,
        DataWorkload,
    ) {
        let target = AccuracyTarget::max_error(0.01).failure_probability(0.001);
        let roots = [0.95, 0.99]
            .iter()
            .enumerate()
            .map(|(id, quantile)| {
                (
                    id,
                    QueryRoot::Operator(lower_promql(
                        &format!("quantile_over_time({quantile}, latency[5m])"),
                        target.clone(),
                    )),
                )
            })
            .collect();
        let entries = (0..2)
            .map(|_| {
                let mut entry = entry_with(
                    target.clone(),
                    QueryRecurrence::Repeated(RepeatedDemand::FixedInterval(RepetitionInterval(
                        300_000,
                    ))),
                );
                entry.predictability = Predictability::Predictable { known_at: None };
                entry
            })
            .collect();
        let data = DataWorkload {
            arrival: DataArrival::Ingesting {
                rows_per_second: 1000.0,
            },
            sources: [(
                "latency".into(),
                SourceFacts {
                    series_count: Some(1),
                    ..Default::default()
                },
            )]
            .into(),
        };
        (roots, entries, data)
    }

    // The standard stage driver exposes the new route without misreporting its catalog.
    #[test]
    fn candidate_connections_driver_returns_validated_plan_and_scope_report() {
        let (roots, entries, data) = workload();
        let run = run_stage1_to_stage3_with(
            roots,
            &entries,
            &data,
            PlanningModels::builtin(),
            Stage1ToStage3Search {
                milp_time_limit_ms: Some(10_000),
                ..Stage1ToStage3Search::with_algorithm(SearchAlgorithm::CandidateConnections)
            },
        )
        .unwrap();
        let selection = run.stage3_plan_selection.unwrap();
        assert_eq!(
            selection.search.algorithm,
            SearchAlgorithm::CandidateConnections
        );
        let report = selection.search.candidate_connections.unwrap();
        assert!(report.catalog_scope.contains("finite capacity"));
        assert!(report.physical_configurations >= report.immediate_candidates);
        assert!(report.refinements > 0);
        assert!(report.solve_calls > 0);
        assert!(report.peak_milp_variables > 0);
        assert!(report.peak_milp_constraints > 0);
        assert_eq!(report.output_references, 2);
        run.stage3_selected.unwrap().dag.validate().unwrap();
        assert!(run.stage1_selected.is_some());
    }

    // Pane rewriting must preserve canonical inputs so equal chains can share state.
    #[test]
    fn local_pane_rules_expose_identical_chains_to_state_groups() {
        use asap_logical_asap_optimization::memo::candidates::try_populate_pass1;
        use asap_logical_asap_optimization::memo::state_groups::expand_concrete_summary_state_groups;
        use asap_logical_asap_optimization::per_computation_candidates::generate_per_computation_candidates;
        let (roots, mut entries, _) = workload();
        for entry in &mut entries {
            entry.recurrence =
                QueryRecurrence::Repeated(RepeatedDemand::FixedInterval(RepetitionInterval(1_000)));
        }
        let initial = LogicalMemo::from_roots(&roots);
        let generated = generate_per_computation_candidates(roots).unwrap();
        let mut memo = try_populate_pass1(initial, &generated).unwrap();
        asap_logical_asap_optimization::memo::window_candidates::add_local_window_composition(
            &mut memo, &entries,
        );
        let expanded = expand_concrete_summary_state_groups(memo).unwrap();
        assert!(
            expanded.state_groups.iter().any(|group| {
                let state = &expanded.memo.groups[group.0 as usize];
                matches!(
                    state.node.asap(),
                    Some(asap_types::ir::ASAPOp::PaneChain { .. })
                ) && state.queries == vec![0, 1]
            }),
            "identical panes must expose a common state producer"
        );
    }

    // Sliding evaluations without retained raw data need a maintained pane chain.
    #[test]
    fn candidate_connections_uses_panes_for_frequent_window_evaluations() {
        let (mut roots, mut entries, data) = workload();
        roots.truncate(1);
        entries.truncate(1);
        entries[0].recurrence =
            QueryRecurrence::Repeated(RepeatedDemand::FixedInterval(RepetitionInterval(1_000)));
        let deployment = asap_logical_asap_optimization::DeploymentCapabilities {
            raw_data_retained: false,
            query_time_results_retained: false,
            ..Default::default()
        };
        let models = PlanningModels {
            deployment_capabilities: &deployment,
            ..PlanningModels::builtin()
        };
        let run = run_stage1_to_stage3_with(
            roots,
            &entries,
            &data,
            models,
            Stage1ToStage3Search {
                milp_time_limit_ms: Some(10_000),
                ..Stage1ToStage3Search::with_algorithm(SearchAlgorithm::CandidateConnections)
            },
        )
        .unwrap();
        let selected = run.stage3_plan_selection.unwrap();
        assert!(selected
            .search
            .candidate_connections
            .as_ref()
            .unwrap()
            .local_candidate_generation_ms
            .is_some());
        let physical = run.stage3_selected.unwrap();
        assert!(physical
            .roots
            .iter()
            .flat_map(|root| match root {
                QueryRoot::Operator(node) => OperatorNode::reachable(node),
                QueryRoot::Scalar(_) => Vec::new(),
            })
            .any(|node| matches!(node.asap(), Some(asap_types::ir::ASAPOp::PaneChain { .. }))));
    }

    // Exhaustive diagnostics retain logical and physical plans on this route.
    #[test]
    fn candidate_connections_exports_exhaustive_archive() {
        let (_, entries, data) = workload();
        let roots = vec![(
            0,
            QueryRoot::Scalar(asap_types::ir::ScalarExpr::Literal(
                asap_types::ir::scalar::ScalarValue::Float64(1.0),
            )),
        )];
        let run = run_stage1_to_stage3_with(
            roots,
            &entries[..1],
            &data,
            PlanningModels::builtin(),
            Stage1ToStage3Search {
                exhaustive: true,
                keep_every_candidate: true,
                ..Stage1ToStage3Search::with_algorithm(SearchAlgorithm::CandidateConnections)
            },
        )
        .unwrap();
        let selection = run.stage3_plan_selection.unwrap();
        assert_eq!(
            selection.search.algorithm,
            SearchAlgorithm::CandidateConnections
        );
        assert_eq!(run.stage1_logical_asap_optimization.len(), 1);
        assert_eq!(run.stage2_physical_asap_optimization.len(), 1);
        assert_eq!(
            run.stage2_physical_asap_optimization[0].from_logical,
            run.stage1_logical_asap_optimization[0].id
        );
        assert_eq!(
            selection.search.candidate_connections.unwrap().status,
            "optimal"
        );
    }
}

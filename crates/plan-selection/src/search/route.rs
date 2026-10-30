//! Driver integration for the explicit, bounded candidate-connection search.
use crate::{PlanningModels, SelectionError, Stage1ToStage3Search};
use asap_cost::CostModel;
use asap_logical_asap_optimization::pass1::per_computation_candidates::PerComputationCandidates;
use asap_logical_asap_optimization::pass2::memo::enumeration::ChosenCandidatePerMemoGroup;
use asap_logical_asap_optimization::pass2::memo::DagOfLogicalMemoGroups;
use asap_types::ir::QueryRoot;
use asap_types::stages::CandidateMemo;
use asap_types::stages::MemoGroupId;
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
    /// The memo steps of the round whose plan was selected, else the last round's.
    pub memo_steps: Option<crate::search::pipeline::MemoSteps>,
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
    selected: Option<asap_types::stages::SelectedCandidateGraph>,
    best: Option<(
        crate::Selection,
        asap_physical_asap_optimization::PhysicalWorkloadPlan,
    )>,
    lower_bound: Option<f64>,
    relative_gap: Option<f64>,
    calibration: asap_cost::CalibrationFit,
    calibration_scope: &'static str,
}

#[derive(Default)]
struct DomainProgress {
    budget_stopped: bool,
    infeasible: bool,
    catalog_completed: bool,
    refinements: usize,
    report: Option<crate::CandidateConnectionReport>,
    /// Pass 2's state between pane-width rounds; `None` before the first
    /// round and after a round whose local window composition timed out.
    pass2_round:
        Option<asap_logical_asap_optimization::pass2::logical_memo_candidates::Pass2RoundState>,
    /// The memo after Stage 0 and after Pass 1, and Pass 1's pruned
    /// candidates, kept with `pass2_round`.
    pass1_steps: Option<(
        asap_types::stages::CandidateMemo,
        asap_types::stages::CandidateMemo,
        Vec<asap_types::stages::PrunedCandidate<asap_logical_asap_optimization::pass1::per_computation_candidates::HowToComputeAggregateMeasure>>,
    )>,
    /// This round's memo steps, with its selected plan's ID when it has one.
    memo_steps: Option<(Option<String>, crate::search::pipeline::MemoSteps)>,
}

/// Obtain an incumbent on seed grids, then progressively expose finer widths.
/// All rounds use the same planner and frozen objective; only a completed
/// full-domain round can establish a bound for the declared search space.
pub(crate) fn candidate_connection_search<Id: Clone>(
    initial: DagOfLogicalMemoGroups,
    context: &CandidateSearchContext<'_, Id>,
    search: Stage1ToStage3Search,
    local_candidate_generation_ms: Option<f64>,
    archive: &mut CandidateArchive,
) -> crate::search::pipeline::PlanSearchResult {
    let models = context.models;
    use crate::search::costing::calibration::CandidateCalibration;
    use asap_logical_asap_optimization::pass2::memo::shared_configurations::PaneWidthDomain;
    use std::time::Instant;

    let started = Instant::now();
    let total_ms = search.milp_time_limit_ms.unwrap_or(30_000);
    let prior_ms = local_candidate_generation_ms.unwrap_or(0.0);
    let calibration = CandidateCalibration::for_models(models).map_err(SelectionError::Solver)?;
    // Pane rules require a temporal range below a summary build. Pass 1 may
    // introduce the build, but cannot invent a missing positive Range window.
    // Instant lookback selectors are not splittable panes. Existing pane
    // chains also retain the width domain when callers provide lowered state.
    // This conservative gate keeps SQL/PromQL windows regardless of arrival
    // mode; recurrence alone does not make a window-free workload pane-capable.
    let has_pane_window = initial.groups.iter().any(|group| {
        matches!(
            group.logical_node.non_asap(),
            Some(asap_types::ir::NonASAPOp::TimeRange {
                kind: asap_types::ir::TimeRangeKind::Range, range, ..
            }) if !range.is_zero()
        ) || matches!(
            group.logical_node.asap(),
            Some(asap_types::ir::ASAPOp::PaneChain { .. })
        )
    });
    // Exhaustive archives and complete-catalog calibration require one domain.
    let seed_enabled = has_pane_window
        && !search.exhaustive
        && !search.keep_every_candidate
        && matches!(calibration, CandidateCalibration::Frozen(_))
        && prior_ms.is_finite()
        && prior_ms >= 0.0
        && total_ms as f64 - prior_ms >= 1_000.0
        && search.max_evaluations.unwrap_or(128) >= 4;
    if !seed_enabled {
        let mut progress = DomainProgress::default();
        let result = search_domain(
            initial,
            context,
            search,
            local_candidate_generation_ms,
            archive,
            PaneWidthDomain::Complete,
            &mut progress,
        );
        archive.memo_steps = progress.memo_steps.map(|(_, steps)| steps);
        return result;
    }
    // Each round's memo steps, with its selected plan's namespaced ID; the
    // archive keeps the last round's for a search that ends without a plan.
    let mut memo_rounds = Vec::new();
    let mut record_round =
        |archive: &mut CandidateArchive, progress: &mut DomainProgress, namespace: &str| {
            if let Some((plan, steps)) = progress.memo_steps.take() {
                archive.memo_steps = Some(steps.clone());
                memo_rounds.push((plan.map(|id| format!("{namespace}{id}")), steps));
            }
        };
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
    )
    .map(|plan| namespace_round(plan, "seed-"));
    record_round(archive, &mut seed_progress, "seed-");
    let seed_elapsed = started.elapsed();
    if seed.is_err() && !seed_progress.budget_stopped && !seed_progress.infeasible {
        return seed;
    }
    let seed_report = seed_progress.report.clone();
    let full_started = Instant::now();
    let mut full_progress = DomainProgress {
        pass2_round: seed_progress.pass2_round.take(),
        pass1_steps: seed_progress.pass1_steps.take(),
        ..Default::default()
    };
    let global_deadline = started.checked_add(
        std::time::Duration::from_millis(total_ms)
            .saturating_sub(std::time::Duration::from_secs_f64(prior_ms / 1000.0)),
    );
    // A literal-only workload has no computation in which pane rules can fire.
    let grids = context
        .entries
        .iter()
        .filter_map(|entry| entry.evaluation_grid_ms())
        .filter(|_| !initial.groups.is_empty());
    let mut frontier =
        asap_logical_asap_optimization::pass2::memo::shared_configurations::PaneWidthFrontier::new(
            grids,
            global_deadline,
        );
    let mut rounds = vec!["SeedGrids".to_owned()];
    let mut full = Err(SelectionError::Solver(
        "pane-width expansion has not started".into(),
    ));
    let mut completed_full_domain = false;
    loop {
        let elapsed_ms = prior_ms + started.elapsed().as_secs_f64() * 1000.0;
        let remaining_ms = total_ms.saturating_sub(elapsed_ms.ceil() as u64);
        let remaining_evaluations = search
            .max_evaluations
            .unwrap_or(128)
            .saturating_sub(seed_progress.refinements + full_progress.refinements);
        if remaining_ms == 0 || remaining_evaluations == 0 {
            break;
        }
        let domain = match &mut frontier {
            Ok(frontier) => match frontier.next_domain(global_deadline) {
                Ok(Some(domain)) => domain,
                Ok(None) => break,
                Err(error) => {
                    if full.is_err() {
                        full = Err(SelectionError::Solver(error.to_string()));
                    }
                    break;
                }
            },
            Err(error) => {
                if full.is_err() {
                    full = Err(SelectionError::Solver(error.to_string()));
                }
                break;
            }
        };
        // Frontier enumeration is part of the same global budget. Never pass
        // the pre-enumeration remainder as a fresh per-round allowance.
        let remaining_ms = total_ms
            .saturating_sub((prior_ms + started.elapsed().as_secs_f64() * 1000.0).ceil() as u64);
        if remaining_ms == 0 {
            break;
        }
        rounds.push(format!("{domain:?}"));
        let complete = domain == PaneWidthDomain::Complete;
        let mut round_progress = DomainProgress {
            pass2_round: full_progress.pass2_round.take(),
            pass1_steps: full_progress.pass1_steps.take(),
            ..Default::default()
        };
        let round = search_domain(
            initial.clone(),
            context,
            Stage1ToStage3Search {
                milp_time_limit_ms: Some(if complete {
                    remaining_ms
                } else {
                    remaining_ms.min(5_000)
                }),
                max_evaluations: Some(if complete {
                    remaining_evaluations
                } else {
                    remaining_evaluations.min(16)
                }),
                ..search
            },
            Some(0.0),
            archive,
            domain,
            &mut round_progress,
        )
        .map(|plan| namespace_round(plan, &format!("pane-round-{}-", rounds.len() - 1)));
        record_round(
            archive,
            &mut round_progress,
            &format!("pane-round-{}-", rounds.len() - 1),
        );
        full_progress.pass2_round = round_progress.pass2_round.take();
        full_progress.pass1_steps = round_progress.pass1_steps.take();
        full_progress.refinements += round_progress.refinements;
        completed_full_domain = complete && round_progress.catalog_completed;
        full_progress.catalog_completed = completed_full_domain;
        full_progress.budget_stopped = round_progress.budget_stopped;
        full_progress.infeasible = complete && round_progress.infeasible;
        if let Some(mut report) = round_progress.report.take() {
            if let Some(previous) = full_progress.report.take() {
                add_phase_metrics(&mut report, &previous);
            }
            full_progress.report = Some(report);
        }
        full = match (full, round) {
            (Ok(previous), Ok(next)) => Ok(merge_validated_rounds(previous, next, models).1),
            (_, Ok(next)) => Ok(next),
            (previous, Err(error)) => {
                if complete && round_progress.infeasible {
                    // The final catalog is infeasible, not a reason to return
                    // an earlier plan. Preserve prior pruning diagnostics in
                    // the aggregated failure report below.
                    full = Err(error);
                    break;
                }
                if !round_progress.budget_stopped && !round_progress.infeasible {
                    return Err(error);
                }
                match previous {
                    Ok(previous) => Ok(previous),
                    Err(_) => Err(error),
                }
            }
        };
        if complete {
            break;
        }
        // A timed-out rule pass leaves no reusable partial memo; stop rather
        // than repeating all generation and pretending the domain expanded.
        if full_progress.pass2_round.is_none() {
            break;
        }
    }
    full_progress.catalog_completed = completed_full_domain;
    if !completed_full_domain {
        full_progress.budget_stopped = true;
        if full_progress.report.is_none() {
            full_progress.report = Some(crate::CandidateConnectionReport {
                status: "time_limit".into(),
                pane_width_rounds: rounds.clone(),
                pane_widths_pending: true,
                catalog_scope: "pane-width expansion interrupted before catalog completion".into(),
                time_limit_ms: total_ms,
                max_refinements: search.max_evaluations.unwrap_or(128),
                ..Default::default()
            });
        }
    }
    if !completed_full_domain {
        if let Some(report) = &mut full_progress.report {
            report.lower_bound = None;
            report.relative_gap = None;
            report.catalog_exhausted = Some(false);
            report.evaluation_complete = Some(false);
            report.full_catalog_completed = false;
            report.pane_widths_pending = true;
            report.status = "pane_width_domain_incomplete".into();
        }
    }
    if let Ok((selection, _, _)) = &mut full {
        if let Some(report) = &full_progress.report {
            selection.search.candidate_connections = Some(report.clone());
        }
    }
    let full_planning_ms = full_started.elapsed().as_secs_f64() * 1_000.0;
    let mut seed_selected = false;
    let mut fallback_reason = None;
    let mut result = match (seed, full) {
        (
            Ok((seed_selection, seed_plan, seed_logical)),
            Ok((full_selection, full_plan, full_logical)),
        ) => {
            let merged = merge_validated_rounds(
                (seed_selection, seed_plan, seed_logical),
                (full_selection, full_plan, full_logical),
                models,
            );
            seed_selected = merged.0;
            merged.1
        }

        (Ok(seed), Err(error)) => {
            // The outer report owns phase accounting; do not embed a stale
            // standalone complete-phase report inside its fallback reason.
            fallback_reason = Some(
                error
                    .to_string()
                    .split(" Search report: ")
                    .next()
                    .unwrap_or_default()
                    .to_owned(),
            );
            let seed = validated_seed_after_stop(seed, error, &full_progress)?;
            seed_selected = true;
            seed
        }
        (_, Err(error)) => {
            if full_progress.budget_stopped || full_progress.infeasible {
                if let Some(mut report) = full_progress.report.take() {
                    if let Some(seed) = &seed_report {
                        add_phase_metrics(&mut report, seed);
                    }
                    report.seed_planning_ms = seed_elapsed.as_secs_f64() * 1_000.0;
                    report.full_planning_ms = full_planning_ms;
                    report.local_candidate_generation_ms = local_candidate_generation_ms;
                    report.time_limit_ms = total_ms;
                    report.max_refinements = search.max_evaluations.unwrap_or(128);
                    report.full_catalog_completed = full_progress.catalog_completed;
                    report.pane_width_rounds = rounds.clone();
                    report.pane_widths_pending = !full_progress.catalog_completed;
                    let metrics = serde_json::to_string(&report)
                        .map_err(|error| SelectionError::Solver(error.to_string()))?;
                    let reason = if full_progress.infeasible {
                        "complete candidate catalog is infeasible under the configured model"
                    } else {
                        "planning budget ended before either seed or complete search found a validated incumbent"
                    };
                    return Err(SelectionError::Solver(format!(
                        "candidate-connections: {reason}. Search report: {metrics}"
                    )));
                }
            }
            return Err(error);
        }
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
            if let Some(seed) = &seed_report {
                add_phase_metrics(report, seed);
            }
        }
        report.full_planning_ms = full_planning_ms;
        report.seed_planning_ms = seed_elapsed.as_secs_f64() * 1_000.0;
        report.seed_selected = seed_selected;
        report.full_catalog_completed = full_progress.catalog_completed;
        report.pane_width_rounds = rounds.clone();
        report.pane_widths_pending = !full_progress.catalog_completed;
        report.seed_fallback_reason = fallback_reason;
        report.time_limit_ms = total_ms;
        report.local_candidate_generation_ms = local_candidate_generation_ms;
        report.max_refinements = search.max_evaluations.unwrap_or(128);
        if report.seed_fallback_reason.is_some() || !full_progress.catalog_completed {
            // The restricted seed's optimum is not a certificate for all widths.
            report.status = "time_or_evaluation_limit_with_validated_incumbent".into();
            report.lower_bound = None;
            report.relative_gap = None;
            report.catalog_exhausted = Some(false);
            report.evaluation_complete = Some(false);
        } else {
            // The selected incumbent may come from any earlier round. Recompute
            // its gap against the complete-domain bound, not the last proposal.
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
    if let (Some(report), Some(cost)) = (
        result.0.search.candidate_connections.as_mut(),
        result.0.search.incumbent_cost,
    ) {
        invalidate_contradictory_bound(report, cost);
    }
    result.0.search.candidates_checked =
        (seed_progress.refinements + full_progress.refinements) as u64;
    // A record only: a plan whose round recorded no memo keeps the last one,
    // marked as not the selected plan's.
    match memo_rounds
        .into_iter()
        .find(|(plan, _)| plan.as_ref() == Some(&result.1.id))
    {
        Some((_, steps)) => archive.memo_steps = Some(steps),
        None => {
            if let Some(steps) = &mut archive.memo_steps {
                steps.selected_chosen_candidates =
                    Err("the selected plan's planning round is not recorded".into());
            }
        }
    }
    Ok(result)
}

type ValidatedRound = (
    crate::Selection,
    asap_physical_asap_optimization::PhysicalWorkloadPlan,
    Vec<QueryRoot>,
);

/// Assign a readable phase identity once, before combining validated results.
fn namespace_round(mut plan: ValidatedRound, namespace: &str) -> ValidatedRound {
    let (selection, physical, _) = &mut plan;
    physical.id = format!("{namespace}{}", physical.id);
    physical.from_logical = format!("{namespace}{}", physical.from_logical);
    selection.selected = format!("{namespace}{}", selection.selected);
    selection.costs = std::mem::take(&mut selection.costs)
        .into_iter()
        .map(|(id, value)| (format!("{namespace}{id}"), value))
        .collect();
    selection.accepted = std::mem::take(&mut selection.accepted)
        .into_iter()
        .map(|(id, value)| (format!("{namespace}{id}"), value))
        .collect();
    for id in &mut selection.front {
        *id = format!("{namespace}{id}");
    }
    for rejection in &mut selection.rejected {
        rejection.id = format!("{namespace}{}", rejection.id);
    }
    plan
}

fn merge_validated_rounds(
    seed: ValidatedRound,
    full: ValidatedRound,
    models: PlanningModels<'_>,
) -> (bool, ValidatedRound) {
    let (full_selection, full_plan, full_logical) = full;
    let identifiers =
        |selection: &crate::Selection,
         plan: &asap_physical_asap_optimization::PhysicalWorkloadPlan| {
            selection
                .costs
                .keys()
                .chain(selection.accepted.keys())
                .chain(selection.front.iter())
                .chain(selection.rejected.iter().map(|rejection| &rejection.id))
                .chain([&selection.selected, &plan.id, &plan.from_logical])
                .cloned()
                .collect::<std::collections::HashSet<_>>()
        };
    let occupied = identifiers(&full_selection, &full_plan);
    let earlier_ids = identifiers(&seed.0, &seed.1);
    // Production phases already have disjoint namespaces. Retain a defensive
    // fallback for standalone callers combining unnamespaced results.
    let seed = if earlier_ids.is_disjoint(&occupied) {
        seed
    } else {
        let mut index = 1;
        loop {
            let namespace = format!("merged-round-{index}-");
            if earlier_ids
                .iter()
                .all(|id| !occupied.contains(&format!("{namespace}{id}")))
            {
                break namespace_round(seed, &namespace);
            }
            index += 1;
        }
    };
    let (seed_selection, seed_plan, seed_logical) = seed;
    let ranking = crate::Ranking::of(&models.plan_selection.cost_model);
    // Both plans were validated under the same frozen calibration. Use
    // PlanSelector's ranking without another unbudgeted evaluator call.
    let seed_selected = ranking.better(
        &seed_selection.costs[&seed_selection.selected],
        &full_selection.costs[&full_selection.selected],
    );
    let full_search = full_selection.search.clone();
    let (mut selection, other, physical, logical) = if seed_selected {
        (seed_selection, full_selection, seed_plan, seed_logical)
    } else {
        (full_selection, seed_selection, full_plan, full_logical)
    };
    // Preserve the validated Pareto front from both searches.
    selection.costs.extend(other.costs);
    selection.accepted.extend(other.accepted);
    selection.rejected.extend(other.rejected);
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
    (seed_selected, (selection, physical, logical))
}

/// An independently validated plan is a witness against a larger global lower
/// bound. Keep the executable plan, but expose the inconsistent certificate
/// instead of silently clamping it or claiming optimality.
fn invalidate_contradictory_bound(report: &mut crate::CandidateConnectionReport, cost: f64) {
    if report
        .lower_bound
        .is_some_and(|bound| crate::cheaper(cost, bound))
    {
        report.status = "invalid_cost_certificate".into();
        report.seed_fallback_reason = Some(format!(
            "full-domain lower bound {:?} exceeds independently validated cost {cost}",
            report.lower_bound,
        ));
        report.lower_bound = None;
        report.relative_gap = None;
        report.catalog_exhausted = Some(false);
        report.evaluation_complete = Some(false);
    }
}

fn add_phase_metrics(
    report: &mut crate::CandidateConnectionReport,
    phase: &crate::CandidateConnectionReport,
) {
    // All progressive rounds use one configured objective and frozen model.
    // Preparation-only reports may lack this metadata without lacking an
    // inventory snapshot (or vice versa); inherit it independently of counts.
    if report.objective.is_empty() && !phase.objective.is_empty() {
        report.objective = phase.objective.clone();
    }
    if report.calibration_scope.is_empty() && !phase.calibration_scope.is_empty() {
        report.calibration_scope = phase.calibration_scope.clone();
        report.calibration_factor = phase.calibration_factor;
    }
    // An early timeout has no inventory snapshot. Retain the latest complete
    // one as a unit, with explicit provenance; never combine field-wise maxima.
    // A genuine empty inventory still has Some(domain) and must replace older
    // counts rather than inheriting them because they happen to be nonzero.
    if report.inventory_pane_width_domain.is_none() && phase.inventory_pane_width_domain.is_some() {
        report.inventory_pane_width_domain = phase.inventory_pane_width_domain.clone();
        report.groups = phase.groups;
        report.logical_candidates = phase.logical_candidates;
        report.immediate_candidates = phase.immediate_candidates;
        report.physical_configurations = phase.physical_configurations;
        report.compatible_connections = phase.compatible_connections;
        report.producer_sets = phase.producer_sets;
        report.stored_producer_ids = phase.stored_producer_ids;
        report.output_references = phase.output_references;
        report.structurally_removed_candidates = phase.structurally_removed_candidates;
        report.rounded_down_cost_bounds = phase.rounded_down_cost_bounds;
        report.independent_components = phase.independent_components;
    }
    report.memo_expansion_ms += phase.memo_expansion_ms;
    report.inventory_construction_ms += phase.inventory_construction_ms;
    report.search_and_validation_ms += phase.search_and_validation_ms;
    report.model_construction_ms += phase.model_construction_ms;
    report.solver_ms += phase.solver_ms;
    report.validation_ms += phase.validation_ms;
    report.early_query_pruning_ms += phase.early_query_pruning_ms;
    for (kind, count) in &phase.early_query_pruning_counts {
        *report
            .early_query_pruning_counts
            .entry(kind.clone())
            .or_default() += count;
    }
    for example in &phase.early_query_pruning_examples {
        if report.early_query_pruning_examples.len() < 8
            && !report.early_query_pruning_examples.contains(example)
        {
            report.early_query_pruning_examples.push(example.clone());
        }
    }
    report.solve_calls += phase.solve_calls;
    report.direct_cost_solve_calls += phase.direct_cost_solve_calls;
    report.joint_price_solve_calls += phase.joint_price_solve_calls;
    report.direct_cost_validated_plans += phase.direct_cost_validated_plans;
    if let Some(cost) = phase.direct_cost_best_validated_cost {
        report.direct_cost_best_validated_cost = Some(
            report
                .direct_cost_best_validated_cost
                .map_or(cost, |previous| previous.min(cost)),
        );
    }
    for reason in &phase.direct_cost_fallback_reasons {
        if report.direct_cost_fallback_reasons.len() < 8
            && !report.direct_cost_fallback_reasons.contains(reason)
        {
            report.direct_cost_fallback_reasons.push(reason.clone());
        }
    }
    report.refinements += phase.refinements;
    report.certified_query_cuts += phase.certified_query_cuts;
    report.peak_milp_variables = report.peak_milp_variables.max(phase.peak_milp_variables);
    report.peak_milp_constraints = report
        .peak_milp_constraints
        .max(phase.peak_milp_constraints);
    for (kind, count) in &phase.validation_rejection_counts {
        *report
            .validation_rejection_counts
            .entry(kind.clone())
            .or_default() += count;
    }
    for example in &phase.validation_rejection_examples {
        if report.validation_rejection_examples.len() < 8
            && !report.validation_rejection_examples.contains(example)
        {
            report.validation_rejection_examples.push(example.clone());
        }
    }
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
    initial: DagOfLogicalMemoGroups,
    context: &CandidateSearchContext<'_, Id>,
    search: Stage1ToStage3Search,
    local_candidate_generation_ms: Option<f64>,
    archive: &mut CandidateArchive,
    domain: asap_logical_asap_optimization::pass2::memo::shared_configurations::PaneWidthDomain,
    progress: &mut DomainProgress,
) -> crate::search::pipeline::PlanSearchResult {
    let inventory = context.inventory;
    let entries = context.entries;
    let data = context.data;
    let models = context.models;
    use crate::search::costing::calibration::CandidateCalibration;
    use crate::search::refinement::planner::CandidatePlanningContext;
    use crate::search::refinement::search::{CandidateSearchLimits, CandidateSearchStatus};
    use asap_logical_asap_optimization::pass1::logical_memo_candidates::generate_per_computation_logical_memo_candidates;
    use asap_logical_asap_optimization::pass2::logical_memo_candidates::{
        generate_logical_asap_memo_candidates, LogicalASAPMemoCandidatesError, Pass2RoundState,
    };
    use asap_logical_asap_optimization::pass2::memo::state_groups::StateGroupExpansionError;
    use asap_physical_asap_optimization::catalog::generate_physical_asap_memo_candidates;
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
            // No solve has started at these preparation checkpoints. Expose
            // measured generation time and budget; leave unfinished catalog
            // counts unset rather than inventing completed model statistics.
            let report = crate::CandidateConnectionReport {
                pane_width_rounds: vec![format!("{domain:?}")],
                pane_widths_pending: domain != asap_logical_asap_optimization::pass2::memo::shared_configurations::PaneWidthDomain::Complete,
                status: "time_limit".into(),
                catalog_scope: format!(
                    "planning stopped after {phase}, before solving; model statistics unavailable"
                ),
                full_catalog_completed: progress.catalog_completed
                    && domain == asap_logical_asap_optimization::pass2::memo::shared_configurations::PaneWidthDomain::Complete,
                max_refinements: search.max_evaluations.unwrap_or(128),
                time_limit_ms: search.milp_time_limit_ms.unwrap_or(30_000),
                local_candidate_generation_ms,
                ..Default::default()
            };
            progress.report = Some(report.clone());
            let metrics =
                serde_json::to_string(&report).map_err(|error| unsupported(&error.to_string()))?;
            Err(unsupported(&format!(
                "planning budget exhausted after {phase}; no unvalidated plan is returned. Search report: {metrics}"
            )))
        } else {
            Ok(())
        }
    };
    check_budget("local candidate generation", progress)?;
    crate::PlanSelector::new(entries, data, models, false)?;
    let calibration_policy =
        CandidateCalibration::for_models(models).map_err(|error| unsupported(&error))?;
    // The output references are the initial memo's query roots, which
    // stand for the inventory's roots only when the memo was built from them.
    if !initial.matches_root_dependencies(&inventory.roots) {
        return Err(unsupported(
            "the workload's query roots are not the initial memo's",
        ));
    }
    let roots = inventory
        .roots
        .iter()
        .map(|(_, root)| root.clone())
        .collect::<Vec<_>>();
    let output_references = asap_types::stages::output_references(&initial.query_roots).len();
    let started = Instant::now();
    let pruned = if progress.pass2_round.is_none() {
        Some(
            crate::search::query_pruning::prune_query_candidates(
                inventory,
                entries,
                data,
                models,
                if search.exhaustive { 0 } else { 64 },
            )
            .map_err(|error| unsupported(&error))?,
        )
    } else {
        None
    };
    let early_query_pruning_ms = started.elapsed().as_secs_f64() * 1000.0;
    check_budget("early query pruning", progress)?;
    let pruning = pruned.as_ref().map(|pruned| &pruned.report);
    let empty_pruning = crate::search::query_pruning::QueryCandidatePruningReport::default();
    let pruning = pruning.unwrap_or(&empty_pruning);
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
    let mut pass2_round = match progress.pass2_round.take() {
        Some(pass2_round) => pass2_round,
        None => {
            let stage0 = CandidateMemo {
                candidates: vec![Vec::new(); initial.groups.len()],
                groups: initial.groups.clone(),
                query_roots: initial.query_roots.clone(),
            };
            // While budget remains, now plus the remaining budget is the same
            // instant before or after population; once it is spent, either
            // instant is already past and generation stops at its first check.
            let pass1 = generate_per_computation_logical_memo_candidates(
                initial,
                pruned.expect("first round pruned").inventory,
                entries,
                data,
                Instant::now().checked_add(remaining()),
            )
            .map_err(|error| unsupported(&error.to_string()))?;
            progress.pass1_steps =
                Some((stage0, pass1.memo.clone(), pass1.pruned_candidates.clone()));
            Pass2RoundState::new(pass1)
        }
    };
    let deadline = Instant::now().checked_add(remaining());
    // The seed-grid round, or the one complete round when there is none.
    let first_round = pass2_round.previous_domain().is_none();
    // Pass 2 keeps its memo before state-group splitting across rounds: later
    // rounds append local widths, then rebuild the state/physical catalog.
    let logical_asap_candidates =
        match generate_logical_asap_memo_candidates(&mut pass2_round, entries, domain, deadline) {
            Ok(candidates) => candidates,
            Err(error) => {
                match &error {
                    // A timed-out window composition leaves partial work: drop it.
                    LogicalASAPMemoCandidatesError::LocalWindowComposition(_) => {
                        progress.budget_stopped = true;
                    }
                    LogicalASAPMemoCandidatesError::Expansion(expansion) => {
                        progress.budget_stopped =
                            matches!(expansion, StateGroupExpansionError::DeadlineExceeded(_));
                        progress.pass2_round = Some(pass2_round);
                    }
                }
                return Err(unsupported(&error.to_string()));
            }
        };
    let memo = &logical_asap_candidates.memo;
    let pass2_round = progress.pass2_round.insert(pass2_round);
    progress.memo_steps = progress
        .pass1_steps
        .as_ref()
        .map(|(stage0, pass1, pass1_pruned)| {
            (
                None,
                crate::search::pipeline::MemoSteps {
                    stage0: stage0.clone(),
                    pass1: pass1.clone(),
                    pass1_pruned_candidates: pass1_pruned.clone(),
                    pass2: pass2_round.unexpanded_memo().clone(),
                    concrete_states: memo.clone(),
                    stage2_operator_nodes: Vec::new(),
                    stage2_pruned_candidates: Vec::new(),
                    selected_chosen_candidates: Err("no plan was selected".into()),
                },
            )
        });
    let memo_expansion_ms = started.elapsed().as_secs_f64() * 1000.0;
    check_budget("memo rule expansion", progress)?;
    let groups = memo.groups.len();
    let logical_candidates = memo.candidates.iter().map(Vec::len).sum();
    let started = Instant::now();
    let physical = generate_physical_asap_memo_candidates(
        &logical_asap_candidates,
        entries,
        data,
        models.deployment_capabilities.query_time_results_retained,
        models.deployment_capabilities.raw_data_retained,
        deadline,
    )
    .map_err(|error| {
        progress.budget_stopped = error.is_deadline_exceeded();
        unsupported(&error.to_string())
    })?;
    let immediate_candidates = physical.operator_nodes_of_candidates_for_memo_groups.len();
    let structurally_removed_candidates = physical.pruned_candidates.len();
    if let Some((_, steps)) = &mut progress.memo_steps {
        steps.stage2_operator_nodes = physical
            .operator_nodes_of_candidates_for_memo_groups
            .clone();
        steps.stage2_pruned_candidates = physical.pruned_candidates.clone();
    }
    let mut configured = crate::search::candidates::dag::PhysicalCandidatesWithCosts::new(physical);
    check_budget("physical candidate generation", progress)?;
    let context = CandidatePlanningContext {
        deadline,
        refine_materialization: domain == asap_logical_asap_optimization::pass2::memo::shared_configurations::PaneWidthDomain::SeedGrids,
        seed_separate_plans: first_round,
        original_roots: &roots,
        entries,
        data,
        models,
        calibration: match calibration_policy {
            CandidateCalibration::Frozen(fit) => fit,
            CandidateCalibration::CompleteCatalog => asap_cost::CalibrationFit::default(),
        },
    };
    let bound_metrics = crate::search::costing::lower_bounds::apply(&mut configured, &context)
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
                    roots: Ok(logical),
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
                    roots: Ok(logical),
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
    let producer_storage = asap_types::stages::producer_storage_metrics(&configured.physical);
    let report = crate::CandidateConnectionReport {
        inventory_pane_width_domain: Some(format!("{domain:?}")),
        producer_sets: producer_storage.unique_producer_sets,
        stored_producer_ids: producer_storage.stored_producer_ids,
        pane_width_rounds: vec![format!("{domain:?}")],
        pane_widths_pending: domain != asap_logical_asap_optimization::pass2::memo::shared_configurations::PaneWidthDomain::Complete,
        rounded_down_cost_bounds: bound_metrics.rounded_down_candidates,
        seed_planning_ms: 0.0,
        full_planning_ms: planning_started.elapsed().as_secs_f64() * 1_000.0,
        seed_selected: false,
        full_catalog_completed: domain == asap_logical_asap_optimization::pass2::memo::shared_configurations::PaneWidthDomain::Complete,
        seed_fallback_reason: None,
        independent_components: result.independent_components,
        catalog_exhausted: result.catalog_exhausted,
        evaluation_complete: result.evaluation_complete,
        catalog_scope: "Pass 1, local panes, shared summary/pane states, exact and sketch rollups, compatible TopK ranking reuse, and scan widening; finite capacity, pane-width, horizon, materialization and execution-scope configurations".into(),
        status, groups, logical_candidates, immediate_candidates,
        physical_configurations: configured.physical.candidates.len(),
        compatible_connections: configured.dag.candidates.iter().flat_map(|candidate| &candidate.producer_candidates_per_input).map(|input| input.candidates.len()).sum(),
        output_references, refinements: result.refinements,
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
        direct_cost_solve_calls: result.metrics.direct_cost_solve_calls,
        joint_price_solve_calls: result.metrics.joint_price_solve_calls,
        direct_cost_validated_plans: result.metrics.direct_cost_validated_plans,
        direct_cost_best_validated_cost: result.metrics.direct_cost_best_validated_cost,
        direct_cost_fallback_reasons: result.metrics.direct_cost_fallback_reasons.clone(),
        peak_milp_variables: result.metrics.peak_variables,
        peak_milp_constraints: result.metrics.peak_constraints,
        validation_rejection_counts: result.rejections.counts,
        certified_query_cuts: result.query_cuts,
        early_query_pruning_ms, early_query_pruning_counts, early_query_pruning_examples,
        structurally_removed_candidates,
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
    if let Some((plan, steps)) = &mut progress.memo_steps {
        *plan = Some(physical.id.clone());
        steps.selected_chosen_candidates =
            selected_chosen_candidates(&steps.concrete_states, &configured.physical, selected);
    }
    selection.search.candidates_checked = result.refinements as u64;
    selection.search.candidate_connections = Some(report);
    Ok((selection, physical, logical))
}

/// The selected plan's memo candidate for each group it uses: the memo
/// candidate whose root each selected candidate is. Factoring may give several
/// memo candidates of a group one root (identical terminal readers of
/// different inputs); the one reading the selected producers' groups is taken.
fn selected_chosen_candidates(
    memo: &CandidateMemo,
    physical: &asap_types::stages::PhysicalASAPMemoCandidates,
    selected: &asap_types::stages::SelectedCandidateGraph,
) -> Result<ChosenCandidatePerMemoGroup, String> {
    use std::collections::HashMap;
    let mut producer_groups = HashMap::<_, Vec<MemoGroupId>>::new();
    for connection in &selected.connections {
        producer_groups
            .entry(connection.consumer)
            .or_default()
            .push(physical.memo_group_of(connection.producer));
    }
    let mut chosen_candidates = ChosenCandidatePerMemoGroup::new();
    for &id in &selected.selected_candidates {
        let node = &physical.operator_nodes_of_candidates_for_memo_groups
            [physical.candidates[id.0].operator_node.0];
        if !node.is_implementation_root {
            continue; // a candidate's private node
        }
        let options = &node.candidates_for_memo_group;
        let reads = |&&asap_types::stages::CandidateForMemoGroupId { group, index }: &&asap_types::stages::CandidateForMemoGroupId| {
            producer_groups.get(&id).is_none_or(|read| {
                read.iter().all(|&child| {
                    child == group
                        || memo.candidates[group.0 as usize][index]
                            .input_groups
                            .contains(&child)
                })
            })
        };
        let &asap_types::stages::CandidateForMemoGroupId { group, index } =
            options.iter().find(reads).ok_or_else(|| {
                format!(
                    "selected candidate {} reads groups none of its memo candidates read",
                    id.0
                )
            })?;
        if let Some(previous) = chosen_candidates.insert(group, index) {
            if previous != index {
                return Err(format!(
                    "the selected plan implements group {} with two candidates, {previous} and {index}",
                    group.0
                ));
            }
        }
    }
    // Every group the chosen candidates need, from the query roots, is chosen.
    let mut pending: Vec<_> = asap_types::stages::root_groups(&memo.query_roots)
        .into_iter()
        .collect();
    let mut seen = std::collections::HashSet::new();
    while let Some(group) = pending.pop() {
        if !seen.insert(group) {
            continue;
        }
        let index = chosen_candidates.get(&group).ok_or_else(|| {
            format!(
                "the selected plan uses group {} but no candidate of it is selected",
                group.0
            )
        })?;
        pending.extend(&memo.candidates[group.0 as usize][*index].input_groups);
    }
    Ok(chosen_candidates)
}

fn selected_logical_roots(
    configured: &crate::search::candidates::dag::PhysicalCandidatesWithCosts,
    selected: &asap_types::stages::SelectedCandidateGraph,
    roots: &[QueryRoot],
) -> Result<Vec<QueryRoot>, String> {
    configured.validate_selected(selected)?;
    let outputs = asap_physical_asap_optimization::catalog::reconstruct_selected_candidates(
        &configured.physical,
        selected,
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
    use crate::run_stage1_to_stage3_with;
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
        let initial = DagOfLogicalMemoGroups::from_roots(&roots);
        let inventory = asap_logical_asap_optimization::pass1::per_computation_candidates::generate_per_computation_candidates(roots).unwrap();
        let error = candidate_connection_search(
            initial,
            &CandidateSearchContext {
                inventory: &inventory,
                entries: &entries,
                data: &data,
                models: PlanningModels::builtin(),
            },
            Stage1ToStage3Search {
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
        let diagnostic = error.to_string();
        let (_, report) = diagnostic.split_once("Search report: ").unwrap();
        let report = serde_json::Deserializer::from_str(report)
            .into_iter::<crate::CandidateConnectionReport>()
            .next()
            .unwrap()
            .unwrap();
        assert_eq!(report.status, "time_limit");
        assert_eq!(report.time_limit_ms, 1);
        assert_eq!(report.local_candidate_generation_ms, Some(2.0));
        assert!(!report.full_catalog_completed);
        assert_eq!(report.solve_calls, 0);
        assert_eq!(report.peak_milp_variables, 0);
    }

    #[test]
    fn contradictory_bound_is_reported_as_invalid_not_clamped() {
        let mut report = crate::CandidateConnectionReport {
            status: "optimal".into(),
            lower_bound: Some(8.533333870933332e-6),
            relative_gap: Some(0.0),
            ..Default::default()
        };
        invalidate_contradictory_bound(&mut report, 8.400000806399998e-6);
        assert_eq!(report.status, "invalid_cost_certificate");
        assert_eq!(report.lower_bound, None);
        assert_eq!(report.relative_gap, None);
        assert!(report
            .seed_fallback_reason
            .unwrap()
            .contains("exceeds independently validated"));
    }

    // Only an explicitly identified budget stop can use an already validated plan.
    #[test]
    fn validated_seed_survives_budget_stop_but_not_internal_failure() {
        use asap_logical_asap_optimization::pass2::memo::shared_configurations::PaneWidthDomain;
        let (_, entries, data) = workload();
        let roots = vec![(
            0,
            QueryRoot::Scalar(asap_types::ir::ScalarExpr::Literal(
                asap_types::ir::scalar::ScalarValue::Float64(1.0),
            )),
        )];
        let initial = DagOfLogicalMemoGroups::from_roots(&roots);
        let inventory = asap_logical_asap_optimization::pass1::per_computation_candidates::generate_per_computation_candidates(roots).unwrap();
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
        let infeasible = validated_seed_after_stop(
            kept.clone(),
            SelectionError::Solver("complete catalog infeasible".into()),
            &DomainProgress {
                infeasible: true,
                catalog_completed: true,
                ..Default::default()
            },
        )
        .unwrap_err();
        assert!(infeasible
            .to_string()
            .contains("complete catalog infeasible"));
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
        let initial = DagOfLogicalMemoGroups::from_roots(&roots);
        let inventory = asap_logical_asap_optimization::pass1::per_computation_candidates::generate_per_computation_candidates(roots).unwrap();
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
        let settings: asap_cost::CostInputs = serde_json::from_value(serde_json::json!({
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
    fn pane_free_pareto_uses_complete_domain_and_frozen_fit() {
        let settings = asap_cost::CostInputs {
            frozen_calibration_factor: Some(3.0),
            cost_model: CostModel::ClusterCapacity(asap_cost::ClusterLimits {
                memory_bytes: 1_000_000,
                disk_bytes: 1_000_000,
                cpu_cores: Some(8),
                objective: asap_cost::ClusterObjective::ParetoFront,
            }),
            ..Default::default()
        };
        let run = scalar_run(&settings, Stage1ToStage3Search::default());
        let selection = run.stage3_plan_selection.unwrap();
        let report = selection.search.candidate_connections.as_ref().unwrap();
        assert_eq!(report.seed_planning_ms, 0.0);
        assert!(report.full_catalog_completed);
        assert_eq!(report.calibration_factor, 3.0);
        assert!(selection.front.contains(&selection.selected));
        assert_eq!(report.pane_width_rounds, vec!["Complete"]);
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

    #[test]
    fn progressive_width_rounds_preserve_incumbent_without_global_certificate() {
        let (roots, entries, data) = workload();
        let run = run_stage1_to_stage3_with(
            roots,
            &entries,
            &data,
            PlanningModels::builtin(),
            Stage1ToStage3Search {
                max_evaluations: Some(4),
                ..Default::default()
            },
        )
        .unwrap();
        let selection = run.stage3_plan_selection.unwrap();
        let report = selection.search.candidate_connections.as_ref().unwrap();
        assert!(selection.accepted.contains_key(&selection.selected));
        assert!(report.pane_width_rounds.len() >= 2);
        assert!(report.pane_width_rounds[1].starts_with("Expanding"));
        assert!(report.pane_widths_pending);
        assert!(!report.full_catalog_completed);
        assert_eq!(report.lower_bound, None);
        assert_eq!(report.relative_gap, None);
        assert_ne!(report.status, "optimal");
        assert!(selection.search.candidates_checked <= 4);
    }

    #[test]
    fn seed_fallback_report_preserves_model_without_inventing_inventory() {
        let previous = crate::CandidateConnectionReport {
            objective: "monetary_cost".into(),
            calibration_scope: "not_required".into(),
            calibration_factor: 1.0,
            ..Default::default()
        };
        let mut interrupted = crate::CandidateConnectionReport {
            status: "time_limit".into(),
            pane_widths_pending: true,
            ..Default::default()
        };
        add_phase_metrics(&mut interrupted, &previous);
        assert_eq!(interrupted.objective, "monetary_cost");
        assert_eq!(interrupted.calibration_scope, "not_required");
        assert_eq!(interrupted.calibration_factor, 1.0);
        assert_eq!(interrupted.inventory_pane_width_domain, None);
        assert_eq!(interrupted.physical_configurations, 0);
        assert!(!interrupted.full_catalog_completed);
        assert!(interrupted.pane_widths_pending);
        assert_eq!(interrupted.status, "time_limit");
        assert_eq!(interrupted.lower_bound, None);
        assert_eq!(interrupted.relative_gap, None);
    }

    #[test]
    fn existing_model_metadata_is_not_overwritten_by_older_phases() {
        let previous = crate::CandidateConnectionReport {
            objective: "monetary_cost".into(),
            calibration_scope: "not_required".into(),
            calibration_factor: 1.0,
            ..Default::default()
        };
        let mut current = crate::CandidateConnectionReport {
            objective: "ParetoFront".into(),
            calibration_scope: "frozen".into(),
            calibration_factor: 3.0,
            ..Default::default()
        };
        add_phase_metrics(&mut current, &previous);
        assert_eq!(current.objective, "ParetoFront");
        assert_eq!(current.calibration_scope, "frozen");
        assert_eq!(current.calibration_factor, 3.0);
    }

    #[test]
    fn interrupted_round_preserves_one_complete_inventory_snapshot() {
        let previous = crate::CandidateConnectionReport {
            inventory_pane_width_domain: Some("Expanding { max_parts: 32 }".into()),
            groups: 7,
            logical_candidates: 11,
            immediate_candidates: 13,
            physical_configurations: 17,
            compatible_connections: 101,
            producer_sets: 3,
            stored_producer_ids: 19,
            output_references: 2,
            structurally_removed_candidates: 5,
            rounded_down_cost_bounds: 2,
            independent_components: 1,
            lower_bound: Some(4.0),
            relative_gap: Some(0.1),
            full_catalog_completed: true,
            ..Default::default()
        };
        let mut interrupted = crate::CandidateConnectionReport {
            status: "pane_width_domain_incomplete".into(),
            pane_widths_pending: true,
            pane_width_rounds: vec!["Expanding { max_parts: 64 }".into()],
            ..Default::default()
        };
        add_phase_metrics(&mut interrupted, &previous);
        assert_eq!(
            interrupted.inventory_pane_width_domain,
            previous.inventory_pane_width_domain
        );
        assert_eq!(
            (
                interrupted.groups,
                interrupted.logical_candidates,
                interrupted.immediate_candidates
            ),
            (7, 11, 13)
        );
        assert_eq!(
            (
                interrupted.physical_configurations,
                interrupted.compatible_connections,
                interrupted.producer_sets,
                interrupted.stored_producer_ids
            ),
            (17, 101, 3, 19)
        );
        assert_eq!(
            (
                interrupted.output_references,
                interrupted.structurally_removed_candidates
            ),
            (2, 5)
        );
        assert_eq!(interrupted.lower_bound, None);
        assert_eq!(interrupted.relative_gap, None);
        assert!(!interrupted.full_catalog_completed);
        assert!(interrupted.pane_widths_pending);
        assert_eq!(interrupted.status, "pane_width_domain_incomplete");
        assert_eq!(
            interrupted.pane_width_rounds,
            vec!["Expanding { max_parts: 64 }"]
        );

        let mut empty = crate::CandidateConnectionReport {
            inventory_pane_width_domain: Some("Complete".into()),
            ..Default::default()
        };
        add_phase_metrics(&mut empty, &previous);
        assert_eq!(
            empty.inventory_pane_width_domain.as_deref(),
            Some("Complete")
        );
        assert_eq!(
            (
                empty.physical_configurations,
                empty.compatible_connections,
                empty.producer_sets,
                empty.stored_producer_ids
            ),
            (0, 0, 0, 0)
        );
    }

    #[test]
    fn repeated_round_ids_preserve_all_cost_and_accuracy_records() {
        let make = |price| {
            let run = scalar_run(
                &asap_cost::CostInputs::default(),
                Stage1ToStage3Search {
                    max_evaluations: Some(1),
                    ..Default::default()
                },
            );
            let mut selection = run.stage3_plan_selection.unwrap();
            let physical = run.stage3_selected.unwrap();
            selection
                .costs
                .get_mut(&selection.selected)
                .unwrap()
                .cost_per_second = Some(price);
            let roots = physical.roots.clone();
            (selection, physical, roots)
        };
        let first = make(1.0);
        let second = make(2.0);
        let initial = make(3.0);
        assert_eq!(first.1.id, second.1.id);
        assert_eq!(first.1.id, initial.1.id);
        let (_, merged) = merge_validated_rounds(first, second, PlanningModels::builtin());
        let (_, merged) = merge_validated_rounds(initial, merged, PlanningModels::builtin());
        assert_eq!(merged.0.costs.len(), 3);
        assert_eq!(merged.0.accepted.len(), 3);
        let mut prices: Vec<_> = merged
            .0
            .costs
            .values()
            .map(|cost| cost.cost_per_second.unwrap())
            .collect();
        prices.sort_by(f64::total_cmp);
        assert_eq!(prices, vec![1.0, 2.0, 3.0]);
        assert_eq!(merged.0.selected, merged.1.id);
        assert_eq!(merged.0.costs[&merged.1.id].cost_per_second, Some(1.0));
        let settings = asap_cost::CostInputs {
            cost_model: CostModel::ClusterCapacity(asap_cost::ClusterLimits {
                memory_bytes: 1_000_000,
                disk_bytes: 1_000_000,
                cpu_cores: Some(8),
                objective: asap_cost::ClusterObjective::ParetoFront,
            }),
            ..Default::default()
        };
        let models = PlanningModels {
            plan_selection: &settings,
            ..PlanningModels::builtin()
        };
        let (_, merged) = merge_validated_rounds(make(1.0), make(2.0), models);
        let (_, merged) = merge_validated_rounds(make(0.5), merged, models);
        assert_eq!(merged.0.costs.len(), 3);
        assert_eq!(merged.0.front.len(), 3);
        assert!(merged.0.front.contains(&merged.1.id));
    }

    #[test]
    fn repeated_window_free_computation_searches_one_complete_catalog() {
        let (_, entries, data) = workload();
        let roots = vec![(
            0,
            QueryRoot::Operator(lower_promql("sum(latency)", AccuracyTarget::exact())),
        )];
        let run = run_stage1_to_stage3_with(
            roots,
            &entries[..1],
            &data,
            PlanningModels::builtin(),
            Stage1ToStage3Search::default(),
        )
        .unwrap();
        let selection = run.stage3_plan_selection.unwrap();
        let report = selection.search.candidate_connections.unwrap();
        assert_eq!(report.pane_width_rounds, vec!["Complete"]);
        assert_eq!(report.seed_planning_ms, 0.0);
        assert!(report.full_catalog_completed);
    }

    #[test]
    fn literal_workload_does_not_repeat_identical_width_models() {
        let run = scalar_run(
            &asap_cost::CostInputs::default(),
            Stage1ToStage3Search::default(),
        );
        let selection = run.stage3_plan_selection.unwrap();
        let report = selection.search.candidate_connections.unwrap();
        assert_eq!(report.pane_width_rounds, vec!["Complete"]);
        assert!(report.full_catalog_completed);
    }

    /// A deployment's accuracy model that accepts no output.
    struct AcceptsNothing;

    impl asap_accuracy::AccuracyModel for AcceptsNothing {
        fn satisfies(
            &self,
            _: &asap_types::ir::properties::AccuracyGuarantee,
            _: &AccuracyTarget,
        ) -> bool {
            false
        }

        fn may_accept_regardless_of_guarantee(&self) -> bool {
            false
        }
    }

    // A failed complete phase must not relabel seed work as initial generation.
    #[test]
    fn no_incumbent_report_preserves_seed_and_full_phase_accounting() {
        let (_, entries, data) = workload();
        let roots = vec![(
            0,
            QueryRoot::Operator(lower_promql("latency[5m]", AccuracyTarget::exact())),
        )];
        let initial = DagOfLogicalMemoGroups::from_roots(&roots);
        let inventory = asap_logical_asap_optimization::pass1::per_computation_candidates::generate_per_computation_candidates(roots).unwrap();
        let error = candidate_connection_search(
            initial,
            &CandidateSearchContext {
                inventory: &inventory,
                entries: &entries[..1],
                data: &data,
                models: PlanningModels::builtin().with_accuracy(&AcceptsNothing),
            },
            Stage1ToStage3Search::default(),
            Some(17.0),
            &mut CandidateArchive::default(),
        )
        .unwrap_err()
        .to_string();
        let report: serde_json::Value =
            serde_json::from_str(error.split_once("Search report: ").unwrap().1).unwrap();
        assert_eq!(report["local_candidate_generation_ms"], 17.0);
        assert!(report["seed_planning_ms"].as_f64().unwrap() > 0.0);
        assert!(report["full_planning_ms"].as_f64().unwrap() > 0.0);
        assert_eq!(
            report["refinements"].as_u64().unwrap() as usize,
            report["pane_width_rounds"].as_array().unwrap().len()
        );
        assert_eq!(report["time_limit_ms"], 30_000);
        assert!(
            report["validation_rejection_counts"]["accuracy_target_missed"]
                .as_u64()
                .unwrap()
                > 0,
            "{}",
            report["validation_rejection_counts"]
        );
    }

    fn scalar_run(
        settings: &asap_cost::CostInputs,
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
                ..Default::default()
            },
        )
        .unwrap();
        let selection = run.stage3_plan_selection.unwrap();
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
        use asap_logical_asap_optimization::pass1::per_computation_candidates::generate_per_computation_candidates;
        use asap_logical_asap_optimization::pass2::memo::candidates::try_populate_pass1;
        use asap_logical_asap_optimization::pass2::memo::shared_configurations::PaneWidthDomain;
        use asap_logical_asap_optimization::test_support::{
            shared_summary_state_groups, sharing_rules_and_shared_state_groups,
        };
        let (roots, mut entries, _) = workload();
        for entry in &mut entries {
            entry.recurrence =
                QueryRecurrence::Repeated(RepeatedDemand::FixedInterval(RepetitionInterval(1_000)));
        }
        let initial = DagOfLogicalMemoGroups::from_roots(&roots);
        let generated = generate_per_computation_candidates(roots).unwrap();
        let mut memo = try_populate_pass1(initial, &generated).unwrap();
        asap_logical_asap_optimization::pass2::memo::window_candidates::add_local_window_composition(
            &mut memo, &entries,
        );
        let expanded =
            sharing_rules_and_shared_state_groups(memo, PaneWidthDomain::Complete).unwrap();
        assert!(
            shared_summary_state_groups(&expanded).iter().any(|group| {
                let state = &expanded.groups[group.0 as usize];
                matches!(
                    state.logical_node.asap(),
                    Some(asap_types::ir::ASAPOp::PaneChain { .. })
                ) && state.reached_by_queries() == [0, 1]
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
                ..Default::default()
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
                QueryRoot::Operator(node) => asap_types::ir::OperatorNode::reachable(node),
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
                ..Default::default()
            },
        )
        .unwrap();
        let selection = run.stage3_plan_selection.unwrap();
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

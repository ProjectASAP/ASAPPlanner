//! The facade's orchestration of the planning stages (#1047): Stage 0
//! (the memo of logical groups), Pass 1, then per pane-width round Pass 2 →
//! Stage 2 → Stage 3, and finally the selected plan built through Stage 2's
//! realizer. No stage calls another; the facade owns the deadline and splits
//! it per round (#1048).

use std::collections::HashSet;
use std::rc::Rc;
use std::time::{Duration, Instant};

use asap_cost::PlanRanking;
use asap_logical_asap_optimization::deployment_capabilities::DeploymentCapabilitiesError;
use asap_logical_asap_optimization::pass1::{
    generate_per_computation_logical_memo_candidates, PerComputationCandidateError,
    PerComputationLogicalMemoCandidates, PerComputationLogicalMemoCandidatesError,
};
use asap_logical_asap_optimization::pass2::logical_memo_candidates::{
    generate_logical_asap_memo_candidates, LogicalASAPMemoCandidatesError, Pass2RoundState,
};
use asap_logical_asap_optimization::pass2::memo::enumeration::ChosenCandidatePerMemoGroup;
use asap_logical_asap_optimization::pass2::memo::shared_configurations::{
    PaneWidthDomain, PaneWidthFrontier,
};
use asap_logical_asap_optimization::pass2::memo::DagOfLogicalMemoGroups;
use asap_physical_asap_optimization::{
    generate_physical_asap_memo_candidates, Stage2AssignmentRealizer,
};
use asap_plan_selection::{select_plan, ErrorSelectingPlan, SearchReport, Selection};
use asap_types::ir::schema::schema_support::with_promql_series_identity;
use asap_types::ir::{ASAPOp, NonASAPOp, OperatorNode, QueryRoot, TimeRangeKind};
use asap_types::stages::{
    AssignmentRealizer, CandidateForMemoGroupId, DagOfSelectedCandidates,
    LogicalASAPMemoCandidates, MemoCandidates, MemoGroupId, OperatorNodeOfCandidateForMemoGroup,
    PhysicalASAPMemoCandidates, PhysicalCandidateError, PhysicalCandidateKey, PrunedCandidate,
    RealizedPlan,
};
use asap_types::workload::bounds::with_confirmed_weights_at;
use asap_types::workload::{DataWorkload, QueryLanguage, QueryWorkloadEntry};

use crate::pass::{
    OptimizationInput, OptimizationPass, OptimizeError, PlanOutput, PlanningModels, QueryPlan,
};

/// The planning budget when [`PlanSearchSettings::time_limit_ms`] is unset.
const DEFAULT_TIME_LIMIT_MS: u64 = 30_000;
/// The seed-width round's share of the remaining budget, and its cap.
const SEED_ROUND_SHARE: u32 = 4;
const SEED_ROUND_MAX: Duration = Duration::from_secs(5);
/// An intermediate pane-width round's budget cap.
const INTERMEDIATE_ROUND_MAX: Duration = Duration::from_secs(5);
/// The least budget left after Pass 1 for the seed round to run first.
const SEED_ROUND_MIN_REMAINING: Duration = Duration::from_secs(1);

/// How the facade searches.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PlanSearchSettings {
    /// The whole planning budget, Pass 1 included, in milliseconds; default
    /// 30 s. The facade splits it over the pane-width rounds, and each
    /// stage stops at its round's deadline (an evaluation in progress may
    /// overrun it).
    pub time_limit_ms: Option<u64>,
}

/// The memo after each step of the planning round whose plan was selected
/// (or of the last round when none was).
#[derive(Debug, Clone)]
pub struct MemoSteps {
    /// After hash-consing the frontends' roots: groups, no candidates.
    /// (The memo after Pass 1 is [`PlanningRun::per_computation_candidates`].)
    pub stage0: MemoCandidates,
    /// After local window composition (pane candidates).
    pub pass2: MemoCandidates,
    /// After concrete summary-state group expansion: the memo Stage 2
    /// implements.
    pub concrete_states: MemoCandidates,
    /// Stage 2's operator nodes over `concrete_states`
    /// (`PhysicalASAPMemoCandidates::operator_nodes_of_candidates_for_memo_groups`),
    /// which `stage2_pruned_candidates` index.
    pub stage2_operator_nodes: Vec<OperatorNodeOfCandidateForMemoGroup>,
    /// The physical candidates Stage 2 pruned
    /// (`PhysicalASAPMemoCandidates::pruned_candidates`).
    pub stage2_pruned_candidates: Vec<PrunedCandidate<PhysicalCandidateKey>>,
    /// The selected plan's candidate for each group of `concrete_states` it
    /// uses, or why there is none.
    pub selected_chosen_candidates: Result<ChosenCandidatePerMemoGroup, String>,
}

/// Why the facade selected no plan.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum SelectionError {
    /// Bad input: the deployment's capabilities contradict the data
    /// workload.
    #[error("deployment capabilities: {0}")]
    InvalidDeploymentCapabilities(DeploymentCapabilitiesError),
    /// Pass 1's memo candidates or a round's Pass 2 failed.
    #[error("logical ASAP-aware optimization: {0}")]
    LogicalASAPCandidates(String),
    /// A round's Stage 2 failed.
    #[error("physical ASAP-aware optimization: {0}")]
    PhysicalASAPCandidates(String),
    /// Stage 3 selected no plan.
    #[error("plan selection: {0}")]
    Selection(ErrorSelectingPlan),
    /// Stage 2 could not build the plan Stage 3 selected.
    #[error("building the selected plan: {0}")]
    Realization(PhysicalCandidateError),
    /// The budget ended before any round selected a plan.
    #[error("the planning budget ended before a plan was selected")]
    DeadlineReached,
}

/// One pane-width round, as the facade ran it.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct PlanningRoundReport {
    /// The round's pane-width domain (`SeedGrids`, `Complete`, ...).
    pub pane_width_domain: String,
    /// Pass 2's output: memo groups and their candidates.
    pub memo_groups: usize,
    pub logical_candidates: usize,
    /// Stage 2's output: operator nodes, physical candidates and pruned
    /// candidates.
    pub operator_nodes: usize,
    pub physical_candidates: usize,
    /// Stage 2's producer links: over every physical candidate and input,
    /// the candidates that may produce it.
    pub physical_producer_links: usize,
    pub stage2_pruned_candidates: usize,
    pub pass2_ms: f64,
    pub stage2_ms: f64,
    pub stage3_ms: f64,
    /// Stage 3's search report, when it selected a plan.
    pub search_report: Option<SearchReport>,
    /// Why the round selected no plan.
    pub error: Option<String>,
}

/// What the facade did: per stage and round, its counts and timings.
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize)]
pub struct PlanningReport {
    /// Stage 0 and Pass 1, in milliseconds.
    pub pass1_ms: f64,
    pub rounds: Vec<PlanningRoundReport>,
    /// The round whose selection is the plan (index into `rounds`).
    pub selected_round: Option<usize>,
    /// Some legal pane widths were not searched: the plan is the best over
    /// the widths searched, not over all of them.
    pub pane_widths_pending: bool,
    /// The whole budget, in milliseconds.
    pub time_limit_ms: u64,
}

/// Every stage's output for one workload ([`run_planning`]).
#[derive(Debug)]
pub struct PlanningRun {
    /// Stage 1, Pass 1's output: the memo groups with their per-computation
    /// candidates, the computation targets and the pruned candidates.
    /// `None` when Pass 1 failed.
    pub per_computation_candidates: Option<PerComputationLogicalMemoCandidates>,
    /// The selected round's Pass 2 output (the last round's when none is
    /// selected).
    pub logical_asap_candidates: Option<LogicalASAPMemoCandidates>,
    /// The selected round's Stage 2 output (the last round's when none is
    /// selected): the catalog `selection`'s candidate ids index.
    pub physical_asap_candidates: Option<PhysicalASAPMemoCandidates>,
    /// Stage 3's selection.
    pub selection: Result<Selection, SelectionError>,
    /// The selected plan as Stage 2 built it, with its pane retention.
    pub selected_physical_plan: Option<RealizedPlan>,
    /// The plan to run: the selected plan's roots with the weight proofs
    /// Stage 3 made applied.
    pub selected_plan: Option<Vec<QueryRoot>>,
    /// The memo after each step of the selected round, and the selected
    /// plan's choice over it.
    pub memo_steps: Option<MemoSteps>,
    pub report: PlanningReport,
}

/// One round's outputs.
struct Round {
    index: usize,
    logical: LogicalASAPMemoCandidates,
    physical: PhysicalASAPMemoCandidates,
    pass2: MemoCandidates,
    selection: Result<Selection, SelectionError>,
}

/// Run the planning stages over the frontends' `roots` (query `i` is
/// `roots[i]`, with workload entry `entries[i]`). Only a Pass 1 failure is
/// an error; a failed selection is reported in the run.
pub fn run_planning(
    roots: Vec<QueryRoot>,
    entries: &[QueryWorkloadEntry],
    data: &DataWorkload,
    models: PlanningModels<'_>,
    settings: PlanSearchSettings,
) -> Result<PlanningRun, PerComputationCandidateError> {
    let started = Instant::now();
    let time_limit_ms = settings.time_limit_ms.unwrap_or(DEFAULT_TIME_LIMIT_MS);
    let global_deadline = started.checked_add(Duration::from_millis(time_limit_ms));
    for root in &roots {
        root.validate_structure()?;
    }
    // Stage 0: the memo of logical groups.
    let initial = DagOfLogicalMemoGroups::from_roots(&roots);
    let stage0 = MemoCandidates {
        candidates: vec![Vec::new(); initial.groups.len()],
        groups: initial.groups.clone(),
        query_roots: initial.query_roots.clone(),
    };
    let has_pane_window = has_pane_window(&initial);
    let has_groups = !initial.groups.is_empty();
    // Pass 1: the per-computation candidates the deployment can run and
    // some query can use.
    let accuracy_targets: Vec<_> = entries
        .iter()
        .map(|entry| entry.requirements.accuracy.clone())
        .collect();
    let accuracy = models.accuracy_check_inputs();
    let pass1 = generate_per_computation_logical_memo_candidates(
        initial,
        &accuracy_targets,
        accuracy.model,
        accuracy.evidence,
        data,
        models
            .deployment_capabilities
            .supported_summaries
            .as_deref(),
        global_deadline,
    );
    // A query Pass 1 cannot plan is an input error.
    let pass1 = match pass1 {
        Err(PerComputationLogicalMemoCandidatesError::PerComputation(error)) => return Err(error),
        pass1 => pass1,
    };
    let mut report = PlanningReport {
        time_limit_ms,
        ..PlanningReport::default()
    };
    let mut run = PlanningRun {
        per_computation_candidates: pass1.as_ref().ok().cloned(),
        logical_asap_candidates: None,
        physical_asap_candidates: None,
        selection: Err(SelectionError::DeadlineReached),
        selected_physical_plan: None,
        selected_plan: None,
        memo_steps: None,
        report: PlanningReport::default(),
    };
    if let Err(error) = models.deployment_capabilities.validate(data) {
        run.selection = Err(SelectionError::InvalidDeploymentCapabilities(error));
        run.report = report;
        return Ok(run);
    }
    let pass1 = match pass1 {
        Ok(pass1) => pass1,
        Err(error) => {
            // A Pass 1 deadline is the planning budget running out.
            run.selection = Err(match error {
                PerComputationLogicalMemoCandidatesError::Timeout(_) => {
                    SelectionError::DeadlineReached
                }
                error => SelectionError::LogicalASAPCandidates(error.to_string()),
            });
            report.pass1_ms = elapsed_ms(started);
            run.report = report;
            return Ok(run);
        }
    };
    report.pass1_ms = elapsed_ms(started);
    let mut state = Some(Pass2RoundState::new(pass1));

    let realizer = Stage2AssignmentRealizer {
        original_roots: &roots,
        entries,
        data,
        query_time_results_retained: models.deployment_capabilities.query_time_results_retained,
    };
    let ranking = PlanRanking::of(&models.plan_selection.cost_model);
    let mut best: Option<Round> = None;
    let mut last: Option<Round> = None;
    let mut final_error: Option<SelectionError> = None;
    let mut completed_full_domain = false;
    let mut run_round = |domain: PaneWidthDomain,
                         deadline: Option<Instant>,
                         state: &mut Option<Pass2RoundState>,
                         report: &mut PlanningReport|
     -> RoundEnd {
        let index = report.rounds.len();
        let outcome = plan_round(
            state, domain, deadline, entries, data, &models, &realizer, index,
        );
        report.rounds.push(outcome.report);
        let Some(round) = outcome.round else {
            return outcome.end;
        };
        match &round.selection {
            Ok(selection) => {
                let replaces = best.as_ref().is_none_or(|best| {
                    let best = best.selection.as_ref().expect("best round selected");
                    !ranking.better(&best.cost, &selection.cost)
                });
                if replaces {
                    best = Some(round);
                } else {
                    last = Some(round);
                }
            }
            Err(_) => last = Some(round),
        }
        outcome.end
    };

    let remaining = || {
        global_deadline.map_or(Duration::MAX, |deadline| {
            deadline.saturating_duration_since(Instant::now())
        })
    };
    let seed_enabled = has_pane_window
        && calibration_is_frozen(models.plan_selection)
        && remaining() >= SEED_ROUND_MIN_REMAINING;
    if !seed_enabled {
        let end = run_round(
            PaneWidthDomain::Complete,
            global_deadline,
            &mut state,
            &mut report,
        );
        completed_full_domain = end.completed;
        if let Some(error) = end.fatal {
            final_error = Some(error);
        }
    } else {
        let seed_deadline =
            Instant::now().checked_add((remaining() / SEED_ROUND_SHARE).min(SEED_ROUND_MAX));
        let end = run_round(
            PaneWidthDomain::SeedGrids,
            seed_deadline,
            &mut state,
            &mut report,
        );
        if let Some(error) = end.fatal {
            final_error = Some(error);
        }
        // A literal-only workload has no computation in which pane rules
        // can fire.
        let grids = entries
            .iter()
            .filter_map(|entry| entry.evaluation_grid_ms())
            .filter(|_| has_groups);
        let mut frontier = PaneWidthFrontier::new(grids, global_deadline);
        while final_error.is_none() && state.is_some() && !remaining().is_zero() {
            let domain = match frontier
                .as_mut()
                .map(|frontier| frontier.next_domain(global_deadline))
            {
                Ok(Ok(Some(domain))) => domain,
                Ok(Ok(None)) | Ok(Err(_)) | Err(_) => break,
            };
            let complete = domain == PaneWidthDomain::Complete;
            let deadline = if complete {
                global_deadline
            } else {
                Instant::now().checked_add(remaining().min(INTERMEDIATE_ROUND_MAX))
            };
            let end = run_round(domain, deadline, &mut state, &mut report);
            if let Some(error) = end.fatal {
                final_error = Some(error);
            }
            if complete {
                completed_full_domain = end.completed;
                break;
            }
        }
    }
    report.pane_widths_pending = !completed_full_domain;

    let (chosen, selected) = match (final_error, best) {
        (Some(error), best) => {
            run.selection = Err(error);
            (last.or(best), false)
        }
        (None, Some(best)) => (Some(best), true),
        (None, None) => {
            let mut last = last;
            run.selection = Err(last
                .as_mut()
                .and_then(|round| {
                    std::mem::replace(&mut round.selection, Err(SelectionError::DeadlineReached))
                        .err()
                })
                .unwrap_or(SelectionError::DeadlineReached));
            (last, false)
        }
    };
    let Some(round) = chosen else {
        run.report = report;
        return Ok(run);
    };
    let mut memo_steps = MemoSteps {
        stage0,
        pass2: round.pass2,
        concrete_states: round.logical.memo.clone(),
        stage2_operator_nodes: round
            .physical
            .operator_nodes_of_candidates_for_memo_groups
            .clone(),
        stage2_pruned_candidates: round.physical.pruned_candidates.clone(),
        selected_chosen_candidates: Err("no plan was selected".into()),
    };
    if let (true, Ok(selection)) = (selected, round.selection) {
        report.selected_round = Some(round.index);
        memo_steps.selected_chosen_candidates = selected_chosen_candidates(
            &round.logical.memo,
            &round.physical,
            &selection.selected_dag,
        );
        match realizer.realize(&round.physical, &selection.selected_dag) {
            Ok(realized) => {
                let proven: HashSet<*const OperatorNode> = selection
                    .nodes_with_proven_non_negative_weights
                    .iter()
                    .filter_map(|node| realized.operator_node_of_selected_node.get(node.0))
                    .map(Rc::as_ptr)
                    .collect();
                run.selected_plan = Some(with_confirmed_weights_at(
                    &realized.plan.roots,
                    data,
                    &|node| proven.contains(&Rc::as_ptr(node)),
                ));
                run.selected_physical_plan = Some(realized);
                run.selection = Ok(selection);
            }
            Err(error) => run.selection = Err(SelectionError::Realization(error)),
        }
    }
    run.memo_steps = Some(memo_steps);
    run.logical_asap_candidates = Some(round.logical);
    run.physical_asap_candidates = Some(round.physical);
    run.report = report;
    Ok(run)
}

/// How a round ended, for the round loop.
struct RoundEnd {
    /// The round ran Pass 2, Stage 2 and Stage 3 to the end (with or
    /// without a plan).
    completed: bool,
    /// An error that ends planning: the complete catalog is infeasible, or
    /// a stage failed for a reason other than its deadline.
    fatal: Option<SelectionError>,
}

struct RoundOutcome {
    round: Option<Round>,
    report: PlanningRoundReport,
    end: RoundEnd,
}

/// One pane-width round: Pass 2 over `state` for `domain`, Stage 2, then
/// Stage 3, each stopping at `deadline`. A Pass 2 timeout in local window
/// composition leaves `state` unusable, so it is dropped.
#[allow(clippy::too_many_arguments)]
fn plan_round(
    state: &mut Option<Pass2RoundState>,
    domain: PaneWidthDomain,
    deadline: Option<Instant>,
    entries: &[QueryWorkloadEntry],
    data: &DataWorkload,
    models: &PlanningModels<'_>,
    realizer: &dyn AssignmentRealizer,
    index: usize,
) -> RoundOutcome {
    let mut report = PlanningRoundReport {
        pane_width_domain: format!("{domain:?}"),
        memo_groups: 0,
        logical_candidates: 0,
        operator_nodes: 0,
        physical_candidates: 0,
        physical_producer_links: 0,
        stage2_pruned_candidates: 0,
        pass2_ms: 0.0,
        stage2_ms: 0.0,
        stage3_ms: 0.0,
        search_report: None,
        error: None,
    };
    let stopped = |report: PlanningRoundReport, fatal: Option<SelectionError>| RoundOutcome {
        round: None,
        report,
        end: RoundEnd {
            completed: false,
            fatal,
        },
    };
    let Some(round_state) = state.as_mut() else {
        report.error = Some("Pass 2's round state was dropped".into());
        return stopped(report, None);
    };
    let started = Instant::now();
    let logical = generate_logical_asap_memo_candidates(round_state, entries, domain, deadline);
    report.pass2_ms = elapsed_ms(started);
    let logical = match logical {
        Ok(logical) => logical,
        Err(error) => {
            report.error = Some(error.to_string());
            if matches!(
                error,
                LogicalASAPMemoCandidatesError::LocalWindowComposition(_)
            ) {
                *state = None;
            }
            let fatal =
                (!past(deadline)).then(|| SelectionError::LogicalASAPCandidates(error.to_string()));
            return stopped(report, fatal);
        }
    };
    let pass2 = round_state.unexpanded_memo().clone();
    report.memo_groups = logical.memo.groups.len();
    report.logical_candidates = logical.memo.candidates.iter().map(Vec::len).sum();
    let started = Instant::now();
    let physical = generate_physical_asap_memo_candidates(
        &logical,
        entries,
        data,
        models.deployment_capabilities.query_time_results_retained,
        models.deployment_capabilities.raw_data_retained,
        deadline,
    );
    report.stage2_ms = elapsed_ms(started);
    let physical = match physical {
        Ok(physical) => physical,
        Err(error) => {
            report.error = Some(error.to_string());
            let fatal = (!error.is_deadline_exceeded())
                .then(|| SelectionError::PhysicalASAPCandidates(error.to_string()));
            return stopped(report, fatal);
        }
    };
    report.operator_nodes = physical.operator_nodes_of_candidates_for_memo_groups.len();
    report.physical_candidates = physical.candidates.len();
    report.physical_producer_links = physical
        .candidates
        .iter()
        .flat_map(|candidate| &candidate.producer_candidates_per_input)
        .map(|producers| producers.len())
        .sum();
    report.stage2_pruned_candidates = physical.pruned_candidates.len();
    let started = Instant::now();
    let selection = select_plan(
        &physical,
        entries,
        data,
        &models.accuracy_check_inputs(),
        models.plan_selection,
        realizer,
        deadline,
    );
    report.stage3_ms = elapsed_ms(started);
    let fatal = match &selection {
        Ok(selection) => {
            report.search_report = Some(selection.search_report.clone());
            None
        }
        Err(error) => {
            report.error = Some(error.to_string());
            match error {
                ErrorSelectingPlan::NoPlanByDeadline(_)
                | ErrorSelectingPlan::EvaluationDeadlineExceeded => None,
                // An infeasible restricted catalog does not make the complete
                // one infeasible.
                ErrorSelectingPlan::NoValidCandidate(_) if domain != PaneWidthDomain::Complete => {
                    None
                }
                error => Some(SelectionError::Selection(clone_error(error))),
            }
        }
    };
    RoundOutcome {
        round: Some(Round {
            index,
            logical,
            physical,
            pass2,
            selection: selection.map_err(SelectionError::Selection),
        }),
        report,
        end: RoundEnd {
            completed: true,
            fatal,
        },
    }
}

/// `error` again, for the fatal-error record (the round keeps the original).
fn clone_error(error: &ErrorSelectingPlan) -> ErrorSelectingPlan {
    match error {
        ErrorSelectingPlan::NoValidCandidate(rejected) => {
            ErrorSelectingPlan::NoValidCandidate(rejected.clone())
        }
        ErrorSelectingPlan::RealizerRejected(reason) => {
            ErrorSelectingPlan::RealizerRejected(reason.clone())
        }
        ErrorSelectingPlan::UnsupportedCostModel(model) => {
            ErrorSelectingPlan::UnsupportedCostModel(model.clone())
        }
        &ErrorSelectingPlan::ObjectiveDiffersFromPlanCost {
            objective_per_second,
            plan_cost_per_second,
        } => ErrorSelectingPlan::ObjectiveDiffersFromPlanCost {
            objective_per_second,
            plan_cost_per_second,
        },
        ErrorSelectingPlan::SelectedPlanFailedCheck { cause, reason } => {
            ErrorSelectingPlan::SelectedPlanFailedCheck {
                cause: *cause,
                reason: reason.clone(),
            }
        }
        error => ErrorSelectingPlan::Solver(error.to_string()),
    }
}

fn past(deadline: Option<Instant>) -> bool {
    deadline.is_some_and(|deadline| Instant::now() >= deadline)
}

fn elapsed_ms(started: Instant) -> f64 {
    started.elapsed().as_secs_f64() * 1000.0
}

/// Whether Stage 3 prices with one fixed calibration factor: a frozen factor
/// is given, or no operator is measured. Only then is the seed-width round
/// comparable with later rounds.
fn calibration_is_frozen(cost_inputs: &asap_cost::CostInputs) -> bool {
    cost_inputs.frozen_calibration_factor.is_some()
        || cost_inputs.measured_operation_costs.operators.is_empty()
}

/// Pane rules require a temporal range below a summary build: a positive
/// `Range` window, or an existing pane chain. Instant lookback selectors are
/// not splittable panes.
fn has_pane_window(initial: &DagOfLogicalMemoGroups) -> bool {
    initial.groups.iter().any(|group| {
        matches!(
            group.logical_node.non_asap(),
            Some(NonASAPOp::TimeRange {
                kind: TimeRangeKind::Range, range, ..
            }) if !range.is_zero()
        ) || matches!(group.logical_node.asap(), Some(ASAPOp::PaneChain { .. }))
    })
}

/// The selected plan's memo candidate for each group it uses: the memo
/// candidate whose root each selected node's candidate is. Factoring may give several
/// memo candidates of a group one root (identical terminal readers of
/// different inputs); the one reading the selected producers' groups is taken.
fn selected_chosen_candidates(
    memo: &MemoCandidates,
    physical: &PhysicalASAPMemoCandidates,
    selected: &DagOfSelectedCandidates,
) -> Result<ChosenCandidatePerMemoGroup, String> {
    use std::collections::HashMap;
    let mut producer_groups = HashMap::<_, Vec<MemoGroupId>>::new();
    for edge in &selected.edges {
        producer_groups
            .entry(edge.consumer)
            .or_default()
            .push(physical.memo_group_of(selected.candidate_of_node[edge.producer.0]));
    }
    let mut chosen_candidates = ChosenCandidatePerMemoGroup::new();
    for (node_id, &id) in selected.candidate_of_node.iter().enumerate() {
        let node_id = asap_types::stages::SelectedNodeId(node_id);
        let node = &physical.operator_nodes_of_candidates_for_memo_groups
            [physical.candidates[id.0].operator_node.0];
        if !node.is_implementation_root {
            continue; // a candidate's private node
        }
        let options = &node.candidates_for_memo_group;
        let reads = |&&CandidateForMemoGroupId { group, index }: &&CandidateForMemoGroupId| {
            producer_groups.get(&node_id).is_none_or(|read| {
                read.iter().all(|&child| {
                    child == group
                        || memo.candidates[group.0 as usize][index]
                            .input_groups
                            .contains(&child)
                })
            })
        };
        let &CandidateForMemoGroupId { group, index } =
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
    let mut seen = HashSet::new();
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

/// The planner pipeline behind the optimization-pass interface: the
/// workload's roots in entry order, PromQL roots with each series' full
/// identity as a column (the row representation per-series state needs at
/// runtime; a root with no such representation is planned over its labels
/// alone), through [`run_planning`] with every entry's accuracy target. The
/// output is the selected plan's roots, with the weight proofs plan
/// selection made applied, and its pane retention.
#[derive(Debug, Default, Clone, Copy)]
pub struct Stage1ToStage3Pass;

impl OptimizationPass for Stage1ToStage3Pass {
    fn name(&self) -> &'static str {
        "stage1-to-stage3"
    }

    fn optimize(&self, input: OptimizationInput<'_>) -> Result<PlanOutput, OptimizeError> {
        let workload = input.workload;
        let promql = workload.query_workload().language == QueryLanguage::PromQL;
        let mut roots: Vec<(usize, QueryRoot)> = workload
            .operator_indices()
            .iter()
            .zip(workload.exprs())
            .map(|(&index, node)| {
                let node = match promql {
                    true => with_promql_series_identity(node).unwrap_or_else(|_| Rc::clone(node)),
                    false => Rc::clone(node),
                };
                (index, QueryRoot::Operator(node))
            })
            .chain(
                workload
                    .scalar_roots()
                    .iter()
                    .map(|(index, expr)| (*index, QueryRoot::Scalar(expr.clone()))),
            )
            .collect();
        roots.sort_by_key(|(index, _)| *index);
        let roots = roots.into_iter().map(|(_, root)| root).collect();
        let entries: Vec<_> = workload.query_workload().entries().collect();
        let data = workload.data_workload().cloned().unwrap_or_default();
        let run = run_planning(
            roots,
            &entries,
            &data,
            input.models,
            PlanSearchSettings::default(),
        )?;
        let selection = run.selection?;
        let roots = run.selected_plan.expect("a selection has a plan");
        let mut output = PlanOutput::new(Vec::new());
        for (entry_index, root) in roots.iter().enumerate() {
            match root {
                QueryRoot::Operator(node) => output.plans.push(QueryPlan {
                    entry_index,
                    root: Rc::clone(node),
                }),
                QueryRoot::Scalar(expr) => {
                    // One plan per distinct operator the scalar reads.
                    let mut refs: Vec<&Rc<OperatorNode>> = Vec::new();
                    for node in expr.operator_refs() {
                        if !refs.iter().any(|r| Rc::ptr_eq(r, node)) {
                            refs.push(node);
                        }
                    }
                    output
                        .scalar_plans
                        .extend(refs.into_iter().map(|node| QueryPlan {
                            entry_index,
                            root: Rc::clone(node),
                        }));
                    output.scalar_roots.push((entry_index, expr.clone()));
                }
            }
        }
        output.pane_retention_ms = run
            .selected_physical_plan
            .map(|realized| realized.pane_retention_ms)
            .unwrap_or_default();
        output.selection = Some(selection);
        Ok(output)
    }
}

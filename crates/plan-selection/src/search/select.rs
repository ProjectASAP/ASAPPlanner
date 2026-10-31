//! [`select_plan`]: Stage 3's only entry. Today's engine runs behind it:
//! cost bounds over the whole catalog, then the evaluator-backed MILP
//! refinement loop (cost models 1 and 2 with a frozen calibration) or the
//! bounded objective traversal (cost model 3, or a measured cost table
//! without a frozen factor). #1095 replaces the engine; this interface stays.

use std::time::{Duration, Instant};

use asap_cost::{CostInputs, CostModel};
use asap_types::ir::{ASAPOp, Operator};
use asap_types::stages::{
    AssignmentRealizer, DagOfSelectedCandidates, PhysicalASAPMemoCandidates,
    PhysicalCandidateError, SelectedNodeId,
};
use asap_types::workload::bounds::confirmed_weights;
use asap_types::workload::{DataWorkload, QueryWorkloadEntry};

use crate::models::{AccuracyCheckInputs, SelectionModels};
use crate::report::{ErrorSelectingPlan, SearchReport, SearchStatus, Selection};
use crate::search::candidates::dag::query_roots_of;
use crate::search::candidates::graph::SelectedCandidateGraph;
use crate::search::costing::calibration::CandidateCalibration;
use crate::search::milp::model::CandidateSolveMetrics;
use crate::search::refinement::planner::CandidatePlanningContext;
use crate::search::refinement::search::{CandidateSearchLimits, CandidateSearchStatus};
use crate::WholePlanSelection;
use asap_types::physical::PhysicalWorkloadPlan;

/// The most plans the engine evaluates in one call; the deadline bounds it
/// too.
const MAX_EVALUATIONS: usize = 128;

/// The search's wall time without a deadline.
const NO_DEADLINE_WALL_TIME: Duration = Duration::from_secs(365 * 24 * 3600);

/// Stage 3, plan selection: select one plan among `physical`'s candidates.
/// Each proposed selection is realized through `realizer` and checked
/// against every query's accuracy target (`accuracy_checks`) and latency
/// bound, priced under `cost_inputs`' cost model, and the best valid one is
/// returned. `entries[q]` is query `q`'s workload entry (accuracy target,
/// latency bound, recurrence); `data` the data workload with its bounds.
/// `deadline` is the only budget.
///
/// Err: invalid cost inputs, an infeasible catalog, no plan by the
/// deadline, or the realizer rejected the selected plan
/// ([`ErrorSelectingPlan::RealizerRejected`]: a Stage 2 catalog bug).
pub fn select_plan(
    physical: &PhysicalASAPMemoCandidates,
    entries: &[QueryWorkloadEntry],
    data: &DataWorkload,
    accuracy_checks: &AccuracyCheckInputs<'_>,
    cost_inputs: &CostInputs,
    realizer: &dyn AssignmentRealizer,
    deadline: Option<Instant>,
) -> Result<Selection, ErrorSelectingPlan> {
    cost_inputs
        .validate()
        .map_err(ErrorSelectingPlan::InvalidSettings)?;
    let models = SelectionModels::new(accuracy_checks, cost_inputs);
    let calibration_policy =
        CandidateCalibration::for_models(models).map_err(ErrorSelectingPlan::Solver)?;
    // A query's root group with no implementation candidate left: Pass 1 or
    // Stage 2 pruned every one (their pruned candidates say why), so no
    // candidate is valid.
    let query_roots =
        query_roots_of(physical).ok_or(ErrorSelectingPlan::NoValidCandidate(Vec::new()))?;
    if query_roots.len() != entries.len() {
        return Err(ErrorSelectingPlan::EntryCount {
            candidate: "the physical memo".into(),
            roots: query_roots.len(),
            entries: entries.len(),
        });
    }
    // The engine's MILP runs each candidate at most once: it selects over
    // execution-scope copies and decodes the selected copies into nodes.
    let expanded =
        crate::search::candidates::scopes::expand_execution_scopes(physical, entries, deadline)
            .map_err(ErrorSelectingPlan::Solver)?;
    let mut configured = expanded.configured.clone();
    let context = CandidatePlanningContext {
        deadline,
        refine_materialization: true,
        query_roots: &query_roots,
        realizer,
        entries,
        data,
        models,
        calibration: match calibration_policy {
            CandidateCalibration::Frozen(fit) => fit,
            CandidateCalibration::CompleteCatalog => asap_cost::CalibrationFit::default(),
        },
    };
    crate::search::costing::lower_bounds::apply(&mut configured, &context)
        .map_err(ErrorSelectingPlan::Solver)?;
    let limits = CandidateSearchLimits {
        max_refinements: MAX_EVALUATIONS,
        wall_time: deadline.map_or(NO_DEADLINE_WALL_TIME, |deadline| {
            deadline.saturating_duration_since(Instant::now())
        }),
    };
    let outcome = if matches!(cost_inputs.cost_model, CostModel::ClusterCapacity(_))
        || matches!(calibration_policy, CandidateCalibration::CompleteCatalog)
    {
        let result = crate::search::refinement::objectives::optimize_objective_candidates(
            &configured,
            &context,
            limits,
            calibration_policy,
        )
        .map_err(ErrorSelectingPlan::Solver)?;
        if result.best.is_none()
            && result.status == CandidateSearchStatus::Infeasible
            && !result.evaluation_rejections.is_empty()
        {
            return Err(ErrorSelectingPlan::NoValidCandidate(
                result.evaluation_rejections,
            ));
        }
        Outcome {
            status: result.status,
            metrics: result.solve_metrics,
            validation_seconds: result.validation_seconds,
            selected: result.selected_graph,
            best: result.best,
            lower_bound: None,
        }
    } else {
        let result = crate::search::refinement::planner::optimize_configured_candidates(
            &configured,
            &context,
            limits,
        )
        .map_err(ErrorSelectingPlan::Solver)?;
        Outcome {
            status: result.search.status,
            metrics: result.search.solve_metrics,
            validation_seconds: result.search.validation_seconds,
            selected: result.search.incumbent.map(|incumbent| incumbent.selected),
            best: result.best,
            lower_bound: Some(result.search.lower_bound),
        }
    };
    let (Some((whole, _)), Some(selected)) = (outcome.best, outcome.selected) else {
        return Err(match outcome.status {
            CandidateSearchStatus::TimeLimit | CandidateSearchStatus::RefinementLimit => {
                ErrorSelectingPlan::NoPlanByDeadline(format!(
                    "the search stopped ({:?}) before it validated a plan",
                    outcome.status
                ))
            }
            CandidateSearchStatus::Infeasible => ErrorSelectingPlan::NoValidCandidate(Vec::new()),
            status => ErrorSelectingPlan::Solver(format!(
                "the search stopped without a validated plan: {status:?}"
            )),
        });
    };
    let selected_dag = expanded.decode(&selected);
    let validation_started = Instant::now();
    let nodes_with_proven_non_negative_weights =
        proven_non_negative_weights(physical, &selected_dag, realizer, data)?;
    let validation_seconds =
        outcome.validation_seconds + validation_started.elapsed().as_secs_f64();
    let cost = whole.costs[&whole.selected].clone();
    let accepted_accuracy_per_query = whole
        .accepted
        .get(&whole.selected)
        .cloned()
        .unwrap_or_default();
    let ms = |seconds: f64| (seconds * 1000.0).round() as u64;
    Ok(Selection {
        selected_dag,
        nodes_with_proven_non_negative_weights,
        cost,
        accepted_accuracy_per_query,
        // TODO(#1095): today's engine records no per-candidate prunes.
        pruned_candidates: Vec::new(),
        search_report: SearchReport {
            status: match outcome.status {
                CandidateSearchStatus::Optimal => SearchStatus::Optimal,
                _ => SearchStatus::DeadlineReached,
            },
            cost_lower_bound_per_second: outcome.lower_bound,
            milp_variables: outcome.metrics.peak_variables,
            milp_constraints: outcome.metrics.peak_constraints,
            model_construction_ms: ms(outcome.metrics.model_construction_seconds),
            solver_ms: ms(outcome.metrics.solver_seconds),
            validation_ms: ms(validation_seconds),
        },
    })
}

/// What the engine returned.
struct Outcome {
    status: CandidateSearchStatus,
    metrics: CandidateSolveMetrics,
    validation_seconds: f64,
    selected: Option<SelectedCandidateGraph>,
    best: Option<(WholePlanSelection, PhysicalWorkloadPlan)>,
    lower_bound: Option<f64>,
}

/// The selected nodes whose summary (`SummaryAgg` or `PaneChain`) reads
/// update weights `data` proves non-negative although the candidate leaves
/// the requirement open: the selected plan realized once more, each node
/// checked.
fn proven_non_negative_weights(
    physical: &PhysicalASAPMemoCandidates,
    selected: &DagOfSelectedCandidates,
    realizer: &dyn AssignmentRealizer,
    data: &DataWorkload,
) -> Result<Vec<SelectedNodeId>, ErrorSelectingPlan> {
    let realized = realizer
        .realize(physical, selected)
        .map_err(|error| match error {
            PhysicalCandidateError::RejectedAssignment(reason) => {
                ErrorSelectingPlan::RealizerRejected(reason)
            }
            error => ErrorSelectingPlan::RealizerRejected(error.to_string()),
        })?;
    Ok(realized
        .operator_node_of_selected_node
        .iter()
        .enumerate()
        .filter(|(_, node)| match &node.operator {
            Operator::ASAP(ASAPOp::SummaryAgg { input, .. } | ASAPOp::PaneChain { input, .. }) => {
                confirmed_weights(&input.weight_domain, data) != input.weight_domain
            }
            _ => false,
        })
        .map(|(node, _)| SelectedNodeId(node))
        .collect())
}

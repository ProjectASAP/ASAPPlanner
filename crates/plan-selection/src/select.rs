//! #1095 Steps 3–6: the new search behind [`select_plan`](crate::select_plan),
//! on while the internal switch [`STAGE3_SEARCH_ENV`] is `milp` (the old
//! engine stays the default until C6; there is no fallback between them).
//!
//! One MILP solve and one realization, no loop:
//! 1. the MILP's inputs (candidate dominance, owners, accuracy positions)
//!    and the model, solved once with HiGHS, the deadline as its time
//!    limit;
//! 2. the solution decoded into a [`DagOfSelectedCandidates`]
//!    ([`crate::milp::decode`]) and validated against the catalog;
//! 3. the plan realized once through the injected realizer: a rejection is
//!    a Stage 2 catalog bug ([`ErrorSelectingPlan::RealizerRejected`]);
//! 4. exact checks: `asap_cost::plan_cost` of the realized plan minus the
//!    request fees equals the objective within a relative 10⁻⁶ (otherwise an
//!    error, or, when the model encoded some term inexactly,
//!    [`SearchReport::inexact_objective`]); every query's accuracy
//!    (`satisfies`) and latency hold, or it is an error.

use std::collections::BTreeMap;
use std::time::Instant;

use asap_cost::{kept_pane_chains, node_map, plan_cost, CostInputs};
use asap_types::ir::{ASAPOp, Operator};
use asap_types::stages::{
    AssignmentRealizer, DagOfSelectedCandidates, PhysicalASAPMemoCandidates,
    PhysicalCandidateError, PhysicalCandidateId, PruneReason, PrunedCandidate, RealizedPlan,
    SelectedNodeId,
};
use asap_types::workload::bounds::confirmed_weights;
use asap_types::workload::{DataWorkload, QueryWorkloadEntry};

use crate::checks::{accuracy_check, accuracy_targets, latency_violation};
use crate::milp::decode::decode;
use crate::milp::model::{
    build_selection_milp, solve_selection_milp, SelectionMilpError, SelectionMilpOptions,
    SelectionMilpStatus,
};
use crate::models::{AccuracyCheckInputs, SelectionModels};
use crate::owners::{prepare_milp_inputs_with, MilpInputs};
use crate::report::{
    ErrorSelectingPlan, InexactObjective, Rejection, RejectionCause, SearchReport, SearchStatus,
    Selection,
};

/// The environment variable that switches Stage 3 to the new search:
/// `ASAP_STAGE3_SEARCH=milp`. Any other value, or none, keeps the old
/// engine (until #1095 C6 makes the new search the default).
pub(crate) const STAGE3_SEARCH_ENV: &str = "ASAP_STAGE3_SEARCH";

/// Whether [`STAGE3_SEARCH_ENV`] turns the new search on.
pub(crate) fn milp_search_enabled() -> bool {
    std::env::var(STAGE3_SEARCH_ENV).is_ok_and(|value| value == "milp")
}

/// The largest relative difference between the objective and `plan_cost`
/// that counts as equal.
const COST_EQUALITY_TOLERANCE: f64 = 1e-6;

/// [`select_plan`](crate::select_plan) with the new search (module
/// documentation). `cost_inputs` is already validated.
pub(crate) fn select_plan_with_milp(
    physical: &PhysicalASAPMemoCandidates,
    entries: &[QueryWorkloadEntry],
    data: &DataWorkload,
    accuracy_checks: &AccuracyCheckInputs<'_>,
    cost_inputs: &CostInputs,
    realizer: &dyn AssignmentRealizer,
    deadline: Option<Instant>,
) -> Result<Selection, ErrorSelectingPlan> {
    // The MILP's coefficients and `plan_cost` must use one calibration: a
    // measured cost table needs a frozen factor.
    if cost_inputs.frozen_calibration_factor.is_none()
        && !cost_inputs.measured_operation_costs.operators.is_empty()
    {
        return Err(ErrorSelectingPlan::UnsupportedCostModel(
            "a measured operation cost table without a frozen calibration factor".into(),
        ));
    }
    let formula_scale = cost_inputs.frozen_calibration_factor.unwrap_or(1.0);
    let models = SelectionModels::new(accuracy_checks, cost_inputs);
    // The A1 rows decide accuracy only when the built-in rule does: a model
    // or evidence that may accept an output otherwise is asked after the
    // solve.
    let enforce_accuracy = !(accuracy_checks.model.may_accept_regardless_of_guarantee()
        || accuracy_checks.evidence.may_provide_evidence());

    let construction_started = Instant::now();
    let inputs = prepare_milp_inputs_with(physical, entries, data, &models);
    let pruned_candidates = pruned_before_the_milp(physical, &inputs);
    let milp = build_selection_milp(
        physical,
        &inputs,
        entries,
        cost_inputs,
        SelectionMilpOptions { enforce_accuracy },
    )
    .map_err(selection_error)?;
    let build_ms = construction_started.elapsed().as_millis() as u64;
    let solution = solve_selection_milp(milp, deadline, build_ms).map_err(selection_error)?;

    let validation_started = Instant::now();
    let selected_dag = decode(physical, &solution);
    selected_dag.validate(physical).map_err(|reason| {
        ErrorSelectingPlan::Solver(format!("the decoded selection is invalid: {reason}"))
    })?;
    let realized = realize(physical, &selected_dag, realizer)?;
    let plan = &realized.plan;
    let recurrence: Vec<_> = entries.iter().map(|e| e.recurrence.clone()).collect();
    let ranges: Vec<_> = entries.iter().map(|e| e.range_evaluation).collect();
    let (cost, _) = plan_cost(
        &plan.dag,
        &node_map(&plan.dag),
        &kept_pane_chains(&plan.dag),
        &recurrence,
        &ranges,
        data,
        cost_inputs,
        formula_scale,
    )
    .map_err(|(node, error)| ErrorSelectingPlan::Cost {
        candidate: plan.id.clone(),
        node,
        error,
    })?;
    let plan_cost_per_second = cost.cost_per_second.ok_or_else(|| {
        ErrorSelectingPlan::UnsupportedCostModel("a cost model without a cost per second".into())
    })? - solution.request_cost_per_second;
    let objective_per_second = solution.cost_per_second;
    let difference = (objective_per_second - plan_cost_per_second).abs();
    let inexact_objective = if difference
        <= COST_EQUALITY_TOLERANCE * plan_cost_per_second.abs().max(f64::MIN_POSITIVE)
    {
        None
    } else if solution.size.inexact.is_empty() {
        return Err(ErrorSelectingPlan::ObjectiveDiffersFromPlanCost {
            objective_per_second,
            plan_cost_per_second,
        });
    } else {
        Some(InexactObjective {
            objective_per_second,
            plan_cost_per_second,
            inexact_terms: solution
                .size
                .inexact
                .iter()
                .map(|(reason, count)| (reason.to_string(), *count))
                .collect(),
        })
    };
    let accepted_accuracy_per_query =
        accuracy_check(plan, &accuracy_targets(entries), data, &models).map_err(|failure| {
            ErrorSelectingPlan::SelectedPlanFailedCheck {
                cause: failure.cause,
                reason: failure.reason,
            }
        })?;
    if let Some((cause, reason)) = latency_violation(&cost, entries) {
        return Err(ErrorSelectingPlan::SelectedPlanFailedCheck { cause, reason });
    }
    let nodes_with_proven_non_negative_weights = proven_non_negative_weights(&realized, data);
    let validation_ms = validation_started.elapsed().as_millis() as u64;
    Ok(Selection {
        selected_dag,
        nodes_with_proven_non_negative_weights,
        cost,
        accepted_accuracy_per_query,
        pruned_candidates,
        search_report: SearchReport {
            status: match solution.status {
                SelectionMilpStatus::Optimal => SearchStatus::Optimal,
                SelectionMilpStatus::TimeLimit => SearchStatus::DeadlineReached,
            },
            // The dual bound is on the objective, which leaves out the
            // request fees every plan pays; the selection's cost has them.
            cost_lower_bound_per_second: solution
                .cost_lower_bound_per_second
                .is_finite()
                .then_some(solution.cost_lower_bound_per_second + solution.request_cost_per_second),
            milp_variables: solution.size.variables,
            milp_constraints: solution.size.rows,
            model_construction_ms: solution.build_ms + solution.warm_start_ms,
            solver_ms: solution.solve_ms,
            validation_ms,
            inexact_objective,
        },
    })
}

/// The selection MILP's error as Stage 3's.
fn selection_error(error: SelectionMilpError) -> ErrorSelectingPlan {
    let infeasible = |id: String, cause: RejectionCause, reason: String| {
        ErrorSelectingPlan::NoValidCandidate(vec![Rejection { id, cause, reason }])
    };
    match error {
        SelectionMilpError::UnsupportedCostModel(model) => {
            ErrorSelectingPlan::UnsupportedCostModel(model.to_string())
        }
        SelectionMilpError::EntryCount { roots, entries } => ErrorSelectingPlan::EntryCount {
            candidate: "the physical memo".into(),
            roots,
            entries,
        },
        SelectionMilpError::Infeasible => infeasible(
            "workload".into(),
            RejectionCause::Infeasible,
            "no plan meets every query's accuracy target, latency bound and the reader \
             constraints together (the selection MILP is infeasible)"
                .into(),
        ),
        SelectionMilpError::NoPlanForQuery { query, reason } => {
            let cause = if reason.contains("bound") {
                RejectionCause::LatencyBoundExceeded
            } else {
                RejectionCause::Infeasible
            };
            infeasible(
                format!("q{}", query + 1),
                cause,
                format!("q{}: no plan even alone: {reason}", query + 1),
            )
        }
        SelectionMilpError::NoSolution { status } => ErrorSelectingPlan::NoPlanByDeadline(format!(
            "HiGHS stopped ({status}) without an integral plan"
        )),
        SelectionMilpError::DeadlineReached => {
            ErrorSelectingPlan::NoPlanByDeadline("the deadline passed before the solve".into())
        }
        SelectionMilpError::Cost(reason) | SelectionMilpError::Solver(reason) => {
            ErrorSelectingPlan::Solver(reason)
        }
    }
}

/// What Stage 3 removes before the MILP: dominated candidates, and those
/// `asap-cost` could not cost (the model leaves them out), in candidate
/// order.
fn pruned_before_the_milp(
    physical: &PhysicalASAPMemoCandidates,
    inputs: &MilpInputs,
) -> Vec<PrunedCandidate<PhysicalCandidateId>> {
    let mut reasons: BTreeMap<PhysicalCandidateId, PruneReason> = inputs
        .pruned_candidates
        .iter()
        .map(|pruned| (pruned.class, PruneReason::DominatedBy(pruned.dominated_by)))
        .collect();
    for (id, data) in inputs.candidate_data.iter().enumerate() {
        if data.usage.is_err() {
            reasons
                .entry(PhysicalCandidateId(id))
                .or_insert(PruneReason::UsageNotCostable);
        }
    }
    reasons
        .into_iter()
        .map(|(candidate, reason)| PrunedCandidate {
            group: physical.memo_group_of(candidate),
            candidate,
            reason,
        })
        .collect()
}

/// `selected` realized through `realizer`; a rejection is a Stage 2
/// catalog bug.
pub(crate) fn realize(
    physical: &PhysicalASAPMemoCandidates,
    selected: &DagOfSelectedCandidates,
    realizer: &dyn AssignmentRealizer,
) -> Result<RealizedPlan, ErrorSelectingPlan> {
    realizer
        .realize(physical, selected)
        .map_err(|error| match error {
            PhysicalCandidateError::RejectedAssignment(reason) => {
                ErrorSelectingPlan::RealizerRejected(reason)
            }
            error => ErrorSelectingPlan::RealizerRejected(error.to_string()),
        })
}

/// The selected nodes whose summary (`SummaryAgg` or `PaneChain`) reads
/// update weights `data` proves non-negative although the candidate leaves
/// the requirement open.
pub(crate) fn proven_non_negative_weights(
    realized: &RealizedPlan,
    data: &DataWorkload,
) -> Vec<SelectedNodeId> {
    realized
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
        .collect()
}

#[cfg(test)]
mod tests;

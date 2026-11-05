//! Stage 3, plan selection: [`select_plan`] runs the group selection MILP
//! (#1095). Inputs it does not price yet (cost models 2 and 3, or a measured
//! operation cost table without a frozen calibration factor) are an
//! `UnsupportedCostModel` error (#888).
//!
//! One MILP solve and one realization, no loop:
//! 1. the MILP's inputs (candidate dominance, execution contexts, accuracy
//!    weights) and the model, solved once with HiGHS from a warm start to
//!    proven optimality (no time limit);
//! 2. the solution decoded into a [`DagOfSelectedCandidates`]
//!    ([`crate::milp::decode`]) and validated against the catalog;
//! 3. the plan realized once through the injected realizer: a rejection is
//!    a Stage 2 catalog bug ([`ErrorSelectingPlan::RealizerRejected`]);
//! 4. exact checks: `asap_cost::plan_cost` of the realized plan minus the
//!    request fees equals the objective within a relative 10⁻⁶ (otherwise
//!    an error, or, when the model encoded some term inexactly,
//!    [`SearchReport::inexact_objective`]); every query's accuracy
//!    (`satisfies`) and latency hold, or it is an error.
//!
//! With `ASAP_STAGE3_REPORT=<path>`, one JSON line per run is appended
//! there: the model's size per family and per scope (the shareable
//! candidates' contexts apart), the timings, the warm start's cost, the
//! objective, `plan_cost` and the dual bound (measurement only).

use std::collections::BTreeMap;
use std::time::Instant;

use asap_cost::{kept_tumbling_window_chains, node_map, plan_cost, CostInputs};
use asap_types::ir::{ASAPOp, Operator};
use asap_types::stages::{
    AssignmentRealizer, DagOfSelectedCandidates, PhysicalASAPMemoCandidates,
    PhysicalCandidateError, PhysicalCandidateId, PruneReason, PrunedCandidate, RealizedPlan,
    SelectedNodeId,
};
use asap_types::workload::bounds::confirmed_weights;
use asap_types::workload::{DataWorkload, QueryWorkloadEntry};

use crate::checks::{accuracy_check, accuracy_targets, latency_violation};
use crate::groups::{prepare_milp_inputs_with, MilpInputs};
use crate::milp::decode::{decode, ChosenCandidates};
use crate::milp::model::{
    build_selection_milp, solve_selection_milp, SelectionMilpError, SelectionMilpOptions,
};
use crate::models::{AccuracyCheckInputs, SelectionModels};
use crate::report::{
    ErrorSelectingPlan, InexactObjective, Rejection, RejectionCause, SearchReport, SearchStatus,
    Selection,
};

/// The largest relative difference between the objective and `plan_cost`
/// that counts as equal.
const COST_EQUALITY_TOLERANCE: f64 = 1e-6;

/// Stage 3, plan selection: select one plan among `physical`'s candidates
/// with the group selection MILP (module documentation). The selection is
/// realized through `realizer`, checked against every query's accuracy
/// target (`accuracy_checks`) and latency bound, and priced under
/// `cost_inputs`. `entries[q]` is query `q`'s workload entry (accuracy
/// target, latency bound, recurrence); `data` the data workload with its
/// bounds. The solve has no time limit: it runs to proven optimality.
///
/// Err: invalid or unsupported cost inputs, an infeasible catalog, a
/// solver failure, or the realizer rejected the selected plan
/// ([`ErrorSelectingPlan::RealizerRejected`]: a Stage 2 catalog bug).
pub fn select_plan(
    physical: &PhysicalASAPMemoCandidates,
    entries: &[QueryWorkloadEntry],
    data: &DataWorkload,
    accuracy_checks: &AccuracyCheckInputs<'_>,
    cost_inputs: &CostInputs,
    realizer: &dyn AssignmentRealizer,
) -> Result<Selection, ErrorSelectingPlan> {
    cost_inputs
        .validate()
        .map_err(ErrorSelectingPlan::InvalidSettings)?;
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
        SelectionMilpOptions {
            enforce_accuracy,
            ..SelectionMilpOptions::default()
        },
    )
    .map_err(selection_error)?;
    let build_ms = construction_started.elapsed().as_millis() as u64;
    let solution = solve_selection_milp(milp, build_ms).map_err(|error| {
        with_uncostable_reasons(selection_error(error), &inputs, &pruned_candidates)
    })?;

    let validation_started = Instant::now();
    let chosen = ChosenCandidates {
        chosen: solution.chosen.clone(),
        selected: solution.selected.clone(),
        slot_reads: solution.slot_reads.clone(),
    };
    let selected_dag = decode(physical, &inputs, &chosen).map_err(|reason| {
        ErrorSelectingPlan::Solver(format!("the selection does not decode: {reason}"))
    })?;
    selected_dag.validate(physical).map_err(|reason| {
        ErrorSelectingPlan::Solver(format!("the decoded selection is invalid: {reason}"))
    })?;
    let realized = realize(physical, &selected_dag, realizer)?;
    let plan = &realized.plan;
    let recurrence: Vec<_> = entries.iter().map(|e| e.recurrence.clone()).collect();
    let ranges: Vec<_> = entries.iter().map(|e| e.range_evaluation).collect();
    let cost_error = |(node, error)| ErrorSelectingPlan::Cost {
        candidate: plan.id.clone(),
        node,
        error,
    };
    let kept_chains = kept_tumbling_window_chains(&plan.dag).map_err(cost_error)?;
    let (cost, _) = plan_cost(
        &plan.dag,
        &node_map(&plan.dag),
        &kept_chains,
        &recurrence,
        &ranges,
        data,
        cost_inputs,
        formula_scale,
    )
    .map_err(cost_error)?;
    let plan_cost_per_second = cost.cost_per_second.ok_or_else(|| {
        ErrorSelectingPlan::UnsupportedCostModel("a cost model without a cost per second".into())
    })? - solution.request_cost_per_second;
    let objective_per_second = solution.cost_per_second;
    let difference = (objective_per_second - plan_cost_per_second).abs();
    let tolerance = COST_EQUALITY_TOLERANCE * plan_cost_per_second.abs().max(f64::MIN_POSITIVE);
    let within = difference <= tolerance;
    report_run(&solution, objective_per_second, plan_cost_per_second);
    let inexact_objective = if within {
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
            status: SearchStatus::Optimal,
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
        SelectionMilpError::Cost(reason) | SelectionMilpError::Solver(reason) => {
            ErrorSelectingPlan::Solver(reason)
        }
    }
}

/// An infeasible selection with the reasons `asap-cost` could not cost
/// the candidates the model left out (e.g. a source missing the fact its
/// pricing needs), each once in candidate order: those candidates may be
/// why no plan is left.
fn with_uncostable_reasons(
    error: ErrorSelectingPlan,
    inputs: &MilpInputs,
    pruned_candidates: &[PrunedCandidate<PhysicalCandidateId>],
) -> ErrorSelectingPlan {
    let ErrorSelectingPlan::NoValidCandidate(mut rejections) = error else {
        return error;
    };
    let mut reasons: Vec<&str> = Vec::new();
    for pruned in pruned_candidates {
        if pruned.reason != PruneReason::UsageNotCostable {
            continue;
        }
        if let Err(reason) = &inputs.candidate_data[pruned.candidate.0].usage {
            if !reasons.contains(&reason.as_str()) {
                reasons.push(reason);
            }
        }
    }
    if let Some(rejection) = rejections.first_mut() {
        if !reasons.is_empty() {
            rejection.reason = format!(
                "{}; candidates left out because they could not be costed: {}",
                rejection.reason,
                reasons.join("; ")
            );
        }
    }
    ErrorSelectingPlan::NoValidCandidate(rejections)
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
fn realize(
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

/// The selected nodes whose summary (`SummaryAgg` or `SummaryTumblingWindowChain`) reads
/// update weights `data` proves non-negative although the candidate leaves
/// the requirement open.
fn proven_non_negative_weights(
    realized: &RealizedPlan,
    data: &DataWorkload,
) -> Vec<SelectedNodeId> {
    realized
        .operator_node_of_selected_node
        .iter()
        .enumerate()
        .filter(|(_, node)| match &node.operator {
            Operator::ASAP(
                ASAPOp::SummaryAgg { input, .. } | ASAPOp::SummaryTumblingWindowChain { input, .. },
            ) => confirmed_weights(&input.weight_domain, data) != input.weight_domain,
            _ => false,
        })
        .map(|(node, _)| SelectedNodeId(node))
        .collect()
}

#[cfg(test)]
mod tests;

/// Appends one JSON line describing this run to `ASAP_STAGE3_REPORT`, if
/// set (measurement only; failures to write are ignored).
fn report_run(
    solution: &crate::milp::model::SelectionMilpSolution,
    objective_per_second: f64,
    plan_cost_per_second: f64,
) {
    let Ok(path) = std::env::var("ASAP_STAGE3_REPORT") else {
        return;
    };
    let line = serde_json::json!({
        "variables": solution.size.variables,
        "binaries": solution.size.binaries,
        "rows": solution.size.rows,
        "nonzeros": solution.size.nonzeros,
        "scopes": solution.size.scopes.iter().map(|(scope, size)| (format!("{scope:?}"), size)).collect::<BTreeMap<_, _>>(),
        "families": solution.size.families,
        "inexact": solution.size.inexact,
        "build_ms": solution.build_ms,
        "warm_start_ms": solution.warm_start_ms,
        "solve_ms": solution.solve_ms,
        "warm_start_cost_per_second": solution.warm_start_cost_per_second,
        "objective_per_second": objective_per_second,
        "plan_cost_per_second": plan_cost_per_second,
        "dual_bound_per_second": solution.cost_lower_bound_per_second,
        "parts": solution.parts,
    });
    use std::io::Write;
    if let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
    {
        let _ = writeln!(file, "{line}");
    }
}

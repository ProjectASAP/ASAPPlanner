//! Stage 3, plan selection: [`select_plan`] prices every physical candidate
//! ($w_v$, [`crate::candidate_cost`]), builds and solves the baseline
//! selection MILP over Stage 2's candidates and admissible edges
//! ([`crate::milp::model`]), decodes $\{x = 1\}$, $\{y = 1\}$ and
//! $\{z = 1\}$ into a
//! [`DagOfSelectedCandidates`](asap_types::stages::DagOfSelectedCandidates),
//! and realizes it once through the injected realizer.

use std::collections::BTreeMap;

use asap_accuracy::AccuracyModel;
use asap_cost::{materialized_tumbling_window_chains, node_map, plan_cost, CostInputs, CostModel};
use asap_types::ir::schema::FieldDataType;
use asap_types::ir::{ASAPOp, OperatorNode, QueryRoot};
use asap_types::stages::{
    AssignmentRealizer, PhysicalASAPMemoCandidates, PhysicalCandidateError, PhysicalCandidateId,
    PruneReason, PrunedCandidate, RealizedPlan,
};
use asap_types::workload::{DataWorkload, QueryWorkloadEntry};

use crate::candidate_cost::candidate_costs;
use crate::milp::model::{solve_selection, SelectionInputs, SelectionMilpError};
use crate::models::AccuracyCheckInputs;
use crate::report::{
    AcceptedAccuracy, ErrorSelectingPlan, InexactObjective, Rejection, RejectionCause,
    SearchReport, SearchStatus, Selection,
};

/// Relative difference under which the MILP's objective and `plan_cost`
/// of the selected plan count as equal.
const COST_EQUALITY_TOLERANCE: f64 = 1e-6;

/// The name of the inexact term of the baseline objective, with the
/// number of selected candidates it covers in [`InexactObjective`].
const FIXED_COST_PER_CANDIDATE: &str =
    "fixed cost per candidate, whichever consumers select it (execution contexts: #1207 PR 7)";

/// Stage 3, plan selection: select one plan among `physical`'s candidates
/// (module documentation). The selection is realized through `realizer`
/// and priced under `cost_inputs` (cost model 1 only). `entries[q]` is
/// query `q`'s workload entry; `data` the data workload with its bounds.
///
/// Accuracy: until constraint (K) (#1207 PR 8), a query's accuracy rests on
/// the parameter floors of its edges, which Pass 2 derived from the query's
/// target ((G): an edge from a producer below its floor is not admissible);
/// `accuracy_checks.model` may accept an output regardless of its
/// guarantee, which the report records.
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
    if !matches!(cost_inputs.cost_model, CostModel::UsageOverTime(_)) {
        return Err(ErrorSelectingPlan::UnsupportedCostModel(
            "cost models 2 and 3 (the selection MILP implements cost model 1 only)".into(),
        ));
    }
    // The MILP's coefficients and `plan_cost` must use one calibration: a
    // measured cost table needs a frozen factor.
    if cost_inputs.frozen_calibration_factor.is_none()
        && !cost_inputs.measured_operation_costs.operators.is_empty()
    {
        return Err(ErrorSelectingPlan::UnsupportedCostModel(
            "a measured operation cost table without a frozen calibration factor".into(),
        ));
    }
    if entries.len() != physical.query_roots.len() {
        return Err(ErrorSelectingPlan::EntryCount {
            candidate: "the physical memo".into(),
            roots: physical.query_roots.len(),
            entries: entries.len(),
        });
    }
    let formula_scale = cost_inputs.frozen_calibration_factor.unwrap_or(1.0);
    let costs = candidate_costs(physical, entries, data, cost_inputs, formula_scale);
    let pruned_candidates: Vec<PrunedCandidate<PhysicalCandidateId>> = costs
        .iter()
        .enumerate()
        .filter(|(_, cost)| cost.is_err())
        .map(|(id, _)| PrunedCandidate {
            group: physical.physical_candidates[id].logical_candidate_id.group,
            candidate: PhysicalCandidateId(id),
            reason: PruneReason::UsageNotCostable,
        })
        .collect();
    let cost_per_second: Vec<Option<f64>> = costs.iter().map(|cost| cost.clone().ok()).collect();

    let solved = solve_selection(&SelectionInputs {
        physical,
        cost_per_second: &cost_per_second,
        entries,
    })
    .map_err(|error| match error {
        SelectionMilpError::Infeasible => {
            ErrorSelectingPlan::NoValidCandidate(vec![infeasible(&costs)])
        }
        SelectionMilpError::Solver(reason) => ErrorSelectingPlan::Solver(reason),
    })?;
    let validation_started = std::time::Instant::now();
    solved.selected_dag.validate(physical).map_err(|reason| {
        ErrorSelectingPlan::Solver(format!("the decoded selection is invalid: {reason}"))
    })?;
    let realized =
        realizer
            .realize(physical, &solved.selected_dag)
            .map_err(|error| match error {
                PhysicalCandidateError::RejectedAssignment(reason) => {
                    ErrorSelectingPlan::RealizerRejected(reason)
                }
                error => ErrorSelectingPlan::RealizerRejected(error.to_string()),
            })?;
    let plan = &realized.plan;
    let recurrence: Vec<_> = entries.iter().map(|e| e.recurrence.clone()).collect();
    let ranges: Vec<_> = entries.iter().map(|e| e.range_evaluation).collect();
    let (cost, _) = plan_cost(
        &plan.dag,
        &node_map(&plan.dag),
        &materialized_tumbling_window_chains(&plan.dag),
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
    })?;
    let objective_per_second = solved.objective_per_second;
    let difference = (objective_per_second - plan_cost_per_second).abs();
    let tolerance = COST_EQUALITY_TOLERANCE * plan_cost_per_second.abs().max(f64::MIN_POSITIVE);
    let inexact_objective = (difference > tolerance).then(|| InexactObjective {
        objective_per_second,
        plan_cost_per_second,
        inexact_terms: BTreeMap::from([(
            FIXED_COST_PER_CANDIDATE.to_string(),
            solved.selected_dag.candidate_of_node.len(),
        )]),
    });
    let accepted_accuracy_per_query = accepted_accuracy(&realized, entries, accuracy_checks.model);
    let validation_ms = validation_started.elapsed().as_millis() as u64;
    Ok(Selection {
        selected_dag: solved.selected_dag,
        cost,
        accepted_accuracy_per_query,
        pruned_candidates,
        search_report: SearchReport {
            status: SearchStatus::Optimal,
            cost_lower_bound_per_second: solved
                .lower_bound_per_second
                .is_finite()
                .then_some(solved.lower_bound_per_second),
            milp_variables: solved.variable_count,
            milp_constraints: solved.constraint_count,
            model_construction_ms: solved.construction_ms,
            solver_ms: solved.solver_ms,
            validation_ms,
            inexact_objective,
        },
    })
}

/// The rejection of an infeasible selection: no plan meets (A)–(I), with
/// the reasons `asap-cost` could not cost the candidates the model left
/// out, each once in candidate order: those candidates may be why no plan
/// is left.
fn infeasible(costs: &[Result<f64, String>]) -> Rejection {
    let mut reasons: Vec<&str> = Vec::new();
    for reason in costs.iter().filter_map(|cost| cost.as_ref().err()) {
        if !reasons.contains(&reason.as_str()) {
            reasons.push(reason);
        }
    }
    let mut reason = "no plan gives every query output a candidate whose every input has an \
                      admissible producer (the selection MILP is infeasible)"
        .to_string();
    if !reasons.is_empty() {
        reason += &format!(
            "; candidates left out because they could not be costed: {}",
            reasons.join("; ")
        );
    }
    Rejection {
        id: "workload".into(),
        cause: RejectionCause::Infeasible,
        reason,
    }
}

/// Per query, the accuracy accepted for the selected plan. An output that
/// reads no sketch is exact. Otherwise, until constraint (K) (#1207 PR 8)
/// checks the composed bound at the root, the output is accepted in its
/// target's metric through the parameter floors of its edges (Pass 2's
/// metric rule admits only edges whose metric converts to it), unless the
/// deployment's model accepts it regardless of its guarantee.
fn accepted_accuracy(
    realized: &RealizedPlan,
    entries: &[QueryWorkloadEntry],
    model: &dyn AccuracyModel,
) -> Vec<AcceptedAccuracy> {
    realized
        .plan
        .roots
        .iter()
        .zip(entries)
        .map(|(root, entry)| {
            let target = &entry.requirements.accuracy;
            let sketches = sketches_read(root);
            let exact = sketches.is_empty();
            let regardless = !exact && model.accepts_output_regardless_of_guarantee(root, target);
            AcceptedAccuracy {
                target_metric: target.accuracy_metric,
                accepted_metric: (!exact && !regardless)
                    .then_some(target.accuracy_metric)
                    .flatten(),
                accepted_regardless_of_guarantee: regardless,
                guarantee_check_rejection: None,
                sketches,
            }
        })
        .collect()
}

/// The sketch algorithms of the summary builds `root` reads, in plan order
/// (depth first, inputs in order), each build once.
fn sketches_read(root: &QueryRoot) -> Vec<String> {
    let mut seen = std::collections::HashSet::new();
    let mut sketches = Vec::new();
    let mut pending: Vec<std::rc::Rc<OperatorNode>> = match root {
        QueryRoot::Operator(node) => vec![std::rc::Rc::clone(node)],
        QueryRoot::Scalar(expression) => expression
            .operator_refs()
            .into_iter()
            .rev()
            .cloned()
            .collect(),
    };
    while let Some(node) = pending.pop() {
        if !seen.insert(std::rc::Rc::as_ptr(&node)) {
            continue;
        }
        if let Some(
            ASAPOp::SummaryAgg {
                family: FieldDataType::Sketch(algorithm, _),
                ..
            }
            | ASAPOp::SummaryTumblingWindowChain {
                family: FieldDataType::Sketch(algorithm, _),
                ..
            },
        ) = node.asap()
        {
            sketches.push(format!("{algorithm:?}"));
        }
        pending.extend(node.children().into_iter().rev().cloned());
    }
    sketches
}

//! Stage 3, plan selection: [`select_plan`] finds every execution of a
//! candidate in a context and every edge between executions the plan may
//! select ([`crate::milp::execution_contexts`]), costs them
//! ([`crate::cost`]), builds and solves the selection MILP over them
//! ([`crate::milp::model`]), and builds the plan from the selected
//! executions and edges as its last step ([`crate::plan`]).

use std::collections::BTreeSet;

use asap_cost::{summary_type_name, CostInputs, CostModel};
use asap_types::ir::physical_export::{PhysicalASAPDAG, PhysicalASAPNodeId};
use asap_types::ir::schema::FieldDataType;
use asap_types::ir::{ASAPOp, Operator, QueryRoot};
use asap_types::stages::{
    output_references, MemoGroupId, PhysicalASAPMemoCandidates, PruneReason, PrunedCandidate,
};
use asap_types::workload::{DataWorkload, QueryWorkloadEntry};

use crate::cost::{execution_costs, ExecutionCosts};
use crate::milp::execution_contexts::{Execution, ExecutionContexts};
use crate::milp::model::{solve_selection, QueryId, SelectionInputs, SelectionMilpError};
use crate::plan::plan_of;
use crate::report::{AcceptedAccuracy, ErrorSelectingPlan, Rejection, RejectionCause, Selection};

/// Stage 3, plan selection: select one plan among `physical`'s candidates
/// (module documentation) under `cost_inputs`, and build it: one
/// `PhysicalASAPDAG` node per selected execution, its inputs from the
/// selected edges, each node's `selected_physical_candidate` set.
/// `entries[q]` is query `q`'s workload entry; `data` the data workload
/// with its bounds.
///
/// Err: invalid or unsupported cost inputs, an infeasible catalog, or a
/// solver failure.
pub fn select_plan(
    physical: &PhysicalASAPMemoCandidates,
    entries: &[QueryWorkloadEntry],
    data: &DataWorkload,
    cost_inputs: &CostInputs,
) -> Result<(PhysicalASAPDAG, Selection), ErrorSelectingPlan> {
    cost_inputs
        .validate()
        .map_err(ErrorSelectingPlan::InvalidSettings)?;
    if !matches!(cost_inputs.cost_model, CostModel::UsageOverTime(_)) {
        return Err(ErrorSelectingPlan::UnsupportedCostModel(
            "cost models 2 and 3 (the selection MILP implements cost model 1 only)".into(),
        ));
    }
    let queries = &physical.logical_candidates.memo.queries;
    if entries.len() != queries.len() {
        return Err(ErrorSelectingPlan::EntryCount {
            candidate: "the physical memo".into(),
            roots: queries.len(),
            entries: entries.len(),
        });
    }
    let formula_scale = cost_inputs.frozen_calibration_factor.unwrap_or(1.0);
    let roots: Vec<QueryRoot<MemoGroupId>> =
        queries.iter().map(|query| query.root.clone()).collect();
    let outputs: Vec<(QueryId, MemoGroupId)> = output_references(&roots)
        .into_iter()
        .map(|(query, group)| (QueryId(query), group))
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    let (contexts, output_executions) = ExecutionContexts::new(physical, entries, &outputs);
    let costs = execution_costs(
        physical,
        &contexts,
        &output_executions,
        entries,
        data,
        cost_inputs,
        formula_scale,
    );
    let query_outputs: Vec<(QueryId, Vec<Execution>)> = output_executions
        .into_iter()
        .map(|(query, executions)| {
            let costed = executions
                .into_iter()
                .filter(|&execution| costs.per_execution[contexts.index_of(execution)].is_ok())
                .collect();
            (query, costed)
        })
        .collect();
    if query_outputs
        .iter()
        .any(|(_, executions)| executions.is_empty())
    {
        return Err(infeasible(&costs));
    }
    let solved = solve_selection(&SelectionInputs {
        physical,
        entries,
        contexts: &contexts,
        costs: &costs,
        query_outputs: &query_outputs,
    })
    .map_err(|error| match error {
        SelectionMilpError::Infeasible => infeasible(&costs),
        SelectionMilpError::Solver(reason) => ErrorSelectingPlan::Solver(reason),
    })?;
    let plan = plan_of(physical, &solved, &outputs).map_err(|error| {
        ErrorSelectingPlan::Solver(format!("the selected plan is invalid: {error}"))
    })?;
    // A candidate none of whose executions could be costed.
    let mut costed = vec![None; physical.physical_candidates.len()];
    for (execution, cost) in contexts.executions().iter().zip(&costs.per_execution) {
        let known = costed[execution.candidate.0].get_or_insert(false);
        *known |= cost.is_ok();
    }
    let selection = Selection {
        accepted_accuracy_per_query: accepted_accuracy(physical, &plan),
        pruned_candidates: costed
            .iter()
            .zip(&physical.physical_candidates)
            .filter(|(costed, _)| **costed == Some(false))
            .map(|(_, &candidate)| PrunedCandidate {
                candidate,
                reason: PruneReason::UsageNotCostable,
            })
            .collect(),
        milp_variables: solved.variables,
        milp_constraints: solved.constraints,
        milp_construction_ms: solved.construction_ms,
        milp_solve_ms: solved.solve_ms,
        milp_objective: solved.objective_per_second,
    };
    Ok((plan, selection))
}

/// The selection MILP is infeasible, with the reasons executions could not
/// be costed.
fn infeasible(costs: &ExecutionCosts) -> ErrorSelectingPlan {
    let mut reasons: Vec<&str> = Vec::new();
    for reason in costs
        .per_execution
        .iter()
        .filter_map(|cost| cost.as_ref().err())
    {
        if !reasons.contains(&reason.as_str()) {
            reasons.push(reason);
        }
    }
    let mut reason = "no plan gives every query output a candidate whose every input has an \
                      admissible producer that delivers on its context (the selection MILP is \
                      infeasible)"
        .to_string();
    if !reasons.is_empty() {
        reason += &format!(
            "; executions left out because they could not be costed: {}",
            reasons.join("; ")
        );
    }
    ErrorSelectingPlan::NoValidCandidate(vec![Rejection {
        id: "workload".into(),
        cause: RejectionCause::Infeasible,
        reason,
    }])
}

/// Per query, the sketches its output reads in `plan` (each sketch build
/// below its root, in node order) and, when it reads one, its target's
/// metric.
fn accepted_accuracy(
    physical: &PhysicalASAPMemoCandidates,
    plan: &PhysicalASAPDAG,
) -> Vec<AcceptedAccuracy> {
    let mut operator_roots = plan.roots.iter();
    physical
        .logical_candidates
        .memo
        .queries
        .iter()
        .enumerate()
        .map(|(query, memo_query)| {
            let starts: Vec<PhysicalASAPNodeId> = match plan
                .scalar_roots
                .iter()
                .find(|(position, _)| *position == query)
            {
                Some((_, expression)) => expression.operator_refs().into_iter().copied().collect(),
                None => vec![*operator_roots.next().expect("one operator root per query")],
            };
            let sketches = sketches_below(plan, starts);
            AcceptedAccuracy {
                accepted_metric: (!sketches.is_empty())
                    .then(|| memo_query.accuracy_target.as_ref()?.accuracy_metric)
                    .flatten(),
                sketches,
            }
        })
        .collect()
}

/// The summary type names of the sketch builds at or below `starts` in
/// `plan`, in node order, each once.
fn sketches_below(plan: &PhysicalASAPDAG, starts: Vec<PhysicalASAPNodeId>) -> Vec<String> {
    let mut below = BTreeSet::new();
    let mut pending = starts;
    while let Some(id) = pending.pop() {
        if below.insert(id) {
            pending.extend(
                plan.edges
                    .iter()
                    .filter(|e| e.consumer == id)
                    .map(|e| e.producer),
            );
        }
    }
    let mut sketches: Vec<String> = Vec::new();
    for node in plan.nodes.iter().filter(|node| below.contains(&node.id)) {
        if let Operator::ASAP(
            ASAPOp::SummaryAgg { summary_type, .. }
            | ASAPOp::SummaryTumblingWindowChain { summary_type, .. },
        ) = &node.payload
        {
            if matches!(summary_type, FieldDataType::Sketch(_)) {
                let name = summary_type_name(summary_type);
                if !sketches.contains(&name) {
                    sketches.push(name);
                }
            }
        }
    }
    sketches
}

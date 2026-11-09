//! Stage 3, plan selection: [`select_plan`] costs every physical candidate
//! and selectable edge ([`crate::cost`]), builds and solves the baseline
//! selection MILP over Stage 2's candidates and admissible edges
//! ([`crate::milp::model`]), and builds the plan from the selected
//! candidates and edges as its last step ([`crate::plan`]).

use std::collections::BTreeSet;

use asap_cost::{summary_type_name, CostInputs, CostModel};
use asap_types::ir::physical_export::{PhysicalASAPDAG, PhysicalASAPNodeId};
use asap_types::ir::schema::FieldDataType;
use asap_types::ir::{ASAPOp, Operator, QueryRoot};
use asap_types::physical::ExecutionTiming;
use asap_types::stages::{
    output_references, producer_satisfies_physical_requirements, EvaluationGrid, ExecutionContext,
    MemoGroupId, PhysicalASAPMemoCandidates, PhysicalCandidate, PhysicalCandidateId, PhysicalEdge,
    PhysicalEdgeId, PruneReason, PrunedCandidate,
};
use asap_types::workload::{DataWorkload, QueryWorkloadEntry};

use crate::cost::{selection_costs, SelectionCosts};
use crate::milp::model::{solve_selection, QueryId, SelectionInputs, SelectionMilpError};
use crate::plan::plan_of;
use crate::report::{AcceptedAccuracy, ErrorSelectingPlan, Rejection, RejectionCause, Selection};

/// Stage 3, plan selection: select one plan among `physical`'s candidates
/// (module documentation) under `cost_inputs`, and build it: one
/// `PhysicalASAPDAG` node per selected candidate, its inputs from the
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
    let selectable: Vec<PhysicalEdgeId> = physical
        .edges()
        .filter(|(_, edge)| edge.is_admissible() && delivers_to_consumer(physical, entries, edge))
        .map(|(id, _)| id)
        .collect();
    let costs = selection_costs(
        physical,
        &selectable,
        entries,
        data,
        cost_inputs,
        formula_scale,
    );
    let roots: Vec<QueryRoot<MemoGroupId>> =
        queries.iter().map(|query| query.root.clone()).collect();
    let outputs: Vec<(QueryId, MemoGroupId)> = output_references(&roots)
        .into_iter()
        .map(|(query, group)| (QueryId(query), group))
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    let query_outputs: Vec<(QueryId, Vec<PhysicalCandidateId>)> = outputs
        .iter()
        .map(|&(query, group)| {
            let candidates = physical
                .physical_candidates
                .iter()
                .enumerate()
                .filter(|(v, candidate)| {
                    candidate.logical_candidate_id.group == group
                        && costs.candidate[*v].is_ok()
                        && producer_satisfies_physical_requirements(
                            candidate,
                            ExecutionTiming::QueryTime,
                        )
                        && candidate.delivers_on(physical, &evaluation_context(&entries[query.0]))
                })
                .map(|(v, _)| PhysicalCandidateId(v))
                .collect();
            (query, candidates)
        })
        .collect();
    if query_outputs
        .iter()
        .any(|(_, candidates)| candidates.is_empty())
    {
        return Err(infeasible(&costs));
    }
    let solved = solve_selection(&SelectionInputs {
        physical,
        costs: &costs,
        query_outputs: &query_outputs,
    })
    .map_err(|error| match error {
        SelectionMilpError::Infeasible => infeasible(&costs),
        SelectionMilpError::Solver(reason) => ErrorSelectingPlan::Solver(reason),
    })?;
    let plan = plan_of(physical, entries, &solved, &outputs).map_err(|error| {
        ErrorSelectingPlan::Solver(format!("the selected plan is invalid: {error}"))
    })?;
    let selection = Selection {
        accepted_accuracy_per_query: accepted_accuracy(physical, &plan),
        pruned_candidates: costs
            .candidate
            .iter()
            .zip(&physical.physical_candidates)
            .filter(|(cost, _)| cost.is_err())
            .map(|(_, &candidate)| PrunedCandidate {
                candidate,
                reason: PruneReason::UsageNotCostable,
            })
            .collect(),
        milp_variables: solved.variables,
        milp_constraints: solved.constraints,
        milp_construction_ms: solved.construction_ms,
        milp_solve_ms: solved.solve_ms,
    };
    Ok((plan, selection))
}

/// The evaluation context of `entry`'s query: the grid its evaluations
/// fall on (`QueryRecurrence::evaluation_grid_ms`), anchored at the epoch.
/// Times that are not known may fall on any millisecond: a 1 ms grid, on
/// which only producers stored at every time deliver.
fn evaluation_context(entry: &QueryWorkloadEntry) -> ExecutionContext {
    ExecutionContext::Evaluation {
        grid: EvaluationGrid {
            period_ms: entry
                .recurrence
                .evaluation_grid_ms(entry.range_evaluation)
                .unwrap_or(1),
            phase_ms: 0,
        },
    }
}

/// Whether `edge`'s producer delivers on every evaluation its query-time
/// consumer serves: on the evaluation context of each query reaching the
/// consumer's group ([`PhysicalCandidate::delivers_on`]). The baseline has
/// no other context: an ingestion-time consumer's (its pipeline) is checked
/// in #1207 PR 7.
fn delivers_to_consumer(
    physical: &PhysicalASAPMemoCandidates,
    entries: &[QueryWorkloadEntry],
    edge: &PhysicalEdge,
) -> bool {
    let producer: &PhysicalCandidate = &physical.physical_candidates[edge.producer_candidate_id.0];
    let consumer = &physical.physical_candidates[edge.consumer_candidate_id.0];
    if consumer.physical_properties.timing() != ExecutionTiming::QueryTime {
        return true;
    }
    physical.logical_candidates.memo.groups()[consumer.logical_candidate_id.group.0 as usize]
        .reached_by_queries()
        .iter()
        .filter_map(|&query| entries.get(query))
        .all(|entry| producer.delivers_on(physical, &evaluation_context(entry)))
}

/// The selection MILP is infeasible, with the reasons candidates could not
/// be costed.
fn infeasible(costs: &SelectionCosts) -> ErrorSelectingPlan {
    let mut reasons: Vec<&str> = Vec::new();
    for reason in costs
        .candidate
        .iter()
        .filter_map(|cost| cost.as_ref().err())
    {
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

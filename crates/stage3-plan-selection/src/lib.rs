//! Stage 3, plan selection
//! (`docs/design_docs/concepts/memo-groups-properties-requirements.md`,
//! section 3, "Stage 3: baseline selection and plan output", and sections
//! 4 and 5).
//!
//! [`select_plan`] reads Stage 2's physical candidates $\mathcal C$ and the
//! admissible part of the physical edges $\mathcal E$, finds every
//! execution of a candidate in an execution context $\kappa$ and every edge
//! between executions the plan may select ([`selectable`]), costs them with
//! `asap-cost` ([`cost`]), and selects with a MILP ([`milp`]) over
//! $x_{\kappa,v}$ per execution (an `AcrossEvaluations` candidate runs
//! once, $x^{\mathrm{mat}}_v$), $y_{\kappa,u,c,i}$, $z_{q,v}$, $t$, $r_s$ and
//! the retention $h_v$, under constraints (A)–(F) per context, the
//! one-consumer rule, (R) ((G) and (H) hold because $y$ exists only on
//! admissible edges whose producer delivers on the consumer's context) and
//! (X): a query reads the groups Pass 2 lists as exact-only for it through
//! exact candidates only, $a_{q,e}$ marking what its selected path reads. It
//! builds the plan (`PhysicalASAPDAG`) from the selected executions and
//! edges as its last step ([`plan`], each node retaining its candidate's
//! $h_v$, the needs from [`retention`]). Accuracy at each query root,
//! constraint (K), with per-candidate error bounds $E_c$ and $D_c$
//! (section 4), is the remaining extension. Costs come from `asap-cost`.

mod cost;
mod milp;
mod plan;
mod report;
mod retention;
mod selectable;

use asap_cost::{request_overhead_cost_per_second, CostInputs, CostModel};
use asap_types::plan::physical_asap_dag::PhysicalASAPDAG;
use asap_types::stages::{PhysicalASAPMemoCandidates, PruneReason, PrunedCandidate};
use asap_types::workload::{DataWorkload, QueryWorkloadEntry};

use crate::cost::execution_costs;
use crate::milp::model::{solve_selection, QueryId, SelectionInputs, SelectionMilpError};
use crate::plan::plan_of;
use crate::report::{accepted_accuracy, infeasible};
use crate::selectable::{output_groups, Execution, ExecutionContexts};

pub use report::{
    large_peak_memory_warning, AcceptedAccuracy, ErrorSelectingPlan, RejectionCause, Selection,
};

/// Stage 3, plan selection: select one plan among `physical`'s candidates
/// (module documentation) under `cost_inputs`, and build it: one
/// `PhysicalASAPDAG` node per selected execution, its inputs from the
/// selected edges, each node's `selected_physical_candidate` set and each
/// materialized view node's `view_coverage` from its memo group
/// (`asap_types::plan::view_coverage`).
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
    let queries = physical.logical_candidates.memo.queries();
    if entries.len() != queries.len() {
        return Err(ErrorSelectingPlan::EntryCount {
            candidate: "the physical memo".into(),
            roots: queries.len(),
            entries: entries.len(),
        });
    }
    let outputs = output_groups(physical);
    let (contexts, output_executions) = ExecutionContexts::new(physical, entries, &outputs);
    let request_overhead_cost_per_second = request_overhead_cost_per_second(entries, cost_inputs)
        .map_err(|error| ErrorSelectingPlan::Cost {
        candidate: "every plan (the request overhead)".into(),
        node: 0,
        error,
    })?;
    let costs = execution_costs(
        physical,
        &contexts,
        &output_executions,
        entries,
        data,
        cost_inputs,
        request_overhead_cost_per_second,
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
        selected_plan_cost_per_second: solved.objective_cost_per_second,
        milp_variable_count: solved.variable_count,
        milp_constraint_count: solved.constraint_count,
        milp_construction_ms: solved.construction_ms,
        milp_solve_ms: solved.solve_ms,
    };
    Ok((plan, selection))
}

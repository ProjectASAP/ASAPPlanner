//! Stage 3, plan selection
//! (`docs/design_docs/concepts/memo-groups-properties-requirements.md`,
//! section 3, "Stage 3: baseline selection and plan output", and sections
//! 4 and 5).
//!
//! [`select_plan`] reads Stage 2's physical candidates $\mathcal C$ and the
//! selectable part of the physical edges $\mathcal E$ ([`selectable`]),
//! costs them with `asap-cost` ([`cost`]), selects with a MILP over $x_v$,
//! $y_{u,c,i}$, $z_{q,v}$, $t_v$ and $r_s$ ([`milp`]; constraints (A)–(F)
//! and the one-consumer rule; (G) and (H) hold because $y$ exists only on
//! admissible edges), and builds the plan (`PhysicalASAPDAG`) from
//! $\{x = 1\}$ and $\{y = 1\}$ as its last step ([`plan`], with each
//! node's retention from [`retention`]). The extensions add execution contexts $\kappa$ with $x^{\mathrm{mat}}_v$,
//! $x_{\kappa,v}$, $y_{\kappa,u,c,i}$ and the retention amount $h_v$
//! (section 5), and accuracy at each query root, constraint (K), with
//! per-candidate error bounds $E_c$ and $D_c$ (section 4). Costs come from
//! `asap-cost`.

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

use crate::cost::selection_costs;
use crate::milp::model::{solve_selection, SelectionInputs, SelectionMilpError};
use crate::plan::plan_of;
use crate::report::{accepted_accuracy, infeasible};
use crate::selectable::{output_groups, query_outputs, selectable_edges};

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
    let selectable = selectable_edges(physical, entries);
    let request_overhead_cost_per_second = request_overhead_cost_per_second(entries, cost_inputs)
        .map_err(|error| ErrorSelectingPlan::Cost {
        candidate: "every plan (the request overhead)".into(),
        node: 0,
        error,
    })?;
    let costs = selection_costs(
        physical,
        &selectable,
        entries,
        data,
        cost_inputs,
        request_overhead_cost_per_second,
    );
    let outputs = output_groups(physical);
    let query_outputs = query_outputs(physical, entries, &costs, &outputs);
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
            .candidate_cost_per_second
            .iter()
            .zip(&physical.physical_candidates)
            .filter(|(cost, _)| cost.is_err())
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

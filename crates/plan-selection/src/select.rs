//! Stage 3, plan selection: [`select_plan`] builds the selection MILP over
//! Stage 2's physical candidates and admissible edges
//! ([`crate::milp::model`]), solves it, and builds the plan from the
//! selected executions as its last step.

use asap_cost::CostInputs;
use asap_types::plan::physical_asap_dag::PhysicalASAPDAG;
use asap_types::stages::PhysicalASAPMemoCandidates;
use asap_types::workload::{DataWorkload, QueryWorkloadEntry};

use crate::report::{ErrorSelectingPlan, Selection};

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
    let _ = (physical, entries, data);
    todo!("#1207 PR 6: the baseline MILP over x, y, z, t, and the PhysicalASAPDAG of its solution")
}

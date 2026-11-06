//! Stage 3, plan selection: [`select_plan`] builds the selection MILP over
//! Stage 2's physical candidates and admissible edges
//! ([`crate::milp::model`]), solves it, decodes $\{x = 1\}$ and $\{y = 1\}$
//! into a [`DagOfSelectedCandidates`](asap_types::stages::DagOfSelectedCandidates),
//! and realizes it once through the injected realizer.

use asap_cost::CostInputs;
use asap_types::stages::{AssignmentRealizer, PhysicalASAPMemoCandidates};
use asap_types::workload::{DataWorkload, QueryWorkloadEntry};

use crate::models::AccuracyCheckInputs;
use crate::report::{ErrorSelectingPlan, Selection};

/// Stage 3, plan selection: select one plan among `physical`'s candidates
/// (module documentation). The selection is realized through `realizer`,
/// checked against every query's accuracy target (`accuracy_checks`) and
/// priced under `cost_inputs`. `entries[q]` is query `q`'s workload entry;
/// `data` the data workload with its bounds.
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
    let _ = (physical, entries, data, accuracy_checks, realizer);
    todo!("#1207 PR 6: the baseline MILP over x, y, z, t, its decode and realization")
}

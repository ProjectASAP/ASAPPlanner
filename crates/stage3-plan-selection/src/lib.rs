//! #509 Stage 3, plan selection (concepts doc
//! `memo-groups-properties-requirements.md`, section 3, "Stage 3: baseline
//! selection and realization", and sections 4 and 5).
//!
//! [`select_plan`] reads Stage 2's physical candidates $\mathcal C$ and the
//! admissible part of the physical edges $\mathcal E$, selects with a MILP
//! over $x_v$, $y_{u,c,i}$, $z_{q,v}$ and $t_v$ (constraints (A)–(I);
//! (G) and (H) hold because $y$ exists only on admissible edges), and
//! builds the plan (`PhysicalASAPDAG`) from $\{x = 1\}$ and $\{y = 1\}$
//! as its last step. The extensions add execution contexts $\kappa$ with $x^{\mathrm{mat}}_v$,
//! $x_{\kappa,v}$, $y_{\kappa,u,c,i}$ and the retention amount $h_v$
//! (section 5), and accuracy at each query root, constraint (K), with
//! per-candidate error bounds $E_c$ and $D_c$ (section 4). Costs come from
//! `asap-cost`.

mod milp;
mod report;

use asap_cost::CostInputs;
use asap_types::plan::physical_asap_dag::PhysicalASAPDAG;
use asap_types::stages::PhysicalASAPMemoCandidates;
use asap_types::workload::{DataWorkload, QueryWorkloadEntry};

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
    let _ = (physical, entries, data);
    todo!("#1207 PR 6: the baseline MILP over x, y, z, t, and the PhysicalASAPDAG of its solution, each materialized view node's view_coverage = asap_types::plan::view_coverage(memo, its group)")
}

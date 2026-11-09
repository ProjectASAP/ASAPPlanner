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

mod cost;
mod milp;
mod plan;
mod report;
mod select;

pub use report::{
    large_peak_memory_warning, AcceptedAccuracy, ErrorSelectingPlan, RejectionCause, Selection,
};
pub use select::select_plan;

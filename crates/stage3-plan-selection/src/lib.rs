//! #509 Stage 3, plan selection (concepts doc
//! `memo-groups-properties-requirements.md`, section 3, "Stage 3: baseline
//! selection and realization", and sections 4 and 5).
//!
//! [`select_plan`] reads Stage 2's physical candidates $\mathcal C$ and the
//! admissible part of the physical edges $\mathcal E$, and selects with a
//! MILP over execution contexts $\kappa$ (section 5): $x_{\kappa,v}$ per
//! execution of a candidate in a context (an `AcrossEvaluations` candidate
//! runs once, $x^{\mathrm{mat}}_v$), $y_{\kappa,u,c,i}$, $z_{q,v}$, $t$ and the
//! retention $h_v$, under constraints (A)–(I) per context and (R) ((G) and
//! (H) hold because $y$ exists only on admissible edges whose producer
//! delivers on the consumer's context). It builds the plan
//! (`PhysicalASAPDAG`) from the selected executions and edges as its last
//! step. Accuracy at each query root, constraint (K), with per-candidate
//! error bounds $E_c$ and $D_c$ (section 4), is the remaining extension.
//! Costs come from `asap-cost`.

mod cost;
mod milp;
mod plan;
mod report;
mod select;

pub use report::{
    large_peak_memory_warning, AcceptedAccuracy, ErrorSelectingPlan, RejectionCause, Selection,
};
pub use select::select_plan;

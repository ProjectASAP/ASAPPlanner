//! #509 Stage 3, plan selection (concepts doc
//! `memo-groups-properties-requirements.md`, section 3, "Stage 3: baseline
//! selection and realization", and sections 4 and 5).
//!
//! [`select_plan`] reads Stage 2's physical candidates $\mathcal C$ and the
//! admissible part of the physical edges $\mathcal E$, and selects with a
//! MILP over execution contexts $\kappa$ (section 5): $x^{\mathrm{mat}}_v$
//! per `AcrossEvaluations` candidate, $x_{\kappa,v}$ per candidate with
//! $\ell$ = None or `OneEvaluation` and context, $y_{\kappa,u,c,i}$ per
//! edge and context of its consumer, $z_{q,v}$, $t$, and the retention
//! amount $h_v$; constraints (A)–(I) per execution ((G) and (H) hold because
//! $y$ exists only on admissible edges) and (R). It decodes $\{x = 1\}$ and
//! $\{y = 1\}$ into a `DagOfSelectedCandidates` (one node per selected
//! execution), which the injected realizer builds. Accuracy at each query
//! root is constraint (K) (section 4): error bounds $E_{\kappa,c,m}$ per
//! execution and metric and failure probabilities $D_{\kappa,c}$, big-M on
//! the selected edges, root checks against each query's target
//! ([`crate::milp::error_bounds`]); the selected plan's composed guarantee
//! is checked again with the deployment's accuracy model. Costs come from
//! `asap-cost`.

mod execution_cost;
mod milp;
mod models;
mod report;
mod select;

pub use models::AccuracyCheckInputs;
pub use report::{
    large_peak_memory_warning, AcceptedAccuracy, ErrorSelectingPlan, InexactObjective,
    RejectionCause, SearchReport, SearchStatus, Selection,
};
pub use select::select_plan;

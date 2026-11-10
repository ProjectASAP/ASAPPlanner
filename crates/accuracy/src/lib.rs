//! `asap-accuracy` — the accuracy rules the planning stages share: the
//! planner's built-in analytical rules (estimators, composition, guarantee
//! propagation through a DAG, sizing, budget split and a target's comparison
//! limit [`target_limit`]), the deployment's measured accuracy ([`AccuracyEvidence`]) and $\alpha^{\log}$
//! of a logical edge ([`producer_satisfies_logical_requirements`]).
//!
//! Estimator models derive theoretical guarantees, composition propagates
//! them, `propagation` composes them through every node of a DAG, `sizing`
//! sizes a summary for its target, the budget split sizes each approximate
//! input of a composition for an equal share of its target, and
//! `propagation_stats` holds the scoped contracts and statistics the rules
//! consult. `guarantee` derives each summary type's theoretical guarantee,
//! `evidence` reads the deployment's measured accuracy, `requirements`
//! decides $\alpha^{\log}$ and `error_propagation` bounds a consumer's
//! output error by its inputs'.
//! See `docs/design_docs/concepts/accuracy-models.md` for the design.
//!
//! It depends only on `asap-types`, never on a stage crate, so every stage
//! can use it.

mod budget;
pub mod composition;
mod error_propagation;
mod evidence;
mod function_rules;
pub mod guarantee;
pub mod propagation;
mod propagation_stats;
mod requirements;
pub mod sizing;
#[cfg(feature = "test-support")]
pub mod test_support;

pub use budget::per_input_target;
pub use error_propagation::{error_propagation, ErrorPropagation};
pub use evidence::{AccuracyEvidence, NoMeasuredGuarantees};
pub use guarantee::theoretical_guarantee;
pub(crate) use propagation_stats::PropagationStats;
pub use requirements::{producer_satisfies_logical_requirements, target_limit};

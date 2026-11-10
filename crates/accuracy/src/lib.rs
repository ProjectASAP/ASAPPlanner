//! `asap-accuracy` — the accuracy rules the planning stages share: the
//! planner's built-in analytical rules (estimators, composition, guarantee
//! propagation through a DAG, sizing, budget split and a target's comparison
//! limit [`target_limit`]), the deployment's measured accuracy ([`AccuracyEvidence`]) and $\alpha^{\log}$
//! of a logical edge ([`producer_satisfies_logical_requirements`]).
//!
//! Estimator models derive theoretical guarantees, composition propagates
//! them, `propagation` composes them through every node of a DAG, `sizing`
//! sizes a summary for its target, the budget split sizes each approximate
//! input of a composition for an equal share of its target, and `evidence`
//! holds the scoped contracts and statistics the rules consult.
//! See `docs/design_docs/concepts/accuracy-models.md` for the design.
//!
//! It depends only on `asap-types`, never on a stage crate, so every stage
//! can use it.

mod budget;
pub mod composition;
mod error_propagation;
pub mod estimators;
mod evidence;
mod function_rules;
mod model;
pub mod propagation;
pub mod sizing;
#[cfg(feature = "test-support")]
pub mod test_support;

pub use budget::per_input_target;
pub use error_propagation::{error_propagation, ErrorPropagation};
pub use estimators::theoretical_guarantee;
pub(crate) use evidence::PropagationStats;
pub use model::{producer_satisfies_logical_requirements, AccuracyEvidence, NoMeasuredGuarantees};

use asap_types::ir::properties::{
    AccuracyError, AccuracyGuarantee, AccuracyMetric, AccuracyTarget, BoundExpr,
    CompositionOperator, GuaranteeSource, ProbabilityExpr,
};
use asap_types::ir::schema::{FieldDataType, SketchAlgorithm, SketchParams, SketchStatistic};

/// Small relative tolerance for comparing an evaluated bound against a
/// target, so a parameter sized by `⌈·⌉` to *exactly* meet ε is not rejected
/// by floating-point noise.
const SATISFACTION_TOLERANCE: f64 = 1e-9;

/// The largest evaluated error (or failure probability) that meets a
/// required `limit`: `limit` widened by [`SATISFACTION_TOLERANCE`], so a
/// bound equal to it up to floating-point noise meets it.
pub fn target_limit(limit: f64) -> f64 {
    limit * (1.0 + SATISFACTION_TOLERANCE) + f64::EPSILON
}

/// Whether `guarantee`'s evaluated error is within `max_error` and, when
/// `failure_probability` is set, its evaluated failure probability within
/// it ([`target_limit`]); unknown values do not meet.
#[cfg(test)]
pub(crate) fn meets(
    guarantee: &AccuracyGuarantee,
    max_error: f64,
    failure_probability: Option<f64>,
) -> bool {
    let within = |value: Option<f64>, limit: f64| value.is_some_and(|v| v <= target_limit(limit));
    within(guarantee.spec.error.evaluate(), max_error)
        && failure_probability
            .is_none_or(|delta| within(guarantee.spec.failure_probability.evaluate(), delta))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A bound equal to the limit up to floating-point noise meets it; a
    /// bound one part in a million above it does not.
    #[test]
    fn target_limit_admits_only_floating_point_noise() {
        assert!(0.01 * (1.0 + 1e-12) <= target_limit(0.01));
        assert!(0.01 * (1.0 + 1e-6) > target_limit(0.01));
        assert!(0.0 <= target_limit(0.0));
    }
}

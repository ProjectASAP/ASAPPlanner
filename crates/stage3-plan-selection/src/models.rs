//! The accuracy models Stage 3 checks with: [`AccuracyCheckInputs`].

use asap_accuracy::{AccuracyEvidenceProvider, AccuracyModel};

/// Inputs of the composed-accuracy check. `raw_data_retained` is a Stage 2
/// input only; to ignore accuracy guarantees the facade passes an
/// accept-all `model` ([`asap_accuracy::IgnoreAccuracyGuarantees`]).
#[derive(Clone, Copy)]
pub struct AccuracyCheckInputs<'a> {
    /// Decides whether a candidate's accuracy guarantee meets its query's
    /// target, and whether the deployment accepts an output regardless of
    /// its guarantee.
    pub model: &'a dyn AccuracyModel,
    /// Deployment facts the guarantees need (operand domains, observed
    /// guarantees).
    pub evidence: &'a dyn AccuracyEvidenceProvider,
}

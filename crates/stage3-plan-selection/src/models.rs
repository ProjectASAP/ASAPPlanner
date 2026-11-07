//! The accuracy models Stage 3 checks with: [`AccuracyCheckInputs`].

use asap_accuracy::{AccuracyEvidenceProvider, AccuracyModel};
use asap_types::ir::QueryRoot;

/// Inputs of the composed-accuracy check, constraint (K). To ignore
/// accuracy guarantees the facade passes an accept-all `model`
/// ([`asap_accuracy::IgnoreAccuracyGuarantees`]): (K) then applies to no
/// query.
#[derive(Clone, Copy)]
pub struct AccuracyCheckInputs<'a> {
    /// Decides whether a candidate's accuracy guarantee meets its query's
    /// target, and whether the deployment accepts an output regardless of
    /// its guarantee.
    pub model: &'a dyn AccuracyModel,
    /// Deployment facts the guarantees need (operand domains, observed
    /// guarantees).
    pub evidence: &'a dyn AccuracyEvidenceProvider,
    /// Each query's root as the front ends lowered it, by query index:
    /// Stage 3 reads how many values one evaluation returns ($G_q$,
    /// `asap_types::workload::bounds::values_per_result`), which a
    /// whole-result failure probability covers, and asks `model` whether
    /// it accepts the output regardless of its guarantee, in which case
    /// (K) does not apply to the query.
    pub query_roots: &'a [QueryRoot],
}

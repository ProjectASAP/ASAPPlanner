//! The deployment's accuracy evidence that plan selection reads.
use super::*;
use asap_types::ir::QueryRoot;

/// Supplies the deployment's empirical accuracy evidence.
pub trait AccuracyEvidenceProvider {
    /// The deployment's empirical accuracy for one candidate's whole query
    /// output, as a guarantee. Plan selection (#509 Stage 3) accepts a
    /// candidate whose output has no theoretical guarantee that meets the
    /// target (e.g. a difference of two quantile estimates) only when this
    /// shows the output meets it. Stage 1 composition never reads it: root
    /// guarantees there come from the built-in theoretical model. `None`
    /// means no evidence.
    fn output_guarantee(&self, _output: &QueryRoot) -> Option<ResultGuarantee> {
        None
    }
}

#[derive(Debug, Default, Clone, Copy)]
pub struct NoAccuracyEvidence;

impl AccuracyEvidenceProvider for NoAccuracyEvidence {}

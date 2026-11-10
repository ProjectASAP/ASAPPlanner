//! Whether a producer meets a consumer's logical requirements:
//! $\alpha^{\log}$ of a logical edge, and the limit an evaluated bound is
//! compared against.

#[cfg(test)]
use asap_types::accuracy::AccuracyGuarantee;
use asap_types::stages::{CandidateForMemoGroupId, Memo};

use crate::AccuracyEvidence;

/// $\alpha^{\log}$ of one edge (concepts doc, section 2): for a readout's
/// input, the producer's guarantee for the readout's statistic at the
/// required failure probability is within the required error, and a
/// top-k's heap holds k; for any other input, the metric rule (section 4).
pub fn producer_satisfies_logical_requirements(
    memo: &Memo,
    producer: CandidateForMemoGroupId,
    consumer: CandidateForMemoGroupId,
    input_index_of_consumer: usize,
    evidence: &dyn AccuracyEvidence,
) -> bool {
    let _ = (memo, producer, consumer, input_index_of_consumer, evidence);
    todo!("#1207 PR 4: the readout rule against RequiredAccuracy, else the metric rule")
}

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

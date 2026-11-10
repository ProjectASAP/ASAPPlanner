//! Stage 1, Pass 1: per-computation candidate generation and the summary
//! realizations it offers. Its one entry is
//! [`pass1_memo`], which fills each group with its initial candidates (exact,
//! exact accumulator, one summary build per listed parameter configuration,
//! fused builds), with every intermediate result its own group (a [`Memo`]),
//! which Pass 2 extends.

/// Stage 1, Pass 1's computation targets. No execution timing is assigned: that is a Stage 2
/// materialization decision. Its target list
/// (`PerComputationCandidates`) is Pass 1's working form, public only to
/// tests (`crate::test_support`, feature `test-support`).
pub(crate) mod computation_targets;
/// Frontend-to-Pass-1 acceptance tests over Pass 1's working form.
#[cfg(test)]
mod computation_targets_tests;
pub(crate) mod realization;

use asap_types::deployment_capabilities::SupportedSummary;
use asap_types::stages::Memo;
use computation_targets::generate_per_computation_candidates;
pub use computation_targets::PerComputationCandidateError;

/// Why [`pass1_memo`] failed.
#[derive(Debug, thiserror::Error)]
pub enum Pass1Error {
    /// A query's DAG cannot be planned: it already holds ASAP or physical
    /// annotations, or a measure's accuracy target is invalid.
    #[error(transparent)]
    PerComputation(#[from] PerComputationCandidateError),
}

/// Stage 1, Pass 1: the initial candidates of the memo groups of `memo`
/// (Stage 0's, without candidates). It finds one computation target per
/// measure of each aggregate group with a realization besides exact
/// execution, drops the summaries the deployment does not support
/// (`supported_summaries`, the deployment's
/// `DeploymentCapabilities::supported_summaries`; `None` supports every
/// one), and adds each way to compute a group as candidates, through
/// `Memo::find_or_add_group` and `Memo::add_candidate`. Accuracy prunes
/// nothing here: Pass 2's dead-candidate elimination does.
pub fn pass1_memo(
    memo: Memo,
    supported_summaries: Option<&[SupportedSummary]>,
) -> Result<Memo, Pass1Error> {
    let per_computation = generate_per_computation_candidates(&memo)?
        .without_unsupported_summaries(supported_summaries);
    let _ = (memo, per_computation);
    todo!("#1207 PR 3: add each way to compute as candidates, one per listed θ")
}

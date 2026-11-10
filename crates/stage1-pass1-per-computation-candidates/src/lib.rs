//! `asap-stage1-pass1-per-computation-candidates` — #509 Stage 1, Pass 1:
//! per-computation candidate generation and the summary realizations it
//! offers. Its one entry is [`pass1_memo`], which fills the
//! memo groups of Stage 0's [`Memo`](asap_types::stages::Memo) with their
//! initial candidates; Pass 2 extends that memo.
//!
//! It depends only on `asap-types` (its input and output memo are in
//! `asap_types::stages`) and the shared accuracy model (`asap-accuracy`).
//! It depends on no other stage crate, and the planner facade calls it
//! after Stage 0. It depends on no query-language frontend either; its own
//! tests lower PromQL through `asap-frontend-promql` (a dev-dependency).
//! The `test-support` feature exports the Pass 1 internals other crates'
//! tests call (`test_support`).
//!
//! Accuracy guarantees come from the built-in analytical accuracy rules in
//! `asap-accuracy` (per-family estimators, composition and budget
//! allocation). They derive a machine-readable
//! [`AccuracyGuarantee`](asap_types::accuracy::AccuracyGuarantee)
//! for every finalized value and propagate it through
//! approximate-over-approximate compositions under conservative rules (no
//! independence assumptions, unknown statistics stay unknown). A
//! composition with no rule leaves the guarantee `None` and keeps the
//! candidate. Pass 1 prunes nothing for accuracy; Pass 2 gives each readout
//! its required accuracy and eliminates the candidates no query can use,
//! with the deployment's measured accuracy (`AccuracyEvidence`) read before
//! each summary's theoretical guarantee.

/// Stage 1, Pass 1's computation targets. No execution timing is assigned: that is a Stage 2
/// materialization decision. Its target list
/// (`PerComputationCandidates`) is Pass 1's working form, public only to
/// tests (`crate::test_support`, feature `test-support`).
mod computation_targets;
/// The summary families that can serve an aggregate intent, the input a
/// summary consumes and the column a top-k ranks.
mod summary_realization;
/// Fixtures this crate's own tests share.
#[cfg(test)]
mod test_fixtures;
/// Test helpers other crates' tests call (feature `test-support`).
#[cfg(any(test, feature = "test-support"))]
pub mod test_support;
/// Frontend-to-Pass-1 acceptance tests over Pass 1's working form.
#[cfg(test)]
mod tests;

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

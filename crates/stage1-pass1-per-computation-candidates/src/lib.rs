//! `asap-stage1-pass1-per-computation-candidates` — #509 Stage 1, Pass 1:
//! each memo group's initial candidates. Its one entry is [`pass1_memo`],
//! which fills the memo groups of Stage 0's
//! [`Memo`](asap_types::stages::Memo) with their initial candidates (exact,
//! exact accumulator, one summary build per listed parameter configuration
//! with separate readouts, fused builds), every intermediate result its own
//! group; Pass 2 extends that memo.
//!
//! It depends only on `asap-types` (its input and output memo are in
//! `asap_types::stages`) and the shared accuracy model (`asap-accuracy`,
//! for the statistic each intent reads). It depends on no other stage
//! crate, and the planner facade calls it after Stage 0. It depends on no
//! query-language frontend either; its tests build their input memo through
//! `Memo::find_or_add_group` and `Memo::add_query`.
//!
//! Pass 1 reads no accuracy target and sets no input requirement: the error
//! of each build follows from its parameters (`parameter_list`), and Pass 2
//! gives each readout its required accuracy, decides the logical edges and
//! eliminates the candidates no query can use.

/// Stage 1, Pass 1's candidates: each way to compute a group, added to the
/// memo for the computation targets.
mod candidates;
/// Stage 1, Pass 1's computation targets over the memo groups.
mod computation_targets;
/// The sketch algorithms that can serve an aggregate intent, the input a
/// summary consumes and the column a top-k ranks.
mod summary_realization;
/// Fixtures this crate's own tests share.
#[cfg(test)]
mod test_fixtures;
/// Pass 1's tests over memos built through the `Memo` methods.
#[cfg(test)]
mod tests;

use asap_types::deployment_capabilities::SupportedSummary;
use asap_types::stages::Memo;

use candidates::add_pass1_candidates;
use computation_targets::computation_targets;
pub use computation_targets::PerComputationCandidateError;

/// Stage 1, Pass 1: the initial candidates of the memo groups of `memo`
/// (Stage 0's, without candidates). It finds one computation target per
/// measure of each aggregate group with a realization besides exact
/// execution, drops the summaries the deployment does not support
/// (`supported_summaries`, the deployment's
/// `DeploymentCapabilities::supported_summaries`; `None` supports every
/// one), and adds each way to compute a group as candidates, through
/// `Memo::find_or_add_group` and `Memo::add_candidate`: every group's own
/// expression, exact accumulators, one summary build per configuration of
/// the algorithm's predefined list with separate readouts, and fused builds.
/// Accuracy prunes nothing here: Pass 2 gives each readout its required
/// accuracy and eliminates the candidates no query can use.
pub fn pass1_memo(
    mut memo: Memo,
    supported_summaries: Option<&[SupportedSummary]>,
) -> Result<Memo, PerComputationCandidateError> {
    let targets = computation_targets(&memo, supported_summaries)?;
    add_pass1_candidates(&mut memo, &targets)?;
    Ok(memo)
}

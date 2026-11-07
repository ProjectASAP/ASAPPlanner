//! Stage 1, Pass 1's entry and output: each group's initial candidates
//! (exact, exact accumulator, one summary build per listed parameter
//! configuration, fused builds), with every intermediate result its own
//! group (a [`Memo`]), which Pass 2 extends.

use asap_types::ir::schema::SupportedSummary;
use asap_types::stages::Memo;

use crate::per_computation_candidates::{
    add_pass1_candidates, computation_targets, PerComputationCandidateError,
};

/// Stage 1, Pass 1: the initial candidates of the memo groups of `memo`
/// (Stage 0's, without candidates). It finds one computation target per
/// measure of each aggregate group with a realization besides exact
/// execution, drops the summaries the deployment does not support
/// (`supported_summaries`, the deployment's
/// `DeploymentCapabilities::supported_summaries`; `None` supports every
/// one), and adds each way to compute a group as candidates, through
/// `Memo::find_or_add_group` and `Memo::add_candidate`: every group's own
/// expression, exact accumulators, one summary build per configuration of
/// the family's predefined list with separate readouts, and fused builds.
/// Pruning by the queries' accuracy targets is decided per edge, after
/// Pass 2 (#1207 PR 4).
pub fn pass1_memo(
    mut memo: Memo,
    supported_summaries: Option<&[SupportedSummary]>,
) -> Result<Memo, PerComputationCandidateError> {
    let targets = computation_targets(&memo, supported_summaries)?;
    add_pass1_candidates(&mut memo, &targets)?;
    Ok(memo)
}

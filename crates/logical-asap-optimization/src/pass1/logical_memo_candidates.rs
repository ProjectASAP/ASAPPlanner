//! Stage 1, Pass 1's entry and output: each group's initial candidates
//! (exact, exact accumulator, one summary build per listed parameter
//! configuration, fused builds), with every intermediate result its own
//! group (a [`Memo`]), which Pass 2 extends.

use asap_types::ir::properties::AccuracyTarget;
use asap_types::workload::DataWorkload;

use asap_accuracy::{AccuracyEvidenceProvider, AccuracyModel};

use crate::pass1::per_computation_candidates::{
    generate_per_computation_candidates, PerComputationCandidateError,
};
use asap_types::deployment_capabilities::SupportedSummary;
use asap_types::stages::Memo;

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
/// one) and, unless `accuracy` or `evidence` may accept an output without
/// its guarantee, the summaries no query reading them can use (a query
/// `q`'s target is `accuracy_targets[q]`). It then adds each way to compute
/// a group as candidates, through `Memo::find_or_add_group` and
/// `Memo::add_candidate`.
pub fn pass1_memo(
    memo: Memo,
    accuracy_targets: &[AccuracyTarget],
    accuracy: &dyn AccuracyModel,
    evidence: &dyn AccuracyEvidenceProvider,
    data: &DataWorkload,
    supported_summaries: Option<&[SupportedSummary]>,
) -> Result<Memo, Pass1Error> {
    let mut per_computation = generate_per_computation_candidates(&memo)?
        .without_unsupported_summaries(supported_summaries);
    if !accuracy.may_accept_regardless_of_guarantee() && !evidence.may_provide_evidence() {
        per_computation =
            per_computation.without_summaries_no_query_can_use(accuracy_targets, data, accuracy);
    }
    let _ = (memo, per_computation);
    todo!("#1207 PR 3: add each way to compute as candidates, one per listed θ")
}

//! `asap-stage1-pass2-reuse-candidates` — #509 Stage 1, Pass 2: the reuse
//! rules over memo groups and the logical edges. Its entry,
//! [`generate_logical_asap_memo_candidates`], extends Pass 1's memo into
//! the [`LogicalASAPMemoCandidates`](asap_types::stages::LogicalASAPMemoCandidates)
//! Stage 2 expands, e.g. `Quantile(x, 0.99)` and `Quantile(x, 0.95)` reading
//! one built sketch.
//!
//! It depends only on `asap-types`, where its input and output are. It
//! depends on no other stage crate, and the planner facade calls it after
//! Pass 1.

/// The rules over groups.
mod memo;

use asap_accuracy::AccuracyEvidence;
use asap_types::stages::{LogicalASAPMemoCandidates, Memo};
use asap_types::workload::QueryWorkloadEntry;
pub use memo::rules::Pass2RuleError;
use memo::rules::{finer_aggregate, rollup, scan_labels, summary_inputs, topk};
use memo::window_candidates::add_local_window_composition;

/// Stage 1, Pass 2: rule 1 ([`pass2_local_window_composition`]), then rules
/// 2–7 and the logical edges ([`pass2_after_local_window_composition`]).
/// `entries[i]` is query `i`'s workload entry; `evidence` is the
/// deployment's measured accuracy, which each edge's $\alpha^{\log}$ reads
/// before the theoretical guarantee.
pub fn generate_logical_asap_memo_candidates(
    pass1: Memo,
    entries: &[QueryWorkloadEntry],
    evidence: &dyn AccuracyEvidence,
) -> Result<LogicalASAPMemoCandidates, Pass2RuleError> {
    pass2_after_local_window_composition(pass2_local_window_composition(pass1, entries)?, evidence)
}

/// Pass 2's rule 1 alone: Pass 1's memo with the tumbling-window layouts
/// of each summary state group (`asap_planner` prints this step).
pub fn pass2_local_window_composition(
    pass1: Memo,
    entries: &[QueryWorkloadEntry],
) -> Result<Memo, Pass2RuleError> {
    let mut memo = pass1;
    // 1. Local window composition.
    add_local_window_composition(&mut memo, entries)?;
    Ok(memo)
}

/// The rest of Pass 2 on [`pass2_local_window_composition`]'s memo: rules
/// 2–7, then the readouts' required accuracy and the logical edges, with
/// $\alpha^{\log}$ from `evidence` or the theoretical guarantee
/// (`asap_accuracy::producer_satisfies_logical_requirements`).
pub fn pass2_after_local_window_composition(
    mut memo: Memo,
    evidence: &dyn AccuracyEvidence,
) -> Result<LogicalASAPMemoCandidates, Pass2RuleError> {
    // 2. Scan widening.
    scan_labels::add_scan_label_candidates(&mut memo)?;
    // 3. Summary inputs.
    summary_inputs::factor_summary_inputs(&mut memo)?;
    // 4. Top-k reuse.
    topk::add_topk_candidates(&mut memo)?;
    // 5. Finer aggregate.
    finer_aggregate::add_finer_aggregate_candidates(&mut memo)?;
    // 6. Rollup.
    rollup::add_rollup_candidates(&mut memo)?;
    // 7. Window rollup.
    rollup::add_window_rollup_candidates(&mut memo)?;
    let _ = (memo, evidence);
    todo!("#1207 PR 4: readout requirements, dead-candidate elimination and the logical edges")
}

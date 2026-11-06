//! Stage 1, Pass 2's entry and output: the Pass 1 memo extended by the
//! rules over groups, and the logical edges
//! ([`asap_types::stages::LogicalASAPMemoCandidates`]), which Stage 2 expands.

use crate::memo::rules::{
    finer_aggregate, rollup, scan_labels, summary_inputs, topk, Pass2RuleError,
};
use crate::memo::window_candidates::add_local_window_composition;
use asap_accuracy::AccuracyEvidence;
use asap_types::stages::{LogicalASAPMemoCandidates, Memo};
use asap_types::workload::QueryWorkloadEntry;

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

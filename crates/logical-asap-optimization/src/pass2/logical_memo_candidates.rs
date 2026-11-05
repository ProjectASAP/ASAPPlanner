//! Stage 1, Pass 2's entry and output: the Pass 1 memo extended with the
//! candidates and groups that share work across computations
//! ([`asap_types::stages::LogicalASAPMemoCandidates`]), which Stage 2 implements.

use crate::pass2::memo::rules::{
    finer_aggregate, rollup, scan_labels, shared_configuration_readers, summary_inputs,
    summary_results, topk,
};
use crate::pass2::memo::state_groups::{
    add_shared_summary_state_groups, check_memo_well_formed, Pass2RuleError,
};
use crate::pass2::memo::window_candidates::add_local_window_composition;
use asap_types::stages::Memo;
use asap_types::stages::{LogicalASAPMemoCandidates, PerComputationLogicalMemoCandidates};
use asap_types::workload::QueryWorkloadEntry;

/// Stage 1, Pass 2: adds to Pass 1's memo the local tumbling-window
/// candidates for every legal tumbling-window width (rule 1), then runs
/// rules 2–9 and `add_shared_summary_state_groups`.
/// `entries[i]` is query `i`'s workload entry.
pub fn generate_logical_asap_memo_candidates(
    pass1: PerComputationLogicalMemoCandidates,
    entries: &[QueryWorkloadEntry],
) -> Result<LogicalASAPMemoCandidates, Pass2RuleError> {
    pass2_after_local_window_composition(pass2_local_window_composition(pass1, entries))
}

/// Pass 2's rule 1 alone: Pass 1's memo with the local tumbling-window
/// candidates for every legal width (`asap_planner` prints this step).
pub fn pass2_local_window_composition(
    pass1: PerComputationLogicalMemoCandidates,
    entries: &[QueryWorkloadEntry],
) -> Memo {
    let mut memo = pass1.memo;
    // 1. Local window composition.
    add_local_window_composition(&mut memo, entries);
    memo
}

/// The rest of Pass 2 on [`pass2_local_window_composition`]'s memo: rules
/// 2–9 and `add_shared_summary_state_groups`.
pub fn pass2_after_local_window_composition(
    mut memo: Memo,
) -> Result<LogicalASAPMemoCandidates, Pass2RuleError> {
    apply_sharing_rules(&mut memo)?;
    // 10. One group per shared summary build, read by its reader groups.
    add_shared_summary_state_groups(&mut memo)?;
    Ok(LogicalASAPMemoCandidates { memo })
}

/// Pass 2's rules 2–9, in order, on a memo after local window composition.
pub(crate) fn apply_sharing_rules(memo: &mut Memo) -> Result<(), Pass2RuleError> {
    check_memo_well_formed(memo)?;
    // 2. Scan widening.
    scan_labels::add_scan_label_candidates(memo)?;
    // 3. Summary inputs.
    summary_inputs::factor_summary_inputs(memo)?;
    // 4. Top-k reuse.
    topk::add_topk_candidates(memo)?;
    // 5. Finer aggregate.
    finer_aggregate::add_finer_aggregate_candidates(memo)?;
    // 6. Rollup.
    rollup::add_rollup_candidates(memo)?;
    // 7. Window rollup.
    rollup::add_window_rollup_candidates(memo)?;
    // 8. Summary results.
    summary_results::factor_summary_results(memo)?;
    // 9. Shared configurations.
    shared_configuration_readers::add_state_configuration_candidates(memo)?;
    Ok(())
}

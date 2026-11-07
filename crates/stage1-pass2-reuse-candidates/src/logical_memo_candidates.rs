//! Stage 1, Pass 2's entry and output: the Pass 1 memo extended by the
//! rules over groups, and the logical edges
//! ([`asap_types::stages::LogicalASAPMemoCandidates`]), which Stage 2 expands.

use crate::logical_edges::logical_edges_with_capped_floors;
use crate::memo::rules::{finer_aggregate, rollup, scan_labels, topk, Pass2RuleError};
use crate::memo::window_candidates::add_local_window_composition;
use asap_types::stages::{LogicalASAPMemoCandidates, Memo};
use asap_types::workload::QueryWorkloadEntry;

/// Stage 1, Pass 2: rule 1 ([`pass2_local_window_composition`]), then the
/// other rules and the logical edges ([`pass2_after_local_window_composition`]).
/// `entries[i]` is query `i`'s workload entry.
pub fn generate_logical_asap_memo_candidates(
    pass1: Memo,
    entries: &[QueryWorkloadEntry],
) -> Result<LogicalASAPMemoCandidates, Pass2RuleError> {
    pass2_after_local_window_composition(pass2_local_window_composition(pass1, entries)?, entries)
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

/// The rest of Pass 2 on [`pass2_local_window_composition`]'s memo: the
/// other rules, then each input's floor tightened by its validity and
/// pruning caps, and the logical edges. `entries[i]` is query `i`'s
/// workload entry (its accuracy target).
///
/// Pass 1 already gives every input of a summary build its own group, so
/// the old rule 3 (summary inputs as groups) has nothing left to do. A
/// coarse state's rollup (rule 6) reads the finer state group, windowed
/// builds included, which covers the old rule 7 (window rollup) over the
/// same range. A longer range read from a shorter range's tumbling windows
/// needs a window requirement on the input, which `LogicalRequirements`
/// does not have: not offered (#1221).
pub fn pass2_after_local_window_composition(
    mut memo: Memo,
    entries: &[QueryWorkloadEntry],
) -> Result<LogicalASAPMemoCandidates, Pass2RuleError> {
    // 2. Scan widening.
    scan_labels::add_scan_label_candidates(&mut memo)?;
    // 4. Top-k reuse.
    topk::add_topk_candidates(&mut memo)?;
    // 5. Finer aggregate.
    finer_aggregate::add_finer_aggregate_candidates(&mut memo)?;
    // 6. Rollup.
    rollup::add_rollup_candidates(&mut memo)?;
    let edges = logical_edges_with_capped_floors(&mut memo, entries)?;
    Ok(LogicalASAPMemoCandidates { memo, edges })
}

//! Rule 1, chain groups (concepts doc, section 3, "Chain groups"): a
//! `SummaryTumblingWindowChain[w](X)` group per width, and a `SummaryMerge`
//! over the `TimeRange` (and `TimeShift`s) over it as a candidate of the
//! summary state group.

use crate::memo::rules::Pass2RuleError;
use asap_types::stages::Memo;
use asap_types::workload::QueryWorkloadEntry;

/// Rule 1: for each summary state group that survived dead-candidate
/// elimination, a chain group and a merge candidate per tumbling width $w$
/// from the gcd closure of the evaluation grids, ranges and offsets of the
/// queries that reach it and repeat on a known grid. `entries[i]` is query
/// `i`'s workload entry.
pub(crate) fn add_local_window_composition(
    memo: &mut Memo,
    entries: &[QueryWorkloadEntry],
) -> Result<(), Pass2RuleError> {
    let _ = (memo, entries);
    todo!("#1207 PR 4: rule 1, chain groups and the merges over them")
}

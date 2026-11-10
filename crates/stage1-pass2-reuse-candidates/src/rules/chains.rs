//! Step 5, chain groups (concepts doc, section 3, "Chain groups"): a
//! `SummaryTumblingWindowChain[w](X)` group per width, and a `SummaryMerge`
//! over the `TimeRange` (and `TimeShift`s) over it as a candidate of the
//! summary state group.

use std::collections::BTreeSet;

use crate::rules::Pass2RuleError;
use asap_types::stages::{CandidateForMemoGroupId, Memo};
use asap_types::workload::QueryWorkloadEntry;

/// For each summary state group with a build that survived dead-candidate
/// elimination (not in `pruned`), a chain group and a merge candidate per
/// tumbling width $w$ from the gcd closure of the evaluation grids, ranges
/// and offsets of the queries that reach it and repeat on a known grid.
/// `entries[q]` is query `q`'s workload entry.
// Step 5 runs after dead-candidate elimination, which #1207 PR 4 adds.
#[allow(dead_code)]
pub(crate) fn add_chain_groups(
    memo: &mut Memo,
    entries: &[QueryWorkloadEntry],
    pruned: &BTreeSet<CandidateForMemoGroupId>,
) -> Result<(), Pass2RuleError> {
    let _ = (memo, entries, pruned);
    todo!("#1207 PR 4: step 5, chain groups and the merges over them")
}

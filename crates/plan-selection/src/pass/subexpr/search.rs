//! Clusters, coupling thresholds with branch and bound, the dynamic
//! program, build-late (design "Memo-group search", steps 3–7).

use asap_logical_asap_optimization::per_computation_candidates::PerComputationCandidates;
use asap_types::workload::QueryWorkloadEntry;

use super::{groups, Memo};

/// The workload's memo: groups, Pass 1 candidates and the Pass 2
/// transformations' candidates.
pub(crate) fn memo<Id: Clone>(
    per_computation: &PerComputationCandidates<Id>,
    entries: &[QueryWorkloadEntry],
) -> Memo {
    let mut memo = groups::memo_of(per_computation);
    super::capability::add_shared_builds(&mut memo);
    super::windows::add_pass2_candidates(&mut memo, entries);
    memo
}

/// A one-line summary of `memo` for logs: groups, shared groups (two or
/// more parents or queries), candidates, distinct coupling sites.
pub(crate) fn stats(memo: &Memo) -> String {
    let shared = memo
        .groups
        .iter()
        .filter(|g| g.parents.len() > 1 || (g.parents.is_empty() && g.roots_of.len() > 1))
        .count();
    let candidates: usize = memo.candidates.iter().map(Vec::len).sum();
    let mut sites: Vec<u64> = memo
        .candidates
        .iter()
        .flatten()
        .flat_map(|c| c.coupling_sites.iter().map(|s| s.key))
        .collect();
    sites.sort_unstable();
    sites.dedup();
    let most = memo.candidates.iter().map(Vec::len).max().unwrap_or(0);
    format!(
        "groups {} shared {} candidates {} (most in one group {}) coupling sites {}",
        memo.groups.len(),
        shared,
        candidates,
        most,
        sites.len()
    )
}

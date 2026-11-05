//! Interim Stage 1 helpers that tell the workload's groups (Stage 0's DAG)
//! from the groups Pass 1 and Pass 2 add beneath them (#1203 2a). They go
//! once no rule needs to tell them apart.

use std::collections::HashSet;

use asap_types::stages::{root_groups, Memo, MemoGroupId};

/// The groups some inputs read through the groups added beneath the
/// workload's groups ([`groups_read_through_added_groups`]).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GroupsReadThroughAddedGroups {
    /// The added groups passed through, in the order first reached.
    pub added_groups: Vec<MemoGroupId>,
    /// The workload groups reached, in the order first reached.
    pub workload_groups: Vec<MemoGroupId>,
}

/// The groups of `memo`'s workload DAG: the query roots' groups and every
/// group below them through logical child groups. The groups added beneath
/// them, which only candidates read, are not among them.
pub fn workload_groups(memo: &Memo) -> HashSet<MemoGroupId> {
    let mut pending: Vec<MemoGroupId> = root_groups(&memo.query_roots).into_iter().collect();
    let mut reached = HashSet::new();
    while let Some(group) = pending.pop() {
        if reached.insert(group) {
            pending.extend(memo.groups()[group.0 as usize].child_groups.iter().copied());
        }
    }
    reached
}

/// The groups `inputs` read, walking through every group not in
/// `workload_groups` (each read through all its candidates' input groups)
/// and stopping at workload groups.
pub fn groups_read_through_added_groups(
    memo: &Memo,
    inputs: &[MemoGroupId],
    workload_groups: &HashSet<MemoGroupId>,
) -> GroupsReadThroughAddedGroups {
    let mut read = GroupsReadThroughAddedGroups::default();
    let mut pending: Vec<MemoGroupId> = inputs.iter().rev().copied().collect();
    let mut seen = HashSet::new();
    while let Some(group) = pending.pop() {
        if !seen.insert(group) {
            continue;
        }
        if workload_groups.contains(&group) {
            read.workload_groups.push(group);
            continue;
        }
        read.added_groups.push(group);
        let below: Vec<MemoGroupId> = memo.candidates[group.0 as usize]
            .iter()
            .flat_map(|candidate| &candidate.input_groups)
            .copied()
            .collect();
        pending.extend(below.into_iter().rev());
    }
    read
}

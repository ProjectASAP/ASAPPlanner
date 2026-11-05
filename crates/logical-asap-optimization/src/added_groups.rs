//! Interim Stage 1 helpers that tell Stage 0's groups (the groups Stage 0
//! produced from the queries' DAG, [`Memo::is_stage0_group`]) from the
//! groups Pass 1 and Pass 2 add beneath them (#1203 2a). They go once no
//! rule needs to tell them apart.

use std::collections::HashSet;

use asap_types::stages::{Memo, MemoGroupId};

/// The groups some inputs read through the groups added beneath Stage 0's
/// groups ([`groups_read_through_added_groups`]).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct GroupsReadThroughAddedGroups {
    /// The added groups passed through, in the order first reached.
    pub(crate) added_groups: Vec<MemoGroupId>,
    /// The Stage 0 groups reached, in the order first reached.
    pub(crate) stage0_groups: Vec<MemoGroupId>,
}

/// The groups `inputs` read, walking through every added group (each read
/// through all its candidates' input groups) and stopping at Stage 0's
/// groups.
pub(crate) fn groups_read_through_added_groups(
    memo: &Memo,
    inputs: &[MemoGroupId],
) -> GroupsReadThroughAddedGroups {
    let mut read = GroupsReadThroughAddedGroups::default();
    let mut pending: Vec<MemoGroupId> = inputs.iter().rev().copied().collect();
    let mut seen = HashSet::new();
    while let Some(group) = pending.pop() {
        if !seen.insert(group) {
            continue;
        }
        if memo.is_stage0_group(group) {
            read.stage0_groups.push(group);
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

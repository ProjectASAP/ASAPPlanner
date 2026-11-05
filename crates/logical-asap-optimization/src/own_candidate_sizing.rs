//! The accuracy budget parts of a group's own candidate, which Pass 1 and
//! Pass 2's sharing rules raise when a reader sized for more parts finds the
//! group ([`crate::find_or_add_group::find_or_add_group`]); the memo itself
//! knows nothing of accuracy.

use std::rc::Rc;

use asap_types::ir::OperatorNode;
use asap_types::stages::{Memo, MemoGroupId, OriginOfCandidateForMemoGroup};

use crate::find_or_add_group::add_group;

/// Sizes `group`'s own candidate (its logical node over its child groups,
/// the one `find_or_add_group` adds) for at least `accuracy_budget_parts`:
/// a reader sized for that many parts reads the group, and the node is the
/// same, so it meets both sizings.
pub(crate) fn size_own_candidate_for(
    memo: &mut Memo,
    group: MemoGroupId,
    accuracy_budget_parts: usize,
) {
    let own = Rc::clone(&memo.groups()[group.0 as usize].logical_node);
    for candidate in &mut memo.candidates[group.0 as usize] {
        if Rc::ptr_eq(&candidate.root_node, &own) {
            candidate.accuracy_budget_parts =
                candidate.accuracy_budget_parts.max(accuracy_budget_parts);
        }
    }
}

/// [`crate::find_or_add_group::find_or_add_group`], with the group found
/// sized for at least `accuracy_budget_parts` ([`size_own_candidate_for`]):
/// a reader sized for more parts may find a group added for fewer.
pub(crate) fn find_or_add_sized_group(
    memo: &mut Memo,
    node: &Rc<OperatorNode>,
    queries: &[usize],
    origin: &OriginOfCandidateForMemoGroup,
    accuracy_budget_parts: usize,
) -> MemoGroupId {
    match memo.group_with_equal_node(node) {
        Some(group) => {
            memo.add_queries(group, queries);
            size_own_candidate_for(memo, group, accuracy_budget_parts);
            group
        }
        None => add_group(memo, node, queries, origin, accuracy_budget_parts),
    }
}

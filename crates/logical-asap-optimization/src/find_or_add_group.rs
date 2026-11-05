//! Finding or adding the memo group of a node (Stage 1 only): the group
//! whose node equals it by Stage 0's equality
//! ([`Memo::group_with_equal_node`]), else a new group over its children's
//! groups, each found or added the same way.

use std::collections::HashMap;
use std::rc::Rc;

use asap_types::ir::OperatorNode;
use asap_types::stages::{
    CandidateForMemoGroup, Memo, MemoGroup, MemoGroupId, OriginOfCandidateForMemoGroup,
};

/// Returns the existing group whose node equals `node`
/// ([`Memo::group_with_equal_node`], Stage 0's equality), after adding
/// `queries` to it and to every group below it ([`Memo::add_queries`]).
/// Otherwise adds a group for `node` over its children's groups
/// ([`add_group`]) and returns it. It never copies a group.
pub(crate) fn find_or_add_group(
    memo: &mut Memo,
    node: &Rc<OperatorNode>,
    queries: &[usize],
    origin: &OriginOfCandidateForMemoGroup,
    accuracy_budget_parts: usize,
) -> MemoGroupId {
    match memo.group_with_equal_node(node) {
        Some(group) => {
            memo.add_queries(group, queries);
            group
        }
        None => add_group(memo, node, queries, origin, accuracy_budget_parts),
    }
}

/// Adds a group for `node`, which no group's node equals, over its
/// children's groups ([`over_child_groups`]). The group's logical node is
/// `node` over its children's groups' nodes, `queries` reach it, and its one
/// candidate is that node over those groups, made by `origin` and sized for
/// `accuracy_budget_parts`.
pub(crate) fn add_group(
    memo: &mut Memo,
    node: &Rc<OperatorNode>,
    queries: &[usize],
    origin: &OriginOfCandidateForMemoGroup,
    accuracy_budget_parts: usize,
) -> MemoGroupId {
    let (logical_node, child_groups) =
        over_child_groups(memo, node, queries, origin, accuracy_budget_parts);
    let mut queries = queries.to_vec();
    queries.sort_unstable();
    queries.dedup();
    let candidate = CandidateForMemoGroup {
        fused_groups: Vec::new(),
        root_node: Rc::clone(&logical_node),
        input_groups: child_groups.clone(),
        origin: origin.clone(),
        accuracy_budget_parts,
    };
    memo.push_group(
        MemoGroup {
            logical_node,
            child_groups,
            parents: Vec::new(),
            queries,
        },
        vec![candidate],
    )
}

/// `node`'s operator over the groups of its children, each found or added
/// with every node below it ([`find_or_add_group`], same arguments), and
/// those groups by child position: group `i` is child `i`'s, so an operator
/// reading one group twice (`x / x`) lists it twice. `node` itself when
/// every child is its group's node.
pub(crate) fn over_child_groups(
    memo: &mut Memo,
    node: &Rc<OperatorNode>,
    queries: &[usize],
    origin: &OriginOfCandidateForMemoGroup,
    accuracy_budget_parts: usize,
) -> (Rc<OperatorNode>, Vec<MemoGroupId>) {
    let mut group_of_child: HashMap<*const OperatorNode, MemoGroupId> = HashMap::new();
    let mut child_groups = Vec::new();
    for child in node.children() {
        let group = match group_of_child.get(&Rc::as_ptr(child)) {
            Some(&group) => group,
            None => {
                let group = find_or_add_group(memo, child, queries, origin, accuracy_budget_parts);
                group_of_child.insert(Rc::as_ptr(child), group);
                group
            }
        };
        child_groups.push(group);
    }
    let group_node = |child: &Rc<OperatorNode>| {
        Rc::clone(&memo.groups()[group_of_child[&Rc::as_ptr(child)].0 as usize].logical_node)
    };
    if node
        .children()
        .into_iter()
        .all(|child| Rc::ptr_eq(child, &group_node(child)))
    {
        return (Rc::clone(node), child_groups);
    }
    let rebuilt = Rc::new(OperatorNode {
        operator: node.operator.map_children(group_node),
        ..(**node).clone()
    });
    (rebuilt, child_groups)
}

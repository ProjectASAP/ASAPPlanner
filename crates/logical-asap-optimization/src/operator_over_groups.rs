//! One operator over groups (#1203): a candidate a rule composes or
//! rewrites as a tree of operators is stored as its root operator alone,
//! every node beneath it found or added as a memo group
//! ([`over_child_groups`], Stage 0's node equality). Pass 1 and the
//! Pass 2 rules store their candidates through [`operator_over_groups`].

use std::collections::HashMap;
use std::rc::Rc;

use asap_types::ir::OperatorNode;
use asap_types::stages::{Memo, MemoGroupId, OriginOfCandidateForMemoGroup};

use crate::added_groups::groups_read_through_added_groups;
use crate::find_or_add_group::over_child_groups;
use crate::own_candidate_sizing::size_own_candidate_for;

/// `root`, a candidate of a group `queries` reach, as one operator over
/// groups: the exact guarantees below it removed
/// ([`without_exact_guarantees_below`]), each child found or added with
/// every node below it ([`over_child_groups`]: an added group is made
/// by `origin`, sized for `accuracy_budget_parts`), and every added group it
/// reads through sized for at least `accuracy_budget_parts` (a reader sized
/// for more parts may find a group added for fewer). Returns the operator
/// over the groups' nodes and those groups by child position.
pub(crate) fn operator_over_groups(
    memo: &mut Memo,
    root: &Rc<OperatorNode>,
    queries: &[usize],
    origin: &OriginOfCandidateForMemoGroup,
    accuracy_budget_parts: usize,
) -> (Rc<OperatorNode>, Vec<MemoGroupId>) {
    let root = without_exact_guarantees_below(root);
    let (node, children) = over_child_groups(memo, &root, queries, origin, accuracy_budget_parts);
    for group in groups_read_through_added_groups(memo, &children).added_groups {
        size_own_candidate_for(memo, group, accuracy_budget_parts);
    }
    (node, children)
}

/// `root` with the exact guarantee removed from every node below it. Stage
/// 0's nodes carry no guarantee, and composition gives each exact node it
/// rebuilds the exact one ("exact over exact inputs"); that difference alone
/// must not split a composed node from the equal Stage 0 group
/// ([`find_or_add_group`](crate::find_or_add_group::find_or_add_group)
/// compares guarantees). An approximate
/// guarantee is kept, so builds that differ only in their failure
/// probability or guarantee stay different groups. `root`'s own guarantee
/// is the candidate's and is kept.
pub(crate) fn without_exact_guarantees_below(root: &Rc<OperatorNode>) -> Rc<OperatorNode> {
    fn without_exact(
        node: &Rc<OperatorNode>,
        done: &mut HashMap<*const OperatorNode, Rc<OperatorNode>>,
    ) -> Rc<OperatorNode> {
        if let Some(found) = done.get(&Rc::as_ptr(node)) {
            return Rc::clone(found);
        }
        let operator = node
            .operator
            .map_children(|child| without_exact(child, done));
        let exact = node.guarantee.as_ref().is_some_and(|g| g.is_exact());
        let changed = exact
            || node
                .children()
                .iter()
                .zip(operator.children())
                .any(|(a, b)| !Rc::ptr_eq(a, b));
        let rebuilt = if changed {
            Rc::new(OperatorNode {
                operator,
                guarantee: if exact { None } else { node.guarantee.clone() },
                ..(**node).clone()
            })
        } else {
            Rc::clone(node)
        };
        done.insert(Rc::as_ptr(node), Rc::clone(&rebuilt));
        rebuilt
    }
    let mut done = HashMap::new();
    let operator = root
        .operator
        .map_children(|child| without_exact(child, &mut done));
    if root
        .children()
        .iter()
        .zip(operator.children())
        .all(|(a, b)| Rc::ptr_eq(a, b))
    {
        return Rc::clone(root);
    }
    Rc::new(OperatorNode {
        operator,
        ..(**root).clone()
    })
}

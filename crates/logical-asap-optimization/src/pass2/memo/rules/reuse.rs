//! Shared boundary handling for producer-level reuse rules. Rule outputs retain
//! canonical input identities after guarantee recomposition rebuilds their DAGs.
use crate::pass2::memo::candidates::{
    CandidateForMemoGroup, CandidateMemo, OriginOfCandidateForMemoGroup,
};
use crate::pass2::memo::state_groups::{boundary_groups, StateGroupExpansionError};
use crate::pass2::memo::{MemoGroup, MemoGroupId};
use asap_types::ir::OperatorNode;
use std::collections::{HashMap, HashSet};
use std::rc::Rc;

pub(in crate::pass2::memo) fn eligible(candidate: &CandidateForMemoGroup) -> bool {
    !candidate.implements.is_empty() && candidate.accuracy_budget_parts > 0
}

pub(in crate::pass2::memo) fn restore_inputs(
    node: &Rc<OperatorNode>,
    memo: &CandidateMemo,
    inputs: &[MemoGroupId],
) -> Rc<OperatorNode> {
    // These are the same declared logical input groups, rebound after a rule
    // cloned the DAG. Value guarantees are recomposed from selected suppliers
    // during reconstruction; state inputs additionally require the same error
    // contract because the intermediate state's guarantee is part of its API.
    let canonical: Vec<_> = inputs
        .iter()
        .flat_map(|id| OperatorNode::reachable(&memo.groups[id.0 as usize].logical_node))
        .collect();
    fn visit(
        node: &Rc<OperatorNode>,
        canonical: &[Rc<OperatorNode>],
        done: &mut HashMap<*const OperatorNode, Rc<OperatorNode>>,
    ) -> Rc<OperatorNode> {
        if let Some(found) = done.get(&Rc::as_ptr(node)) {
            return Rc::clone(found);
        }
        let operator = node
            .operator
            .map_children(|child| visit(child, canonical, done));
        let rebuilt = Rc::new(OperatorNode {
            operator,
            ..(**node).clone()
        });
        let result = canonical
            .iter()
            .find(|known| {
                same_node(
                    known,
                    &rebuilt,
                    known.result_kind == asap_types::ir::OperatorResultKind::State,
                )
            })
            .cloned()
            .unwrap_or(rebuilt);
        done.insert(Rc::as_ptr(node), Rc::clone(&result));
        result
    }
    visit(node, &canonical, &mut HashMap::new())
}

pub(in crate::pass2::memo) fn same_node(
    left: &OperatorNode,
    right: &OperatorNode,
    guarantees: bool,
) -> bool {
    left.operator.map_children(|_| ()) == right.operator.map_children(|_| ())
        && left.schema == right.schema
        && left.result_kind == right.result_kind
        && left.timing == right.timing
        && left.materialization == right.materialization
        && left.children().len() == right.children().len()
        && left
            .children()
            .iter()
            .zip(right.children())
            .all(|(a, b)| Rc::ptr_eq(a, b))
        && serde_json::to_string(&(
            left.operator.map_children(|_| ()),
            &left.schema,
            left.result_kind,
            left.timing,
            left.materialization,
        ))
        .ok()
            == serde_json::to_string(&(
                right.operator.map_children(|_| ()),
                &right.schema,
                right.result_kind,
                right.timing,
                right.materialization,
            ))
            .ok()
        && (!guarantees || left.guarantee == right.guarantee)
}

pub(in crate::pass2::memo) fn append_producer(
    memo: &mut CandidateMemo,
    node: Rc<OperatorNode>,
    queries: &[usize],
    origin: OriginOfCandidateForMemoGroup,
    budget_share: usize,
) -> Result<MemoGroupId, StateGroupExpansionError> {
    // Reuse only dedicated producer groups. A logical result group can contain
    // other implementations and cannot enforce this producer's exact contract.
    if let Some((index, _)) = memo.groups.iter().enumerate().find(|(index, group)| {
        group.roots_of.is_empty()
            && memo.candidates[*index].len() == 1
            && memo.candidates[*index][0].origin == origin
            && memo.candidates[*index][0].accuracy_budget_parts == budget_share
            && same_node(&group.logical_node, &node, true)
    }) {
        return Ok(MemoGroupId(index as u32));
    }
    let id = MemoGroupId(
        u32::try_from(memo.groups.len()).map_err(|_| StateGroupExpansionError::TooManyGroups)?,
    );
    let node = Rc::new((*node).clone());
    let boundaries = memo
        .groups
        .iter()
        .enumerate()
        .map(|(id, group)| (Rc::as_ptr(&group.logical_node), MemoGroupId(id as u32)))
        .collect();
    let children = boundary_groups(&node, &boundaries);
    memo.groups.push(MemoGroup {
        logical_node: Rc::clone(&node),
        child_groups: children.clone(),
        parents: Vec::new(),
        queries: queries.to_vec(),
        roots_of: Vec::new(),
    });
    memo.candidates.push(vec![CandidateForMemoGroup {
        implements: vec![id],
        root_node: node,
        input_groups: children,
        origin,
        accuracy_budget_parts: budget_share,
    }]);
    Ok(id)
}

pub(in crate::pass2::memo) fn append_reader(
    memo: &mut CandidateMemo,
    owner: usize,
    original: &CandidateForMemoGroup,
    node: Rc<OperatorNode>,
    origin: OriginOfCandidateForMemoGroup,
) -> Result<bool, StateGroupExpansionError> {
    if original.root_node.schema.fields != node.schema.fields
        || original.root_node.schema.time_index != node.schema.time_index
        || original.root_node.schema.unique_keys != node.schema.unique_keys
        || (original.root_node.schema.closed && !node.schema.closed)
        || original.root_node.result_kind != node.result_kind
    {
        return Err(StateGroupExpansionError::ReuseRule(format!(
            "reuse changed the reader output contract ({origin:?}): expected {:?}, got {:?}",
            original.root_node.schema, node.schema
        )));
    }
    let boundaries = memo
        .groups
        .iter()
        .enumerate()
        .filter(|(id, _)| *id != owner)
        .map(|(id, group)| (Rc::as_ptr(&group.logical_node), MemoGroupId(id as u32)))
        .collect();
    let children = boundary_groups(&node, &boundaries);
    if memo.candidates[owner].iter().any(|known| {
        known.input_groups == children
            && known.accuracy_budget_parts == original.accuracy_budget_parts
            && *known.root_node == *node
            && serde_json::to_string(&known.root_node).ok() == serde_json::to_string(&node).ok()
    }) {
        return Ok(false);
    }
    let mut candidate = original.clone();
    candidate.root_node = node;
    candidate.input_groups = children;
    candidate.origin = origin;
    memo.candidates[owner].push(candidate);
    Ok(true)
}

pub(in crate::pass2::memo) fn replace(
    node: &Rc<OperatorNode>,
    replacements: &HashMap<*const OperatorNode, Rc<OperatorNode>>,
) -> Rc<OperatorNode> {
    fn visit(
        node: &Rc<OperatorNode>,
        replacements: &HashMap<*const OperatorNode, Rc<OperatorNode>>,
        done: &mut HashMap<*const OperatorNode, Rc<OperatorNode>>,
    ) -> Rc<OperatorNode> {
        if let Some(found) = replacements
            .get(&Rc::as_ptr(node))
            .or_else(|| done.get(&Rc::as_ptr(node)))
        {
            return Rc::clone(found);
        }
        let mut changed = false;
        let operator = node.operator.map_children(|child| {
            let replacement = visit(child, replacements, done);
            changed |= !Rc::ptr_eq(child, &replacement);
            replacement
        });
        let rebuilt = if changed {
            Rc::new(OperatorNode {
                operator,
                ..(**node).clone()
            })
        } else {
            Rc::clone(node)
        };
        done.insert(Rc::as_ptr(node), Rc::clone(&rebuilt));
        rebuilt
    }
    visit(node, replacements, &mut HashMap::new())
}

pub(in crate::pass2::memo) fn combined_inputs(
    left: &CandidateForMemoGroup,
    right: &CandidateForMemoGroup,
) -> Vec<MemoGroupId> {
    let mut seen = HashSet::new();
    left.input_groups
        .iter()
        .chain(&right.input_groups)
        .copied()
        .filter(|id| seen.insert(*id))
        .collect()
}

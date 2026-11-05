//! Shared boundary handling for producer-level reuse rules. Rule outputs retain
//! canonical input identities after guarantee recomposition rebuilds their DAGs.
use crate::find_or_add_group::find_or_add_group;
use crate::operator_over_groups::{operator_over_groups, without_exact_guarantees_below};
use crate::pass2::memo::state_groups::{boundary_groups, Pass2RuleError};
use asap_types::ir::OperatorNode;
use asap_types::stages::{CandidateForMemoGroup, Memo, OriginOfCandidateForMemoGroup};
use asap_types::stages::{MemoGroup, MemoGroupId};
use std::collections::{HashMap, HashSet};
use std::rc::Rc;

pub(in crate::pass2::memo) fn eligible(candidate: &CandidateForMemoGroup) -> bool {
    candidate.accuracy_budget_parts > 0
}

pub(in crate::pass2::memo) fn restore_inputs(
    node: &Rc<OperatorNode>,
    memo: &Memo,
    inputs: &[MemoGroupId],
) -> Rc<OperatorNode> {
    // These are the same declared logical input groups, rebound after a rule
    // cloned the DAG. Value guarantees are recomposed from selected producers
    // during reconstruction; state inputs additionally require the same error
    // contract because the intermediate state's guarantee is part of its API.
    let canonical: Vec<_> = inputs
        .iter()
        .flat_map(|id| OperatorNode::reachable(&memo.groups()[id.0 as usize].logical_node))
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
    memo: &mut Memo,
    node: Rc<OperatorNode>,
    queries: &[usize],
    origin: OriginOfCandidateForMemoGroup,
    accuracy_budget_parts: usize,
) -> Result<MemoGroupId, Pass2RuleError> {
    // Reuse only dedicated producer groups. A logical result group can contain
    // other implementations and cannot enforce this producer's exact contract.
    let root_groups = asap_types::stages::root_groups(&memo.query_roots);
    if let Some((index, _)) = memo.groups().iter().enumerate().find(|(index, group)| {
        !root_groups.contains(&MemoGroupId(*index as u32))
            && memo.candidates[*index].len() == 1
            && memo.candidates[*index][0].origin == origin
            && memo.candidates[*index][0].accuracy_budget_parts == accuracy_budget_parts
            && same_node(&group.logical_node, &node, true)
    }) {
        return Ok(MemoGroupId(index as u32));
    }
    // The new group's id must fit a `MemoGroupId`.
    u32::try_from(memo.groups().len()).map_err(|_| Pass2RuleError::TooManyGroups)?;
    let node = Rc::new((*node).clone());
    let children = boundary_groups(memo, &node, |_| true);
    Ok(memo.push_group(
        MemoGroup {
            logical_node: Rc::clone(&node),
            child_groups: children.clone(),
            parents: Vec::new(),
            queries: queries.to_vec(),
        },
        vec![CandidateForMemoGroup {
            fused_groups: Vec::new(),
            root_node: node,
            input_groups: children,
            origin,
            accuracy_budget_parts,
        }],
    ))
}

/// Whether `actual` has `expected`'s output: the same result kind, fields,
/// time index and unique keys, and a closed label set where `expected`'s is
/// closed (a projection may enumerate every output of an open schema).
pub(in crate::pass2::memo) fn same_output_contract(
    expected: &OperatorNode,
    actual: &OperatorNode,
) -> bool {
    expected.result_kind == actual.result_kind
        && expected.schema.fields == actual.schema.fields
        && expected.schema.time_index == actual.schema.time_index
        && expected.schema.unique_keys == actual.schema.unique_keys
        && (!expected.schema.closed || actual.schema.closed)
}

/// Rejects `node`, `original` rewritten by `origin`, when its output differs
/// from `original`'s ([`same_output_contract`]).
fn check_output_contract(
    original: &CandidateForMemoGroup,
    node: &OperatorNode,
    origin: &OriginOfCandidateForMemoGroup,
) -> Result<(), Pass2RuleError> {
    if !same_output_contract(&original.root_node, node) {
        return Err(Pass2RuleError::ReuseRule(format!(
            "reuse changed the reader output contract ({origin:?}): expected {:?}, got {:?}",
            original.root_node.schema, node.schema
        )));
    }
    Ok(())
}

/// Adds to group `owner` the candidate `original` rewritten by `origin` to
/// `node`, as one operator over groups ([`operator_over_groups`]: every node
/// beneath `node` is found or added as a group made by `origin`, sized for
/// `original`'s budget parts). `false` when `owner` has an equal candidate
/// already ([`Memo::add_candidate`]), or when `node` reads `owner`
/// itself.
pub(in crate::pass2::memo) fn add_candidate_over_groups(
    memo: &mut Memo,
    owner: MemoGroupId,
    original: &CandidateForMemoGroup,
    node: Rc<OperatorNode>,
    origin: OriginOfCandidateForMemoGroup,
) -> Result<bool, Pass2RuleError> {
    check_output_contract(original, &node, &origin)?;
    let queries = memo.groups()[owner.0 as usize].queries.clone();
    let (root_node, input_groups) = operator_over_groups(
        memo,
        &node,
        &queries,
        &origin,
        original.accuracy_budget_parts,
    );
    if input_groups.contains(&owner) {
        return Ok(false);
    }
    let mut candidate = original.clone();
    candidate.root_node = root_node;
    candidate.input_groups = input_groups;
    candidate.origin = origin;
    let known = memo.candidates[owner.0 as usize].len();
    Ok(memo.add_candidate(owner, candidate).index == known)
}

/// The group of `node`, a producer a rule factors out of its readers, made
/// by `origin` when no group's node equals it ([`find_or_add_group`]), its
/// exact guarantees removed first so it equals the groups of the exact nodes
/// it repeats. A producer holds no approximate state of its own, so its
/// candidate is sized for the whole budget (1 part).
pub(in crate::pass2::memo) fn find_or_add_producer(
    memo: &mut Memo,
    node: &Rc<OperatorNode>,
    queries: &[usize],
    origin: &OriginOfCandidateForMemoGroup,
) -> MemoGroupId {
    let below = without_exact_guarantees_below(node);
    let node = if below.guarantee.as_ref().is_some_and(|g| g.is_exact()) {
        Rc::new(OperatorNode {
            guarantee: None,
            ..(*below).clone()
        })
    } else {
        below
    };
    find_or_add_group(memo, &node, queries, origin, 1)
}

pub(in crate::pass2::memo) fn append_reader(
    memo: &mut Memo,
    owner: usize,
    original: &CandidateForMemoGroup,
    node: Rc<OperatorNode>,
    origin: OriginOfCandidateForMemoGroup,
) -> Result<bool, Pass2RuleError> {
    check_output_contract(original, &node, &origin)?;
    let children = boundary_groups(memo, &node, |group| group.0 as usize != owner);
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

//! What the producer-level reuse rules share: which candidates they read
//! ([`eligible`]), and how they add what they derive, as one operator over
//! groups ([`add_candidate_over_groups`], [`find_or_add_producer`]).
use crate::find_or_add_group::find_or_add_group;
use crate::operator_over_groups::{operator_over_groups, without_exact_guarantees};
use crate::pass2::memo::state_groups::Pass2RuleError;
use asap_types::ir::OperatorNode;
use asap_types::stages::MemoGroupId;
use asap_types::stages::{CandidateForMemoGroup, Memo, OriginOfCandidateForMemoGroup};
use std::collections::HashMap;
use std::rc::Rc;

pub(in crate::pass2::memo) fn eligible(candidate: &CandidateForMemoGroup) -> bool {
    candidate.accuracy_budget_parts > 0
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
    find_or_add_group(memo, &without_exact_guarantees(node), queries, origin, 1)
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

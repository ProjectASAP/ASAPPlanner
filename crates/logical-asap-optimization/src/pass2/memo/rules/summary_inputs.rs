//! Expose private value projections and filters below summary builds as canonical inputs.
//! This preserves their full expressions and schema, including opaque series
//! identity, rather than assuming that a projection is an identity operation.
use super::reuse::{add_candidate_over_groups, eligible, find_or_add_producer, replace};
use crate::pass2::memo::state_groups::{boundary_ptrs, nodes_within, Pass2RuleError};
use asap_types::ir::{ASAPOp, NonASAPOp, OperatorNode};
use asap_types::stages::{Memo, MemoGroupId, OriginOfCandidateForMemoGroup};
use std::collections::{HashMap, HashSet};
use std::rc::Rc;

pub(crate) fn factor_summary_inputs(memo: &mut Memo) -> Result<(), Pass2RuleError> {
    let originals: Vec<_> = memo
        .candidates
        .iter()
        .enumerate()
        .flat_map(|(owner, candidates)| {
            candidates
                .iter()
                .filter(|candidate| eligible(candidate))
                .map(move |candidate| (MemoGroupId(owner as u32), candidate.clone()))
        })
        .collect();
    for (owner, original) in originals {
        let boundaries = boundary_ptrs(memo, &original);
        let mut inputs = Vec::new();
        for node in nodes_within(&original.root_node, &boundaries) {
            if matches!(
                node.asap(),
                Some(ASAPOp::SummaryAgg { .. } | ASAPOp::SummaryTumblingWindowChain { .. })
            ) {
                for child in node.children() {
                    if !boundaries.contains(&Rc::as_ptr(child)) {
                        inputs.push(Rc::clone(child));
                    }
                }
            }
        }
        let queries = memo.groups()[owner.0 as usize].queries.clone();
        let mut replacements = HashMap::new();
        for input in inputs {
            canonical_preprocessing(memo, &input, &boundaries, &queries, &mut replacements);
        }
        if !replacements.is_empty() {
            let reader = replace(&original.root_node, &replacements);
            add_candidate_over_groups(memo, owner, &original, reader, original.origin.clone())?;
        }
    }
    Ok(())
}

fn canonical_preprocessing(
    memo: &mut Memo,
    node: &Rc<OperatorNode>,
    boundaries: &HashSet<*const OperatorNode>,
    queries: &[usize],
    replacements: &mut HashMap<*const OperatorNode, Rc<OperatorNode>>,
) -> bool {
    if replacements.contains_key(&Rc::as_ptr(node)) || boundaries.contains(&Rc::as_ptr(node)) {
        return true;
    }
    if !matches!(
        node.non_asap(),
        Some(NonASAPOp::Project { .. } | NonASAPOp::Filter { .. })
    ) {
        return false;
    }
    // Predicates/projections can reference subqueries. Every dependency must
    // be canonical, not only the row-stream child.
    for child in node.children() {
        if !canonical_preprocessing(memo, child, boundaries, queries, replacements) {
            return false;
        }
    }
    let canonical = replace(node, replacements);
    let group = find_or_add_producer(
        memo,
        &canonical,
        queries,
        // These operators introduce no approximate state, so the group does
        // not depend on how a downstream consumer divides its error budget
        // ([`find_or_add_producer`] sizes it for the whole budget).
        &OriginOfCandidateForMemoGroup::SummaryInput,
    );
    replacements.insert(
        Rc::as_ptr(node),
        Rc::clone(&memo.groups()[group.0 as usize].logical_node),
    );
    true
}

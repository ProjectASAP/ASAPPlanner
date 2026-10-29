//! Expose private value projections and filters below summary builds as canonical inputs.
//! This preserves their full expressions and schema, including opaque series
//! identity, rather than assuming that a projection is an identity operation.
use super::reuse::{append_producer, append_reader, eligible, replace};
use crate::pass2::memo::candidates::{CandidateMemo, CandidateOrigin};
use crate::pass2::memo::check_generation_deadline;
use crate::pass2::memo::state_groups::StateGroupExpansionError;
use asap_types::ir::{ASAPOp, NonASAPOp, OperatorNode};
use std::collections::{HashMap, HashSet};
use std::rc::Rc;
use std::time::Instant;

pub(in crate::pass2::memo) fn factor_summary_inputs(
    memo: &mut CandidateMemo,
    deadline: Option<Instant>,
) -> Result<(), StateGroupExpansionError> {
    check_generation_deadline(deadline)?;
    let originals: Vec<_> = memo
        .candidates
        .iter()
        .enumerate()
        .flat_map(|(owner, candidates)| {
            candidates
                .iter()
                .filter(|candidate| eligible(candidate))
                .map(move |candidate| (owner, candidate.clone()))
        })
        .collect();
    for (owner, original) in originals {
        check_generation_deadline(deadline)?;
        let boundaries: HashSet<_> = original
            .children
            .iter()
            .map(|group| Rc::as_ptr(&memo.groups[group.0 as usize].node))
            .collect();
        let mut pending = vec![Rc::clone(&original.node)];
        let mut seen = HashSet::new();
        let mut inputs = Vec::new();
        while let Some(node) = pending.pop() {
            check_generation_deadline(deadline)?;
            if boundaries.contains(&Rc::as_ptr(&node)) || !seen.insert(Rc::as_ptr(&node)) {
                continue;
            }
            if matches!(
                node.asap(),
                Some(ASAPOp::SummaryAgg { .. } | ASAPOp::PaneChain { .. })
            ) {
                for child in node.children() {
                    check_generation_deadline(deadline)?;
                    if !boundaries.contains(&Rc::as_ptr(child)) {
                        inputs.push(Rc::clone(child));
                    }
                }
            }
            pending.extend(node.children().into_iter().cloned());
        }
        let queries = memo.groups[owner].queries.clone();
        let mut replacements = HashMap::new();
        for input in inputs {
            check_generation_deadline(deadline)?;
            canonical_preprocessing(memo, &input, &boundaries, &queries, &mut replacements)?;
        }
        if !replacements.is_empty() {
            let reader = replace(&original.node, &replacements);
            append_reader(memo, owner, &original, reader, original.origin.clone())?;
        }
    }
    check_generation_deadline(deadline)?;
    Ok(())
}

fn canonical_preprocessing(
    memo: &mut CandidateMemo,
    node: &Rc<OperatorNode>,
    boundaries: &HashSet<*const OperatorNode>,
    queries: &[usize],
    replacements: &mut HashMap<*const OperatorNode, Rc<OperatorNode>>,
) -> Result<bool, StateGroupExpansionError> {
    if replacements.contains_key(&Rc::as_ptr(node)) {
        return Ok(true);
    }
    if boundaries.contains(&Rc::as_ptr(node)) {
        // Logical TimeRange groups can remain distinct when their inputs
        // differ structurally. The existing state-sharing rule nevertheless
        // permits identical range descriptors over the same canonical source.
        if let Some(group) = memo
            .groups
            .iter()
            .find(|group| crate::pass2::memo::state_groups::same_range_input(&group.node, node))
        {
            replacements.insert(Rc::as_ptr(node), Rc::clone(&group.node));
        }
        return Ok(true);
    }
    if !matches!(
        node.non_asap(),
        Some(NonASAPOp::Project { .. } | NonASAPOp::Filter { .. })
    ) {
        return Ok(false);
    }
    // Predicates/projections can reference subqueries. Every dependency must
    // be canonical, not only the row-stream child.
    for child in node.children() {
        if !canonical_preprocessing(memo, child, boundaries, queries, replacements)? {
            return Ok(false);
        }
    }
    let canonical = replace(node, replacements);
    let group = append_producer(
        memo,
        canonical,
        queries,
        CandidateOrigin::SummaryInput,
        // These operators introduce no approximate state. Their identity does
        // not depend on how a downstream consumer divides its error budget.
        1,
    )?;
    replacements.insert(
        Rc::as_ptr(node),
        Rc::clone(&memo.groups[group.0 as usize].node),
    );
    Ok(true)
}

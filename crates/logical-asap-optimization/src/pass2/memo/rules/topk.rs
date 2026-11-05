//! Rule 4, top-k reuse: exact top-k groups over one input and partition with
//! different `k` read the ranking of the largest `k`, `Limit(k_max)` over
//! `Sort` of the input, which Pass 1 already made groups of.
use crate::operator_tree::add_operator_tree;
use crate::pass2::asap_aware_cse::topk_reuse_candidates;
use crate::pass2::memo::well_formed::Pass2RuleError;
use asap_types::ir::operator::{AggIntent, Reduction};
use asap_types::ir::properties::AccuracyGuarantee;
use asap_types::ir::{NonASAPOp, Operator, OperatorNode, QueryRoot};
use asap_types::stages::{Memo, MemoGroupId, OriginOfCandidateForMemoGroup};
use std::collections::HashMap;
use std::rc::Rc;

/// Rule 4: adds to each exact top-k group whose input and partition another
/// top-k group with a larger `k` shares a candidate
/// `Project(Limit(k)(G_ranking))`, `G_ranking` the group of
/// `Limit(k_max)(Sort(G_input))`. Returns the number of candidates added.
pub(crate) fn add_topk_candidates(memo: &mut Memo) -> Result<usize, Pass2RuleError> {
    // Each top-k group's node over an input marked exact, so the ranking
    // reuse rule matches it; accuracy is checked at the query's root.
    let mut owners = HashMap::new();
    let mut roots = Vec::new();
    for (group, entry) in memo.groups.iter().enumerate() {
        let [input] = entry.logical_node.children()[..] else {
            continue;
        };
        let exact = Rc::new(
            (**input)
                .clone()
                .with_guarantee(Some(AccuracyGuarantee::exact("a top-k input"))),
        );
        let Ok(copy) = entry.logical_node.with_new_children(|_| Rc::clone(&exact)) else {
            continue;
        };
        let copy = Rc::new(copy);
        owners.insert(Rc::as_ptr(&copy), MemoGroupId(group as u32));
        roots.push(QueryRoot::Operator(copy));
    }
    let mut readers = Vec::new();
    for reuse in topk_reuse_candidates(&roots) {
        let Some(&first) = reuse.members.first().and_then(|m| owners.get(&Rc::as_ptr(m))) else {
            continue;
        };
        let Some(&input_group) = memo.groups[first.0 as usize].child_groups.first() else {
            continue;
        };
        let input = Rc::clone(&memo.groups[input_group.0 as usize].logical_node);
        let rerun = |node: &OperatorNode, child: Rc<OperatorNode>| {
            node.with_new_children(|_| Rc::clone(&child))
                .map(Rc::new)
                .map_err(|error| Pass2RuleError::ReuseRule(error.to_string()))
        };
        let sort = rerun(reuse.ranking.children()[0], input)?;
        let ranking = rerun(&reuse.ranking, sort)?;
        let Some(NonASAPOp::Limit { n: Some(k_max), .. }) = ranking.non_asap() else {
            continue;
        };
        for member in &reuse.members {
            let Some(&owner) = owners.get(&Rc::as_ptr(member)) else {
                continue;
            };
            let logical = Rc::clone(&memo.groups[owner.0 as usize].logical_node);
            let Some(NonASAPOp::Aggregate {
                reduction: Reduction::Reduce(keys),
                measures,
                ..
            }) = logical.non_asap()
            else {
                continue;
            };
            let [AggIntent::TopK { k, .. }] = measures.as_slice() else {
                continue;
            };
            if k == k_max {
                continue;
            }
            let limited = OperatorNode::new_shared(Operator::NonASAP(NonASAPOp::Limit {
                n: Some(*k),
                offset: 0,
                partition_by: keys.clone(),
                child: Rc::clone(&ranking),
            }))
            .map_err(|error| Pass2RuleError::ReuseRule(error.to_string()))?;
            let columns = asap_types::ir::schema::topk::ranked_row_projection(
                &ranking,
                &Reduction::Reduce(keys.clone()),
                &logical.schema.fields[keys.len()].name,
            )
            .map_err(|error| Pass2RuleError::ReuseRule(error.to_string()))?;
            let reader = OperatorNode::new_shared(Operator::NonASAP(NonASAPOp::Project {
                cols: columns,
                qualifier: None,
                child: limited,
            }))
            .map_err(|error| Pass2RuleError::ReuseRule(error.to_string()))?;
            readers.push((owner, reader));
        }
    }
    let mut added = 0;
    for (owner, reader) in readers {
        let candidate =
            add_operator_tree(memo, &reader, &OriginOfCandidateForMemoGroup::TopKReuse, 1);
        let known = memo.candidates[owner.0 as usize].len();
        added += usize::from(memo.add_candidate(owner, candidate).index == known);
    }
    Ok(added)
}

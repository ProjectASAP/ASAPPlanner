//! Exact ranking reuse with explicit exact-input groups. No global selection
//! flags are required: only exact implementations can supply these inputs.
use super::reuse::{append_producer, append_reader, eligible, replace};
use crate::asap_aware_cse::topk_reuse_candidates;
use crate::memo::candidates::{CandidateMemo, CandidateOrigin};
use crate::memo::check_generation_deadline;
use crate::memo::state_groups::StateGroupExpansionError;
use crate::memo::MemoGroupId;
use asap_types::ir::operator::{AggIntent, Reduction};
use asap_types::ir::properties::AccuracyGuarantee;
use asap_types::ir::schema::FieldDataType;
use asap_types::ir::{ASAPOp, NonASAPOp, Operator, OperatorNode, QueryRoot};
use std::collections::{HashMap, HashSet};
use std::rc::Rc;
use std::time::Instant;

pub(in crate::memo) fn add_topk_candidates(
    memo: &mut CandidateMemo,
    deadline: Option<Instant>,
) -> Result<usize, StateGroupExpansionError> {
    check_generation_deadline(deadline)?;
    let mut owners = HashMap::new();
    let mut roots = Vec::new();
    for (group, entry) in memo.groups.iter().enumerate() {
        check_generation_deadline(deadline)?;
        let [input] = entry.node.children()[..] else {
            continue;
        };
        let exact = Rc::new(
            (**input)
                .clone()
                .with_guarantee(Some(AccuracyGuarantee::exact(
                    "exact input candidate group",
                ))),
        );
        let Ok(copy) = entry.node.with_new_children(|_| Rc::clone(&exact)) else {
            continue;
        };
        let copy = Rc::new(copy);
        owners.insert(Rc::as_ptr(&copy), group);
        roots.push(QueryRoot::Operator(copy));
    }
    let mut exact_groups = HashMap::new();
    let mut added = 0;
    for reuse in topk_reuse_candidates(&roots) {
        check_generation_deadline(deadline)?;
        let members: Vec<_> = reuse
            .members
            .iter()
            .filter_map(|node| owners.get(&Rc::as_ptr(node)).copied())
            .collect();
        if members.len() < 2 {
            continue;
        }
        let Some(input_group) = memo.groups[members[0]].children.first().copied() else {
            continue;
        };
        let Some(exact_input) = exact_input_group(memo, input_group, &mut exact_groups)? else {
            continue;
        };
        let input = Rc::clone(&memo.groups[exact_input.0 as usize].node);
        let queries: Vec<_> = members
            .iter()
            .flat_map(|group| memo.groups[*group].queries.iter().copied())
            .collect::<std::collections::BTreeSet<_>>()
            .into_iter()
            .collect();
        let sort = reuse.ranking.children()[0]
            .with_new_children(|_| Rc::clone(&input))
            .map_err(|error| StateGroupExpansionError::ReuseRule(error.to_string()))?;
        let sort_group =
            append_producer(memo, Rc::new(sort), &queries, CandidateOrigin::TopKReuse, 1)?;
        let sort = Rc::clone(&memo.groups[sort_group.0 as usize].node);
        let ranking = reuse
            .ranking
            .with_new_children(|_| Rc::clone(&sort))
            .map_err(|error| StateGroupExpansionError::ReuseRule(error.to_string()))?;
        let ranking_group = append_producer(
            memo,
            Rc::new(ranking),
            &queries,
            CandidateOrigin::TopKReuse,
            1,
        )?;
        let ranking = Rc::clone(&memo.groups[ranking_group.0 as usize].node);
        for owner in members {
            check_generation_deadline(deadline)?;
            let logical = Rc::clone(&memo.groups[owner].node);
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
            let Some(original) = memo.candidates[owner].iter().find(|candidate| eligible(candidate) && matches!(candidate.node.non_asap(), Some(NonASAPOp::Aggregate {measures, ..}) if matches!(measures.as_slice(), [AggIntent::TopK {..}]))).cloned() else { continue };
            let limited = OperatorNode::new_shared(Operator::NonASAP(NonASAPOp::Limit {
                n: Some(*k),
                offset: 0,
                partition_by: keys.clone(),
                child: Rc::clone(&ranking),
            }))
            .map_err(|error| StateGroupExpansionError::ReuseRule(error.to_string()))?;
            let columns = asap_types::ir::schema::topk::ranked_row_projection(
                &ranking,
                &Reduction::Reduce(keys.clone()),
                &logical.schema.fields[keys.len()].name,
            )
            .map_err(|error| StateGroupExpansionError::ReuseRule(error.to_string()))?;
            let reader = OperatorNode::new_shared(Operator::NonASAP(NonASAPOp::Project {
                cols: columns,
                qualifier: None,
                child: limited,
            }))
            .map_err(|error| StateGroupExpansionError::ReuseRule(error.to_string()))?;
            added += usize::from(append_reader(
                memo,
                owner,
                &original,
                reader,
                CandidateOrigin::TopKReuse,
            )?);
        }
    }
    check_generation_deadline(deadline)?;
    Ok(added)
}

fn exact_input_group(
    memo: &mut CandidateMemo,
    group: MemoGroupId,
    known: &mut HashMap<MemoGroupId, Option<MemoGroupId>>,
) -> Result<Option<MemoGroupId>, StateGroupExpansionError> {
    if let Some(result) = known.get(&group) {
        return Ok(*result);
    }
    // An in-progress marker also prevents malformed cyclic inventories recursing.
    known.insert(group, None);
    let originals = memo.candidates[group.0 as usize].clone();
    let original_children: Vec<_> = originals
        .iter()
        .map(|candidate| candidate.children.clone())
        .collect();
    let mut candidates = Vec::new();
    for mut candidate in originals {
        if !eligible(&candidate) {
            continue;
        }
        let boundaries: HashSet<_> = candidate
            .children
            .iter()
            .map(|id| Rc::as_ptr(&memo.groups[id.0 as usize].node))
            .collect();
        let mut pending = vec![Rc::clone(&candidate.node)];
        let mut seen = HashSet::new();
        let mut exact = true;
        while let Some(node) = pending.pop() {
            if boundaries.contains(&Rc::as_ptr(&node)) || !seen.insert(Rc::as_ptr(&node)) {
                continue;
            }
            if matches!(node.asap(), Some(ASAPOp::SummaryAgg {family, ..} | ASAPOp::PaneChain {family, ..}) if !matches!(family, FieldDataType::ExactAggregate(..)))
            {
                exact = false;
                break;
            }
            pending.extend(node.children().into_iter().cloned());
        }
        if !exact {
            continue;
        }
        let mut replacements = HashMap::new();
        let mut children = Vec::new();
        for child in &candidate.children {
            let Some(exact_child) = exact_input_group(memo, *child, known)? else {
                exact = false;
                break;
            };
            replacements.insert(
                Rc::as_ptr(&memo.groups[child.0 as usize].node),
                Rc::clone(&memo.groups[exact_child.0 as usize].node),
            );
            children.push(exact_child);
        }
        if !exact {
            continue;
        }
        candidate.node = replace(&candidate.node, &replacements);
        candidate.children = children;
        candidates.push(candidate);
    }
    if candidates.is_empty() {
        return Ok(None);
    }
    if candidates.len() == original_children.len()
        && candidates
            .iter()
            .zip(&original_children)
            .all(|(candidate, children)| candidate.children == *children)
    {
        // An already exact-only group remains the shared source for ordinary
        // consumers too; cloning it would unnecessarily split scan/state reuse.
        known.insert(group, Some(group));
        return Ok(Some(group));
    }
    let node = Rc::new((*candidates[0].node).clone().with_guarantee(Some(
        AccuracyGuarantee::exact("only exact implementations and exact inputs"),
    )));
    let queries = memo.groups[group.0 as usize].queries.clone();
    let id = append_producer(memo, node, &queries, CandidateOrigin::TopKReuse, 1)?;
    for candidate in &mut candidates {
        candidate.implements = vec![id];
    }
    memo.candidates[id.0 as usize] = candidates;
    known.insert(group, Some(id));
    Ok(Some(id))
}

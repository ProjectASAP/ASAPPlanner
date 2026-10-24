//! Scan-label widening for explicit memo state sharing. Reuses the existing
//! Pass 2 widening rule; only its source-chain boundaries are interned here.
use std::collections::{HashMap, HashSet};
use std::rc::Rc;

use asap_types::ir::operator::Reduction;
use asap_types::ir::{ASAPOp, NonASAPOp, OperatorNode, QueryRoot};

use super::candidates::{CandidateMemo, CandidateOrigin, ImplementationCandidate};
use super::state_groups::{boundary_groups, simple_build, StateGroupExpansionError};
use super::{MemoGroup, MemoGroupId};

/// Preserve local implementations and append readers over union-label inputs.
/// Parent/query reachability is rebuilt by the enclosing state-group expansion.
pub(super) fn add_scan_label_candidates(
    memo: &mut CandidateMemo,
) -> Result<usize, StateGroupExpansionError> {
    let mut owners = Vec::new();
    let mut roots = Vec::new();
    for (group, candidates) in memo.candidates.iter().enumerate() {
        for (index, candidate) in candidates.iter().enumerate() {
            let Some((build, children)) = simple_build(memo, candidate) else {
                continue;
            };
            if !matches!(
                build.asap(),
                Some(
                    ASAPOp::SummaryAgg {
                        reduction: Reduction::PerEntity,
                        filter: None,
                        ..
                    } | ASAPOp::PaneChain {
                        reduction: Reduction::PerEntity,
                        filter: None,
                        ..
                    }
                )
            ) || candidate
                .children
                .iter()
                .any(|child| !children.contains(child))
                || children
                    .iter()
                    .any(|child| !is_source_chain(&memo.groups[child.0 as usize].node))
            {
                continue;
            }
            owners.push((group, index));
            roots.push(QueryRoot::Operator(Rc::clone(&candidate.node)));
        }
    }
    let Some(widened) = crate::asap_aware_cse::with_union_scan_labels(&roots)
        .map_err(|error| StateGroupExpansionError::ScanLabelWidening(error.to_string()))?
    else {
        return Ok(0);
    };
    let mut added = 0;
    let mut canonical = HashMap::new();
    for ((group, index), root) in owners.into_iter().zip(widened) {
        let QueryRoot::Operator(node) = root else {
            unreachable!("operator readers")
        };
        let original = memo.candidates[group][index].clone();
        if *node == *original.node || !same_output_contract(&original.node, &node) {
            continue;
        }
        let queries = memo.groups[group].queries.clone();
        let node = intern_inputs(&node, memo, &queries, &mut canonical)?;
        let boundaries = memo
            .groups
            .iter()
            .enumerate()
            .map(|(id, group)| (Rc::as_ptr(&group.node), MemoGroupId(id as u32)))
            .collect();
        let mut reader = original;
        reader.node = node;
        reader.children = boundary_groups(&reader.node, &boundaries);
        reader.origin = CandidateOrigin::WidenedScans;
        if memo.candidates[group].iter().any(|known| {
            known.origin == CandidateOrigin::WidenedScans
                && known.children == reader.children
                && *known.node == *reader.node
        }) {
            continue;
        }
        memo.candidates[group].push(reader);
        added += 1;
    }
    Ok(added)
}

fn same_output_contract(expected: &OperatorNode, actual: &OperatorNode) -> bool {
    expected.result_kind == actual.result_kind
        && expected.schema.fields == actual.schema.fields
        && expected.schema.time_index == actual.schema.time_index
        && expected.schema.unique_keys == actual.schema.unique_keys
        // A projection may enumerate every output of an originally open schema.
        && (!expected.schema.closed || actual.schema.closed)
}

fn is_source_chain(node: &OperatorNode) -> bool {
    match node.non_asap() {
        Some(NonASAPOp::Scan { .. }) => true,
        Some(NonASAPOp::TimeRange { child, .. } | NonASAPOp::TimeShift { child, .. }) => {
            is_source_chain(child)
        }
        _ => false,
    }
}

fn same_source_node(left: &OperatorNode, right: &OperatorNode) -> bool {
    // These operators preserve exact source rows; recomposition's exact
    // guarantee annotation does not change their logical/execution contract.
    left.operator.map_children(|_| ()) == right.operator.map_children(|_| ())
        && left.schema == right.schema
        && left.result_kind == right.result_kind
        && left.timing == right.timing
        && left.materialization == right.materialization
        && left
            .children()
            .iter()
            .zip(right.children())
            .all(|(a, b)| Rc::ptr_eq(a, b))
        && left.children().len() == right.children().len()
}

fn intern_inputs(
    node: &Rc<OperatorNode>,
    memo: &mut CandidateMemo,
    queries: &[usize],
    done: &mut HashMap<*const OperatorNode, Rc<OperatorNode>>,
) -> Result<Rc<OperatorNode>, StateGroupExpansionError> {
    if let Some(known) = done.get(&Rc::as_ptr(node)) {
        return Ok(Rc::clone(known));
    }
    let mut children = HashMap::new();
    for child in node.children() {
        children.insert(
            Rc::as_ptr(child),
            intern_inputs(child, memo, queries, done)?,
        );
    }
    let rewritten = Rc::new(OperatorNode {
        operator: node
            .operator
            .map_children(|child| Rc::clone(&children[&Rc::as_ptr(child)])),
        ..(**node).clone()
    });
    let result = if is_source_chain(&rewritten) {
        if let Some(existing) = memo
            .groups
            .iter()
            .find(|group| same_source_node(&group.node, &rewritten))
        {
            Rc::clone(&existing.node)
        } else {
            let group = MemoGroupId(
                u32::try_from(memo.groups.len())
                    .map_err(|_| StateGroupExpansionError::TooManyGroups)?,
            );
            let mut seen = HashSet::new();
            let inputs: Vec<_> = rewritten
                .children()
                .iter()
                .map(|child| {
                    MemoGroupId(
                        memo.groups
                            .iter()
                            .position(|group| Rc::ptr_eq(child, &group.node))
                            .expect("source-chain children were interned")
                            as u32,
                    )
                })
                .filter(|input| seen.insert(*input))
                .collect();
            memo.groups.push(MemoGroup {
                node: Rc::clone(&rewritten),
                children: inputs.clone(),
                parents: Vec::new(),
                queries: queries.to_vec(),
                roots_of: Vec::new(),
            });
            memo.candidates.push(vec![ImplementationCandidate {
                implements: vec![group],
                node: Rc::clone(&rewritten),
                children: inputs,
                origin: CandidateOrigin::WidenedScans,
                coupling_sites: Vec::new(),
                budget_share: 1,
                requires_exact_children: Vec::new(),
                rollup_of: None,
                materializations: Vec::new(),
                requires_sites: Vec::new(),
            }]);
            rewritten
        }
    } else {
        rewritten
    };
    done.insert(Rc::as_ptr(node), Rc::clone(&result));
    Ok(result)
}

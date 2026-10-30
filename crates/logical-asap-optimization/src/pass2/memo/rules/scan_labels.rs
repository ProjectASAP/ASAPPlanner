//! Scan-label widening for explicit memo state sharing. Reuses the existing
//! Pass 2 widening rule; only its source-chain boundaries are interned here.
use crate::pass2::memo::check_generation_deadline;
use std::collections::{HashMap, HashSet};
use std::rc::Rc;
use std::time::Instant;

use asap_accuracy::propagation::recompose_guarantees;
use asap_types::ir::operator::Reduction;
use asap_types::ir::{ASAPOp, NonASAPOp, OperatorNode, QueryRoot};

use crate::pass2::memo::state_groups::{boundary_groups, simple_build, StateGroupExpansionError};
use asap_types::stages::{CandidateForMemoGroup, MemoCandidates, OriginOfCandidateForMemoGroup};
use asap_types::stages::{MemoGroup, MemoGroupId};

/// Preserve local implementations and append readers over union-label inputs.
/// Parent/query reachability is rebuilt by the enclosing state-group expansion.
pub(crate) fn add_scan_label_candidates(
    memo: &mut MemoCandidates,
    deadline: Option<Instant>,
) -> Result<usize, StateGroupExpansionError> {
    check_generation_deadline(deadline)?;
    let mut owners = Vec::new();
    let mut roots = Vec::new();
    for (group, candidates) in memo.candidates.iter().enumerate() {
        check_generation_deadline(deadline)?;
        for (index, candidate) in candidates.iter().enumerate() {
            check_generation_deadline(deadline)?;
            // Source-group candidates expose scan reuse to every downstream
            // operator, including exact/non-summary computations. Their public
            // schemas stay narrow through the existing projection rewrite.
            if is_source_chain(&candidate.root_node) && super::reuse::eligible(candidate) {
                owners.push((group, index));
                roots.push(QueryRoot::Operator(Rc::clone(&candidate.root_node)));
                continue;
            }
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
                .input_groups
                .iter()
                .any(|child| !children.contains(child))
                || children
                    .iter()
                    .any(|child| !is_source_chain(&memo.groups[child.0 as usize].logical_node))
            {
                continue;
            }
            owners.push((group, index));
            roots.push(QueryRoot::Operator(Rc::clone(&candidate.root_node)));
        }
    }
    let Some(widened) = crate::pass2::asap_aware_cse::with_union_scan_labels(&roots)
        .map_err(|error| StateGroupExpansionError::ScanLabelWidening(error.to_string()))?
    else {
        return Ok(0);
    };
    // Widening recomposes every root's guarantees, also those it left
    // unchanged, so a root is unchanged when it equals its recomposed
    // original. Compared with the stored original instead, an unchanged root
    // (a scan that already has every label) that differs only in its
    // guarantee annotation would pass for a widened reader, and interning
    // would map it back to its own group: a candidate reading the group it
    // implements.
    let unwidened = recompose_guarantees(&roots)
        .map_err(|error| StateGroupExpansionError::ScanLabelWidening(error.to_string()))?;
    let mut added = 0;
    let mut canonical = HashMap::new();
    for (((group, index), root), unwidened) in owners.into_iter().zip(widened).zip(unwidened) {
        check_generation_deadline(deadline)?;
        let (QueryRoot::Operator(node), QueryRoot::Operator(unwidened)) = (root, unwidened) else {
            unreachable!("operator readers")
        };
        let original = memo.candidates[group][index].clone();
        if *node == *unwidened || !same_output_contract(&original.root_node, &node) {
            continue;
        }
        let queries = memo.groups[group].queries.clone();
        let mut node = intern_inputs(&node, memo, &queries, &mut canonical)?;
        if memo
            .groups
            .iter()
            .any(|group| Rc::ptr_eq(&group.logical_node, &node))
        {
            // Interning an equivalent range/source can return another group's
            // canonical value. Keep a concrete reader instead of a root alias.
            node =
                OperatorNode::new_shared(asap_types::ir::Operator::NonASAP(NonASAPOp::Project {
                    cols: node
                        .schema
                        .fields
                        .iter()
                        .enumerate()
                        .map(|(column, field)| asap_types::ir::ProjectItem {
                            alias: Some(field.name.clone()),
                            expr: asap_types::ir::ScalarExpr::Column(column),
                        })
                        .collect(),
                    qualifier: None,
                    child: node,
                }))
                .map_err(|error| StateGroupExpansionError::ScanLabelWidening(error.to_string()))?;
            if !same_output_contract(&original.root_node, &node) {
                continue;
            }
        }
        let boundaries = memo
            .groups
            .iter()
            .enumerate()
            .map(|(id, group)| (Rc::as_ptr(&group.logical_node), MemoGroupId(id as u32)))
            .collect();
        let mut reader = original;
        reader.root_node = node;
        reader.input_groups = boundary_groups(&reader.root_node, &boundaries);
        reader.origin = OriginOfCandidateForMemoGroup::WidenedScans;
        if memo.candidates[group].iter().any(|known| {
            known.origin == OriginOfCandidateForMemoGroup::WidenedScans
                && known.input_groups == reader.input_groups
                && *known.root_node == *reader.root_node
        }) {
            continue;
        }
        memo.candidates[group].push(reader);
        added += 1;
    }
    check_generation_deadline(deadline)?;
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
    memo: &mut MemoCandidates,
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
            .find(|group| same_source_node(&group.logical_node, &rewritten))
        {
            Rc::clone(&existing.logical_node)
        } else {
            // The new group's id must fit a `MemoGroupId`.
            u32::try_from(memo.groups.len())
                .map_err(|_| StateGroupExpansionError::TooManyGroups)?;
            let mut seen = HashSet::new();
            let inputs: Vec<_> = rewritten
                .children()
                .iter()
                .map(|child| {
                    MemoGroupId(
                        memo.groups
                            .iter()
                            .position(|group| Rc::ptr_eq(child, &group.logical_node))
                            .expect("source-chain children were interned")
                            as u32,
                    )
                })
                .filter(|input| seen.insert(*input))
                .collect();
            memo.groups.push(MemoGroup {
                logical_node: Rc::clone(&rewritten),
                child_groups: inputs.clone(),
                parents: Vec::new(),
                queries: queries.to_vec(),
            });
            memo.candidates.push(vec![CandidateForMemoGroup {
                fused_groups: Vec::new(),
                root_node: Rc::clone(&rewritten),
                input_groups: inputs,
                origin: OriginOfCandidateForMemoGroup::WidenedScans,
                accuracy_budget_parts: 1,
            }]);
            rewritten
        }
    } else {
        rewritten
    };
    done.insert(Rc::as_ptr(node), Rc::clone(&result));
    Ok(result)
}

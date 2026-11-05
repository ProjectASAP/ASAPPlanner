//! Scan-label widening for explicit memo state sharing. Reuses the existing
//! Pass 2 widening rule; only its source-chain boundaries are interned here.
use std::rc::Rc;

use asap_accuracy::propagation::recompose_guarantees;
use asap_types::ir::operator::Reduction;
use asap_types::ir::{ASAPOp, NonASAPOp, OperatorNode, QueryRoot};

use super::reuse::{add_candidate_over_groups, same_output_contract};
use crate::added_groups::groups_read_through_added_groups;
use crate::operator_over_groups::without_exact_guarantees;
use crate::pass2::memo::state_groups::{build_down_to_stage0_groups, Pass2RuleError};
use asap_types::stages::{Memo, MemoGroupId, OriginOfCandidateForMemoGroup};

/// Preserve local implementations and append readers over union-label inputs.
/// Parent/query reachability is rebuilt by the enclosing state-group expansion.
pub(crate) fn add_scan_label_candidates(memo: &mut Memo) -> Result<usize, Pass2RuleError> {
    let mut owners = Vec::new();
    let mut roots = Vec::new();
    // Pass 1's intermediate groups (a summary build, its readout) are
    // widened inside their consumers' candidates, down to Stage 0's groups.
    for (group, candidates) in memo.candidates.iter().enumerate() {
        if !memo.is_stage0_group(MemoGroupId(group as u32)) {
            continue;
        }
        for (index, candidate) in candidates.iter().enumerate() {
            // Source-group candidates expose scan reuse to every downstream
            // operator, including exact/non-summary computations. Their public
            // schemas stay narrow through the existing projection rewrite.
            if is_source_chain(&candidate.root_node) && super::reuse::eligible(candidate) {
                owners.push((group, index));
                roots.push(QueryRoot::Operator(Rc::clone(&candidate.root_node)));
                continue;
            }
            if !super::reuse::eligible(candidate) {
                continue;
            }
            let Some((build, children)) = build_down_to_stage0_groups(memo, candidate) else {
                continue;
            };
            if !matches!(
                build.asap(),
                Some(
                    ASAPOp::SummaryAgg {
                        reduction: Reduction::PerEntity,
                        filter: None,
                        ..
                    } | ASAPOp::SummaryTumblingWindowChain {
                        reduction: Reduction::PerEntity,
                        filter: None,
                        ..
                    }
                )
            ) || groups_read_through_added_groups(memo, &candidate.input_groups)
                .stage0_groups
                .iter()
                .any(|child| !children.contains(child))
                || children
                    .iter()
                    .any(|child| !is_source_chain(&memo.groups()[child.0 as usize].logical_node))
            {
                continue;
            }
            owners.push((group, index));
            roots.push(QueryRoot::Operator(Rc::clone(&candidate.root_node)));
        }
    }
    let Some(widened) = crate::pass2::asap_aware_cse::with_union_scan_labels(&roots)
        .map_err(|error| Pass2RuleError::ScanLabelWidening(error.to_string()))?
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
        .map_err(|error| Pass2RuleError::ScanLabelWidening(error.to_string()))?;
    let mut added = 0;
    for (((group, index), root), unwidened) in owners.into_iter().zip(widened).zip(unwidened) {
        let (QueryRoot::Operator(node), QueryRoot::Operator(unwidened)) = (root, unwidened) else {
            unreachable!("operator readers")
        };
        let original = memo.candidates[group][index].clone();
        if *node == *unwidened {
            continue;
        }
        // A widened source chain equal to a group's node is read through a
        // projection instead of aliasing that group.
        let node = if memo
            .group_with_equal_node(&without_exact_guarantees(&node))
            .is_some()
        {
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
            .map_err(|error| Pass2RuleError::ScanLabelWidening(error.to_string()))?
        } else {
            node
        };
        // A reader whose output the widening changed is not added.
        if !same_output_contract(&original.root_node, &node) {
            continue;
        }
        added += usize::from(add_candidate_over_groups(
            memo,
            MemoGroupId(group as u32),
            &original,
            node,
            OriginOfCandidateForMemoGroup::WidenedScans,
        )?);
    }
    Ok(added)
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

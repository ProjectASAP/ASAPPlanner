//! Rule 2, scan widening: the scans of one metric with different label sets
//! are widened to their union (`asap_aware_cse::with_union_scan_labels`); the
//! widened source chain is its own group, and each scan group (and each
//! per-entity summary build over one) gets a candidate reading it.

use asap_accuracy::propagation::recompose_guarantees;
use asap_types::ir::operator::Reduction;
use asap_types::ir::{ASAPOp, NonASAPOp, OperatorNode, QueryRoot};

use super::reuse::same_output_contract;
use crate::operator_tree::add_operator_tree;
use crate::pass2::memo::well_formed::Pass2RuleError;
use asap_types::stages::{CandidateForMemoGroupId, Memo, MemoGroupId, OriginOfCandidateForMemoGroup};

/// Rule 2: adds to each source-chain group, and to each per-entity summary
/// build over a source chain, a candidate reading the source chain widened
/// to the union of its metric's label sets, through a `Project` that keeps
/// the group's output. Returns the number of candidates added.
pub(crate) fn add_scan_label_candidates(memo: &mut Memo) -> Result<usize, Pass2RuleError> {
    let mut owners = Vec::new();
    let mut roots = Vec::new();
    for (group, candidates) in memo.candidates.iter().enumerate() {
        for (index, candidate) in candidates.iter().enumerate() {
            if !candidate.fused_groups.is_empty() {
                continue;
            }
            let id = CandidateForMemoGroupId {
                group: MemoGroupId(group as u32),
                index,
            };
            let Ok(node) = memo.operator_node_of(id) else {
                continue;
            };
            let widens = is_source_chain(&node)
                || matches!(
                    node.asap(),
                    Some(
                        ASAPOp::SummaryAgg {
                            reduction: Reduction::PerEntity,
                            filter: None,
                            child,
                            ..
                        } | ASAPOp::SummaryTumblingWindowChain {
                            reduction: Reduction::PerEntity,
                            filter: None,
                            child,
                            ..
                        }
                    ) if is_source_chain(child)
                );
            if widens {
                owners.push(id);
                roots.push(QueryRoot::Operator(node));
            }
        }
    }
    let Some(widened) = crate::pass2::asap_aware_cse::with_union_scan_labels(&roots)
        .map_err(|error| Pass2RuleError::ScanLabelWidening(error.to_string()))?
    else {
        return Ok(0);
    };
    // Widening recomposes every root's guarantees, also those it left
    // unchanged, so a root is unchanged when it equals its recomposed
    // original.
    let unwidened = recompose_guarantees(&roots)
        .map_err(|error| Pass2RuleError::ScanLabelWidening(error.to_string()))?;
    let mut added = 0;
    for (((owner, root), unwidened), original) in owners.into_iter().zip(widened).zip(unwidened).zip(&roots) {
        let (QueryRoot::Operator(node), QueryRoot::Operator(unwidened), QueryRoot::Operator(original)) =
            (root, unwidened, original)
        else {
            unreachable!("operator consumers")
        };
        if *node == *unwidened {
            continue;
        }
        // A widened source chain is read through a projection, which keeps
        // the group's columns.
        let node = if is_source_chain(&node) {
            OperatorNode::new_shared(asap_types::ir::Operator::NonASAP(NonASAPOp::Project {
                cols: original
                    .schema
                    .fields
                    .iter()
                    .map(|field| asap_types::ir::ProjectItem {
                        alias: Some(field.name.clone()),
                        expr: asap_types::ir::ScalarExpr::Column(
                            node.schema
                                .fields
                                .iter()
                                .position(|widened| widened.name == field.name)
                                .unwrap_or_default(),
                        ),
                    })
                    .collect(),
                qualifier: None,
                child: node,
            }))
            .map_err(|error| Pass2RuleError::ScanLabelWidening(error.to_string()))?
        } else {
            node
        };
        // A consumer whose output the widening changed is not added.
        if !same_output_contract(original, &node) {
            continue;
        }
        let parts = memo.candidate(owner).accuracy_budget_parts;
        let candidate = add_operator_tree(
            memo,
            &node,
            &OriginOfCandidateForMemoGroup::WidenedScans,
            parts,
        );
        // A candidate never reads the group it computes.
        if candidate.expression.operator.children().contains(&&owner.group) {
            continue;
        }
        let known = memo.candidates[owner.group.0 as usize].len();
        added += usize::from(memo.add_candidate(owner.group, candidate).index == known);
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

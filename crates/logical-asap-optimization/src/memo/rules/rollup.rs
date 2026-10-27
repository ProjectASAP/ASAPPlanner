//! Producer-level exact and sketch rollups, using the existing capability rules.
use super::reuse::{
    append_producer, append_reader, combined_inputs, eligible, replace, restore_inputs,
};
use crate::asap_aware_cse::{rollup_candidate_for, rollup_pass2_candidates};
use crate::memo::candidates::{CandidateMemo, CandidateOrigin};
use crate::memo::check_generation_deadline;
use crate::memo::state_groups::{simple_build, StateGroupExpansionError};
use asap_types::ir::schema::FieldDataType;
use asap_types::ir::{
    ASAPOp, NonASAPOp, Operator, OperatorNode, ProjectItem, QueryRoot, ScalarExpr,
};
use std::collections::HashMap;
use std::rc::Rc;
use std::time::Instant;

pub(in crate::memo) fn add_rollup_candidates(
    memo: &mut CandidateMemo,
    deadline: Option<Instant>,
) -> Result<usize, StateGroupExpansionError> {
    check_generation_deadline(deadline)?;
    let mut candidates = Vec::new();
    for (group, entries) in memo.candidates.iter().enumerate() {
        check_generation_deadline(deadline)?;
        for candidate in entries {
            check_generation_deadline(deadline)?;
            if eligible(candidate)
                && matches!(
                    candidate.origin,
                    CandidateOrigin::Pass1 | CandidateOrigin::WidenedScans
                )
            {
                if let Some((build, _)) = simple_build(memo, candidate) {
                    candidates.push((group, candidate.clone(), build));
                }
            }
        }
    }
    let mut added = 0;
    for (coarse_group, coarse, coarse_build) in &candidates {
        check_generation_deadline(deadline)?;
        for (fine_group, fine, _) in &candidates {
            check_generation_deadline(deadline)?;
            if coarse_group == fine_group {
                continue;
            }
            let roots = [
                QueryRoot::Operator(Rc::clone(&coarse.node)),
                QueryRoot::Operator(Rc::clone(&fine.node)),
            ];
            let Some(rule) = rollup_candidate_for(&roots, Rc::as_ptr(coarse_build)) else {
                continue;
            };
            if !OperatorNode::reachable(&fine.node)
                .iter()
                .any(|node| Rc::ptr_eq(node, &rule.finer))
            {
                continue;
            }
            let state_rollup = OperatorNode::reachable(&rule.rollup).iter().any(|node| {
                matches!(
                    node.asap(),
                    Some(ASAPOp::SummaryMerge {
                        group_by: Some(_),
                        ..
                    })
                )
            });
            let is_sketch = matches!(
                rule.finer.asap(),
                Some(ASAPOp::SummaryAgg {
                    family: FieldDataType::Sketch(..),
                    ..
                })
            );
            let rewritten = rollup_pass2_candidates(&roots, &[rule])
                .map_err(|error| StateGroupExpansionError::ReuseRule(error.to_string()))?;
            let Some(result) = rewritten.into_iter().next() else {
                continue;
            };
            // Existing rule API marks individually noncomposable rewrites as
            // invalid candidates; retain the local implementation in that case.
            let Ok(rewritten) = result.roots else {
                continue;
            };
            let [QueryRoot::Operator(coarse_node), QueryRoot::Operator(fine_node)] =
                rewritten.as_slice()
            else {
                unreachable!("two operator roots")
            };
            let inputs = combined_inputs(coarse, fine);
            let coarse_node = restore_inputs(coarse_node, memo, &inputs);
            let fine_node = restore_inputs(fine_node, memo, &inputs);
            if is_sketch || state_rollup {
                // Concrete resized states become ordinary state groups after all
                // producer-level configurations have been expanded.
                added += usize::from(append_reader(
                    memo,
                    *coarse_group,
                    coarse,
                    coarse_node,
                    CandidateOrigin::Rollup,
                )?);
                added += usize::from(append_reader(
                    memo,
                    *fine_group,
                    fine,
                    fine_node,
                    CandidateOrigin::Rollup,
                )?);
            } else {
                let Some(value) =
                    OperatorNode::reachable(&coarse_node)
                        .into_iter()
                        .find_map(|node| match node.asap() {
                            Some(ASAPOp::SummaryAgg { child, .. })
                                if matches!(
                                    child.asap(),
                                    Some(ASAPOp::FinalizeExactAccumulator { .. })
                                ) =>
                            {
                                Some(Rc::clone(child))
                            }
                            _ => None,
                        })
                else {
                    continue;
                };
                let mut queries = memo.groups[*coarse_group].queries.clone();
                queries.extend(&memo.groups[*fine_group].queries);
                queries.sort_unstable();
                queries.dedup();
                let producer = append_producer(
                    memo,
                    Rc::clone(&value),
                    &queries,
                    CandidateOrigin::Rollup,
                    1,
                )?;
                let canonical = Rc::clone(&memo.groups[producer.0 as usize].node);
                let coarse_node = replace(
                    &coarse_node,
                    &HashMap::from([(Rc::as_ptr(&value), Rc::clone(&canonical))]),
                );
                added += usize::from(append_reader(
                    memo,
                    *coarse_group,
                    coarse,
                    coarse_node,
                    CandidateOrigin::Rollup,
                )?);
                if let Some(own_value) = OperatorNode::reachable(&fine_node)
                    .into_iter()
                    .find(|node| **node == *value)
                {
                    let fine_reader = if Rc::ptr_eq(&own_value, &fine_node) {
                        OperatorNode::new_shared(Operator::NonASAP(NonASAPOp::Project {
                            cols: canonical
                                .schema
                                .fields
                                .iter()
                                .enumerate()
                                .map(|(column, field)| ProjectItem {
                                    alias: Some(field.name.clone()),
                                    expr: ScalarExpr::Column(column),
                                })
                                .collect(),
                            qualifier: None,
                            child: Rc::clone(&canonical),
                        }))
                        .map_err(|error| StateGroupExpansionError::ReuseRule(error.to_string()))?
                    } else {
                        replace(
                            &fine_node,
                            &HashMap::from([(Rc::as_ptr(&own_value), Rc::clone(&canonical))]),
                        )
                    };
                    added += usize::from(append_reader(
                        memo,
                        *fine_group,
                        fine,
                        fine_reader,
                        CandidateOrigin::Rollup,
                    )?);
                }
            }
        }
    }
    check_generation_deadline(deadline)?;
    Ok(added)
}

/// Existing pane rollup rules applied to producer pairs on already generated
/// legal local grids. No whole-workload choice combinations are constructed.
pub(in crate::memo) fn add_window_rollup_candidates(
    memo: &mut CandidateMemo,
    deadline: Option<Instant>,
) -> Result<usize, StateGroupExpansionError> {
    check_generation_deadline(deadline)?;
    use crate::asap_aware_cse::{pane_candidates, window_composition_shared_chain_candidates};
    let mut grids = vec![std::collections::BTreeSet::new(); memo.groups.len()];
    for (group, candidates) in memo.candidates.iter().enumerate() {
        check_generation_deadline(deadline)?;
        for candidate in candidates
            .iter()
            .filter(|candidate| candidate.origin == CandidateOrigin::Panes && eligible(candidate))
        {
            if let Some((build, _)) = simple_build(memo, candidate) {
                if let Some(ASAPOp::PaneChain { pane_width_ms, .. }) = build.asap() {
                    grids[group].insert(*pane_width_ms);
                }
            }
        }
    }
    let candidates: Vec<_> = memo
        .candidates
        .iter()
        .enumerate()
        .flat_map(|(group, candidates)| {
            candidates
                .iter()
                .filter(|candidate| {
                    eligible(candidate) && candidate.origin == CandidateOrigin::Pass1
                })
                .map(move |candidate| (group, candidate.clone()))
        })
        .collect();
    let mut added = 0;
    for (coarse_group, coarse) in &candidates {
        check_generation_deadline(deadline)?;
        for (fine_group, fine) in &candidates {
            check_generation_deadline(deadline)?;
            if coarse_group == fine_group || grids[*fine_group].is_empty() {
                continue;
            }
            for grid in &grids[*coarse_group] {
                check_generation_deadline(deadline)?;
                let roots = [
                    QueryRoot::Operator(Rc::clone(&coarse.node)),
                    QueryRoot::Operator(Rc::clone(&fine.node)),
                ];
                let panes = pane_candidates(&roots, &[Some(*grid), Some(*grid)]);
                if panes.len() != 2 {
                    continue;
                }
                for rewritten in window_composition_shared_chain_candidates(&roots, &panes) {
                    check_generation_deadline(deadline)?;
                    if rewritten.shared_chains.is_empty() {
                        continue;
                    }
                    let Ok(nodes) = rewritten.roots else { continue };
                    let [QueryRoot::Operator(coarse_node), QueryRoot::Operator(fine_node)] =
                        nodes.as_slice()
                    else {
                        continue;
                    };
                    if !OperatorNode::reachable(coarse_node).iter().any(|node| {
                        matches!(
                            node.asap(),
                            Some(ASAPOp::SummaryMerge {
                                group_by: Some(_),
                                ..
                            })
                        )
                    }) {
                        continue;
                    }
                    let inputs = combined_inputs(coarse, fine);
                    let coarse_node = restore_inputs(coarse_node, memo, &inputs);
                    let fine_node = restore_inputs(fine_node, memo, &inputs);
                    added += usize::from(append_reader(
                        memo,
                        *coarse_group,
                        coarse,
                        coarse_node,
                        CandidateOrigin::Rollup,
                    )?);
                    added += usize::from(append_reader(
                        memo,
                        *fine_group,
                        fine,
                        fine_node,
                        CandidateOrigin::Rollup,
                    )?);
                }
            }
        }
    }
    check_generation_deadline(deadline)?;
    Ok(added)
}

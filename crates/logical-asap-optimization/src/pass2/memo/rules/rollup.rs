//! Producer-level exact and sketch rollups, using the existing capability rules.
use super::reuse::{
    append_producer, append_reader, combined_inputs, eligible, replace, restore_inputs,
};
use crate::pass2::asap_aware_cse::{rollup_candidate_for, rollup_pass2_candidates};
use crate::pass2::memo::candidates::{CandidateMemo, OriginOfCandidateForMemoGroup};
use crate::pass2::memo::check_generation_deadline;
use crate::pass2::memo::state_groups::{simple_build, StateGroupExpansionError};
use asap_types::ir::schema::FieldDataType;
use asap_types::ir::{
    ASAPOp, NonASAPOp, Operator, OperatorNode, ProjectItem, QueryRoot, ScalarExpr,
};
use std::collections::HashMap;
use std::rc::Rc;
use std::time::Instant;

pub(in crate::pass2::memo) fn add_rollup_candidates(
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
                    OriginOfCandidateForMemoGroup::Pass1
                        | OriginOfCandidateForMemoGroup::WidenedScans
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
                QueryRoot::Operator(Rc::clone(&coarse.root_node)),
                QueryRoot::Operator(Rc::clone(&fine.root_node)),
            ];
            let Some(rule) = rollup_candidate_for(&roots, Rc::as_ptr(coarse_build)) else {
                continue;
            };
            if !OperatorNode::reachable(&fine.root_node)
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
                    OriginOfCandidateForMemoGroup::Rollup,
                )?);
                added += usize::from(append_reader(
                    memo,
                    *fine_group,
                    fine,
                    fine_node,
                    OriginOfCandidateForMemoGroup::Rollup,
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
                // When the finer query's result is the value itself and every
                // implementation of it is exact, read the finer query's group:
                // one stored result then serves both queries, as the
                // whole-workload rewrite does, without a separate producer the
                // finer query would read through an identity projection.
                // Exactness is checked when this rule runs; a later
                // implementation of the group is still checked by plan
                // reconstruction (input contract) and Stage 3 (guarantee).
                let fine_result_is_value = *fine_node == *value
                    && memo.groups[*fine_group].logical_node.schema == value.schema
                    && memo.candidates[*fine_group].iter().all(|candidate| {
                        candidate
                            .root_node
                            .guarantee
                            .as_ref()
                            .is_some_and(|guarantee| guarantee.is_exact())
                    });
                if fine_result_is_value {
                    let fine_result = Rc::clone(&memo.groups[*fine_group].logical_node);
                    let coarse_node = replace(
                        &coarse_node,
                        &HashMap::from([(Rc::as_ptr(&value), fine_result)]),
                    );
                    added += usize::from(append_reader(
                        memo,
                        *coarse_group,
                        coarse,
                        coarse_node,
                        OriginOfCandidateForMemoGroup::Rollup,
                    )?);
                    continue;
                }
                let mut queries = memo.groups[*coarse_group].queries.clone();
                queries.extend(&memo.groups[*fine_group].queries);
                queries.sort_unstable();
                queries.dedup();
                let producer = append_producer(
                    memo,
                    Rc::clone(&value),
                    &queries,
                    OriginOfCandidateForMemoGroup::Rollup,
                    1,
                )?;
                let canonical = Rc::clone(&memo.groups[producer.0 as usize].logical_node);
                let coarse_node = replace(
                    &coarse_node,
                    &HashMap::from([(Rc::as_ptr(&value), Rc::clone(&canonical))]),
                );
                added += usize::from(append_reader(
                    memo,
                    *coarse_group,
                    coarse,
                    coarse_node,
                    OriginOfCandidateForMemoGroup::Rollup,
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
                        OriginOfCandidateForMemoGroup::Rollup,
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
pub(in crate::pass2::memo) fn add_window_rollup_candidates(
    memo: &mut CandidateMemo,
    deadline: Option<Instant>,
) -> Result<usize, StateGroupExpansionError> {
    check_generation_deadline(deadline)?;
    use crate::pass2::asap_aware_cse::{
        pane_candidates, window_composition_shared_chain_candidates,
    };
    let mut grids = vec![std::collections::BTreeSet::new(); memo.groups.len()];
    for (group, candidates) in memo.candidates.iter().enumerate() {
        check_generation_deadline(deadline)?;
        for candidate in candidates.iter().filter(|candidate| {
            candidate.origin == OriginOfCandidateForMemoGroup::Panes && eligible(candidate)
        }) {
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
                    eligible(candidate) && candidate.origin == OriginOfCandidateForMemoGroup::Pass1
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
                    QueryRoot::Operator(Rc::clone(&coarse.root_node)),
                    QueryRoot::Operator(Rc::clone(&fine.root_node)),
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
                        OriginOfCandidateForMemoGroup::Rollup,
                    )?);
                    added += usize::from(append_reader(
                        memo,
                        *fine_group,
                        fine,
                        fine_node,
                        OriginOfCandidateForMemoGroup::Rollup,
                    )?);
                }
            }
        }
    }
    check_generation_deadline(deadline)?;
    Ok(added)
}

#[cfg(test)]
mod tests {
    use crate::pass1::per_computation_candidates::generate_per_computation_candidates;
    use crate::pass2::memo::candidates::{try_populate_pass1, OriginOfCandidateForMemoGroup};
    use crate::pass2::memo::state_groups::expand_concrete_summary_state_groups;
    use crate::pass2::memo::DagOfLogicalMemoGroups;
    use crate::test_support::lower_promql;
    use asap_types::ir::properties::AccuracyTarget;
    use asap_types::ir::QueryRoot;

    // When the finer query's own result is the finalized value the coarse
    // query rolls up, the rollup reads the finer query's group, so one
    // stored result serves both queries without a copy or an identity
    // projection.
    #[test]
    fn a_rollup_reads_the_finer_query_result_itself() {
        let roots: Vec<_> = ["sum by (job) (m)", "sum by (job, instance) (m)"]
            .into_iter()
            .enumerate()
            .map(|(query, text)| {
                (
                    query,
                    QueryRoot::Operator(lower_promql(text, AccuracyTarget::exact())),
                )
            })
            .collect();
        let initial = DagOfLogicalMemoGroups::from_roots(&roots);
        let inventory = generate_per_computation_candidates(roots).unwrap();
        let memo =
            expand_concrete_summary_state_groups(try_populate_pass1(initial, &inventory).unwrap())
                .unwrap()
                .memo;
        let root_of = |query| {
            memo.groups
                .iter()
                .position(|group| group.roots_of == [query])
                .unwrap()
        };
        let (coarse, fine) = (root_of(0), root_of(1));
        assert!(
            memo.candidates[coarse].iter().any(|candidate| {
                candidate.origin == OriginOfCandidateForMemoGroup::Rollup
                    && candidate
                        .input_groups
                        .iter()
                        .any(|child| child.0 as usize == fine)
            }),
            "the coarse rollup reads the finer query's group"
        );
        assert!(
            !memo.candidates[fine]
                .iter()
                .any(|candidate| candidate.origin == OriginOfCandidateForMemoGroup::Rollup),
            "the finer query needs no reader of a separate producer"
        );
    }
}

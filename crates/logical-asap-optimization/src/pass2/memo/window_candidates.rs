//! Rule 1, local window composition: a summary state group whose build can
//! be split into tumbling windows gets, for every tumbling-window width the
//! evaluation grids of the queries reaching it allow, a candidate merging a
//! chain of those windows. The chain is its own group, one per state family
//! and input whatever the width and count, so chains of builds over one
//! input share it; a merge of a shorter window reads the newest windows of
//! a longer chain of the same width there.
use super::tumbling_window_widths::tumbling_window_width_domain;
use crate::operator_tree::{add_operator_tree, candidate_of};
use crate::pass2::asap_aware_cse::{tumbling_window_candidates, tumbling_window_chain};
use asap_types::ir::{ASAPOp, Operator, OperatorNode, QueryRoot};
use asap_types::stages::{
    CandidateForMemoGroupId, Memo, MemoGroupId, OriginOfCandidateForMemoGroup, WindowLayout,
};
use asap_types::workload::QueryWorkloadEntry;
use std::rc::Rc;

/// Rule 1: adds to every summary state group a candidate
/// `SummaryMerge(G_chain)` per legal tumbling-window width, `G_chain` the
/// group of the build's `SummaryTumblingWindowChain` at that width; then,
/// where the chain group holds a longer chain of the same width and
/// parameters, a candidate merging its newest windows. Stage 2 checks the
/// consumers' timing and phases.
pub fn add_local_window_composition(memo: &mut Memo, entries: &[QueryWorkloadEntry]) {
    let origin = OriginOfCandidateForMemoGroup::TumblingWindows;
    let mut merges = Vec::new();
    for g in 0..memo.groups.len() {
        let widths = tumbling_window_width_domain(
            memo.groups[g]
                .queries
                .iter()
                .filter_map(|&query| entries.get(query)?.evaluation_grid_ms()),
        );
        if widths.is_empty() {
            continue;
        }
        for (index, candidate) in memo.candidates[g].iter().enumerate() {
            if candidate.origin != OriginOfCandidateForMemoGroup::Pass1
                || !candidate.fused_groups.is_empty()
                || !matches!(
                    candidate.expression.operator,
                    Operator::ASAP(ASAPOp::SummaryAgg { .. })
                )
            {
                continue;
            }
            let id = CandidateForMemoGroupId {
                group: MemoGroupId(g as u32),
                index,
            };
            let Ok(build) = memo.operator_node_of(id) else {
                continue;
            };
            let roots = [QueryRoot::Operator(build)];
            for &width in &widths {
                for windows in tumbling_window_candidates(&roots, &[Some(width)]) {
                    let Ok(chain) = tumbling_window_chain(&windows) else {
                        continue;
                    };
                    let Ok(merge) = OperatorNode::new(Operator::ASAP(ASAPOp::SummaryMerge {
                        children: vec![chain],
                        newest_tumbling_windows: None,
                        group_by: None,
                    })) else {
                        continue;
                    };
                    merges.push((id, Rc::new(merge), candidate.accuracy_budget_parts));
                }
            }
        }
    }
    for (build, merge, parts) in merges {
        let candidate = add_operator_tree(memo, &merge, &origin, parts);
        memo.add_candidate(build.group, candidate);
    }
    add_newest_window_merges(memo);
}

/// For every merge of all `n` windows of a chain of width `w`, a merge of
/// the newest `n` windows when the chain group holds a chain of width `w`,
/// more than `n` windows and the merge's parameters.
fn add_newest_window_merges(memo: &mut Memo) {
    let origin = OriginOfCandidateForMemoGroup::TumblingWindows;
    let mut added = Vec::new();
    for g in 0..memo.groups.len() {
        for candidate in &memo.candidates[g] {
            let Operator::ASAP(ASAPOp::SummaryMerge {
                children,
                newest_tumbling_windows: None,
                group_by: None,
            }) = &candidate.expression.operator
            else {
                continue;
            };
            let (Some(WindowLayout::Tumbling { width_ms, count }), [chain_group]) =
                (candidate.logical_properties.window_layout(), &children[..])
            else {
                continue;
            };
            let longer = memo.candidates[chain_group.0 as usize]
                .iter()
                .enumerate()
                .find(|(_, chain)| {
                    matches!(
                        chain.logical_properties.window_layout(),
                        Some(WindowLayout::Tumbling { width_ms: w, count: c })
                            if w == width_ms && c > count
                    ) && chain.logical_properties.representation()
                        == candidate.logical_properties.representation()
                });
            if let Some((index, _)) = longer {
                added.push((
                    MemoGroupId(g as u32),
                    CandidateForMemoGroupId {
                        group: *chain_group,
                        index,
                    },
                    *count,
                    candidate.accuracy_budget_parts,
                ));
            }
        }
    }
    for (group, chain, newest, parts) in added {
        let Ok(chain_node) = memo.operator_node_of(chain) else {
            continue;
        };
        let expression = Operator::ASAP(ASAPOp::SummaryMerge {
            children: vec![chain.group],
            newest_tumbling_windows: Some(newest),
            group_by: None,
        });
        let Ok(merge) = OperatorNode::new(Operator::ASAP(ASAPOp::SummaryMerge {
            children: vec![chain_node],
            newest_tumbling_windows: Some(newest),
            group_by: None,
        })) else {
            continue;
        };
        memo.add_candidate(group, candidate_of(&merge, expression, &origin, parts));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pass1::add_pass1_candidates::try_add_pass1_candidates;
    use crate::pass1::per_computation_candidates::generate_per_computation_candidates;
    use crate::stage0::generate_dag_of_logical_memo_groups;
    use crate::test_support::{entry_with, lower_promql};
    use asap_types::ir::properties::AccuracyTarget;
    use asap_types::workload::{QueryRecurrence, RepeatedDemand, RepetitionInterval};

    fn repeated(interval: u32) -> QueryRecurrence {
        QueryRecurrence::Repeated(RepeatedDemand::FixedInterval(RepetitionInterval(interval)))
    }

    fn memo_after_rule_1(recurrences: &[QueryRecurrence]) -> Memo {
        let target = AccuracyTarget::max_error(0.05).failure_probability(0.1);
        let root = lower_promql("quantile_over_time(0.95, latency[15m])", target.clone());
        let roots: Vec<_> = recurrences
            .iter()
            .map(|_| QueryRoot::Operator(Rc::clone(&root)))
            .collect();
        let initial = generate_dag_of_logical_memo_groups(&roots);
        let generated =
            generate_per_computation_candidates(&generate_dag_of_logical_memo_groups(&roots))
                .unwrap();
        let mut memo =
            try_add_pass1_candidates(initial, &generated, &[], &Default::default()).unwrap();
        let entries: Vec<_> = recurrences
            .iter()
            .map(|recurrence| entry_with(target.clone(), recurrence.clone()))
            .collect();
        add_local_window_composition(&mut memo, &entries);
        memo
    }

    fn merges(memo: &Memo) -> usize {
        memo.candidates
            .iter()
            .flatten()
            .filter(|candidate| candidate.origin == OriginOfCandidateForMemoGroup::TumblingWindows)
            .count()
    }

    // A query that does not repeat at a fixed interval cannot take away
    // another query's tumbling windows.
    #[test]
    fn a_nongridded_query_keeps_the_tumbling_windows_of_a_gridded_one() {
        assert!(merges(&memo_after_rule_1(&[repeated(60_000), QueryRecurrence::Unknown])) > 0);
        assert_eq!(merges(&memo_after_rule_1(&[QueryRecurrence::Unknown])), 0);
    }

    // The merge is a candidate of the state group, and every chain of the
    // build, whatever its width, is a candidate of one chain group.
    #[test]
    fn every_width_is_a_candidate_of_the_state_group_and_one_chain_group() {
        let memo = memo_after_rule_1(&[repeated(120_000)]);
        let state_groups: std::collections::BTreeSet<_> = memo
            .candidates
            .iter()
            .enumerate()
            .filter(|(_, candidates)| {
                candidates
                    .iter()
                    .any(|c| c.origin == OriginOfCandidateForMemoGroup::TumblingWindows)
            })
            .map(|(g, _)| g)
            .collect();
        assert_eq!(state_groups.len(), 1);
        let state = *state_groups.iter().next().unwrap();
        assert!(memo.candidates[state].iter().any(|c| matches!(
            c.expression.operator,
            Operator::ASAP(ASAPOp::SummaryAgg { .. })
        )));
        let chain_groups: std::collections::BTreeSet<_> = memo.candidates[state]
            .iter()
            .filter(|c| c.origin == OriginOfCandidateForMemoGroup::TumblingWindows)
            .flat_map(|c| c.expression.operator.children())
            .copied()
            .collect();
        assert_eq!(chain_groups.len(), 1);
        let chain = chain_groups.into_iter().next().unwrap();
        assert!(memo.candidates[chain.0 as usize].len() > 1);
    }
}

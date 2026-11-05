//! Rules 6 and 7, rollups: a coarse group answered from a finer group whose
//! state rolls up (`asap_aware_cse::rollup_candidate_for`), and a coarse
//! state group merging the tumbling windows of a finer build's chain
//! (`asap_aware_cse::window_composition_shared_chain_candidates`).
use crate::operator_tree::add_operator_tree;
use crate::pass2::asap_aware_cse::{
    rollup_candidate_for, tumbling_window_candidates, window_composition_shared_chain_candidates,
};
use crate::pass2::memo::well_formed::Pass2RuleError;
use asap_types::ir::schema::FieldDataType;
use asap_types::ir::{ASAPOp, Operator, OperatorNode, QueryRoot};
use asap_types::stages::{
    CandidateForMemoGroupId, Memo, MemoGroupId, OriginOfCandidateForMemoGroup, WindowLayout,
};
use std::collections::BTreeSet;
use std::rc::Rc;

/// The candidates of `memo` whose expression `matches`, made by Pass 1 or
/// scan widening and not fused, with their operator nodes.
fn candidates_where(
    memo: &Memo,
    matches: impl Fn(&Operator<MemoGroupId>) -> bool,
) -> Vec<(CandidateForMemoGroupId, Rc<OperatorNode>, usize)> {
    let mut found = Vec::new();
    for (group, candidates) in memo.candidates.iter().enumerate() {
        for (index, candidate) in candidates.iter().enumerate() {
            if !matches!(
                candidate.origin,
                OriginOfCandidateForMemoGroup::Pass1 | OriginOfCandidateForMemoGroup::WidenedScans
            ) || !candidate.fused_groups.is_empty()
                || !matches(&candidate.expression.operator)
            {
                continue;
            }
            let id = CandidateForMemoGroupId {
                group: MemoGroupId(group as u32),
                index,
            };
            if let Ok(node) = memo.operator_node_of(id) {
                found.push((id, node, candidate.accuracy_budget_parts));
            }
        }
    }
    found
}

/// Adds `candidate` (from `node`) to `owner` unless it reads `owner`; 1 when
/// it was new.
fn add_to(memo: &mut Memo, owner: MemoGroupId, node: &Rc<OperatorNode>, parts: usize) -> usize {
    let candidate = add_operator_tree(memo, node, &OriginOfCandidateForMemoGroup::Rollup, parts);
    if candidate.expression.operator.children().contains(&&owner) {
        return 0;
    }
    let known = memo.candidates[owner.0 as usize].len();
    usize::from(memo.add_candidate(owner, candidate).index == known)
}

/// Rule 6: for every pair of consumers of summary state (a
/// `FinalizeExactAccumulator` or a `SummaryEstimate`) whose coarse state
/// rolls up from the finer one, a candidate of the coarse group: for a
/// sketch, of the coarse state group, merging the finer states by the
/// coarse keys; for an exact accumulator, of the coarse value's group,
/// accumulating the finer values. Returns the number of candidates added.
pub(crate) fn add_rollup_candidates(memo: &mut Memo) -> Result<usize, Pass2RuleError> {
    let consumers = candidates_where(memo, |operator| {
        matches!(
            operator,
            Operator::ASAP(
                ASAPOp::FinalizeExactAccumulator { .. } | ASAPOp::SummaryEstimate { .. }
            )
        )
    });
    let mut rollups = Vec::new();
    for (coarse, coarse_node, parts) in &consumers {
        let [build] = coarse_node.children()[..] else {
            continue;
        };
        let Some(build_group) = memo.candidate(*coarse).expression.operator.children().first().map(|g| **g)
        else {
            continue;
        };
        for (fine, fine_node, _) in &consumers {
            if fine.group == coarse.group {
                continue;
            }
            let roots = [
                QueryRoot::Operator(Rc::clone(coarse_node)),
                QueryRoot::Operator(Rc::clone(fine_node)),
            ];
            let Some(rule) = rollup_candidate_for(&roots, Rc::as_ptr(build)) else {
                continue;
            };
            if !OperatorNode::reachable(fine_node)
                .iter()
                .any(|node| Rc::ptr_eq(node, &rule.finer))
            {
                continue;
            }
            let owner = match build.asap() {
                Some(ASAPOp::SummaryAgg {
                    family: FieldDataType::Sketch(..),
                    ..
                }) => build_group,
                _ => coarse.group,
            };
            rollups.push((owner, rule.rollup, *parts));
        }
    }
    Ok(rollups
        .into_iter()
        .map(|(owner, node, parts)| add_to(memo, owner, &node, parts))
        .sum())
}

/// Rule 7: for every pair of summary state groups whose builds both have
/// tumbling windows of one width, a candidate of the coarse state group
/// merging, by its keys, the newest windows of a chain the finer build's
/// windows give. Returns the number of candidates added.
pub(crate) fn add_window_rollup_candidates(memo: &mut Memo) -> Result<usize, Pass2RuleError> {
    let widths: Vec<BTreeSet<u64>> = memo
        .candidates
        .iter()
        .map(|candidates| {
            candidates
                .iter()
                .filter(|c| c.origin == OriginOfCandidateForMemoGroup::TumblingWindows)
                .filter_map(|c| match c.logical_properties.window_layout() {
                    Some(WindowLayout::Tumbling { width_ms, .. }) => Some(*width_ms),
                    None => None,
                })
                .collect()
        })
        .collect();
    let builds = candidates_where(memo, |operator| {
        matches!(operator, Operator::ASAP(ASAPOp::SummaryAgg { .. }))
    });
    let mut rollups = Vec::new();
    for (coarse, coarse_node, parts) in &builds {
        for (fine, fine_node, _) in &builds {
            if coarse.group == fine.group {
                continue;
            }
            for width in widths[coarse.group.0 as usize].intersection(&widths[fine.group.0 as usize]) {
                let roots = [
                    QueryRoot::Operator(Rc::clone(coarse_node)),
                    QueryRoot::Operator(Rc::clone(fine_node)),
                ];
                let windows = tumbling_window_candidates(&roots, &[Some(*width), Some(*width)]);
                if windows.len() != 2 {
                    continue;
                }
                for rewritten in window_composition_shared_chain_candidates(&roots, &windows) {
                    if rewritten.shared_chains.is_empty() {
                        continue;
                    }
                    let Ok(nodes) = rewritten.roots else { continue };
                    let Some(QueryRoot::Operator(coarse_read)) = nodes.into_iter().next() else {
                        continue;
                    };
                    if !matches!(
                        coarse_read.asap(),
                        Some(ASAPOp::SummaryMerge {
                            group_by: Some(_),
                            ..
                        })
                    ) {
                        continue;
                    }
                    rollups.push((coarse.group, coarse_read, *parts));
                }
            }
        }
    }
    Ok(rollups
        .into_iter()
        .map(|(owner, node, parts)| add_to(memo, owner, &node, parts))
        .sum())
}

#[cfg(test)]
mod tests {
    use crate::pass1::add_pass1_candidates::try_add_pass1_candidates;
    use crate::pass1::per_computation_candidates::generate_per_computation_candidates;
    use crate::stage0::generate_dag_of_logical_memo_groups;
    use crate::test_support::{lower_promql, sharing_rules};
    use asap_types::ir::properties::AccuracyTarget;
    use asap_types::ir::OperatorNode;
    use asap_types::ir::QueryRoot;
    use asap_types::stages::OriginOfCandidateForMemoGroup;
    use std::rc::Rc;

    // When the finer query's own result is the finalized value the coarse
    // query rolls up, the rollup reads the finer query's group, so one
    // stored result serves both queries without a copy or an identity
    // projection.
    #[test]
    fn a_rollup_reads_the_finer_query_result_itself() {
        let roots: Vec<_> = ["sum by (job) (m)", "sum by (job, instance) (m)"]
            .into_iter()
            .map(|text| QueryRoot::Operator(lower_promql(text, AccuracyTarget::exact())))
            .collect();
        let initial = generate_dag_of_logical_memo_groups(&roots);
        let inventory =
            generate_per_computation_candidates(&generate_dag_of_logical_memo_groups(&roots))
                .unwrap();
        let memo = sharing_rules(
            try_add_pass1_candidates(initial, &inventory, &[], &Default::default()).unwrap(),
        )
        .unwrap();
        let root_of = |query: usize| memo.query_roots[query].as_operator().unwrap().0 as usize;
        let (coarse, fine) = (root_of(0), root_of(1));
        assert!(
            memo.candidates[coarse].iter().any(|candidate| {
                candidate.origin == OriginOfCandidateForMemoGroup::Rollup
                    && OperatorNode::reachable(&candidate.root_node)
                        .iter()
                        .any(|node| Rc::ptr_eq(node, &memo.groups()[fine].logical_node))
            }),
            "the coarse rollup reads the finer query's group, through its own groups"
        );
        assert!(
            !memo.candidates[fine]
                .iter()
                .any(|candidate| candidate.origin == OriginOfCandidateForMemoGroup::Rollup),
            "the finer query needs no reader of a separate producer"
        );
    }
}

//! Local tumbling-window candidates generated from compatible reader grids. Selected
//! connections, not the union of potential consumers, determine eligibility.
#[cfg(test)]
use super::shared_configurations::tumbling_window_width_divisors;
use super::shared_configurations::tumbling_window_width_domain;
use crate::operator_over_groups::operator_over_groups;
use crate::pass2::asap_aware_cse::{
    tumbling_window_candidates, window_composition_pass2_candidates,
};
use asap_types::ir::{OperatorNode, QueryRoot};
use asap_types::stages::MemoGroupId as GroupId;
use asap_types::stages::{CandidateForMemoGroup, Memo, OriginOfCandidateForMemoGroup};
use asap_types::workload::QueryWorkloadEntry;
#[cfg(test)]
use std::collections::BTreeSet;
use std::rc::Rc;

/// Offer each supported reader-grid configuration, for every legal
/// tumbling-window width. Stage 2 checks the actual chosen consumers,
/// including ingestion alignment and range evaluation steps. Each candidate
/// is one operator over groups ([`operator_over_groups`]): the tumbling-window
/// chain, its merge and its readout are groups, which every reader of the
/// same state at the same width shares.
pub fn add_local_window_composition(memo: &mut Memo, entries: &[QueryWorkloadEntry]) {
    for (group, from, node) in local_tumbling_window_candidates(memo, entries) {
        let queries = memo.groups()[group].queries.clone();
        let origin = OriginOfCandidateForMemoGroup::TumblingWindows;
        let (root_node, input_groups) =
            operator_over_groups(memo, &node, &queries, &origin, from.accuracy_budget_parts);
        // A candidate never reads a group it implements.
        if std::iter::once(GroupId(group as u32))
            .chain(from.fused_groups.iter().copied())
            .any(|implemented| input_groups.contains(&implemented))
        {
            continue;
        }
        memo.add_candidate(
            GroupId(group as u32),
            CandidateForMemoGroup {
                fused_groups: from.fused_groups,
                root_node,
                input_groups,
                origin,
                accuracy_budget_parts: from.accuracy_budget_parts,
            },
        );
    }
}

/// Expand legal tumbling-window widths while preserving canonical memo inputs.
fn local_tumbling_window_candidates(
    memo: &Memo,
    entries: &[QueryWorkloadEntry],
) -> Vec<(usize, CandidateForMemoGroup, Rc<OperatorNode>)> {
    let mut own: Vec<(usize, CandidateForMemoGroup, Rc<OperatorNode>)> = Vec::new();
    for (g, group) in memo.groups().iter().enumerate() {
        // Pass 1's intermediate groups (a summary build, its readout) are
        // rewritten inside their consumers' candidates instead.
        if !memo.is_stage0_group(GroupId(g as u32)) {
            continue;
        }
        let grids = tumbling_window_width_domain(
            group
                .queries
                .iter()
                .filter_map(|&query| entries.get(query)?.evaluation_grid_ms()),
        );
        for grid in grids {
            for candidate in &memo.candidates[g] {
                if !matches!(
                    candidate.origin,
                    OriginOfCandidateForMemoGroup::Pass1
                        | OriginOfCandidateForMemoGroup::SharedBuild
                ) {
                    continue;
                }
                let roots = [QueryRoot::Operator(Rc::clone(&candidate.root_node))];
                let tumbling_windows = tumbling_window_candidates(&roots, &[Some(grid)]);
                if tumbling_windows.is_empty() {
                    continue;
                }
                let Ok(pass2_candidates) =
                    window_composition_pass2_candidates(&roots, &tumbling_windows)
                else {
                    continue;
                };
                for pass2_candidate in pass2_candidates {
                    let Ok(rewritten) = pass2_candidate.roots else {
                        continue;
                    };
                    let [QueryRoot::Operator(node)] = &rewritten[..] else {
                        continue;
                    };
                    own.push((g, candidate.clone(), Rc::clone(node)));
                }
            }
        }
    }
    own
}

// Union of complete divisor domains covers individual readers and every
// compatible subset: common widths belong to each reader's divisor domain.
// Nongridded potential consumers do not veto a repeated reader's tumbling windows.
#[cfg(test)]
fn compatible_reader_grids(
    group: &asap_types::stages::MemoGroup,
    entries: &[QueryWorkloadEntry],
) -> Vec<u64> {
    super::shared_configurations::tumbling_window_width_domain(
        group
            .queries
            .iter()
            .filter_map(|&query| entries.get(query)?.evaluation_grid_ms()),
    )
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

    fn fixture(recurrences: &[QueryRecurrence]) -> (Memo, Vec<QueryWorkloadEntry>) {
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
        let memo = try_add_pass1_candidates(initial, &generated, &[], &Default::default()).unwrap();
        let entries = recurrences
            .iter()
            .map(|recurrence| entry_with(target.clone(), recurrence.clone()))
            .collect();
        (memo, entries)
    }

    // A nongridded potential reader cannot eliminate another reader's tumbling windows.
    #[test]
    fn nongridded_potential_reader_preserves_local_tumbling_window_candidates() {
        let (memo, entries) = fixture(&[repeated(60_000), QueryRecurrence::Unknown]);
        let candidates = local_tumbling_window_candidates(&memo, &entries);
        assert!(!candidates.is_empty());
        assert!(candidates.iter().all(|(_, from, _)| matches!(
            from.origin,
            OriginOfCandidateForMemoGroup::Pass1 | OriginOfCandidateForMemoGroup::SharedBuild
        )));
    }

    // Individual grids and subset GCDs allow separate or shared physical choices.
    #[test]
    fn reader_grids_include_subsets_instead_of_only_full_union() {
        let (memo, entries) = fixture(&[
            repeated(120_000),
            repeated(180_000),
            QueryRecurrence::Unknown,
        ]);
        let group = &memo.groups()[memo.query_roots[0].as_operator().unwrap().0 as usize];
        assert_eq!(
            compatible_reader_grids(group, &entries),
            tumbling_window_width_divisors(120_000)
                .into_iter()
                .chain(tumbling_window_width_divisors(180_000))
                .collect::<BTreeSet<_>>()
                .into_iter()
                .collect::<Vec<_>>()
        );
    }
}

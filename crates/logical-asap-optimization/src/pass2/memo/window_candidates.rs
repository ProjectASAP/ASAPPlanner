//! Local tumbling-window candidates generated from compatible reader grids. Selected
//! connections, not the union of potential consumers, determine eligibility.
#[cfg(test)]
use super::shared_configurations::tumbling_window_width_divisors;
use super::shared_configurations::tumbling_window_width_domain;
use crate::pass2::asap_aware_cse::{
    tumbling_window_candidates, window_composition_pass2_candidates,
};
use asap_types::ir::flat::flatten;
use asap_types::ir::{OperatorNode, QueryRoot};
use asap_types::stages::{CandidateForMemoGroup, Memo, OriginOfCandidateForMemoGroup};
use asap_types::stages::{MemoGroup, MemoGroupId as GroupId};
use asap_types::workload::QueryWorkloadEntry;
#[cfg(test)]
use std::collections::BTreeSet;
use std::collections::{HashMap, HashSet};
use std::rc::Rc;

/// Offer each supported reader-grid configuration, for every legal
/// tumbling-window width. Stage 2 checks the actual chosen consumers,
/// including ingestion alignment and range evaluation steps.
pub fn add_local_window_composition(memo: &mut Memo, entries: &[QueryWorkloadEntry]) {
    // Flatten each existing/generated graph once. Repeatedly flattening the
    // growing candidate list makes a complete tumbling-window-width domain quadratic.
    let mut known: HashMap<(usize, String), Vec<usize>> = HashMap::new();
    for (group, candidates) in memo.candidates.iter().enumerate() {
        for (index, candidate) in candidates.iter().enumerate() {
            if let Ok(signature) = candidate_signature(candidate) {
                known.entry((group, signature)).or_default().push(index);
            }
        }
    }
    for (group, candidate) in local_tumbling_window_candidates(memo, entries) {
        let Ok(signature) = candidate_signature(&candidate) else {
            memo.candidates[group].push(candidate);
            continue;
        };
        let bucket = known.entry((group, signature)).or_default();
        if !bucket.iter().any(|&index| {
            let previous = &memo.candidates[group][index];
            previous.origin == candidate.origin
                && previous.input_groups == candidate.input_groups
                && previous.fused_groups == candidate.fused_groups
                && previous.accuracy_budget_parts == candidate.accuracy_budget_parts
                && previous.root_node == candidate.root_node
        }) {
            bucket.push(memo.candidates[group].len());
            memo.candidates[group].push(candidate);
        }
    }
}

/// Expand legal tumbling-window widths while preserving canonical memo inputs.
fn local_tumbling_window_candidates(
    memo: &Memo,
    entries: &[QueryWorkloadEntry],
) -> Vec<(usize, CandidateForMemoGroup)> {
    let group_of = group_of_node(&memo.groups);
    let mut own: Vec<(usize, CandidateForMemoGroup)> = Vec::new();
    for (g, group) in memo.groups.iter().enumerate() {
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
                let canonical_scans = OperatorNode::reachable(&candidate.root_node)
                    .into_iter()
                    .filter(|node| {
                        matches!(
                            node.non_asap(),
                            Some(asap_types::ir::NonASAPOp::Scan { .. })
                        ) && group_of.contains_key(&Rc::as_ptr(node))
                    })
                    .collect::<Vec<_>>();
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
                    // Guarantee recomposition can clone unchanged source scans.
                    // Restore only an unambiguous original scan with the same full
                    // source/schema/execution contract; scans have intrinsic exact
                    // semantics. Otherwise the tumbling window's canonical input disappears.
                    let mut restored = HashMap::new();
                    for rewritten in OperatorNode::reachable(node) {
                        let mut matches = canonical_scans.iter().filter(|original| {
                            original.operator == rewritten.operator
                                && original.schema == rewritten.schema
                                && original.result_kind == rewritten.result_kind
                        });
                        if let Some(original) = matches.next() {
                            if matches.next().is_none() {
                                restored.insert(Rc::as_ptr(&rewritten), original);
                            }
                        }
                    }
                    let node = restore_scan_boundaries(node, &restored, &mut HashMap::new());
                    own.push((
                        g,
                        rewritten_candidate(
                            &group_of,
                            GroupId(g as u32),
                            candidate,
                            node,
                            OriginOfCandidateForMemoGroup::TumblingWindows,
                        ),
                    ));
                }
            }
        }
    }
    own
}

fn restore_scan_boundaries(
    node: &Rc<OperatorNode>,
    restored: &HashMap<*const OperatorNode, &Rc<OperatorNode>>,
    done: &mut HashMap<*const OperatorNode, Rc<OperatorNode>>,
) -> Rc<OperatorNode> {
    if let Some(scan) = restored.get(&Rc::as_ptr(node)) {
        return Rc::clone(scan);
    }
    if let Some(previous) = done.get(&Rc::as_ptr(node)) {
        return Rc::clone(previous);
    }
    let mut changed = false;
    let operator = node.operator.map_children(|child| {
        let rebuilt = restore_scan_boundaries(child, restored, done);
        changed |= !Rc::ptr_eq(child, &rebuilt);
        rebuilt
    });
    let rebuilt = if changed {
        Rc::new(
            OperatorNode::with_schema(operator, node.schema.clone())
                .with_guarantee(node.guarantee.clone()),
        )
    } else {
        Rc::clone(node)
    };
    done.insert(Rc::as_ptr(node), Rc::clone(&rebuilt));
    rebuilt
}

// Union of complete divisor domains covers individual readers and every
// compatible subset: common widths belong to each reader's divisor domain.
// Nongridded potential consumers do not veto a repeated reader's tumbling windows.
#[cfg(test)]
fn compatible_reader_grids(group: &MemoGroup, entries: &[QueryWorkloadEntry]) -> Vec<u64> {
    super::shared_configurations::tumbling_window_width_domain(
        group
            .queries
            .iter()
            .filter_map(|&query| entries.get(query)?.evaluation_grid_ms()),
    )
}

fn group_of_node(groups: &[MemoGroup]) -> HashMap<*const OperatorNode, usize> {
    groups
        .iter()
        .enumerate()
        .map(|(g, group)| (Rc::as_ptr(&group.logical_node), g))
        .collect()
}

/// `from`, a candidate of group `owner`, with root `node` and `origin`.
fn rewritten_candidate(
    group_of: &HashMap<*const OperatorNode, usize>,
    owner: GroupId,
    from: &CandidateForMemoGroup,
    node: Rc<OperatorNode>,
    origin: OriginOfCandidateForMemoGroup,
) -> CandidateForMemoGroup {
    let implements: Vec<GroupId> = std::iter::once(owner)
        .chain(from.fused_groups.iter().copied())
        .collect();
    let reached = |node: &Rc<OperatorNode>| child_groups(group_of, node, &implements);
    let children = if reached(&from.root_node) == from.input_groups {
        reached(&node)
    } else {
        from.input_groups.clone()
    };
    CandidateForMemoGroup {
        fused_groups: from.fused_groups.clone(),
        root_node: node,
        input_groups: children,
        origin,
        accuracy_budget_parts: from.accuracy_budget_parts,
    }
}

fn child_groups(
    group_of: &HashMap<*const OperatorNode, usize>,
    node: &Rc<OperatorNode>,
    implements: &[GroupId],
) -> Vec<GroupId> {
    fn walk(
        node: &Rc<OperatorNode>,
        group_of: &HashMap<*const OperatorNode, usize>,
        implements: &[GroupId],
        seen: &mut HashSet<*const OperatorNode>,
        out: &mut Vec<GroupId>,
    ) {
        for child in node.children() {
            if !seen.insert(Rc::as_ptr(child)) {
                continue;
            }
            match group_of.get(&Rc::as_ptr(child)) {
                Some(&g) if !implements.contains(&GroupId(g as u32)) => {
                    if !out.contains(&GroupId(g as u32)) {
                        out.push(GroupId(g as u32));
                    }
                }
                _ => walk(child, group_of, implements, seen, out),
            }
        }
    }
    let mut out = Vec::new();
    walk(node, group_of, implements, &mut HashSet::new(), &mut out);
    out
}

fn candidate_signature(candidate: &CandidateForMemoGroup) -> Result<String, serde_json::Error> {
    serde_json::to_string(&flatten(&[QueryRoot::Operator(Rc::clone(&candidate.root_node))]).0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pass1::memo_population::try_populate_pass1;
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
        let memo = try_populate_pass1(initial, &generated).unwrap();
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
        assert!(candidates.iter().all(
            |(_, candidate)| candidate.origin == OriginOfCandidateForMemoGroup::TumblingWindows
        ));
    }

    // Individual grids and subset GCDs allow separate or shared physical choices.
    #[test]
    fn reader_grids_include_subsets_instead_of_only_full_union() {
        let (memo, entries) = fixture(&[
            repeated(120_000),
            repeated(180_000),
            QueryRecurrence::Unknown,
        ]);
        let group = &memo.groups[memo.query_roots[0].as_operator().unwrap().0 as usize];
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

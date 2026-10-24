//! Local pane candidates generated from compatible reader grids. Selected
//! connections, not the union of potential consumers, determine eligibility.
use super::candidates::{CandidateMemo as Memo, CandidateOrigin, ImplementationCandidate};
use super::{MemoGroup, MemoGroupId as GroupId};
use crate::asap_aware_cse::{pane_candidates, window_composition_pass2_candidates};
use asap_types::ir::flat::flatten;
use asap_types::ir::{OperatorNode, QueryRoot};
use asap_types::workload::{greatest_common_divisor, QueryWorkloadEntry};
use std::collections::{BTreeSet, HashMap, HashSet};
use std::rc::Rc;

/// Offer each supported reader-grid configuration. Stage 2 checks the actual
/// chosen consumers, including ingestion alignment and range evaluation steps.
pub fn add_local_window_composition(memo: &mut Memo, entries: &[QueryWorkloadEntry]) {
    for (group, candidate) in local_pane_candidates(memo, entries, true) {
        push_new(memo, group, candidate);
    }
}

/// Compatibility hook for the retiring selector: false retains its all-reader
/// grid and private scan boundaries. The connection planner uses true.
pub fn local_pane_candidates(
    memo: &Memo,
    entries: &[QueryWorkloadEntry],
    preserve_group_boundaries: bool,
) -> Vec<(usize, ImplementationCandidate)> {
    let group_of = group_of_node(&memo.groups);
    let mut own: Vec<(usize, ImplementationCandidate)> = Vec::new();
    for (g, group) in memo.groups.iter().enumerate() {
        let grids = if preserve_group_boundaries {
            compatible_reader_grids(group, entries)
        } else {
            group_grid_ms(group, entries).into_iter().collect()
        };
        for grid in grids {
            for candidate in &memo.candidates[g] {
                if !matches!(
                    candidate.origin,
                    CandidateOrigin::Pass1 | CandidateOrigin::SharedBuild
                ) {
                    continue;
                }
                let canonical_scans = OperatorNode::reachable(&candidate.node)
                    .into_iter()
                    .filter(|node| {
                        preserve_group_boundaries
                            && matches!(
                                node.non_asap(),
                                Some(asap_types::ir::NonASAPOp::Scan { .. })
                            )
                            && group_of.contains_key(&Rc::as_ptr(node))
                    })
                    .collect::<Vec<_>>();
                let roots = [QueryRoot::Operator(Rc::clone(&candidate.node))];
                let panes = pane_candidates(&roots, &[Some(grid)]);
                if panes.is_empty() {
                    continue;
                }
                let Ok(pass2_candidates) = window_composition_pass2_candidates(&roots, &panes)
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
                    // semantics. Otherwise the pane's canonical input disappears.
                    let mut restored = HashMap::new();
                    for rewritten in OperatorNode::reachable(node) {
                        let mut matches = canonical_scans.iter().filter(|original| {
                            original.operator == rewritten.operator
                                && original.schema == rewritten.schema
                                && original.result_kind == rewritten.result_kind
                                && original.timing == rewritten.timing
                                && original.materialization == rewritten.materialization
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
                        rewritten_candidate(&group_of, candidate, node, CandidateOrigin::Panes),
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
                .with_guarantee(node.guarantee.clone())
                .with_timing(node.timing)
                .with_materialization(node.materialization),
        )
    } else {
        Rc::clone(node)
    };
    done.insert(Rc::as_ptr(node), Rc::clone(&rebuilt));
    rebuilt
}

// GCD closure offers grids for individual readers and compatible subsets.
// A nongridded potential consumer must not veto a repeated reader's panes.
fn compatible_reader_grids(group: &MemoGroup, entries: &[QueryWorkloadEntry]) -> Vec<u64> {
    let mut grids = BTreeSet::new();
    for grid in group
        .queries
        .iter()
        .filter_map(|&query| entries.get(query)?.evaluation_grid_ms())
        .filter(|grid| *grid > 0)
    {
        let added: Vec<_> = grids
            .iter()
            .map(|&known| greatest_common_divisor(known, grid))
            .chain(std::iter::once(grid))
            .collect();
        grids.extend(added);
    }
    grids.into_iter().collect()
}

fn group_of_node(groups: &[MemoGroup]) -> HashMap<*const OperatorNode, usize> {
    groups
        .iter()
        .enumerate()
        .map(|(g, group)| (Rc::as_ptr(&group.node), g))
        .collect()
}

fn rewritten_candidate(
    group_of: &HashMap<*const OperatorNode, usize>,
    from: &ImplementationCandidate,
    node: Rc<OperatorNode>,
    origin: CandidateOrigin,
) -> ImplementationCandidate {
    let reached = |node: &Rc<OperatorNode>| child_groups(group_of, node, &from.implements);
    let children = if reached(&from.node) == from.children {
        reached(&node)
    } else {
        from.children.clone()
    };
    ImplementationCandidate {
        implements: from.implements.clone(),
        node,
        children,
        origin,
        coupling_sites: from.coupling_sites.clone(),
        materializations: from.materializations.clone(),
        budget_share: from.budget_share,
        requires_exact_children: Vec::new(),
        rollup_of: None,
        requires_sites: Vec::new(),
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

fn push_new(memo: &mut Memo, g: usize, candidate: ImplementationCandidate) {
    let flat = |node: &Rc<OperatorNode>| flatten(&[QueryRoot::Operator(Rc::clone(node))]).0;
    let new = flat(&candidate.node);
    let known = memo.candidates[g].iter().any(|c| {
        c.origin == candidate.origin
            && c.coupling_sites == candidate.coupling_sites
            && flat(&c.node) == new
    });
    if !known {
        memo.candidates[g].push(candidate);
    }
}

fn group_grid_ms(group: &MemoGroup, entries: &[QueryWorkloadEntry]) -> Option<u64> {
    group
        .queries
        .iter()
        .map(|&q| entries[q].evaluation_grid_ms())
        .try_fold(0, |grid, query| Some(greatest_common_divisor(grid, query?)))
        .filter(|&grid| grid > 0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memo::{candidates::try_populate_pass1, LogicalMemo};
    use crate::per_computation_candidates::generate_per_computation_candidates;
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
            .enumerate()
            .map(|(id, _)| (id, QueryRoot::Operator(Rc::clone(&root))))
            .collect();
        let initial = LogicalMemo::from_roots(&roots);
        let generated = generate_per_computation_candidates(roots).unwrap();
        let memo = try_populate_pass1(initial, &generated).unwrap();
        let entries = recurrences
            .iter()
            .map(|recurrence| entry_with(target.clone(), recurrence.clone()))
            .collect();
        (memo, entries)
    }

    // A nongridded potential reader cannot eliminate another reader's panes.
    #[test]
    fn nongridded_potential_reader_preserves_local_pane_candidates() {
        let (memo, entries) = fixture(&[repeated(60_000), QueryRecurrence::Unknown]);
        assert!(local_pane_candidates(&memo, &entries, false).is_empty());
        let candidates = local_pane_candidates(&memo, &entries, true);
        assert!(!candidates.is_empty());
        assert!(candidates
            .iter()
            .all(|(_, candidate)| candidate.origin == CandidateOrigin::Panes));
    }

    // Individual grids and subset GCDs allow separate or shared physical choices.
    #[test]
    fn reader_grids_include_subsets_instead_of_only_full_union() {
        let (memo, entries) = fixture(&[
            repeated(120_000),
            repeated(180_000),
            QueryRecurrence::Unknown,
        ]);
        let group = memo
            .groups
            .iter()
            .find(|group| group.roots_of.contains(&0))
            .unwrap();
        assert_eq!(
            compatible_reader_grids(group, &entries),
            vec![60_000, 120_000, 180_000]
        );
    }
}

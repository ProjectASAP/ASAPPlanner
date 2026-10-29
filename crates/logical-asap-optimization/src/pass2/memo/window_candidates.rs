//! Local pane candidates generated from compatible reader grids. Selected
//! connections, not the union of potential consumers, determine eligibility.
use super::candidates::{
    CandidateForMemoGroup, CandidateMemo as Memo, OriginOfCandidateForMemoGroup,
};
#[cfg(test)]
use super::shared_configurations::pane_width_divisors;
use super::shared_configurations::{pane_width_domain_before, PaneWidthDomain};
use super::{check_generation_deadline, CandidateGenerationTimeout};
use super::{MemoGroup, MemoGroupId as GroupId};
use crate::pass2::asap_aware_cse::{pane_candidates, window_composition_pass2_candidates};
use asap_types::ir::flat::flatten;
use asap_types::ir::{OperatorNode, QueryRoot};
use asap_types::workload::QueryWorkloadEntry;
#[cfg(test)]
use std::collections::BTreeSet;
use std::collections::{HashMap, HashSet};
use std::rc::Rc;
use std::time::Instant;

/// Offer each supported reader-grid configuration. Stage 2 checks the actual
/// chosen consumers, including ingestion alignment and range evaluation steps.
pub fn add_local_window_composition(memo: &mut Memo, entries: &[QueryWorkloadEntry]) {
    add_local_window_composition_in_domain(memo, entries, PaneWidthDomain::Complete);
}

pub fn add_local_window_composition_in_domain(
    memo: &mut Memo,
    entries: &[QueryWorkloadEntry],
    domain: PaneWidthDomain,
) {
    try_add_local_window_composition_in_domain(memo, entries, domain, None)
        .expect("generation without a deadline cannot time out");
}

/// On timeout the caller must discard this memo, which may contain partial work.
pub fn try_add_local_window_composition_in_domain(
    memo: &mut Memo,
    entries: &[QueryWorkloadEntry],
    domain: PaneWidthDomain,
    deadline: Option<Instant>,
) -> Result<(), CandidateGenerationTimeout> {
    extend_local_window_composition(memo, entries, None, domain, deadline)
}

/// Extend a pre-state-expansion memo with only newly exposed pane widths.
/// `previous` must describe widths already generated for this memo.
pub fn extend_local_window_composition(
    memo: &mut Memo,
    entries: &[QueryWorkloadEntry],
    previous: Option<PaneWidthDomain>,
    domain: PaneWidthDomain,
    deadline: Option<Instant>,
) -> Result<(), CandidateGenerationTimeout> {
    check_generation_deadline(deadline)?;
    // Flatten each existing/generated graph once. Repeatedly flattening the
    // growing candidate list makes a complete pane-width domain quadratic.
    let mut known: HashMap<(usize, String), Vec<usize>> = HashMap::new();
    for (group, candidates) in memo.candidates.iter().enumerate() {
        check_generation_deadline(deadline)?;
        for (index, candidate) in candidates.iter().enumerate() {
            check_generation_deadline(deadline)?;
            if let Ok(signature) = candidate_signature(candidate) {
                known.entry((group, signature)).or_default().push(index);
            }
        }
    }
    for (group, candidate) in
        local_pane_candidates_delta(memo, entries, previous, domain, deadline)?
    {
        check_generation_deadline(deadline)?;
        let Ok(signature) = candidate_signature(&candidate) else {
            memo.candidates[group].push(candidate);
            continue;
        };
        let bucket = known.entry((group, signature)).or_default();
        if !bucket.iter().any(|&index| {
            let previous = &memo.candidates[group][index];
            previous.origin == candidate.origin
                && previous.children == candidate.children
                && previous.implements == candidate.implements
                && previous.budget_share == candidate.budget_share
                && previous.node == candidate.node
        }) {
            bucket.push(memo.candidates[group].len());
            memo.candidates[group].push(candidate);
        }
    }
    check_generation_deadline(deadline)?;
    Ok(())
}

/// Expand legal pane widths while preserving canonical memo inputs.
pub fn local_pane_candidates(
    memo: &Memo,
    entries: &[QueryWorkloadEntry],
) -> Vec<(usize, CandidateForMemoGroup)> {
    local_pane_candidates_before(memo, entries, PaneWidthDomain::Complete, None)
        .expect("generation without a deadline cannot time out")
}

fn local_pane_candidates_before(
    memo: &Memo,
    entries: &[QueryWorkloadEntry],
    domain: PaneWidthDomain,
    deadline: Option<Instant>,
) -> Result<Vec<(usize, CandidateForMemoGroup)>, CandidateGenerationTimeout> {
    local_pane_candidates_delta(memo, entries, None, domain, deadline)
}

fn local_pane_candidates_delta(
    memo: &Memo,
    entries: &[QueryWorkloadEntry],
    previous: Option<PaneWidthDomain>,
    domain: PaneWidthDomain,
    deadline: Option<Instant>,
) -> Result<Vec<(usize, CandidateForMemoGroup)>, CandidateGenerationTimeout> {
    check_generation_deadline(deadline)?;
    let group_of = group_of_node(&memo.groups);
    let mut own: Vec<(usize, CandidateForMemoGroup)> = Vec::new();
    for (g, group) in memo.groups.iter().enumerate() {
        check_generation_deadline(deadline)?;
        let grids = pane_width_domain_before(
            group
                .queries
                .iter()
                .filter_map(|&query| entries.get(query)?.evaluation_grid_ms()),
            domain,
            deadline,
        )?;
        let previous_widths = previous
            .map(|previous| {
                pane_width_domain_before(
                    group
                        .queries
                        .iter()
                        .filter_map(|&query| entries.get(query)?.evaluation_grid_ms()),
                    previous,
                    deadline,
                )
            })
            .transpose()?
            .unwrap_or_default();
        for grid in grids {
            if previous_widths.binary_search(&grid).is_ok() {
                continue;
            }
            check_generation_deadline(deadline)?;
            for candidate in &memo.candidates[g] {
                check_generation_deadline(deadline)?;
                if !matches!(
                    candidate.origin,
                    OriginOfCandidateForMemoGroup::Pass1
                        | OriginOfCandidateForMemoGroup::SharedBuild
                ) {
                    continue;
                }
                let canonical_scans = OperatorNode::reachable(&candidate.node)
                    .into_iter()
                    .filter(|node| {
                        matches!(
                            node.non_asap(),
                            Some(asap_types::ir::NonASAPOp::Scan { .. })
                        ) && group_of.contains_key(&Rc::as_ptr(node))
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
                    check_generation_deadline(deadline)?;
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
                        check_generation_deadline(deadline)?;
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
                        rewritten_candidate(
                            &group_of,
                            candidate,
                            node,
                            OriginOfCandidateForMemoGroup::Panes,
                        ),
                    ));
                }
            }
        }
    }
    check_generation_deadline(deadline)?;
    Ok(own)
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

// Union of complete divisor domains covers individual readers and every
// compatible subset: common widths belong to each reader's divisor domain.
// Nongridded potential consumers do not veto a repeated reader's panes.
#[cfg(test)]
fn compatible_reader_grids(group: &MemoGroup, entries: &[QueryWorkloadEntry]) -> Vec<u64> {
    super::shared_configurations::pane_width_domain(
        group
            .queries
            .iter()
            .filter_map(|&query| entries.get(query)?.evaluation_grid_ms()),
        PaneWidthDomain::Complete,
    )
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
    from: &CandidateForMemoGroup,
    node: Rc<OperatorNode>,
    origin: OriginOfCandidateForMemoGroup,
) -> CandidateForMemoGroup {
    let reached = |node: &Rc<OperatorNode>| child_groups(group_of, node, &from.implements);
    let children = if reached(&from.node) == from.children {
        reached(&node)
    } else {
        from.children.clone()
    };
    CandidateForMemoGroup {
        implements: from.implements.clone(),
        node,
        children,
        origin,
        budget_share: from.budget_share,
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
    serde_json::to_string(&flatten(&[QueryRoot::Operator(Rc::clone(&candidate.node))]).0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pass1::per_computation_candidates::generate_per_computation_candidates;
    use crate::pass2::memo::{candidates::try_populate_pass1, DagOfLogicalMemoGroups};
    use crate::test_support::{entry_with, lower_promql};
    use asap_types::ir::properties::AccuracyTarget;
    use asap_types::workload::{QueryRecurrence, RepeatedDemand, RepetitionInterval};

    // An expired generation budget reports an incomplete catalog explicitly;
    // callers must never interpret it as an empty, fully explored candidate set.
    #[test]
    fn expired_window_generation_returns_timeout() {
        let (mut memo, entries) = fixture(&[repeated(60_000)]);
        let original_sizes: Vec<_> = memo.candidates.iter().map(Vec::len).collect();
        assert!(try_add_local_window_composition_in_domain(
            &mut memo,
            &entries,
            PaneWidthDomain::Complete,
            Some(Instant::now())
        )
        .is_err());
        assert_eq!(
            memo.candidates.iter().map(Vec::len).collect::<Vec<_>>(),
            original_sizes
        );
    }

    #[test]
    fn incremental_local_widths_match_one_complete_pass() {
        let (base, entries) = fixture(&[repeated(60)]);
        let mut incremental = base.clone();
        let mut previous = None;
        for domain in [
            PaneWidthDomain::SeedGrids,
            PaneWidthDomain::Expanding { max_parts: 4 },
            PaneWidthDomain::Expanding { max_parts: 8 },
            PaneWidthDomain::Complete,
        ] {
            extend_local_window_composition(&mut incremental, &entries, previous, domain, None)
                .unwrap();
            assert!(local_pane_candidates_delta(
                &incremental,
                &entries,
                Some(domain),
                domain,
                None
            )
            .unwrap()
            .is_empty());
            previous = Some(domain);
        }
        let mut complete = base;
        add_local_window_composition(&mut complete, &entries);
        let signatures = |memo: &Memo| {
            memo.candidates
                .iter()
                .enumerate()
                .flat_map(|(group, candidates)| {
                    candidates
                        .iter()
                        .map(move |candidate| (group, candidate_signature(candidate).unwrap()))
                })
                .collect::<BTreeSet<_>>()
        };
        assert_eq!(signatures(&incremental), signatures(&complete));
    }

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
        let initial = DagOfLogicalMemoGroups::from_roots(&roots);
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
        let candidates = local_pane_candidates(&memo, &entries);
        assert!(!candidates.is_empty());
        assert!(candidates
            .iter()
            .all(|(_, candidate)| candidate.origin == OriginOfCandidateForMemoGroup::Panes));
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
            pane_width_divisors(120_000)
                .into_iter()
                .chain(pane_width_divisors(180_000))
                .collect::<BTreeSet<_>>()
                .into_iter()
                .collect::<Vec<_>>()
        );
    }
}

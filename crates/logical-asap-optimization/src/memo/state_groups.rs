//! Opt-in concrete summary-state groups for the candidate-connection planner.
//!
//! This does not change the production selection path. Only simple Pass 1 builds and local pane chains
//! directly over canonical input groups are expanded. Capacity, guarantee,
//! update/filter semantics, and materialization annotations must match exactly;
//! larger-capacity substitution and private preprocessing are not inferred.

use std::collections::{HashMap, HashSet};
use std::rc::Rc;

use asap_types::ir::{ASAPOp, NonASAPOp, OperatorNode};

use super::candidates::{CandidateMemo, CandidateOrigin, ImplementationCandidate};
use super::{MemoGroup, MemoGroupId};

#[derive(Debug)]
pub struct StateGroupExpansion {
    pub memo: CandidateMemo,
    pub state_groups: Vec<MemoGroupId>,
    pub added_reader_candidates: usize,
}

#[derive(Debug, thiserror::Error)]
pub enum StateGroupExpansionError {
    #[error("memo group and candidate vectors have different lengths")]
    MismatchedGroups,
    #[error("memo contains an out-of-range group reference")]
    InvalidGroupReference,
    #[error("memo group capacity exceeds the group identifier range")]
    TooManyGroups,
    #[error("memo dependencies contain a cycle")]
    CyclicDependencies,
    #[error("explicit summary-state groups have already been introduced")]
    AlreadyExpanded,
    #[error("a previously discovered reader no longer exposes its summary boundary")]
    InvalidReaderBoundary,
}

struct BuildReaders {
    build: Rc<OperatorNode>,
    children: Vec<MemoGroupId>,
    readers: Vec<(usize, usize)>,
}

/// Preserve local candidates and add explicit shared-state reader candidates.
/// The input must precede explicit state-group expansion. Unsupported candidate
/// shapes are retained unchanged, rather than guessed into a shared interface.
pub fn expand_concrete_summary_state_groups(
    mut memo: CandidateMemo,
) -> Result<StateGroupExpansion, StateGroupExpansionError> {
    validate_references(&memo)?;
    if memo.groups.iter().any(|group| {
        matches!(
            group.node.asap(),
            Some(ASAPOp::SummaryAgg { .. } | ASAPOp::PaneChain { .. })
        )
    }) {
        return Err(StateGroupExpansionError::AlreadyExpanded);
    }
    if memo.topological_order().is_none() {
        return Err(StateGroupExpansionError::CyclicDependencies);
    }
    let mut builds: Vec<BuildReaders> = Vec::new();
    for (group, candidates) in memo.candidates.iter().enumerate() {
        for (index, candidate) in candidates.iter().enumerate() {
            let Some((build, children)) = simple_build(&memo, candidate) else {
                continue;
            };
            if let Some(existing) = builds
                .iter_mut()
                .find(|entry| same_concrete_build(&entry.build, &build))
            {
                existing.readers.push((group, index));
            } else {
                builds.push(BuildReaders {
                    build,
                    children,
                    readers: vec![(group, index)],
                });
            }
        }
    }
    let mut state_groups = Vec::new();
    let mut added_reader_candidates = 0;
    for build in builds {
        if !build
            .readers
            .iter()
            .any(|&(group, _)| group != build.readers[0].0)
        {
            continue;
        }
        let state_group = MemoGroupId(
            u32::try_from(memo.groups.len())
                .map_err(|_| StateGroupExpansionError::TooManyGroups)?,
        );
        let mut queries: Vec<_> = build
            .readers
            .iter()
            .flat_map(|&(group, _)| memo.groups[group].queries.iter().copied())
            .collect();
        queries.sort_unstable();
        queries.dedup();
        memo.groups.push(MemoGroup {
            node: Rc::clone(&build.build),
            children: build.children.clone(),
            parents: Vec::new(),
            queries,
            roots_of: Vec::new(),
        });
        memo.candidates.push(vec![ImplementationCandidate {
            implements: vec![state_group],
            node: Rc::clone(&build.build),
            children: build.children,
            origin: CandidateOrigin::SharedBuild,
            coupling_sites: Vec::new(),
            budget_share: 1,
            requires_exact_children: Vec::new(),
            rollup_of: None,
            materializations: Vec::new(),
            requires_sites: Vec::new(),
        }]);
        for (group, index) in build.readers {
            let mut reader = memo.candidates[group][index].clone();
            // Each reader may have an equal but separately allocated build.
            let (own_build, _) = simple_build(&memo, &reader)
                .ok_or(StateGroupExpansionError::InvalidReaderBoundary)?;
            reader.node = replace_node(&reader.node, &own_build, &build.build);
            let boundaries: HashMap<_, _> = reader
                .children
                .iter()
                .map(|&id| (Rc::as_ptr(&memo.groups[id.0 as usize].node), id))
                .chain([(Rc::as_ptr(&build.build), state_group)])
                .collect();
            reader.children = boundary_groups(&reader.node, &boundaries);
            reader.origin = CandidateOrigin::SharedBuild;
            memo.candidates[group].push(reader);
            added_reader_candidates += 1;
        }
        state_groups.push(state_group);
    }
    // Parents describe every possible group/candidate dependency, not a selected plan.
    for group in &mut memo.groups {
        group.parents.clear();
    }
    for group in 0..memo.groups.len() {
        let dependencies: HashSet<_> = memo.groups[group]
            .children
            .iter()
            .chain(
                memo.candidates[group]
                    .iter()
                    .flat_map(|candidate| &candidate.children),
            )
            .copied()
            .collect();
        for child in dependencies {
            memo.groups[child.0 as usize]
                .parents
                .push(MemoGroupId(group as u32));
        }
    }
    if memo.topological_order().is_none() {
        return Err(StateGroupExpansionError::CyclicDependencies);
    }
    // The chosen representative input may now serve additional queries through
    // state reuse; propagate potential reach along the expanded dependency DAG.
    for group in memo
        .topological_order()
        .ok_or(StateGroupExpansionError::CyclicDependencies)?
        .into_iter()
        .rev()
    {
        let queries = memo.groups[group.0 as usize].queries.clone();
        let children: HashSet<_> = memo.groups[group.0 as usize]
            .children
            .iter()
            .chain(
                memo.candidates[group.0 as usize]
                    .iter()
                    .flat_map(|candidate| &candidate.children),
            )
            .copied()
            .collect();
        for child in children {
            memo.groups[child.0 as usize].queries.extend(&queries);
            memo.groups[child.0 as usize].queries.sort_unstable();
            memo.groups[child.0 as usize].queries.dedup();
        }
    }
    Ok(StateGroupExpansion {
        memo,
        state_groups,
        added_reader_candidates,
    })
}

fn validate_references(memo: &CandidateMemo) -> Result<(), StateGroupExpansionError> {
    if memo.groups.len() != memo.candidates.len() {
        return Err(StateGroupExpansionError::MismatchedGroups);
    }
    if memo.groups.len() > u32::MAX as usize {
        return Err(StateGroupExpansionError::TooManyGroups);
    }
    for group in &memo.groups {
        if group
            .children
            .iter()
            .chain(&group.parents)
            .any(|id| id.0 as usize >= memo.groups.len())
        {
            return Err(StateGroupExpansionError::InvalidGroupReference);
        }
    }
    for candidate in memo.candidates.iter().flatten() {
        if candidate
            .children
            .iter()
            .chain(&candidate.implements)
            .chain(&candidate.requires_exact_children)
            .chain(candidate.requires_sites.iter().map(|(group, _)| group))
            .any(|id| id.0 as usize >= memo.groups.len())
            || candidate
                .rollup_of
                .as_ref()
                .is_some_and(|rollup| rollup.finer_group.0 as usize >= memo.groups.len())
        {
            return Err(StateGroupExpansionError::InvalidGroupReference);
        }
    }
    Ok(())
}

fn simple_build(
    memo: &CandidateMemo,
    candidate: &ImplementationCandidate,
) -> Option<(Rc<OperatorNode>, Vec<MemoGroupId>)> {
    if !matches!(
        candidate.origin,
        CandidateOrigin::Pass1 | CandidateOrigin::Panes
    ) || candidate.implements.len() != 1
        || !candidate.coupling_sites.is_empty()
        || !candidate.requires_sites.is_empty()
        || !candidate.requires_exact_children.is_empty()
        || candidate.rollup_of.is_some()
        || !candidate.materializations.is_empty()
        || candidate.budget_share != 1
    {
        return None;
    }
    let boundaries: HashMap<_, _> = candidate
        .children
        .iter()
        .map(|&group| (Rc::as_ptr(&memo.groups[group.0 as usize].node), group))
        .collect();
    let mut stack = vec![Rc::clone(&candidate.node)];
    let mut seen = HashSet::new();
    let mut build = None;
    while let Some(node) = stack.pop() {
        if boundaries.contains_key(&Rc::as_ptr(&node)) || !seen.insert(Rc::as_ptr(&node)) {
            continue;
        }
        if matches!(
            node.asap(),
            Some(ASAPOp::SummaryAgg { .. } | ASAPOp::PaneChain { .. })
        ) {
            if build.is_some() {
                return None;
            }
            build = Some(Rc::clone(&node));
        }
        stack.extend(node.children().into_iter().cloned());
    }
    let build = build?;
    if Rc::ptr_eq(&build, &candidate.node) {
        return None;
    }
    let mut children = Vec::new();
    for child in build.children() {
        let group = *boundaries.get(&Rc::as_ptr(child))?;
        if !children.contains(&group) {
            children.push(group);
        }
    }
    Some((build, children))
}

fn boundary_groups(
    node: &Rc<OperatorNode>,
    boundaries: &HashMap<*const OperatorNode, MemoGroupId>,
) -> Vec<MemoGroupId> {
    let mut found = Vec::new();
    let mut stack = vec![Rc::clone(node)];
    let mut seen = HashSet::new();
    while let Some(node) = stack.pop() {
        if !seen.insert(Rc::as_ptr(&node)) {
            continue;
        }
        if let Some(&group) = boundaries.get(&Rc::as_ptr(&node)) {
            if !found.contains(&group) {
                found.push(group);
            }
        } else {
            stack.extend(node.children().into_iter().rev().cloned());
        }
    }
    found
}

// Immediate inputs are canonical memo nodes. Compare local fields without
// recursively walking their potentially shared DAGs; serialization additionally
// distinguishes signed-zero parameters, matching conservative CSE semantics.
fn same_concrete_build(left: &OperatorNode, right: &OperatorNode) -> bool {
    let left_inputs = left.children();
    let right_inputs = right.children();
    left_inputs.len() == right_inputs.len()
        && left_inputs
            .iter()
            .zip(right_inputs)
            .all(|(left, right)| Rc::ptr_eq(left, right) || same_range_input(left, right))
        && same_local_fields(left, right)
}

// TimeRange lacks a unique row key and therefore is not generally memo-interned.
// Equal ranges over the same canonical input nevertheless feed identical data
// into a summary build, as the existing ASAP-aware summary reuse rule allows.
fn same_range_input(left: &OperatorNode, right: &OperatorNode) -> bool {
    let (
        Some(NonASAPOp::TimeRange {
            child: left_child, ..
        }),
        Some(NonASAPOp::TimeRange {
            child: right_child, ..
        }),
    ) = (left.non_asap(), right.non_asap())
    else {
        return false;
    };
    Rc::ptr_eq(left_child, right_child) && same_local_fields(left, right)
}

fn same_local_fields(left: &OperatorNode, right: &OperatorNode) -> bool {
    let signature = |node: &OperatorNode| {
        (
            node.operator.map_children(|_| ()),
            node.result_kind,
            node.schema.clone(),
            node.guarantee.clone(),
            node.timing,
            node.materialization,
        )
    };
    let left = signature(left);
    let right = signature(right);
    left == right
        && matches!((serde_json::to_string(&left), serde_json::to_string(&right)), (Ok(left), Ok(right)) if left == right)
}

fn replace_node(
    node: &Rc<OperatorNode>,
    old: &Rc<OperatorNode>,
    new: &Rc<OperatorNode>,
) -> Rc<OperatorNode> {
    replace_node_cached(node, old, new, &mut HashMap::new())
}

fn replace_node_cached(
    node: &Rc<OperatorNode>,
    old: &Rc<OperatorNode>,
    new: &Rc<OperatorNode>,
    rebuilt: &mut HashMap<*const OperatorNode, Rc<OperatorNode>>,
) -> Rc<OperatorNode> {
    if Rc::ptr_eq(node, old) {
        return Rc::clone(new);
    }
    if let Some(existing) = rebuilt.get(&Rc::as_ptr(node)) {
        return Rc::clone(existing);
    }
    let mut changed = false;
    let operator = node.operator.map_children(|child| {
        let replacement = replace_node_cached(child, old, new, rebuilt);
        changed |= !Rc::ptr_eq(child, &replacement);
        replacement
    });
    let result = if changed {
        Rc::new(OperatorNode {
            operator,
            ..(**node).clone()
        })
    } else {
        Rc::clone(node)
    };
    rebuilt.insert(Rc::as_ptr(node), Rc::clone(&result));
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memo::candidates::try_populate_pass1;
    use crate::memo::LogicalMemo;
    use crate::per_computation_candidates::generate_per_computation_candidates;
    use asap_types::ir::properties::AccuracyTarget;
    use asap_types::ir::QueryRoot;

    fn workload(queries: &[(&str, f64)]) -> CandidateMemo {
        let roots: Vec<_> = queries
            .iter()
            .enumerate()
            .map(|(id, (query, error))| {
                (
                    id,
                    QueryRoot::Operator(crate::test_support::lower_promql(
                        query,
                        AccuracyTarget::max_error(*error).failure_probability(0.001),
                    )),
                )
            })
            .collect();
        let initial = LogicalMemo::from_roots(&roots);
        let inventory = generate_per_computation_candidates(roots).unwrap();
        try_populate_pass1(initial, &inventory).unwrap()
    }

    // Equal-sized quantiles share one canonical state while local choices remain.
    #[test]
    fn quantile_readers_share_concrete_state_and_retain_local_candidates() {
        let original = workload(&[
            ("quantile_over_time(0.95, latency[5m])", 0.01),
            ("quantile_over_time(0.99, latency[5m])", 0.01),
        ]);
        let original_count = original.groups.len();
        let expanded = expand_concrete_summary_state_groups(original.clone()).unwrap();
        assert!(!expanded.state_groups.is_empty());
        assert!(expanded.added_reader_candidates >= 2);
        let order = expanded.memo.topological_order().unwrap();
        for state in &expanded.state_groups {
            let group = &expanded.memo.groups[state.0 as usize];
            assert_eq!(group.queries, vec![0, 1]);
            for input in &group.children {
                assert_eq!(expanded.memo.groups[input.0 as usize].queries, vec![0, 1]);
                assert!(group
                    .node
                    .children()
                    .iter()
                    .any(|node| Rc::ptr_eq(node, &expanded.memo.groups[input.0 as usize].node)));
            }
            assert_eq!(group.parents.len(), 2);
            assert!(group.roots_of.is_empty());
            for &parent in &group.parents {
                assert!(
                    order.iter().position(|id| id == state).unwrap()
                        < order.iter().position(|id| id == &parent).unwrap()
                );
                let reader = expanded.memo.candidates[parent.0 as usize]
                    .iter()
                    .find(|candidate| candidate.children.contains(state))
                    .unwrap();
                assert!(OperatorNode::reachable(&reader.node)
                    .iter()
                    .any(|node| Rc::ptr_eq(node, &group.node)));
                assert_eq!(reader.children, vec![*state]);
            }
        }
        for group in 0..original_count {
            for (before, after) in original.candidates[group]
                .iter()
                .zip(&expanded.memo.candidates[group])
            {
                assert!(Rc::ptr_eq(&before.node, &after.node));
                assert_eq!(before.children, after.children);
            }
        }
        assert!(matches!(
            expand_concrete_summary_state_groups(expanded.memo),
            Err(StateGroupExpansionError::AlreadyExpanded)
        ));
    }

    // Capacity and source predicates are part of the concrete producer identity.
    #[test]
    fn distinct_sizes_and_filters_do_not_share_concrete_builds() {
        for queries in [
            [
                ("quantile_over_time(0.95, latency[5m])", 0.01),
                ("quantile_over_time(0.99, latency[5m])", 0.02),
            ],
            [
                ("quantile_over_time(0.95, latency{job=\"a\"}[5m])", 0.01),
                ("quantile_over_time(0.99, latency{job=\"b\"}[5m])", 0.01),
            ],
        ] {
            let original = workload(&queries);
            let count = original.groups.len();
            let expanded = expand_concrete_summary_state_groups(original).unwrap();
            assert!(expanded.state_groups.is_empty());
            assert_eq!(expanded.memo.groups.len(), count);
            assert_eq!(expanded.added_reader_candidates, 0);
        }
    }

    fn with_local_panes(mut memo: CandidateMemo, grids: &[u64]) -> CandidateMemo {
        use crate::asap_aware_cse::{pane_candidates, window_composition_pass2_candidates};

        let boundaries: HashMap<_, _> = memo
            .groups
            .iter()
            .enumerate()
            .map(|(id, group)| (Rc::as_ptr(&group.node), MemoGroupId(id as u32)))
            .collect();
        for group in 0..memo.groups.len() {
            let grid = grids[memo.groups[group].queries[0]];
            for candidate in memo.candidates[group].clone() {
                let roots = vec![QueryRoot::Operator(Rc::clone(&candidate.node))];
                let panes = pane_candidates(&roots, &[Some(grid)]);
                if panes.len() != 1 {
                    continue;
                }
                let rewritten = window_composition_pass2_candidates(&roots, &panes)
                    .unwrap()
                    .remove(0)
                    .roots
                    .unwrap();
                let QueryRoot::Operator(node) = &rewritten[0] else {
                    unreachable!()
                };
                // Pane rewriting recomposes guarantees; restore the original
                // canonical scan boundary rather than making it a private input.
                let input = panes[0].build.children()[0].children()[0].clone();
                let chain = OperatorNode::reachable(node)
                    .into_iter()
                    .find(|node| matches!(node.asap(), Some(ASAPOp::PaneChain { .. })))
                    .unwrap();
                let node = replace_node(node, chain.children()[0], &input);
                let mut pane = candidate;
                pane.node = Rc::clone(&node);
                pane.children = boundary_groups(&node, &boundaries);
                pane.origin = CandidateOrigin::Panes;
                memo.candidates[group].push(pane);
            }
        }
        memo
    }

    // Real pane rewrites expose one identical chain to distinct quantile readers.
    #[test]
    fn identical_local_pane_chains_share_state_without_removing_local_choices() {
        let original = with_local_panes(
            workload(&[
                ("quantile_over_time(0.95, latency[5m])", 0.01),
                ("quantile_over_time(0.99, latency[5m])", 0.01),
            ]),
            &[60_000, 60_000],
        );
        let local_panes = original
            .candidates
            .iter()
            .flatten()
            .filter(|candidate| candidate.origin == CandidateOrigin::Panes)
            .count();
        assert!(local_panes >= 2);
        let expanded = expand_concrete_summary_state_groups(original).unwrap();
        let chains: Vec<_> = expanded
            .state_groups
            .iter()
            .filter(|id| {
                matches!(
                    expanded.memo.groups[id.0 as usize].node.asap(),
                    Some(ASAPOp::PaneChain { .. })
                )
            })
            .collect();
        assert!(!chains.is_empty());
        for id in chains {
            let group = &expanded.memo.groups[id.0 as usize];
            assert_eq!(group.queries, vec![0, 1]);
            assert_eq!(group.parents.len(), 2);
            assert!(group.children.iter().all(|input| group
                .node
                .children()
                .iter()
                .any(|node| Rc::ptr_eq(node, &expanded.memo.groups[input.0 as usize].node))));
        }
        assert_eq!(
            expanded
                .memo
                .candidates
                .iter()
                .flatten()
                .filter(|candidate| candidate.origin == CandidateOrigin::Panes)
                .count(),
            local_panes
        );
        assert!(matches!(
            expand_concrete_summary_state_groups(expanded.memo),
            Err(StateGroupExpansionError::AlreadyExpanded)
        ));
    }

    // Different pane widths and retention counts remain different state contracts.
    #[test]
    fn different_local_pane_configurations_do_not_share_state() {
        let memo = with_local_panes(
            workload(&[
                ("quantile_over_time(0.95, latency[5m])", 0.01),
                ("quantile_over_time(0.99, latency[5m])", 0.01),
            ]),
            &[60_000, 30_000],
        );
        let expanded = expand_concrete_summary_state_groups(memo).unwrap();
        assert!(expanded.state_groups.iter().all(|id| !matches!(
            expanded.memo.groups[id.0 as usize].node.asap(),
            Some(ASAPOp::PaneChain { .. })
        )));
    }

    // Public expansion rejects malformed IDs before indexing dependency arrays.
    #[test]
    fn malformed_references_fail_before_dependency_traversal() {
        let mut memo = workload(&[("quantile_over_time(0.95, latency[5m])", 0.01)]);
        memo.groups[0].children.push(MemoGroupId(u32::MAX));
        assert!(matches!(
            expand_concrete_summary_state_groups(memo),
            Err(StateGroupExpansionError::InvalidGroupReference)
        ));
    }
}

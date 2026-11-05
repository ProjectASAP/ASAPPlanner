//! Opt-in concrete summary-state groups for the candidate-connection planner.
//!
//! Simple summary builds and tumbling-window chains directly over canonical input groups
//! gain finite compatible sizes and tumbling-window horizons before exact state interning.
//! Concrete state identity includes capacity, guarantee, update/filter semantics,
//! and materialization. Supported private projections/filters gain explicit input groups.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::rc::Rc;

use asap_types::ir::{ASAPOp, NonASAPOp, OperatorNode};

use crate::own_candidate_sizing::find_or_add_sized_group;
use asap_types::stages::hash_consing::HashConsingTable;
use asap_types::stages::{CandidateForMemoGroup, Memo, MemoGroupId, OriginOfCandidateForMemoGroup};

/// Why Pass 2's rules failed on a memo: its precondition check, a rule
/// (2–9), or the shared summary-state groups (10).
#[derive(Debug, thiserror::Error)]
pub enum Pass2RuleError {
    #[error("memo group and candidate vectors have different lengths")]
    MismatchedGroups,
    #[error("memo contains an out-of-range group reference")]
    InvalidGroupReference,
    #[error("memo group capacity exceeds the group identifier range")]
    TooManyGroups,
    #[error("memo dependencies contain a cycle")]
    CyclicDependencies,
    #[error("scan-label candidate expansion: {0}")]
    ScanLabelWidening(String),
    #[error("state configuration expansion: {0}")]
    StateConfiguration(String),
    #[error("producer reuse rule: {0}")]
    ReuseRule(String),
}

/// A private summary build and its occurrences: equal builds (Stage 0's
/// equality) are one.
struct BuildReaders {
    build: Rc<OperatorNode>,
    /// Each occurrence: the reader's group, its candidate's index there, and
    /// the build node in that candidate.
    readers: Vec<(usize, usize, Rc<OperatorNode>)>,
}

/// Rejects a memo the Pass 2 rules cannot extend: mismatched or
/// out-of-range group references, or cyclic dependencies. Pass 2 checks
/// this before its rules.
pub(crate) fn check_group_references_valid_and_acyclic(memo: &Memo) -> Result<(), Pass2RuleError> {
    validate_references(memo)?;
    if memo.topological_order().is_none() {
        return Err(Pass2RuleError::CyclicDependencies);
    }
    Ok(())
}

/// Pass 2's last step, after its rules: give each private summary build that
/// two or more candidates make equally (Stage 0's equality) its own group,
/// and add to each reader group a candidate reading that group; a build
/// equal to an existing group's node is read from that group instead, its
/// own candidate sized for the most budget parts of its readers. Every
/// existing candidate stays (unsupported candidate shapes unchanged). Then
/// rebuilds every group's parents and the queries it may serve. On error,
/// discard `memo`: it may hold partial work.
pub(crate) fn add_shared_summary_state_groups(memo: &mut Memo) -> Result<(), Pass2RuleError> {
    let mut builds: Vec<BuildReaders> = Vec::new();
    let mut equal_builds: HashConsingTable<usize> = HashConsingTable::default();
    for (group, candidates) in memo.candidates.iter().enumerate() {
        for (index, candidate) in candidates.iter().enumerate() {
            for (build, _) in direct_input_builds(memo, candidate) {
                match equal_builds.find(&build) {
                    Some(entry) => builds[entry].readers.push((group, index, build)),
                    None => {
                        equal_builds.insert(Rc::clone(&build), builds.len());
                        builds.push(BuildReaders {
                            build: Rc::clone(&build),
                            readers: vec![(group, index, build)],
                        });
                    }
                }
            }
        }
    }
    let mut state_groups = Vec::new();
    let mut replacements: BTreeMap<(usize, usize), HashMap<*const OperatorNode, Rc<OperatorNode>>> =
        BTreeMap::new();
    for build in builds {
        // The build is its readers' own build, so it is sized for their
        // shares of the budget (equal builds have equal sizes).
        let accuracy_budget_parts = build
            .readers
            .iter()
            .map(|(group, index, _)| memo.candidates[*group][*index].accuracy_budget_parts)
            .max()
            .unwrap_or(1);
        // A build no group's node equals is given a group when two or more
        // candidates make it. Repeated private builds within one result are
        // reusable too: the reader list records concrete build occurrences,
        // not just owners.
        if build.readers.len() < 2 && memo.group_with_equal_node(&build.build).is_none() {
            continue;
        }
        let mut queries: Vec<_> = build
            .readers
            .iter()
            .flat_map(|(group, _, _)| memo.groups()[*group].queries.iter().copied())
            .collect();
        queries.sort_unstable();
        queries.dedup();
        let state_group = find_or_add_sized_group(
            memo,
            &build.build,
            &queries,
            &OriginOfCandidateForMemoGroup::SharedBuild,
            accuracy_budget_parts,
        );
        let node = Rc::clone(&memo.groups()[state_group.0 as usize].logical_node);
        for (group, index, own_build) in build.readers {
            replacements
                .entry((group, index))
                .or_default()
                .insert(Rc::as_ptr(&own_build), Rc::clone(&node));
        }
        if !state_groups.contains(&state_group) {
            state_groups.push(state_group);
        }
    }
    for ((group, index), replacements) in replacements {
        // Factor every available producer boundary together. A producer used by
        // only one selected reader is still legal; no state-subset enumeration
        // is required. Retain the original entirely local implementation too.
        let mut reader = memo.candidates[group][index].clone();
        // Only declared inputs and newly lifted states are boundaries. A
        // factored result group's representative can equal its candidate root;
        // treating every memo node as an input would create a self-alias here.
        let boundaries = reader
            .input_groups
            .iter()
            .chain(&state_groups)
            .map(|id| (Rc::as_ptr(&memo.groups()[id.0 as usize].logical_node), *id))
            .collect();
        reader.root_node = super::rules::reuse::replace(&reader.root_node, &replacements);
        reader.input_groups = boundary_groups(&reader.root_node, &boundaries);
        reader.origin = OriginOfCandidateForMemoGroup::SharedBuild;
        memo.candidates[group].push(reader);
    }
    // Parents describe every possible group/candidate dependency, not a selected plan.
    memo.rebuild_parents();
    // The chosen representative input may now serve additional queries through
    // state reuse; propagate potential reach along the expanded dependency
    // DAG, readers first.
    for group in memo
        .topological_order()
        .ok_or(Pass2RuleError::CyclicDependencies)?
        .into_iter()
        .rev()
    {
        let queries = memo.groups()[group.0 as usize].queries.clone();
        memo.add_queries(group, &queries);
    }
    Ok(())
}

fn validate_references(memo: &Memo) -> Result<(), Pass2RuleError> {
    if memo.groups().len() != memo.candidates.len() {
        return Err(Pass2RuleError::MismatchedGroups);
    }
    if memo.groups().len() > u32::MAX as usize {
        return Err(Pass2RuleError::TooManyGroups);
    }
    for group in memo.groups() {
        if group
            .child_groups
            .iter()
            .chain(&group.parents)
            .any(|id| id.0 as usize >= memo.groups().len())
        {
            return Err(Pass2RuleError::InvalidGroupReference);
        }
    }
    for candidate in memo.candidates.iter().flatten() {
        if candidate
            .input_groups
            .iter()
            .chain(&candidate.fused_groups)
            .any(|id| id.0 as usize >= memo.groups().len())
        {
            return Err(Pass2RuleError::InvalidGroupReference);
        }
    }
    Ok(())
}

/// Where a candidate reads its one summary build ([`build_of`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum BuildPlace {
    /// One of the candidate's private nodes.
    Private,
    /// An input group of the candidate (a build Pass 1 added), which its
    /// root reads directly.
    InputGroup(MemoGroupId),
    /// Beneath the groups the candidate reads, down to Stage 0's groups.
    BeneathGroups,
}

/// The one summary build `candidate` reads, and where, the first found of:
/// - its one private build ([`direct_input_builds`]), when the sharing steps
///   read its builds by origin;
/// - with no private build, its one input group whose node is a
///   `SummaryAgg`, which its root reads directly;
/// - the one build beneath its root down to Stage 0's groups, through the
///   groups added beneath them ([`build_down_to_stage0_groups`]).
///
/// `None` when there is none, or more than one where it looks.
pub(super) fn build_of(
    memo: &Memo,
    candidate: &CandidateForMemoGroup,
) -> Option<(Rc<OperatorNode>, BuildPlace)> {
    if direct_input_builds_eligible(candidate) {
        let mut builds = direct_input_builds(memo, candidate);
        match builds.len() {
            1 => return Some((builds.remove(0).0, BuildPlace::Private)),
            0 => {
                if let Some(found) = input_group_build(memo, candidate) {
                    return Some(found);
                }
            }
            _ => {}
        }
    }
    let (build, _) = build_down_to_stage0_groups(memo, candidate)?;
    Some((build, BuildPlace::BeneathGroups))
}

/// The only summary build beneath `candidate`'s root down to Stage 0's
/// groups, through its private nodes and the groups added beneath them
/// ([`builds_beneath`]), with the Stage 0 groups it reads; `None` unless
/// there is exactly one and it reads only Stage 0 groups. Unlike
/// [`build_of`]'s first look, a build over another private node counts:
/// scan widening rewrites the whole candidate, so it needs the only build.
pub(super) fn build_down_to_stage0_groups(
    memo: &Memo,
    candidate: &CandidateForMemoGroup,
) -> Option<(Rc<OperatorNode>, Vec<MemoGroupId>)> {
    let [(build, children)]: [_; 1] = builds_beneath(&candidate.root_node, &|node| {
        memo.group_whose_node_is(node)
            .filter(|&group| memo.is_stage0_group(group))
    })
    .try_into()
    .ok()?;
    Some((build, children?))
}

/// `candidate`'s one input group whose node is a `SummaryAgg`, which its
/// root reads directly, with that node.
fn input_group_build(
    memo: &Memo,
    candidate: &CandidateForMemoGroup,
) -> Option<(Rc<OperatorNode>, BuildPlace)> {
    let mut builds = candidate.input_groups.iter().filter(|group| {
        matches!(
            memo.groups()[group.0 as usize].logical_node.asap(),
            Some(ASAPOp::SummaryAgg { .. })
        )
    });
    let group = *builds.next()?;
    let build = &memo.groups()[group.0 as usize].logical_node;
    if builds.next().is_some()
        || !candidate
            .root_node
            .children()
            .iter()
            .any(|child| Rc::ptr_eq(child, build))
    {
        return None;
    }
    Some((Rc::clone(build), BuildPlace::InputGroup(group)))
}

/// Whether the sharing steps read `candidate`'s builds: one made by Pass 1
/// or a rule that composes its own build, sized for some budget parts.
fn direct_input_builds_eligible(candidate: &CandidateForMemoGroup) -> bool {
    matches!(
        candidate.origin,
        OriginOfCandidateForMemoGroup::Pass1
            | OriginOfCandidateForMemoGroup::TumblingWindows
            | OriginOfCandidateForMemoGroup::WidenedScans
            | OriginOfCandidateForMemoGroup::Rollup
            | OriginOfCandidateForMemoGroup::SummaryResult
    ) && candidate.accuracy_budget_parts != 0
}

/// The summary builds `candidate` makes beneath its root, each with the
/// input groups it reads ([`builds_beneath`] down to its input groups): a
/// build reading another of its private nodes is left out. Empty unless
/// the candidate is one the sharing steps read by origin.
pub(super) fn direct_input_builds(
    memo: &Memo,
    candidate: &CandidateForMemoGroup,
) -> Vec<(Rc<OperatorNode>, Vec<MemoGroupId>)> {
    if !direct_input_builds_eligible(candidate) {
        return Vec::new();
    }
    let boundaries: HashMap<_, _> = candidate
        .input_groups
        .iter()
        .map(|&group| {
            (
                Rc::as_ptr(&memo.groups()[group.0 as usize].logical_node),
                group,
            )
        })
        .collect();
    builds_beneath(&candidate.root_node, &|node| {
        boundaries.get(&Rc::as_ptr(node)).copied()
    })
    .into_iter()
    .filter_map(|(build, children)| Some((build, children?)))
    .collect()
}

/// The summary builds (`SummaryAgg`, `SummaryTumblingWindowChain`) beneath
/// `root`, down to the nodes `boundary` gives a group (excluded), each
/// once, with the boundary groups it reads (repeats next to each other
/// removed), or `None` when it reads a node that is not a boundary.
pub(super) fn builds_beneath(
    root: &Rc<OperatorNode>,
    boundary: &dyn Fn(&Rc<OperatorNode>) -> Option<MemoGroupId>,
) -> Vec<(Rc<OperatorNode>, Option<Vec<MemoGroupId>>)> {
    let mut stack: Vec<Rc<OperatorNode>> = root.children().into_iter().cloned().collect();
    let mut seen = HashSet::new();
    let mut builds = Vec::new();
    while let Some(node) = stack.pop() {
        if boundary(&node).is_some() || !seen.insert(Rc::as_ptr(&node)) {
            continue;
        }
        if matches!(
            node.asap(),
            Some(ASAPOp::SummaryAgg { .. } | ASAPOp::SummaryTumblingWindowChain { .. })
        ) {
            let children: Option<Vec<_>> = node
                .children()
                .iter()
                .map(|child| boundary(child))
                .collect();
            builds.push((
                Rc::clone(&node),
                children.map(|mut children| {
                    children.dedup();
                    children
                }),
            ));
        }
        stack.extend(node.children().into_iter().cloned());
    }
    builds
}

pub(super) fn boundary_groups(
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

/// The nodes of `candidate`'s input groups, where its own nodes end.
pub(super) fn boundary_ptrs(
    memo: &Memo,
    candidate: &CandidateForMemoGroup,
) -> HashSet<*const OperatorNode> {
    candidate
        .input_groups
        .iter()
        .map(|group| Rc::as_ptr(&memo.groups()[group.0 as usize].logical_node))
        .collect()
}

/// `root` and the nodes beneath it down to `boundaries` (excluded), each
/// once, in depth-first order (last child first).
pub(super) fn nodes_within(
    root: &Rc<OperatorNode>,
    boundaries: &HashSet<*const OperatorNode>,
) -> Vec<Rc<OperatorNode>> {
    let mut pending = vec![Rc::clone(root)];
    let mut seen = HashSet::new();
    let mut nodes = Vec::new();
    while let Some(node) = pending.pop() {
        if boundaries.contains(&Rc::as_ptr(&node)) || !seen.insert(Rc::as_ptr(&node)) {
            continue;
        }
        pending.extend(node.children().into_iter().cloned());
        nodes.push(node);
    }
    nodes
}

// TimeRange lacks a unique row key and therefore is not generally memo-interned.
// Equal ranges over the same canonical input nevertheless feed identical data
// into a summary build, as the existing ASAP-aware summary reuse rule allows.
pub(super) fn same_range_input(left: &OperatorNode, right: &OperatorNode) -> bool {
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

#[cfg(test)]
fn replace_node(
    node: &Rc<OperatorNode>,
    old: &Rc<OperatorNode>,
    new: &Rc<OperatorNode>,
) -> Rc<OperatorNode> {
    super::rules::reuse::replace(node, &HashMap::from([(Rc::as_ptr(old), Rc::clone(new))]))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pass1::add_pass1_candidates::try_add_pass1_candidates;
    use crate::pass1::per_computation_candidates::generate_per_computation_candidates;
    use crate::stage0::generate_dag_of_logical_memo_groups;
    use asap_types::ir::properties::AccuracyTarget;
    use asap_types::ir::QueryRoot;

    /// An expanded memo with its shared summary-state groups.
    struct Expanded {
        memo: Memo,
        /// The shared summary-state groups
        /// ([`crate::test_support::shared_summary_state_groups`]).
        state_groups: Vec<MemoGroupId>,
    }

    /// Pass 2 after local window composition, over the complete tumbling-window-width
    /// domain, as the Pass 2 entry runs it.
    fn expand(memo: Memo) -> Result<Expanded, Pass2RuleError> {
        let memo = crate::test_support::sharing_rules_and_shared_state_groups(memo)?;
        Ok(expanded(memo))
    }

    fn expanded(memo: Memo) -> Expanded {
        Expanded {
            state_groups: crate::test_support::shared_summary_state_groups(&memo),
            memo,
        }
    }

    fn workload(queries: &[(&str, f64)]) -> Memo {
        workload_with_identity(queries, false)
    }

    fn workload_with_identity(queries: &[(&str, f64)], identity: bool) -> Memo {
        let roots: Vec<_> = queries
            .iter()
            .map(|(query, error)| {
                let root = crate::test_support::lower_promql(
                    query,
                    AccuracyTarget::max_error(*error).failure_probability(0.001),
                );
                let root = if identity {
                    asap_types::ir::schema::schema_support::with_promql_series_identity(&root)
                        .unwrap()
                } else {
                    root
                };
                QueryRoot::Operator(root)
            })
            .collect();
        let initial = generate_dag_of_logical_memo_groups(&roots);
        let inventory =
            generate_per_computation_candidates(&generate_dag_of_logical_memo_groups(&roots))
                .unwrap();
        try_add_pass1_candidates(initial, &inventory, &[], &Default::default()).unwrap()
    }

    // Equal-sized quantiles share one canonical state (Pass 1 adds it as a group)
    // while local choices remain.
    #[test]
    fn quantile_readers_share_concrete_state_and_retain_local_candidates() {
        let original = workload(&[
            ("quantile_over_time(0.95, latency[5m])", 0.01),
            ("quantile_over_time(0.99, latency[5m])", 0.01),
        ]);
        let original_count = original.groups().len();
        let expanded = expand(original.clone()).unwrap();
        assert!(!expanded.state_groups.is_empty());
        let order = expanded.memo.topological_order().unwrap();
        for state in &expanded.state_groups {
            let group = &expanded.memo.groups()[state.0 as usize];
            assert_eq!(group.queries, vec![0, 1]);
            for input in &group.child_groups {
                assert_eq!(expanded.memo.groups()[input.0 as usize].queries, vec![0, 1]);
                assert!(group.logical_node.children().iter().any(|node| Rc::ptr_eq(
                    node,
                    &expanded.memo.groups()[input.0 as usize].logical_node
                )));
            }
            assert_eq!(group.parents.len(), 2);
            assert!(!asap_types::stages::root_groups(&expanded.memo.query_roots).contains(state));
            for &parent in &group.parents {
                assert!(
                    order.iter().position(|id| id == state).unwrap()
                        < order.iter().position(|id| id == &parent).unwrap()
                );
                let reader = expanded.memo.candidates[parent.0 as usize]
                    .iter()
                    .find(|candidate| candidate.input_groups.contains(state))
                    .unwrap();
                assert!(OperatorNode::reachable(&reader.root_node)
                    .iter()
                    .any(|node| Rc::ptr_eq(node, &group.logical_node)));
                assert_eq!(reader.input_groups, vec![*state]);
            }
        }
        for group in 0..original_count {
            for (before, after) in original.candidates[group]
                .iter()
                .zip(&expanded.memo.candidates[group])
            {
                assert!(Rc::ptr_eq(&before.root_node, &after.root_node));
                assert_eq!(before.input_groups, after.input_groups);
            }
        }
    }

    // A stricter concrete configuration can serve both readers; local sizes remain.
    #[test]
    fn distinct_sizes_gain_compatible_shared_configurations() {
        let original = workload(&[
            ("quantile_over_time(0.95, latency[5m])", 0.01),
            ("quantile_over_time(0.99, latency[5m])", 0.02),
        ]);
        let expanded = expand(original.clone()).unwrap();
        assert!(!expanded.state_groups.is_empty());
        for (group, candidates) in original.candidates.iter().enumerate() {
            for candidate in candidates {
                assert!(expanded.memo.candidates[group]
                    .iter()
                    .any(|after| Rc::ptr_eq(&candidate.root_node, &after.root_node)));
            }
        }
        assert!(expanded
            .state_groups
            .iter()
            .any(|id| expanded.memo.groups()[id.0 as usize].queries == vec![0, 1]));
    }

    // Resizing never makes different source predicates compatible.
    #[test]
    fn distinct_filters_do_not_share_concrete_builds() {
        let original = workload(&[
            ("quantile_over_time(0.95, latency{job=\"a\"}[5m])", 0.01),
            ("quantile_over_time(0.99, latency{job=\"b\"}[5m])", 0.02),
        ]);
        let count = original.groups().len();
        let expanded = expand(original).unwrap();
        assert!(expanded.state_groups.is_empty());
        assert_eq!(expanded.memo.groups().len(), count);
    }

    fn with_local_tumbling_windows(mut memo: Memo, grids: &[u64]) -> Memo {
        use crate::pass2::asap_aware_cse::{
            tumbling_window_candidates, window_composition_pass2_candidates,
        };

        let boundaries: HashMap<_, _> = memo
            .groups()
            .iter()
            .enumerate()
            .map(|(id, group)| (Rc::as_ptr(&group.logical_node), MemoGroupId(id as u32)))
            .collect();
        // As local window composition does: Stage 0's groups only.
        for group in 0..memo.groups().len() {
            if !memo.is_stage0_group(MemoGroupId(group as u32)) {
                continue;
            }
            let grid = grids[memo.groups()[group].queries[0]];
            for candidate in memo.candidates[group].clone() {
                let roots = vec![QueryRoot::Operator(Rc::clone(&candidate.root_node))];
                let tumbling_windows = tumbling_window_candidates(&roots, &[Some(grid)]);
                if tumbling_windows.len() != 1 {
                    continue;
                }
                let rewritten = window_composition_pass2_candidates(&roots, &tumbling_windows)
                    .unwrap()
                    .remove(0)
                    .roots
                    .unwrap();
                let QueryRoot::Operator(node) = &rewritten[0] else {
                    unreachable!()
                };
                // Tumbling window rewriting recomposes guarantees; restore the original
                // canonical scan boundary rather than making it a private input.
                let input = tumbling_windows[0].build.children()[0].children()[0].clone();
                let chain = OperatorNode::reachable(node)
                    .into_iter()
                    .find(|node| {
                        matches!(node.asap(), Some(ASAPOp::SummaryTumblingWindowChain { .. }))
                    })
                    .unwrap();
                let node = replace_node(node, chain.children()[0], &input);
                let mut tumbling_window = candidate;
                tumbling_window.root_node = Rc::clone(&node);
                tumbling_window.input_groups = boundary_groups(&node, &boundaries);
                tumbling_window.origin = OriginOfCandidateForMemoGroup::TumblingWindows;
                memo.candidates[group].push(tumbling_window);
            }
        }
        memo
    }

    // Different label projections can consume one wider tumbling-window state through
    // typed reader projections; no original scan needs to contain the union.
    #[test]
    fn disjoint_scan_labels_share_a_union_tumbling_window_state_with_typed_readers() {
        let original = with_local_tumbling_windows(
            workload(&[
                ("avg by (job) (quantile_over_time(0.95, latency[5m]))", 0.01),
                (
                    "avg by (region) (quantile_over_time(0.99, latency[5m]))",
                    0.01,
                ),
            ]),
            &[60_000, 60_000],
        );
        assert!(!original.groups().iter().any(|group| matches!(
            group.logical_node.non_asap(),
            Some(NonASAPOp::Scan { .. })
        ) && group
            .logical_node
            .schema
            .fields
            .iter()
            .any(|field| field.name == "job")
            && group
                .logical_node
                .schema
                .fields
                .iter()
                .any(|field| field.name == "region")));
        let local_nodes: Vec<_> = original
            .candidates
            .iter()
            .map(|candidates| {
                candidates
                    .iter()
                    .map(|candidate| Rc::clone(&candidate.root_node))
                    .collect::<Vec<_>>()
            })
            .collect();
        let expanded = expand(original).unwrap();
        let shared = expanded
            .state_groups
            .iter()
            .find(|id| {
                let group = &expanded.memo.groups()[id.0 as usize];
                matches!(
                    group.logical_node.asap(),
                    Some(ASAPOp::SummaryTumblingWindowChain { .. })
                ) && group.queries == vec![0, 1]
                    && ["job", "region"].iter().all(|name| {
                        group
                            .logical_node
                            .schema
                            .fields
                            .iter()
                            .any(|field| field.name == *name)
                    })
            })
            .expect("one state over union labels");
        let state = &expanded.memo.groups()[shared.0 as usize];
        assert!(state.parents.len() >= 2);
        for parent in &state.parents {
            let reader = expanded.memo.candidates[parent.0 as usize]
                .iter()
                .find(|candidate| candidate.input_groups.contains(shared))
                .unwrap();
            assert_eq!(
                reader.root_node.schema.fields,
                expanded.memo.groups()[parent.0 as usize]
                    .logical_node
                    .schema
                    .fields
            );
            assert!(OperatorNode::reachable(&reader.root_node)
                .iter()
                .any(|node| matches!(node.non_asap(), Some(NonASAPOp::Project { .. }))));
            reader.root_node.validate_structure().unwrap();
        }
        for (group, locals) in local_nodes.into_iter().enumerate() {
            assert!(locals
                .iter()
                .zip(&expanded.memo.candidates[group])
                .all(|(before, after)| Rc::ptr_eq(before, &after.root_node)));
        }
        assert!(expanded.memo.topological_order().is_some());
        assert!(state
            .child_groups
            .iter()
            .all(|child| expanded.memo.groups()[child.0 as usize].queries == vec![0, 1]));
    }

    // Widening shifts identity's position but must retain its full typed field
    // and remap the source's unique key rather than treating it as a plain label.
    #[test]
    fn widened_tumbling_window_inputs_preserve_series_identity_and_keys() {
        let memo = with_local_tumbling_windows(
            workload_with_identity(
                &[
                    ("avg by (job) (quantile_over_time(0.95, latency[5m]))", 0.01),
                    (
                        "avg by (region) (quantile_over_time(0.99, latency[5m]))",
                        0.01,
                    ),
                ],
                true,
            ),
            &[60_000, 60_000],
        );
        let expanded = expand(memo).unwrap();
        let state = expanded
            .state_groups
            .iter()
            .map(|id| &expanded.memo.groups()[id.0 as usize])
            .find(|group| {
                matches!(
                    group.logical_node.asap(),
                    Some(ASAPOp::SummaryTumblingWindowChain { .. })
                ) && group.queries == vec![0, 1]
                    && ["job", "region"].iter().all(|name| {
                        group
                            .logical_node
                            .schema
                            .fields
                            .iter()
                            .any(|field| field.name == *name)
                    })
            })
            .expect("identity-carrying union tumbling-window state");
        let input = state.logical_node.children()[0];
        assert!(input.schema.has_promql_series_identity());
        let identity = input
            .schema
            .column_id(asap_types::ir::schema::PROMQL_SERIES_IDENTITY)
            .unwrap();
        let time = input.schema.time_index.unwrap();
        assert!(input
            .schema
            .unique_keys
            .iter()
            .any(|key| key.contains(&identity) && key.contains(&time)));
        for parent in &state.parents {
            let reader = expanded.memo.candidates[parent.0 as usize]
                .iter()
                .find(|candidate| {
                    candidate.input_groups.contains(&MemoGroupId(
                        expanded
                            .memo
                            .groups()
                            .iter()
                            .position(|group| Rc::ptr_eq(&group.logical_node, &state.logical_node))
                            .unwrap() as u32,
                    ))
                })
                .unwrap();
            assert_eq!(
                reader.root_node.schema,
                expanded.memo.groups()[parent.0 as usize]
                    .logical_node
                    .schema
            );
        }
    }

    /// The groups `group`'s candidates read, directly or through the
    /// candidates of the groups they read.
    fn groups_read_by(memo: &Memo, group: usize) -> HashSet<usize> {
        let mut read = HashSet::new();
        let mut pending = vec![group];
        while let Some(group) = pending.pop() {
            for input in memo.candidates[group]
                .iter()
                .flat_map(|candidate| &candidate.input_groups)
            {
                if read.insert(input.0 as usize) {
                    pending.push(input.0 as usize);
                }
            }
        }
        read
    }

    // A scan (or time range) that already has every label of its metric is
    // not widened: widening recomposes guarantees, and an unwidened root
    // that differs from the original only in its guarantee annotation must
    // not become an identity `Project` over its own group.
    #[test]
    fn scan_label_widening_never_reads_its_own_group() {
        for queries in [
            [
                ("sum by (job, region) (m)", 0.01),
                ("sum by (job) (m)", 0.01),
            ],
            [
                ("sum by (job, region) (rate(m[5m]))", 0.01),
                ("sum by (job) (rate(m[5m]))", 0.01),
            ],
        ] {
            let mut memo = workload_with_identity(&queries, true);
            crate::pass2::logical_memo_candidates::apply_sharing_rules(&mut memo).unwrap();
            assert!(
                memo.candidates.iter().flatten().any(
                    |candidate| candidate.origin == OriginOfCandidateForMemoGroup::WidenedScans
                ),
                "{queries:?}: the narrower scan is widened"
            );
            for group in 0..memo.groups().len() {
                assert!(
                    !groups_read_by(&memo, group).contains(&group),
                    "{queries:?}: group {group} reads itself: {:?}",
                    memo.candidates[group]
                        .iter()
                        .map(|candidate| (&candidate.origin, &candidate.input_groups))
                        .collect::<Vec<_>>()
                );
            }
            let expanded = expand(workload_with_identity(&queries, true)).unwrap();
            for group in 0..expanded.memo.groups().len() {
                assert!(!groups_read_by(&expanded.memo, group).contains(&group));
            }
        }
    }

    // A candidate reading its own group is a cycle the memo rejects.
    #[test]
    fn a_group_reading_itself_is_a_cyclic_memo() {
        let mut memo = workload(&[("sum by (job) (m)", 0.01)]);
        assert!(check_group_references_valid_and_acyclic(&memo).is_ok());
        let mut reader = memo.candidates[0][0].clone();
        reader.input_groups = vec![MemoGroupId(0)];
        memo.candidates[0].push(reader);
        assert_eq!(memo.topological_order(), None);
        assert!(matches!(
            check_group_references_valid_and_acyclic(&memo),
            Err(Pass2RuleError::CyclicDependencies)
        ));
    }

    // Label widening must not erase source predicates in order to create reuse.
    #[test]
    fn scan_label_widening_keeps_different_filters_separate() {
        let original = with_local_tumbling_windows(
            workload(&[
                (
                    "avg by (job) (quantile_over_time(0.95, latency{env=\"a\"}[5m]))",
                    0.01,
                ),
                (
                    "avg by (region) (quantile_over_time(0.99, latency{env=\"b\"}[5m]))",
                    0.01,
                ),
            ]),
            &[60_000, 60_000],
        );
        let expanded = expand(original).unwrap();
        assert!(expanded
            .state_groups
            .iter()
            .all(|id| expanded.memo.groups()[id.0 as usize].queries.len() == 1));
    }

    // Real tumbling window rewrites expose one identical chain to distinct quantile readers.
    #[test]
    fn identical_local_tumbling_window_chains_share_state_without_removing_local_choices() {
        let original = with_local_tumbling_windows(
            workload(&[
                ("quantile_over_time(0.95, latency[5m])", 0.01),
                ("quantile_over_time(0.99, latency[5m])", 0.01),
            ]),
            &[60_000, 60_000],
        );
        let local_tumbling_windows = original
            .candidates
            .iter()
            .flatten()
            .filter(|candidate| candidate.origin == OriginOfCandidateForMemoGroup::TumblingWindows)
            .count();
        assert!(local_tumbling_windows >= 2);
        let expanded = expand(original.clone()).unwrap();
        let chains: Vec<_> = expanded
            .state_groups
            .iter()
            .filter(|id| {
                matches!(
                    expanded.memo.groups()[id.0 as usize].logical_node.asap(),
                    Some(ASAPOp::SummaryTumblingWindowChain { .. })
                )
            })
            .collect();
        assert!(!chains.is_empty());
        for id in chains {
            let group = &expanded.memo.groups()[id.0 as usize];
            assert_eq!(group.queries, vec![0, 1]);
            assert_eq!(group.parents.len(), 2);
            assert!(group.child_groups.iter().all(|input| group
                .logical_node
                .children()
                .iter()
                .any(|node| Rc::ptr_eq(
                    node,
                    &expanded.memo.groups()[input.0 as usize].logical_node
                ))));
        }
        for (group, candidates) in original.candidates.iter().enumerate() {
            for original in candidates {
                assert!(expanded.memo.candidates[group]
                    .iter()
                    .any(|candidate| Rc::ptr_eq(&original.root_node, &candidate.root_node)));
            }
        }
    }

    // A finer grid and longer horizon serves a shorter reader through newest_tumbling_windows.
    #[test]
    fn different_tumbling_window_grids_and_horizons_gain_explicit_newest_tumbling_window_readers() {
        let original = with_local_tumbling_windows(
            workload(&[
                ("quantile_over_time(0.95, latency[5m])", 0.01),
                ("quantile_over_time(0.99, latency[15m])", 0.02),
            ]),
            &[60_000, 30_000],
        );
        let expanded = expand(original.clone()).unwrap();
        let state = expanded
            .state_groups
            .iter()
            .find(|id| {
                matches!(
                    expanded.memo.groups()[id.0 as usize].logical_node.asap(),
                    Some(ASAPOp::SummaryTumblingWindowChain {
                        tumbling_window_width_ms: 30_000,
                        tumbling_window_count: 30,
                        ..
                    })
                )
            })
            .expect("shared fine grid covering fifteen minutes");
        assert_eq!(expanded.memo.groups()[state.0 as usize].queries, vec![0, 1]);
        assert!(expanded.memo.candidates.iter().flatten().any(|reader| {
            reader.input_groups.contains(state)
                && OperatorNode::reachable(&reader.root_node)
                    .iter()
                    .any(|node| {
                        matches!(
                            node.asap(),
                            Some(ASAPOp::SummaryMerge {
                                newest_tumbling_windows: Some(10),
                                ..
                            })
                        )
                    })
        }));
        for (group, candidates) in original.candidates.iter().enumerate() {
            assert!(candidates
                .iter()
                .all(|before| expanded.memo.candidates[group]
                    .iter()
                    .any(|after| Rc::ptr_eq(&before.root_node, &after.root_node))));
        }
        assert!(expanded.memo.topological_order().is_some());
    }

    // Rollup readers preserve original outputs while referencing the finer
    // query's finalized values through an ordinary child-group dependency:
    // the finer query's own result group, whose result is those values.
    #[test]
    fn exact_rollup_exposes_shared_finalized_values() {
        let original = workload(&[
            ("sum by (job) (m)", 0.01),
            ("sum by (job, instance) (m)", 0.01),
        ]);
        let expanded = expand(original).unwrap();
        let memo = &expanded.memo;
        let fine = memo.query_roots[1].as_operator().unwrap().0 as usize;
        assert!(memo.candidates[fine].iter().all(|candidate| matches!(
            candidate.root_node.asap(),
            Some(ASAPOp::FinalizeExactAccumulator { .. })
        )));
        assert!(memo
            .candidates
            .iter()
            .enumerate()
            .filter(|(group, _)| *group != fine)
            .flat_map(|(_, candidates)| candidates)
            .any(
                |candidate| candidate.origin == OriginOfCandidateForMemoGroup::Rollup
                    && candidate.input_groups.contains(&MemoGroupId(fine as u32))
            ));
        assert!(memo.topological_order().is_some());
    }

    // Sketch rollup exposes the resized finer state through an ordinary group
    // dependency, while each output retains its own grouping and quantile.
    #[test]
    fn sketch_rollup_exposes_shared_finer_state() {
        let original = workload(&[
            ("quantile(0.9, m)", 0.01),
            ("quantile by (job) (0.9, m)", 0.02),
        ]);
        let expanded = expand(original).unwrap();
        assert!(expanded
            .memo
            .candidates
            .iter()
            .flatten()
            .any(
                |candidate| candidate.origin == OriginOfCandidateForMemoGroup::SharedBuild
                    && OperatorNode::reachable(&candidate.root_node)
                        .iter()
                        .any(|node| matches!(
                            node.asap(),
                            Some(ASAPOp::SummaryMerge {
                                group_by: Some(_),
                                ..
                            })
                        ))
            ));
        assert!(expanded.memo.topological_order().is_some());
    }

    // Different k outputs share one sorted ranking, backed by a recursively
    // exact candidate group rather than an unenforced exact-input annotation.
    #[test]
    fn topk_outputs_share_explicit_exact_ranking() {
        let original = workload(&[
            ("topk by (job) (3, sum_over_time(m[1m]))", 0.01),
            ("topk by (job) (5, sum_over_time(m[1m]))", 0.01),
        ]);
        let expanded = expand(original).unwrap();
        assert!(expanded.memo.groups().iter().any(|group| matches!(
            group.logical_node.non_asap(),
            Some(NonASAPOp::Limit { n: Some(5), .. })
        ) && group.parents.len() >= 2));
        assert!(expanded
            .memo
            .candidates
            .iter()
            .flatten()
            .any(|candidate| candidate.origin == OriginOfCandidateForMemoGroup::TopKReuse));
        assert!(expanded.memo.topological_order().is_some());
    }

    // Widened source-group candidates make shared scans available even to
    // ordinary operators that do not build an ASAP summary.
    #[test]
    fn general_source_groups_offer_typed_union_scan_readers() {
        let original = workload(&[
            ("topk by (job) (3, m)", 0.01),
            ("topk by (region) (5, m)", 0.01),
        ]);
        let expanded = expand(original.clone()).unwrap();
        let union = expanded
            .memo
            .groups()
            .iter()
            .enumerate()
            .find(|(_, group)| {
                matches!(group.logical_node.non_asap(), Some(NonASAPOp::Scan { .. }))
                    && ["job", "region"].iter().all(|name| {
                        group
                            .logical_node
                            .schema
                            .fields
                            .iter()
                            .any(|field| field.name == *name)
                    })
            })
            .map(|(id, _)| MemoGroupId(id as u32))
            .expect("union scan");
        let readers = original
            .groups()
            .iter()
            .enumerate()
            .filter(|(_, group)| {
                matches!(group.logical_node.non_asap(), Some(NonASAPOp::Scan { .. }))
            })
            .filter(|(group, _)| {
                expanded.memo.candidates[*group].iter().any(|candidate| {
                    candidate.origin == OriginOfCandidateForMemoGroup::WidenedScans
                        && candidate.input_groups.contains(&union)
                })
            })
            .count();
        assert!(readers >= 2);
    }

    // A composite average shares only its sum state with another query and
    // keeps its count state: Pass 1 adds each state as its own group.
    #[test]
    fn multiple_states_share_only_the_common_state() {
        let original = workload(&[("avg(m)", 0.01), ("sum(m)", 0.01)]);
        let expanded = expand(original).unwrap();
        let states_of = |queries: &[usize]| {
            expanded
                .memo
                .groups()
                .iter()
                .filter(|group| {
                    matches!(group.logical_node.asap(), Some(ASAPOp::SummaryAgg { .. }))
                        && group.queries == queries
                })
                .count()
        };
        assert!(
            states_of(&[0, 1]) >= 1,
            "sum state is reusable by both queries"
        );
        assert!(
            states_of(&[0]) >= 1,
            "count state belongs only to the average query"
        );
        assert!(expanded.memo.topological_order().is_some());
    }

    // A global SQL-window quantile can consume newest tumbling windows of a longer,
    // finer-grouped chain through the existing regrouping rule.
    #[test]
    fn grouped_window_rollup_exposes_one_finer_chain() {
        use crate::test_support::{agg, scan_from, time_range};
        use asap_types::ir::operator::{AggIntent, Source};
        use asap_types::ir::schema::{DataType, Field, Schema};
        let scan = || {
            scan_from(
                Source::Table {
                    table_ref: "metrics".into(),
                },
                Schema::with_time_index(
                    vec![
                        Field::plain("ts", DataType::Timestamp, false),
                        Field::plain("city", DataType::Utf8, true),
                        Field::plain("value", DataType::Float64, true),
                    ],
                    0,
                    vec![],
                ),
            )
        };
        let quantile = |q| AggIntent::Quantile {
            col: Some(2),
            q,
            accuracy: AccuracyTarget::max_error(0.01),
        };
        let roots = vec![
            QueryRoot::Operator(agg(
                vec![],
                quantile(0.5),
                time_range(std::time::Duration::from_secs(300), scan()),
            )),
            QueryRoot::Operator(agg(
                vec![1],
                quantile(0.9),
                time_range(std::time::Duration::from_secs(1200), scan()),
            )),
        ];
        let initial = generate_dag_of_logical_memo_groups(&roots);
        let generated =
            generate_per_computation_candidates(&generate_dag_of_logical_memo_groups(&roots))
                .unwrap();
        let memo = with_local_tumbling_windows(
            try_add_pass1_candidates(initial, &generated, &[], &Default::default()).unwrap(),
            &[10_000, 10_000],
        );
        let expanded = expand(memo).unwrap();
        assert!(expanded
            .memo
            .candidates
            .iter()
            .flatten()
            .any(
                |candidate| candidate.origin == OriginOfCandidateForMemoGroup::SharedBuild
                    && OperatorNode::reachable(&candidate.root_node)
                        .iter()
                        .any(|node| matches!(
                            node.asap(),
                            Some(ASAPOp::SummaryMerge {
                                group_by: Some(_),
                                ..
                            })
                        ))
                    && candidate.input_groups.iter().any(|group| matches!(
                        expanded.memo.groups()[group.0 as usize].logical_node.asap(),
                        Some(ASAPOp::SummaryTumblingWindowChain {
                            tumbling_window_count: 120,
                            ..
                        })
                    ))
            ));
    }

    // Private identity projections retain their full typed contract, but no
    // longer hide compatible builds from canonical state sharing.
    #[test]
    fn private_projection_inputs_gain_canonical_shared_states() {
        check_private_inputs(false);
    }

    // Key-only HAVING uses private Filter nodes below summary builds; equal
    // filters can share without dropping their predicates.
    #[test]
    fn private_filter_inputs_gain_canonical_shared_states() {
        check_private_inputs(true);
    }

    fn check_private_inputs(filter: bool) {
        use asap_types::ir::scalar::ScalarValue;
        use asap_types::ir::{Operator, Predicate, ProjectItem, ScalarExpr};
        // Pass 1 adds its builds as groups; the local tumbling-window
        // candidates keep private builds.
        let mut memo = with_local_tumbling_windows(
            workload(&[
                ("quantile_over_time(0.95, latency[5m])", 0.01),
                ("quantile_over_time(0.99, latency[5m])", 0.01),
            ]),
            &[60_000, 60_000],
        );
        let mut injected = 0;
        for owner in 0..memo.candidates.len() {
            let originals = memo.candidates[owner].clone();
            for mut candidate in originals {
                let builds = direct_input_builds(&memo, &candidate);
                if builds.is_empty() {
                    continue;
                }
                let mut replacements = HashMap::new();
                for (build, _) in builds {
                    let input = build.children()[0];
                    let project = OperatorNode::new_shared(Operator::NonASAP(NonASAPOp::Project {
                        cols: input
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
                        child: Rc::clone(input),
                    }))
                    .unwrap();
                    let project = if filter {
                        OperatorNode::new_shared(Operator::NonASAP(NonASAPOp::Filter {
                            pred: Predicate(ScalarExpr::Literal(ScalarValue::Boolean(true))),
                            child: project,
                        }))
                        .unwrap()
                    } else {
                        project
                    };
                    replacements.insert(Rc::as_ptr(input), project);
                }
                candidate.root_node =
                    crate::pass2::memo::rules::reuse::replace(&candidate.root_node, &replacements);
                assert!(direct_input_builds(&memo, &candidate).is_empty());
                memo.candidates[owner].push(candidate);
                injected += 1;
            }
        }
        assert!(injected >= 2);
        let expanded = expand(memo).unwrap();
        assert!(expanded.state_groups.iter().any(|state| {
            let group = &expanded.memo.groups()[state.0 as usize];
            group.queries == vec![0, 1]
                && group.child_groups.iter().any(|input| {
                    expanded.memo.candidates[input.0 as usize]
                        .iter()
                        .any(|candidate| {
                            candidate.origin == OriginOfCandidateForMemoGroup::SummaryInput
                        })
                })
        }));
        assert!(expanded.memo.topological_order().is_some());
    }

    // Real identity-aware frequency candidates retain UnivMon inputs and their
    // error metadata when private preprocessing is made an explicit boundary.
    #[test]
    fn univmon_identity_inputs_remain_admissible_after_factoring() {
        use asap_types::ir::schema::{sketch::SketchAlgorithm, FieldDataType};
        let mut memo = workload_with_identity(&[("count_over_time(latency[5m])", 0.01)], true);
        let is_univmon = |node: &OperatorNode| matches!(node.asap(), Some(ASAPOp::SummaryAgg {family: FieldDataType::Sketch(kind, _), ..}) if kind.algorithm() == &SketchAlgorithm::UnivMon);
        let before: Vec<_> = memo
            .candidates
            .iter()
            .flatten()
            .flat_map(|candidate| OperatorNode::reachable(&candidate.root_node))
            .filter(|node| is_univmon(node))
            .collect();
        assert!(!before.is_empty(), "real generation must offer UnivMon");
        crate::pass2::memo::rules::summary_inputs::factor_summary_inputs(&mut memo).unwrap();
        // Pass 1 added the UnivMon build as its own group.
        assert!(memo
            .groups()
            .iter()
            .any(|group| is_univmon(&group.logical_node)));
        for original in before {
            assert!(
                memo.candidates
                    .iter()
                    .flatten()
                    .flat_map(|candidate| OperatorNode::reachable(&candidate.root_node))
                    .any(|node| Rc::ptr_eq(&original, &node)),
                "local generated candidates remain intact"
            );
        }
    }

    // Reader deduplication must retain distinct recorded probability contracts
    // even though SketchParams structural equality intentionally ignores them.
    #[test]
    fn same_capacity_readers_retain_distinct_failure_probability_contracts() {
        use asap_types::ir::schema::{
            sketch::{SketchAlgorithm, SketchKind, SketchParams},
            FieldDataType,
        };
        let mut memo = workload(&[("quantile_over_time(0.95, latency[5m])", 0.01)]);
        let (owner, mut base, build) = memo.candidates.iter().enumerate().find_map(|(owner, candidates)| candidates.iter().find_map(|candidate| {
            let (build, BuildPlace::InputGroup(_)) = build_of(&memo, candidate)? else { return None };
            matches!(build.asap(), Some(ASAPOp::SummaryAgg {family: FieldDataType::Sketch(kind, _), ..}) if kind.algorithm() == &SketchAlgorithm::Kll).then(|| (owner, candidate.clone(), build))
        })).unwrap();
        let Some(ASAPOp::SummaryAgg {
            family: FieldDataType::Sketch(kind, grouping),
            ..
        }) = build.asap()
        else {
            unreachable!()
        };
        let SketchParams::Kll { k, .. } = kind.params() else {
            unreachable!()
        };
        let boundaries: HashSet<_> = base
            .input_groups
            .iter()
            .map(|id| Rc::as_ptr(&memo.groups()[id.0 as usize].logical_node))
            .collect();
        fn without_private_guarantees(
            node: &Rc<OperatorNode>,
            boundaries: &HashSet<*const OperatorNode>,
        ) -> Rc<OperatorNode> {
            if boundaries.contains(&Rc::as_ptr(node)) {
                return Rc::clone(node);
            }
            Rc::new(OperatorNode {
                operator: node
                    .operator
                    .map_children(|child| without_private_guarantees(child, boundaries)),
                guarantee: None,
                ..(**node).clone()
            })
        }
        let rewritten = |probability| {
            let family = FieldDataType::Sketch(
                SketchKind::new(
                    SketchAlgorithm::Kll,
                    SketchParams::Kll {
                        k: *k,
                        failure_probability: Some(probability),
                    },
                ),
                grouping.clone(),
            );
            let roots = crate::pass2::asap_aware_cse::with_resized_build(
                &[QueryRoot::Operator(Rc::clone(&base.root_node))],
                &build,
                family,
            )
            .unwrap();
            let QueryRoot::Operator(root) = &roots[0] else {
                unreachable!()
            };
            let root =
                crate::pass2::memo::rules::reuse::restore_inputs(root, &memo, &base.input_groups);
            without_private_guarantees(&root, &boundaries)
        };
        let weak = rewritten(0.001);
        let strict = rewritten(0.0001);
        assert_eq!(
            *weak, *strict,
            "ordinary equality omits the probability metadata"
        );
        base.root_node = weak;
        memo.candidates[owner] = vec![base.clone()];
        assert!(crate::pass2::memo::rules::reuse::append_reader(
            &mut memo,
            owner,
            &base,
            strict,
            OriginOfCandidateForMemoGroup::Rollup
        )
        .unwrap());
        assert_eq!(memo.candidates[owner].len(), 2);
    }

    // Equal private producers can be shared within one composite result; they
    // do not need to originate from different memo owner groups.
    #[test]
    fn repeated_states_within_one_owner_share_a_producer() {
        // A local tumbling-window candidate keeps its build private.
        let mut memo = with_local_tumbling_windows(
            workload(&[("quantile_over_time(0.95, latency[5m])", 0.01)]),
            &[60_000],
        );
        let (owner, mut candidate) = memo
            .candidates
            .iter()
            .enumerate()
            .find_map(|(group, candidates)| {
                candidates
                    .iter()
                    .find(|candidate| {
                        matches!(build_of(&memo, candidate), Some((_, BuildPlace::Private)))
                    })
                    .map(|candidate| (group, candidate.clone()))
            })
            .unwrap();
        let boundaries: HashSet<_> = candidate
            .input_groups
            .iter()
            .map(|id| Rc::as_ptr(&memo.groups()[id.0 as usize].logical_node))
            .collect();
        fn duplicate(
            node: &Rc<OperatorNode>,
            boundaries: &HashSet<*const OperatorNode>,
        ) -> Rc<OperatorNode> {
            if boundaries.contains(&Rc::as_ptr(node)) {
                return Rc::clone(node);
            }
            Rc::new(OperatorNode {
                operator: node
                    .operator
                    .map_children(|child| duplicate(child, boundaries)),
                ..(**node).clone()
            })
        }
        let second = duplicate(&candidate.root_node, &boundaries);
        let combined =
            OperatorNode::new_shared(asap_types::ir::Operator::NonASAP(NonASAPOp::Concat {
                children: vec![Rc::clone(&candidate.root_node), second],
            }))
            .unwrap();
        candidate.root_node = Rc::clone(&combined);
        memo.groups_mut()[owner].logical_node = combined;
        memo.candidates[owner] = vec![candidate];
        let expanded = expand(memo).unwrap();
        assert!(expanded.state_groups.iter().any(|state| {
            expanded.memo.groups()[state.0 as usize]
                .parents
                .contains(&MemoGroupId(owner as u32))
                && expanded.memo.candidates[owner].iter().any(|candidate| {
                    candidate.origin == OriginOfCandidateForMemoGroup::SharedBuild
                        && candidate.input_groups.contains(state)
                })
        }));
    }

    /// A private build equal to a build group is read from that group, and
    /// the group's own candidate is sized for the most budget parts of its
    /// readers.
    #[test]
    fn a_private_build_equal_to_a_group_raises_its_budget_parts() {
        let mut memo = workload(&[("quantile_over_time(0.95, latency[5m])", 0.01)]);
        let (build_group, estimate_group) = (0..memo.groups().len())
            .find_map(|group| {
                let [own] = memo.candidates[group].as_slice() else {
                    return None;
                };
                let (_, BuildPlace::InputGroup(build)) = build_of(&memo, own)? else {
                    return None;
                };
                matches!(
                    memo.groups()[build.0 as usize].logical_node.asap(),
                    Some(ASAPOp::SummaryAgg { .. })
                )
                .then_some((build, group))
            })
            .expect("an estimate over a build group");
        assert_eq!(
            memo.candidates[build_group.0 as usize][0].accuracy_budget_parts,
            1
        );
        // The estimate over a private copy of the build, sized for 2 parts.
        let build_node = Rc::clone(&memo.groups()[build_group.0 as usize].logical_node);
        let private_build = Rc::new((*build_node).clone());
        let mut reader = memo.candidates[estimate_group][0].clone();
        reader.root_node = replace_node(&reader.root_node, &build_node, &private_build);
        reader.input_groups = memo.groups()[build_group.0 as usize].child_groups.clone();
        reader.accuracy_budget_parts = 2;
        memo.candidates[estimate_group].push(reader);
        add_shared_summary_state_groups(&mut memo).unwrap();
        assert_eq!(
            memo.candidates[build_group.0 as usize][0].accuracy_budget_parts,
            2
        );
        assert!(memo.candidates[estimate_group].iter().any(|candidate| {
            candidate.origin == OriginOfCandidateForMemoGroup::SharedBuild
                && candidate.input_groups == [build_group]
        }));
    }

    // Public expansion rejects malformed IDs before indexing dependency arrays.
    #[test]
    fn malformed_references_fail_before_dependency_traversal() {
        let mut memo = workload(&[("quantile_over_time(0.95, latency[5m])", 0.01)]);
        memo.groups_mut()[0]
            .child_groups
            .push(MemoGroupId(u32::MAX));
        assert!(matches!(
            expand(memo),
            Err(Pass2RuleError::InvalidGroupReference)
        ));
    }
}

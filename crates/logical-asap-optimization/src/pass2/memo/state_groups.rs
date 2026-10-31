//! Opt-in concrete summary-state groups for the candidate-connection planner.
//!
//! Simple summary builds and pane chains directly over canonical input groups
//! gain finite compatible sizes and pane horizons before exact state interning.
//! Concrete state identity includes capacity, guarantee, update/filter semantics,
//! and materialization. Supported private projections/filters gain explicit input groups.

use super::check_generation_deadline;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::rc::Rc;
use std::time::Instant;

use asap_types::ir::{ASAPOp, NonASAPOp, OperatorNode};

use asap_types::stages::{CandidateForMemoGroup, MemoCandidates, OriginOfCandidateForMemoGroup};
use asap_types::stages::{MemoGroup, MemoGroupId};

#[derive(Debug, thiserror::Error)]
pub enum StateGroupExpansionError {
    #[error(transparent)]
    DeadlineExceeded(#[from] asap_types::stages::CandidateGenerationTimeout),
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
    #[error("scan-label candidate expansion: {0}")]
    ScanLabelWidening(String),
    #[error("state configuration expansion: {0}")]
    StateConfiguration(String),
    #[error("producer reuse rule: {0}")]
    ReuseRule(String),
}

struct BuildReaders {
    build: Rc<OperatorNode>,
    children: Vec<MemoGroupId>,
    readers: Vec<(usize, usize, Rc<OperatorNode>)>,
}

/// Rejects a memo the Pass 2 rules cannot extend: mismatched or
/// out-of-range group references, shared summary-state groups already
/// added, or cyclic dependencies. Pass 2 checks this before its rules.
pub(crate) fn check_unexpanded_memo(memo: &MemoCandidates) -> Result<(), StateGroupExpansionError> {
    validate_references(memo)?;
    if memo.groups.iter().any(|group| {
        matches!(
            group.logical_node.asap(),
            Some(ASAPOp::SummaryAgg { .. } | ASAPOp::PaneChain { .. })
        )
    }) {
        return Err(StateGroupExpansionError::AlreadyExpanded);
    }
    if memo.topological_order().is_none() {
        return Err(StateGroupExpansionError::CyclicDependencies);
    }
    Ok(())
}

/// Pass 2's last step, after its rules: give each summary build that two or
/// more candidates make identically its own group, and add to each reader
/// group a candidate reading that group. Every existing candidate stays
/// (unsupported candidate shapes unchanged). Then rebuilds every group's
/// parents and the queries it may serve. On error, discard `memo`: it may
/// hold partial work.
pub(crate) fn add_shared_summary_state_groups(
    memo: &mut MemoCandidates,
    deadline: Option<Instant>,
) -> Result<(), StateGroupExpansionError> {
    add_shared_summary_state_groups_with_index(memo, deadline, true)
}

/// `indexed` looks builds up by fingerprint; otherwise by a linear scan
/// (the reference the index is tested against).
fn add_shared_summary_state_groups_with_index(
    memo: &mut MemoCandidates,
    deadline: Option<Instant>,
    indexed: bool,
) -> Result<(), StateGroupExpansionError> {
    let mut builds: Vec<BuildReaders> = Vec::new();
    let mut build_index: HashMap<Option<BuildFingerprint>, Vec<usize>> = HashMap::new();
    for (group, candidates) in memo.candidates.iter().enumerate() {
        check_generation_deadline(deadline)?;
        for (index, candidate) in candidates.iter().enumerate() {
            check_generation_deadline(deadline)?;
            for (build, children) in direct_input_builds(memo, candidate) {
                check_generation_deadline(deadline)?;
                let fingerprint = build_fingerprint(&build);
                let bucket = build_index.entry(fingerprint).or_default();
                let matching = if indexed {
                    bucket
                        .iter()
                        .copied()
                        .find(|&index| same_concrete_build(&builds[index].build, &build))
                } else {
                    builds
                        .iter()
                        .position(|entry| same_concrete_build(&entry.build, &build))
                };
                if let Some(build_index) = matching {
                    builds[build_index].readers.push((group, index, build));
                } else {
                    bucket.push(builds.len());
                    builds.push(BuildReaders {
                        build: Rc::clone(&build),
                        children,
                        readers: vec![(group, index, build)],
                    });
                }
            }
        }
    }
    let mut state_groups = Vec::new();
    let mut replacements: BTreeMap<(usize, usize), HashMap<*const OperatorNode, Rc<OperatorNode>>> =
        BTreeMap::new();
    for build in builds {
        check_generation_deadline(deadline)?;
        // Repeated private builds within one result are reusable too. The
        // reader list records concrete build occurrences, not just owners.
        if build.readers.len() < 2 {
            continue;
        }
        let state_group = MemoGroupId(
            u32::try_from(memo.groups.len())
                .map_err(|_| StateGroupExpansionError::TooManyGroups)?,
        );
        let mut queries: Vec<_> = build
            .readers
            .iter()
            .flat_map(|(group, _, _)| memo.groups[*group].queries.iter().copied())
            .collect();
        queries.sort_unstable();
        queries.dedup();
        memo.groups.push(MemoGroup {
            logical_node: Rc::clone(&build.build),
            child_groups: build.children.clone(),
            parents: Vec::new(),
            queries,
        });
        // The build is its readers' own build, so it is sized for their
        // shares of the budget (equal builds have equal sizes).
        let accuracy_budget_parts = build
            .readers
            .iter()
            .map(|(group, index, _)| memo.candidates[*group][*index].accuracy_budget_parts)
            .max()
            .unwrap_or(1);
        memo.candidates.push(vec![CandidateForMemoGroup {
            fused_groups: Vec::new(),
            root_node: Rc::clone(&build.build),
            input_groups: build.children,
            origin: OriginOfCandidateForMemoGroup::SharedBuild,
            accuracy_budget_parts,
        }]);
        for (group, index, own_build) in build.readers {
            check_generation_deadline(deadline)?;
            replacements
                .entry((group, index))
                .or_default()
                .insert(Rc::as_ptr(&own_build), Rc::clone(&build.build));
        }
        state_groups.push(state_group);
    }
    for ((group, index), replacements) in replacements {
        check_generation_deadline(deadline)?;
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
            .map(|id| (Rc::as_ptr(&memo.groups[id.0 as usize].logical_node), *id))
            .collect();
        reader.root_node = super::rules::reuse::replace(&reader.root_node, &replacements);
        reader.input_groups = boundary_groups(&reader.root_node, &boundaries);
        reader.origin = OriginOfCandidateForMemoGroup::SharedBuild;
        memo.candidates[group].push(reader);
    }
    // Parents describe every possible group/candidate dependency, not a selected plan.
    for group in &mut memo.groups {
        check_generation_deadline(deadline)?;
        group.parents.clear();
    }
    for group in 0..memo.groups.len() {
        check_generation_deadline(deadline)?;
        let dependencies: HashSet<_> = memo.groups[group]
            .child_groups
            .iter()
            .chain(
                memo.candidates[group]
                    .iter()
                    .flat_map(|candidate| &candidate.input_groups),
            )
            .copied()
            .collect();
        for child in dependencies {
            check_generation_deadline(deadline)?;
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
            .child_groups
            .iter()
            .chain(
                memo.candidates[group.0 as usize]
                    .iter()
                    .flat_map(|candidate| &candidate.input_groups),
            )
            .copied()
            .collect();
        for child in children {
            check_generation_deadline(deadline)?;
            memo.groups[child.0 as usize].queries.extend(&queries);
            memo.groups[child.0 as usize].queries.sort_unstable();
            memo.groups[child.0 as usize].queries.dedup();
        }
    }
    check_generation_deadline(deadline)?;
    Ok(())
}

fn validate_references(memo: &MemoCandidates) -> Result<(), StateGroupExpansionError> {
    if memo.groups.len() != memo.candidates.len() {
        return Err(StateGroupExpansionError::MismatchedGroups);
    }
    if memo.groups.len() > u32::MAX as usize {
        return Err(StateGroupExpansionError::TooManyGroups);
    }
    for group in &memo.groups {
        if group
            .child_groups
            .iter()
            .chain(&group.parents)
            .any(|id| id.0 as usize >= memo.groups.len())
        {
            return Err(StateGroupExpansionError::InvalidGroupReference);
        }
    }
    for candidate in memo.candidates.iter().flatten() {
        if candidate
            .input_groups
            .iter()
            .chain(&candidate.fused_groups)
            .any(|id| id.0 as usize >= memo.groups.len())
        {
            return Err(StateGroupExpansionError::InvalidGroupReference);
        }
    }
    Ok(())
}

pub(super) fn simple_build(
    memo: &MemoCandidates,
    candidate: &CandidateForMemoGroup,
) -> Option<(Rc<OperatorNode>, Vec<MemoGroupId>)> {
    let mut builds = direct_input_builds(memo, candidate);
    (builds.len() == 1).then(|| builds.remove(0))
}

pub(super) fn direct_input_builds(
    memo: &MemoCandidates,
    candidate: &CandidateForMemoGroup,
) -> Vec<(Rc<OperatorNode>, Vec<MemoGroupId>)> {
    if !matches!(
        candidate.origin,
        OriginOfCandidateForMemoGroup::Pass1
            | OriginOfCandidateForMemoGroup::Panes
            | OriginOfCandidateForMemoGroup::WidenedScans
            | OriginOfCandidateForMemoGroup::Rollup
            | OriginOfCandidateForMemoGroup::SummaryResult
    ) || candidate.accuracy_budget_parts == 0
    {
        return Vec::new();
    }
    let boundaries: HashMap<_, _> = candidate
        .input_groups
        .iter()
        .map(|&group| {
            (
                Rc::as_ptr(&memo.groups[group.0 as usize].logical_node),
                group,
            )
        })
        .collect();
    let mut stack = vec![Rc::clone(&candidate.root_node)];
    let mut seen = HashSet::new();
    let mut builds = Vec::new();
    while let Some(node) = stack.pop() {
        if boundaries.contains_key(&Rc::as_ptr(&node)) || !seen.insert(Rc::as_ptr(&node)) {
            continue;
        }
        if !Rc::ptr_eq(&node, &candidate.root_node)
            && matches!(
                node.asap(),
                Some(ASAPOp::SummaryAgg { .. } | ASAPOp::PaneChain { .. })
            )
        {
            let children: Option<Vec<_>> = node
                .children()
                .iter()
                .map(|child| boundaries.get(&Rc::as_ptr(child)).copied())
                .collect();
            if let Some(mut children) = children {
                children.dedup();
                builds.push((Rc::clone(&node), children));
            }
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

// Hashing narrows comparisons only; full strict equality below remains the
// authority, including probability metadata and floating-point corner cases.
#[derive(Hash, PartialEq, Eq)]
struct BuildFingerprint {
    local: String,
    inputs: Vec<(usize, Option<String>)>,
}

fn local_fingerprint(node: &OperatorNode) -> Option<String> {
    serde_json::to_string(&(
        node.operator.map_children(|_| ()),
        &node.schema,
        node.result_kind,
        &node.guarantee,
        node.timing,
        node.materialization,
    ))
    .ok()
}

fn build_fingerprint(node: &OperatorNode) -> Option<BuildFingerprint> {
    let inputs = node
        .children()
        .into_iter()
        .map(|input| {
            match input.non_asap() {
                Some(NonASAPOp::TimeRange { child, .. }) => {
                    // Exactly the existing same_range_input contract: identical
                    // local range semantics over the same canonical source.
                    Some((Rc::as_ptr(child) as usize, Some(local_fingerprint(input)?)))
                }
                _ => Some((Rc::as_ptr(input) as usize, None)),
            }
        })
        .collect::<Option<Vec<_>>>()?;
    Some(BuildFingerprint {
        local: local_fingerprint(node)?,
        inputs,
    })
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
    use crate::pass1::per_computation_candidates::generate_per_computation_candidates;
    use crate::pass2::memo::generate_dag_of_logical_memo_groups;
    use crate::pass2::memo::groups::try_populate_pass1;
    use asap_types::ir::properties::AccuracyTarget;
    use asap_types::ir::QueryRoot;

    /// An expanded memo with what its shared summary-state step added.
    struct Expanded {
        memo: MemoCandidates,
        /// The groups the shared summary-state step added.
        state_groups: Vec<MemoGroupId>,
        /// The candidates it added to reader groups.
        added_reader_candidates: usize,
    }

    /// Pass 2 after local window composition, over the complete pane-width
    /// domain, as the Pass 2 entry runs it.
    fn expand(memo: MemoCandidates) -> Result<Expanded, StateGroupExpansionError> {
        let memo = crate::test_support::sharing_rules_and_shared_state_groups(
            memo,
            super::super::shared_configurations::PaneWidthDomain::Complete,
        )?;
        Ok(expanded(memo))
    }

    fn expanded(memo: MemoCandidates) -> Expanded {
        let state_groups = crate::test_support::shared_summary_state_groups(&memo);
        // Before this step no candidate is a shared build; after it, each
        // state group has one and each added reader candidate is one.
        let shared_builds = memo
            .candidates
            .iter()
            .flatten()
            .filter(|candidate| candidate.origin == OriginOfCandidateForMemoGroup::SharedBuild)
            .count();
        Expanded {
            added_reader_candidates: shared_builds - state_groups.len(),
            state_groups,
            memo,
        }
    }

    // Hash indexing is only an acceleration: independently run the original
    // linear state lookup and compare every generated candidate and state.
    #[test]
    fn indexed_state_interning_preserves_linear_catalog() {
        let memo = with_local_panes(
            workload(&[
                ("quantile_over_time(0.95, latency[5m])", 0.01),
                ("quantile_over_time(0.99, latency[15m])", 0.02),
                ("quantile_over_time(0.9, latency{job=\"other\"}[5m])", 0.01),
            ]),
            &[6, 6, 6],
        );
        let domain = super::super::shared_configurations::PaneWidthDomain::Complete;
        let mut memo = memo;
        crate::pass2::logical_memo_candidates::apply_sharing_rules(&mut memo, domain, None)
            .unwrap();
        let mut linear = memo.clone();
        let mut indexed = memo;
        add_shared_summary_state_groups_with_index(&mut indexed, None, true).unwrap();
        add_shared_summary_state_groups_with_index(&mut linear, None, false).unwrap();
        let (indexed, linear) = (expanded(indexed), expanded(linear));
        assert_eq!(indexed.state_groups, linear.state_groups);
        assert_eq!(indexed.memo.groups.len(), linear.memo.groups.len());
        for (left, right) in indexed.memo.candidates.iter().zip(&linear.memo.candidates) {
            let signatures = |candidates: &[CandidateForMemoGroup]| {
                candidates
                    .iter()
                    .map(|candidate| {
                        (
                            candidate.input_groups.clone(),
                            candidate.accuracy_budget_parts,
                            serde_json::to_string(
                                &asap_types::ir::flat::flatten(&[QueryRoot::Operator(Rc::clone(
                                    &candidate.root_node,
                                ))])
                                .0,
                            )
                            .unwrap(),
                        )
                    })
                    .collect::<Vec<_>>()
            };
            assert_eq!(signatures(left), signatures(right));
        }
        for (left, right) in indexed.memo.groups.iter().zip(&linear.memo.groups) {
            assert_eq!(left.child_groups, right.child_groups);
            assert_eq!(left.parents, right.parents);
            assert_eq!(left.queries, right.queries);
        }
    }

    // State expansion returns a typed timeout, not a partial success that a
    // solver could incorrectly report as optimal over the complete catalog.
    #[test]
    fn expired_state_expansion_returns_timeout() {
        let mut memo = workload(&[("quantile_over_time(0.95, latency[5m])", 0.01)]);
        assert!(matches!(
            crate::pass2::logical_memo_candidates::apply_sharing_rules(
                &mut memo,
                super::super::shared_configurations::PaneWidthDomain::Complete,
                Some(Instant::now())
            ),
            Err(StateGroupExpansionError::DeadlineExceeded(_))
        ));
    }

    fn workload(queries: &[(&str, f64)]) -> MemoCandidates {
        workload_with_identity(queries, false)
    }

    fn workload_with_identity(queries: &[(&str, f64)], identity: bool) -> MemoCandidates {
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
        let expanded = expand(original.clone()).unwrap();
        assert!(!expanded.state_groups.is_empty());
        assert!(expanded.added_reader_candidates >= 2);
        let order = expanded.memo.topological_order().unwrap();
        for state in &expanded.state_groups {
            let group = &expanded.memo.groups[state.0 as usize];
            assert_eq!(group.queries, vec![0, 1]);
            for input in &group.child_groups {
                assert_eq!(expanded.memo.groups[input.0 as usize].queries, vec![0, 1]);
                assert!(group.logical_node.children().iter().any(|node| Rc::ptr_eq(
                    node,
                    &expanded.memo.groups[input.0 as usize].logical_node
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
        assert!(matches!(
            expand(expanded.memo),
            Err(StateGroupExpansionError::AlreadyExpanded)
        ));
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
            .any(|id| expanded.memo.groups[id.0 as usize].queries == vec![0, 1]));
    }

    // Resizing never makes different source predicates compatible.
    #[test]
    fn distinct_filters_do_not_share_concrete_builds() {
        let original = workload(&[
            ("quantile_over_time(0.95, latency{job=\"a\"}[5m])", 0.01),
            ("quantile_over_time(0.99, latency{job=\"b\"}[5m])", 0.02),
        ]);
        let count = original.groups.len();
        let expanded = expand(original).unwrap();
        assert!(expanded.state_groups.is_empty());
        assert_eq!(expanded.memo.groups.len(), count);
    }

    fn with_local_panes(mut memo: MemoCandidates, grids: &[u64]) -> MemoCandidates {
        use crate::pass2::asap_aware_cse::{pane_candidates, window_composition_pass2_candidates};

        let boundaries: HashMap<_, _> = memo
            .groups
            .iter()
            .enumerate()
            .map(|(id, group)| (Rc::as_ptr(&group.logical_node), MemoGroupId(id as u32)))
            .collect();
        for group in 0..memo.groups.len() {
            let grid = grids[memo.groups[group].queries[0]];
            for candidate in memo.candidates[group].clone() {
                let roots = vec![QueryRoot::Operator(Rc::clone(&candidate.root_node))];
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
                pane.root_node = Rc::clone(&node);
                pane.input_groups = boundary_groups(&node, &boundaries);
                pane.origin = OriginOfCandidateForMemoGroup::Panes;
                memo.candidates[group].push(pane);
            }
        }
        memo
    }

    // Different label projections can consume one wider pane state through
    // typed reader projections; no original scan needs to contain the union.
    #[test]
    fn disjoint_scan_labels_share_a_union_pane_state_with_typed_readers() {
        let original = with_local_panes(
            workload(&[
                ("avg by (job) (quantile_over_time(0.95, latency[5m]))", 0.01),
                (
                    "avg by (region) (quantile_over_time(0.99, latency[5m]))",
                    0.01,
                ),
            ]),
            &[60_000, 60_000],
        );
        assert!(!original.groups.iter().any(|group| matches!(
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
                let group = &expanded.memo.groups[id.0 as usize];
                matches!(group.logical_node.asap(), Some(ASAPOp::PaneChain { .. }))
                    && group.queries == vec![0, 1]
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
        let state = &expanded.memo.groups[shared.0 as usize];
        assert!(state.parents.len() >= 2);
        for parent in &state.parents {
            let reader = expanded.memo.candidates[parent.0 as usize]
                .iter()
                .find(|candidate| candidate.input_groups.contains(shared))
                .unwrap();
            assert_eq!(
                reader.root_node.schema.fields,
                expanded.memo.groups[parent.0 as usize]
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
            .all(|child| expanded.memo.groups[child.0 as usize].queries == vec![0, 1]));
    }

    // Widening shifts identity's position but must retain its full typed field
    // and remap the source's unique key rather than treating it as a plain label.
    #[test]
    fn widened_pane_inputs_preserve_series_identity_and_keys() {
        let memo = with_local_panes(
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
            .map(|id| &expanded.memo.groups[id.0 as usize])
            .find(|group| {
                matches!(group.logical_node.asap(), Some(ASAPOp::PaneChain { .. }))
                    && group.queries == vec![0, 1]
                    && ["job", "region"].iter().all(|name| {
                        group
                            .logical_node
                            .schema
                            .fields
                            .iter()
                            .any(|field| field.name == *name)
                    })
            })
            .expect("identity-carrying union pane state");
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
                            .groups
                            .iter()
                            .position(|group| Rc::ptr_eq(&group.logical_node, &state.logical_node))
                            .unwrap() as u32,
                    ))
                })
                .unwrap();
            assert_eq!(
                reader.root_node.schema,
                expanded.memo.groups[parent.0 as usize].logical_node.schema
            );
        }
    }

    /// The groups `group`'s candidates read, directly or through the
    /// candidates of the groups they read.
    fn groups_read_by(memo: &MemoCandidates, group: usize) -> HashSet<usize> {
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
            crate::pass2::logical_memo_candidates::apply_sharing_rules(
                &mut memo,
                super::super::shared_configurations::PaneWidthDomain::Complete,
                None,
            )
            .unwrap();
            assert!(
                memo.candidates.iter().flatten().any(
                    |candidate| candidate.origin == OriginOfCandidateForMemoGroup::WidenedScans
                ),
                "{queries:?}: the narrower scan is widened"
            );
            for group in 0..memo.groups.len() {
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
            for group in 0..expanded.memo.groups.len() {
                assert!(!groups_read_by(&expanded.memo, group).contains(&group));
            }
        }
    }

    // A candidate reading its own group is a cycle the memo rejects.
    #[test]
    fn a_group_reading_itself_is_a_cyclic_memo() {
        let mut memo = workload(&[("sum by (job) (m)", 0.01)]);
        assert!(check_unexpanded_memo(&memo).is_ok());
        let mut reader = memo.candidates[0][0].clone();
        reader.input_groups = vec![MemoGroupId(0)];
        memo.candidates[0].push(reader);
        assert_eq!(memo.topological_order(), None);
        assert!(matches!(
            check_unexpanded_memo(&memo),
            Err(StateGroupExpansionError::CyclicDependencies)
        ));
    }

    // Label widening must not erase source predicates in order to create reuse.
    #[test]
    fn scan_label_widening_keeps_different_filters_separate() {
        let original = with_local_panes(
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
            .all(|id| expanded.memo.groups[id.0 as usize].queries.len() == 1));
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
            .filter(|candidate| candidate.origin == OriginOfCandidateForMemoGroup::Panes)
            .count();
        assert!(local_panes >= 2);
        let expanded = expand(original.clone()).unwrap();
        let chains: Vec<_> = expanded
            .state_groups
            .iter()
            .filter(|id| {
                matches!(
                    expanded.memo.groups[id.0 as usize].logical_node.asap(),
                    Some(ASAPOp::PaneChain { .. })
                )
            })
            .collect();
        assert!(!chains.is_empty());
        for id in chains {
            let group = &expanded.memo.groups[id.0 as usize];
            assert_eq!(group.queries, vec![0, 1]);
            assert_eq!(group.parents.len(), 2);
            assert!(group.child_groups.iter().all(|input| group
                .logical_node
                .children()
                .iter()
                .any(|node| Rc::ptr_eq(
                    node,
                    &expanded.memo.groups[input.0 as usize].logical_node
                ))));
        }
        for (group, candidates) in original.candidates.iter().enumerate() {
            for original in candidates {
                assert!(expanded.memo.candidates[group]
                    .iter()
                    .any(|candidate| Rc::ptr_eq(&original.root_node, &candidate.root_node)));
            }
        }
        assert!(matches!(
            expand(expanded.memo),
            Err(StateGroupExpansionError::AlreadyExpanded)
        ));
    }

    // A finer grid and longer horizon serves a shorter reader through newest_panes.
    #[test]
    fn different_pane_grids_and_horizons_gain_explicit_newest_pane_readers() {
        let original = with_local_panes(
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
                    expanded.memo.groups[id.0 as usize].logical_node.asap(),
                    Some(ASAPOp::PaneChain {
                        pane_width_ms: 30_000,
                        pane_count: 30,
                        ..
                    })
                )
            })
            .expect("shared fine grid covering fifteen minutes");
        assert_eq!(expanded.memo.groups[state.0 as usize].queries, vec![0, 1]);
        assert!(expanded.memo.candidates.iter().flatten().any(|reader| {
            reader.input_groups.contains(state)
                && OperatorNode::reachable(&reader.root_node)
                    .iter()
                    .any(|node| {
                        matches!(
                            node.asap(),
                            Some(ASAPOp::SummaryMerge {
                                newest_panes: Some(10),
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
        assert!(expanded.memo.groups.iter().any(|group| matches!(
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
            .groups
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
            .groups
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

    // A composite average can share only its sum state with another query while
    // retaining the count state locally; reader budget allocation is preserved.
    #[test]
    fn multiple_private_states_can_share_only_the_common_producer() {
        let mut original = workload(&[("avg(m)", 0.01), ("sum(m)", 0.01)]);
        let average = original.query_roots[0].as_operator().unwrap().0 as usize;
        assert!(original.candidates[average]
            .iter()
            .any(|candidate| direct_input_builds(&original, candidate).len() >= 2));
        for candidates in &mut original.candidates {
            for candidate in candidates {
                candidate.accuracy_budget_parts = 2;
            }
        }
        let expanded = expand(original.clone()).unwrap();
        assert!(!expanded.state_groups.is_empty());
        let reader = expanded.memo.candidates[average]
            .iter()
            .find(|candidate| {
                candidate.origin == OriginOfCandidateForMemoGroup::SharedBuild
                    && candidate
                        .input_groups
                        .iter()
                        .any(|group| expanded.state_groups.contains(group))
            })
            .expect("average has shared state input");
        assert_eq!(reader.accuracy_budget_parts, 2);
        // The shared build is the readers' own, sized for two shares too.
        for state in &expanded.state_groups {
            assert_eq!(
                expanded.memo.candidates[state.0 as usize][0].accuracy_budget_parts,
                2
            );
        }
        assert!(
            reader
                .input_groups
                .iter()
                .any(|id| expanded.state_groups.contains(id)
                    && expanded.memo.groups[id.0 as usize].queries == vec![0, 1]),
            "sum state is reusable by both queries"
        );
        assert!(
            reader
                .input_groups
                .iter()
                .any(|id| expanded.state_groups.contains(id)
                    && expanded.memo.groups[id.0 as usize].queries == vec![0]),
            "count state belongs only to the average query"
        );
        assert!(original.candidates[average]
            .iter()
            .all(|before| expanded.memo.candidates[average]
                .iter()
                .any(|after| Rc::ptr_eq(&before.root_node, &after.root_node))));
    }

    // A global SQL-window quantile can consume newest panes of a longer,
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
        let memo = with_local_panes(
            try_populate_pass1(initial, &generated).unwrap(),
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
                        expanded.memo.groups[group.0 as usize].logical_node.asap(),
                        Some(ASAPOp::PaneChain {
                            pane_count: 120,
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
        let mut memo = workload(&[
            ("quantile_over_time(0.95, latency[5m])", 0.01),
            ("quantile_over_time(0.99, latency[5m])", 0.01),
        ]);
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
            let group = &expanded.memo.groups[state.0 as usize];
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
        crate::pass2::memo::rules::summary_inputs::factor_summary_inputs(&mut memo, None).unwrap();
        assert!(memo
            .candidates
            .iter()
            .flatten()
            .any(|candidate| direct_input_builds(&memo, candidate)
                .iter()
                .any(|(node, _)| is_univmon(node))));
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
            let (build, _) = simple_build(&memo, candidate)?;
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
            .map(|id| Rc::as_ptr(&memo.groups[id.0 as usize].logical_node))
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
        let mut memo = workload(&[("quantile_over_time(0.95, latency[5m])", 0.01)]);
        let (owner, mut candidate) = memo
            .candidates
            .iter()
            .enumerate()
            .find_map(|(group, candidates)| {
                candidates
                    .iter()
                    .find(|candidate| simple_build(&memo, candidate).is_some())
                    .map(|candidate| (group, candidate.clone()))
            })
            .unwrap();
        let boundaries: HashSet<_> = candidate
            .input_groups
            .iter()
            .map(|id| Rc::as_ptr(&memo.groups[id.0 as usize].logical_node))
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
                discriminator_unique_key: None,
                children: vec![Rc::clone(&candidate.root_node), second],
            }))
            .unwrap();
        candidate.root_node = Rc::clone(&combined);
        memo.groups[owner].logical_node = combined;
        memo.candidates[owner] = vec![candidate];
        let expanded = expand(memo).unwrap();
        assert_eq!(expanded.state_groups.len(), 1);
        let state = expanded.state_groups[0];
        assert!(expanded.memo.groups[state.0 as usize]
            .parents
            .contains(&MemoGroupId(owner as u32)));
        assert!(expanded.memo.candidates[owner]
            .iter()
            .any(
                |candidate| candidate.origin == OriginOfCandidateForMemoGroup::SharedBuild
                    && candidate.input_groups.contains(&state)
            ));
    }

    // Public expansion rejects malformed IDs before indexing dependency arrays.
    #[test]
    fn malformed_references_fail_before_dependency_traversal() {
        let mut memo = workload(&[("quantile_over_time(0.95, latency[5m])", 0.01)]);
        memo.groups[0].child_groups.push(MemoGroupId(u32::MAX));
        assert!(matches!(
            expand(memo),
            Err(StateGroupExpansionError::InvalidGroupReference)
        ));
    }
}

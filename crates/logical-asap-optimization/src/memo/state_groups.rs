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
    #[error(transparent)]
    DeadlineExceeded(#[from] super::CandidateGenerationTimeout),
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

/// Preserve local candidates and add explicit shared-state reader candidates.
/// The input must precede explicit state-group expansion. Unsupported candidate
/// shapes are retained unchanged, rather than guessed into a shared interface.
pub fn expand_concrete_summary_state_groups(
    memo: CandidateMemo,
) -> Result<StateGroupExpansion, StateGroupExpansionError> {
    expand_concrete_summary_state_groups_in_domain(
        memo,
        super::state_configurations::PaneWidthDomain::Complete,
    )
}

pub fn expand_concrete_summary_state_groups_in_domain(
    memo: CandidateMemo,
    domain: super::state_configurations::PaneWidthDomain,
) -> Result<StateGroupExpansion, StateGroupExpansionError> {
    expand_concrete_summary_state_groups_before(memo, domain, None)
}

/// An interrupted catalog is discarded, never returned as a complete search space.
pub fn expand_concrete_summary_state_groups_before(
    memo: CandidateMemo,
    domain: super::state_configurations::PaneWidthDomain,
    deadline: Option<Instant>,
) -> Result<StateGroupExpansion, StateGroupExpansionError> {
    expand_with_state_index(memo, domain, deadline, true)
}

fn expand_with_state_index(
    mut memo: CandidateMemo,
    domain: super::state_configurations::PaneWidthDomain,
    deadline: Option<Instant>,
    indexed: bool,
) -> Result<StateGroupExpansion, StateGroupExpansionError> {
    check_generation_deadline(deadline)?;
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
    super::rules::scan_labels::add_scan_label_candidates(&mut memo, deadline)?;
    super::rules::summary_inputs::factor_summary_inputs(&mut memo, deadline)?;
    super::rules::topk::add_topk_candidates(&mut memo, deadline)?;
    super::rules::filtered_group::add_filtered_group_candidates(&mut memo, deadline)?;
    super::rules::rollup::add_rollup_candidates(&mut memo, deadline)?;
    super::rules::rollup::add_window_rollup_candidates(&mut memo, deadline)?;
    super::rules::summary_results::factor_summary_results(&mut memo, deadline)?;
    super::rules::state_configurations::add_state_configuration_candidates_in_domain(
        &mut memo, domain, deadline,
    )?;
    let mut builds: Vec<BuildReaders> = Vec::new();
    let mut build_index: HashMap<Option<BuildFingerprint>, Vec<usize>> = HashMap::new();
    for (group, candidates) in memo.candidates.iter().enumerate() {
        check_generation_deadline(deadline)?;
        for (index, candidate) in candidates.iter().enumerate() {
            check_generation_deadline(deadline)?;
            for (build, children) in direct_input_builds(&memo, candidate) {
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
            budget_share: 1,
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
    let mut added_reader_candidates = 0;
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
            .children
            .iter()
            .chain(&state_groups)
            .map(|id| (Rc::as_ptr(&memo.groups[id.0 as usize].node), *id))
            .collect();
        reader.node = super::rules::reuse::replace(&reader.node, &replacements);
        reader.children = boundary_groups(&reader.node, &boundaries);
        reader.origin = CandidateOrigin::SharedBuild;
        memo.candidates[group].push(reader);
        added_reader_candidates += 1;
    }
    // Parents describe every possible group/candidate dependency, not a selected plan.
    for group in &mut memo.groups {
        check_generation_deadline(deadline)?;
        group.parents.clear();
    }
    for group in 0..memo.groups.len() {
        check_generation_deadline(deadline)?;
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
            check_generation_deadline(deadline)?;
            memo.groups[child.0 as usize].queries.extend(&queries);
            memo.groups[child.0 as usize].queries.sort_unstable();
            memo.groups[child.0 as usize].queries.dedup();
        }
    }
    check_generation_deadline(deadline)?;
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
            .any(|id| id.0 as usize >= memo.groups.len())
        {
            return Err(StateGroupExpansionError::InvalidGroupReference);
        }
    }
    Ok(())
}

pub(super) fn simple_build(
    memo: &CandidateMemo,
    candidate: &ImplementationCandidate,
) -> Option<(Rc<OperatorNode>, Vec<MemoGroupId>)> {
    let mut builds = direct_input_builds(memo, candidate);
    (builds.len() == 1).then(|| builds.remove(0))
}

pub(super) fn direct_input_builds(
    memo: &CandidateMemo,
    candidate: &ImplementationCandidate,
) -> Vec<(Rc<OperatorNode>, Vec<MemoGroupId>)> {
    if !matches!(
        candidate.origin,
        CandidateOrigin::Pass1
            | CandidateOrigin::Panes
            | CandidateOrigin::WidenedScans
            | CandidateOrigin::Rollup
            | CandidateOrigin::SummaryResult
    ) || candidate.implements.is_empty()
        || candidate.budget_share == 0
    {
        return Vec::new();
    }
    let boundaries: HashMap<_, _> = candidate
        .children
        .iter()
        .map(|&group| (Rc::as_ptr(&memo.groups[group.0 as usize].node), group))
        .collect();
    let mut stack = vec![Rc::clone(&candidate.node)];
    let mut seen = HashSet::new();
    let mut builds = Vec::new();
    while let Some(node) = stack.pop() {
        if boundaries.contains_key(&Rc::as_ptr(&node)) || !seen.insert(Rc::as_ptr(&node)) {
            continue;
        }
        if !Rc::ptr_eq(&node, &candidate.node)
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
    use crate::memo::candidates::try_populate_pass1;
    use crate::memo::LogicalMemo;
    use crate::per_computation_candidates::generate_per_computation_candidates;
    use asap_types::ir::properties::AccuracyTarget;
    use asap_types::ir::QueryRoot;

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
        let domain = super::super::state_configurations::PaneWidthDomain::Complete;
        let indexed = expand_with_state_index(memo.clone(), domain, None, true).unwrap();
        let linear = expand_with_state_index(memo, domain, None, false).unwrap();
        assert_eq!(indexed.state_groups, linear.state_groups);
        assert_eq!(indexed.memo.groups.len(), linear.memo.groups.len());
        for (left, right) in indexed.memo.candidates.iter().zip(&linear.memo.candidates) {
            let signatures = |candidates: &[ImplementationCandidate]| {
                candidates
                    .iter()
                    .map(|candidate| {
                        (
                            candidate.children.clone(),
                            candidate.budget_share,
                            serde_json::to_string(
                                &asap_types::ir::flat::flatten(&[QueryRoot::Operator(Rc::clone(
                                    &candidate.node,
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
            assert_eq!(left.children, right.children);
            assert_eq!(left.parents, right.parents);
            assert_eq!(left.queries, right.queries);
        }
    }

    // State expansion returns a typed timeout, not a partial success that a
    // solver could incorrectly report as optimal over the complete catalog.
    #[test]
    fn expired_state_expansion_returns_timeout() {
        let memo = workload(&[("quantile_over_time(0.95, latency[5m])", 0.01)]);
        assert!(matches!(
            expand_concrete_summary_state_groups_before(
                memo,
                super::super::state_configurations::PaneWidthDomain::Complete,
                Some(Instant::now())
            ),
            Err(StateGroupExpansionError::DeadlineExceeded(_))
        ));
    }

    fn workload(queries: &[(&str, f64)]) -> CandidateMemo {
        workload_with_identity(queries, false)
    }

    fn workload_with_identity(queries: &[(&str, f64)], identity: bool) -> CandidateMemo {
        let roots: Vec<_> = queries
            .iter()
            .enumerate()
            .map(|(id, (query, error))| {
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
                (id, QueryRoot::Operator(root))
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

    // A stricter concrete configuration can serve both readers; local sizes remain.
    #[test]
    fn distinct_sizes_gain_compatible_shared_configurations() {
        let original = workload(&[
            ("quantile_over_time(0.95, latency[5m])", 0.01),
            ("quantile_over_time(0.99, latency[5m])", 0.02),
        ]);
        let expanded = expand_concrete_summary_state_groups(original.clone()).unwrap();
        assert!(!expanded.state_groups.is_empty());
        for (group, candidates) in original.candidates.iter().enumerate() {
            for candidate in candidates {
                assert!(expanded.memo.candidates[group]
                    .iter()
                    .any(|after| Rc::ptr_eq(&candidate.node, &after.node)));
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
        let expanded = expand_concrete_summary_state_groups(original).unwrap();
        assert!(expanded.state_groups.is_empty());
        assert_eq!(expanded.memo.groups.len(), count);
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
            group.node.non_asap(),
            Some(NonASAPOp::Scan { .. })
        ) && group
            .node
            .schema
            .fields
            .iter()
            .any(|field| field.name == "job")
            && group
                .node
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
                    .map(|candidate| Rc::clone(&candidate.node))
                    .collect::<Vec<_>>()
            })
            .collect();
        let expanded = expand_concrete_summary_state_groups(original).unwrap();
        let shared = expanded
            .state_groups
            .iter()
            .find(|id| {
                let group = &expanded.memo.groups[id.0 as usize];
                matches!(group.node.asap(), Some(ASAPOp::PaneChain { .. }))
                    && group.queries == vec![0, 1]
                    && ["job", "region"].iter().all(|name| {
                        group
                            .node
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
                .find(|candidate| candidate.children.contains(shared))
                .unwrap();
            assert_eq!(
                reader.node.schema.fields,
                expanded.memo.groups[parent.0 as usize].node.schema.fields
            );
            assert!(OperatorNode::reachable(&reader.node)
                .iter()
                .any(|node| matches!(node.non_asap(), Some(NonASAPOp::Project { .. }))));
            reader.node.validate_structure().unwrap();
        }
        for (group, locals) in local_nodes.into_iter().enumerate() {
            assert!(locals
                .iter()
                .zip(&expanded.memo.candidates[group])
                .all(|(before, after)| Rc::ptr_eq(before, &after.node)));
        }
        assert!(expanded.memo.topological_order().is_some());
        assert!(state
            .children
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
        let expanded = expand_concrete_summary_state_groups(memo).unwrap();
        let state = expanded
            .state_groups
            .iter()
            .map(|id| &expanded.memo.groups[id.0 as usize])
            .find(|group| {
                matches!(group.node.asap(), Some(ASAPOp::PaneChain { .. }))
                    && group.queries == vec![0, 1]
                    && ["job", "region"].iter().all(|name| {
                        group
                            .node
                            .schema
                            .fields
                            .iter()
                            .any(|field| field.name == *name)
                    })
            })
            .expect("identity-carrying union pane state");
        let input = state.node.children()[0];
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
                    candidate.children.contains(&MemoGroupId(
                        expanded
                            .memo
                            .groups
                            .iter()
                            .position(|group| Rc::ptr_eq(&group.node, &state.node))
                            .unwrap() as u32,
                    ))
                })
                .unwrap();
            assert_eq!(
                reader.node.schema,
                expanded.memo.groups[parent.0 as usize].node.schema
            );
        }
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
        let expanded = expand_concrete_summary_state_groups(original).unwrap();
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
            .filter(|candidate| candidate.origin == CandidateOrigin::Panes)
            .count();
        assert!(local_panes >= 2);
        let expanded = expand_concrete_summary_state_groups(original.clone()).unwrap();
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
        for (group, candidates) in original.candidates.iter().enumerate() {
            for original in candidates {
                assert!(expanded.memo.candidates[group]
                    .iter()
                    .any(|candidate| Rc::ptr_eq(&original.node, &candidate.node)));
            }
        }
        assert!(matches!(
            expand_concrete_summary_state_groups(expanded.memo),
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
        let expanded = expand_concrete_summary_state_groups(original.clone()).unwrap();
        let state = expanded
            .state_groups
            .iter()
            .find(|id| {
                matches!(
                    expanded.memo.groups[id.0 as usize].node.asap(),
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
            reader.children.contains(state)
                && OperatorNode::reachable(&reader.node).iter().any(|node| {
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
                    .any(|after| Rc::ptr_eq(&before.node, &after.node))));
        }
        assert!(expanded.memo.topological_order().is_some());
    }

    // Rollup readers preserve original outputs while referencing explicit finer
    // producers through ordinary child-group dependencies.
    #[test]
    fn exact_rollup_exposes_shared_finalized_values() {
        let original = workload(&[
            ("sum by (job) (m)", 0.01),
            ("sum by (job, instance) (m)", 0.01),
        ]);
        let expanded = expand_concrete_summary_state_groups(original.clone()).unwrap();
        let rollups: Vec<_> = expanded
            .memo
            .candidates
            .iter()
            .flatten()
            .filter(|candidate| candidate.origin == CandidateOrigin::Rollup)
            .collect();
        assert!(!rollups.is_empty());
        assert!(expanded.memo.groups.iter().enumerate().any(|(id, group)| id
            >= original.groups.len()
            && matches!(
                group.node.asap(),
                Some(ASAPOp::FinalizeExactAccumulator { .. })
            )
            && group.parents.len() >= 2));
        assert!(expanded.memo.topological_order().is_some());
    }

    // Sketch rollup exposes the resized finer state through an ordinary group
    // dependency, while each output retains its own grouping and quantile.
    #[test]
    fn sketch_rollup_exposes_shared_finer_state() {
        let original = workload(&[
            ("quantile(0.9, m)", 0.01),
            ("quantile by (job) (0.9, m)", 0.02),
        ]);
        let expanded = expand_concrete_summary_state_groups(original).unwrap();
        assert!(expanded
            .memo
            .candidates
            .iter()
            .flatten()
            .any(|candidate| candidate.origin == CandidateOrigin::SharedBuild
                && OperatorNode::reachable(&candidate.node)
                    .iter()
                    .any(|node| matches!(
                        node.asap(),
                        Some(ASAPOp::SummaryMerge {
                            group_by: Some(_),
                            ..
                        })
                    ))));
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
        let expanded = expand_concrete_summary_state_groups(original).unwrap();
        assert!(expanded.memo.groups.iter().any(|group| matches!(
            group.node.non_asap(),
            Some(NonASAPOp::Limit { n: Some(5), .. })
        ) && group.parents.len() >= 2));
        assert!(expanded
            .memo
            .candidates
            .iter()
            .flatten()
            .any(|candidate| candidate.origin == CandidateOrigin::TopKReuse));
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
        let expanded = expand_concrete_summary_state_groups(original.clone()).unwrap();
        let union = expanded
            .memo
            .groups
            .iter()
            .enumerate()
            .find(|(_, group)| {
                matches!(group.node.non_asap(), Some(NonASAPOp::Scan { .. }))
                    && ["job", "region"].iter().all(|name| {
                        group
                            .node
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
            .filter(|(_, group)| matches!(group.node.non_asap(), Some(NonASAPOp::Scan { .. })))
            .filter(|(group, _)| {
                expanded.memo.candidates[*group].iter().any(|candidate| {
                    candidate.origin == CandidateOrigin::WidenedScans
                        && candidate.children.contains(&union)
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
        let average = original
            .groups
            .iter()
            .position(|group| group.roots_of.contains(&0))
            .unwrap();
        assert!(original.candidates[average]
            .iter()
            .any(|candidate| direct_input_builds(&original, candidate).len() >= 2));
        for candidates in &mut original.candidates {
            for candidate in candidates {
                candidate.budget_share = 2;
            }
        }
        let expanded = expand_concrete_summary_state_groups(original.clone()).unwrap();
        assert!(!expanded.state_groups.is_empty());
        let reader = expanded.memo.candidates[average]
            .iter()
            .find(|candidate| {
                candidate.origin == CandidateOrigin::SharedBuild
                    && candidate
                        .children
                        .iter()
                        .any(|group| expanded.state_groups.contains(group))
            })
            .expect("average has shared state input");
        assert_eq!(reader.budget_share, 2);
        assert!(
            reader
                .children
                .iter()
                .any(|id| expanded.state_groups.contains(id)
                    && expanded.memo.groups[id.0 as usize].queries == vec![0, 1]),
            "sum state is reusable by both queries"
        );
        assert!(
            reader
                .children
                .iter()
                .any(|id| expanded.state_groups.contains(id)
                    && expanded.memo.groups[id.0 as usize].queries == vec![0]),
            "count state belongs only to the average query"
        );
        assert!(original.candidates[average]
            .iter()
            .all(|before| expanded.memo.candidates[average]
                .iter()
                .any(|after| Rc::ptr_eq(&before.node, &after.node))));
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
            (
                0,
                QueryRoot::Operator(agg(
                    vec![],
                    quantile(0.5),
                    time_range(std::time::Duration::from_secs(300), scan()),
                )),
            ),
            (
                1,
                QueryRoot::Operator(agg(
                    vec![1],
                    quantile(0.9),
                    time_range(std::time::Duration::from_secs(1200), scan()),
                )),
            ),
        ];
        let initial = LogicalMemo::from_roots(&roots);
        let generated = generate_per_computation_candidates(roots).unwrap();
        let memo = with_local_panes(
            try_populate_pass1(initial, &generated).unwrap(),
            &[10_000, 10_000],
        );
        let expanded = expand_concrete_summary_state_groups(memo).unwrap();
        assert!(expanded
            .memo
            .candidates
            .iter()
            .flatten()
            .any(|candidate| candidate.origin == CandidateOrigin::SharedBuild
                && OperatorNode::reachable(&candidate.node)
                    .iter()
                    .any(|node| matches!(
                        node.asap(),
                        Some(ASAPOp::SummaryMerge {
                            group_by: Some(_),
                            ..
                        })
                    ))
                && candidate.children.iter().any(|group| matches!(
                    expanded.memo.groups[group.0 as usize].node.asap(),
                    Some(ASAPOp::PaneChain {
                        pane_count: 120,
                        ..
                    })
                ))));
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
                candidate.node = crate::memo::rules::reuse::replace(&candidate.node, &replacements);
                assert!(direct_input_builds(&memo, &candidate).is_empty());
                memo.candidates[owner].push(candidate);
                injected += 1;
            }
        }
        assert!(injected >= 2);
        let expanded = expand_concrete_summary_state_groups(memo).unwrap();
        assert!(expanded.state_groups.iter().any(|state| {
            let group = &expanded.memo.groups[state.0 as usize];
            group.queries == vec![0, 1]
                && group.children.iter().any(|input| {
                    expanded.memo.candidates[input.0 as usize]
                        .iter()
                        .any(|candidate| candidate.origin == CandidateOrigin::SummaryInput)
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
            .flat_map(|candidate| OperatorNode::reachable(&candidate.node))
            .filter(|node| is_univmon(node))
            .collect();
        assert!(!before.is_empty(), "real generation must offer UnivMon");
        crate::memo::rules::summary_inputs::factor_summary_inputs(&mut memo, None).unwrap();
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
                    .flat_map(|candidate| OperatorNode::reachable(&candidate.node))
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
            .children
            .iter()
            .map(|id| Rc::as_ptr(&memo.groups[id.0 as usize].node))
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
            let roots = crate::asap_aware_cse::with_resized_build(
                &[QueryRoot::Operator(Rc::clone(&base.node))],
                &build,
                family,
            )
            .unwrap();
            let QueryRoot::Operator(root) = &roots[0] else {
                unreachable!()
            };
            let root = crate::memo::rules::reuse::restore_inputs(root, &memo, &base.children);
            without_private_guarantees(&root, &boundaries)
        };
        let weak = rewritten(0.001);
        let strict = rewritten(0.0001);
        assert_eq!(
            *weak, *strict,
            "ordinary equality omits the probability metadata"
        );
        base.node = weak;
        memo.candidates[owner] = vec![base.clone()];
        assert!(crate::memo::rules::reuse::append_reader(
            &mut memo,
            owner,
            &base,
            strict,
            CandidateOrigin::Rollup
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
            .children
            .iter()
            .map(|id| Rc::as_ptr(&memo.groups[id.0 as usize].node))
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
        let second = duplicate(&candidate.node, &boundaries);
        let combined =
            OperatorNode::new_shared(asap_types::ir::Operator::NonASAP(NonASAPOp::Concat {
                discriminator_unique_key: None,
                children: vec![Rc::clone(&candidate.node), second],
            }))
            .unwrap();
        candidate.node = Rc::clone(&combined);
        memo.groups[owner].node = combined;
        memo.candidates[owner] = vec![candidate];
        let expanded = expand_concrete_summary_state_groups(memo).unwrap();
        assert_eq!(expanded.state_groups.len(), 1);
        let state = expanded.state_groups[0];
        assert!(expanded.memo.groups[state.0 as usize]
            .parents
            .contains(&MemoGroupId(owner as u32)));
        assert!(expanded.memo.candidates[owner]
            .iter()
            .any(|candidate| candidate.origin == CandidateOrigin::SharedBuild
                && candidate.children.contains(&state)));
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

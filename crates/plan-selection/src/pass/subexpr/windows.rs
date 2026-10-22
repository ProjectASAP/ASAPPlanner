//! The window-composition, rollup and exact top-k reuse transformations
//! (joint-planning-search design, "Memo-group search", stage interfaces):
//! Pass 2 rules that add implementation candidates to the memo's groups.
//!
//! - **Window composition** ([`add_window_composition`]): a group whose
//!   candidate builds a summary over a range `TimeRange(W)`, read only by
//!   queries that repeat on a grid, gets the candidate with that build in
//!   tumbling panes (the width rules of `pane_candidates`). When another
//!   group builds the same family over the same input, it also gets the
//!   candidate that reads the newest panes of a shared chain, with a
//!   [`CouplingKind::PaneChain`] site: the chain's grid, longest window and
//!   size are the site's threshold ([`chain_thresholds`], [`may_read`],
//!   [`read_at_threshold`]).
//! - **Rollup** ([`add_rollup`]): a coarse exact accumulator answered by
//!   regrouping a finer group's values (`rollup_candidates`' conditions).
//! - **Exact top-k reuse** ([`add_topk_reuse`]): a top-k answered from a
//!   ranking group, `Limit(k_max)` over a descending `Sort` of the input,
//!   added to the memo when missing.
//!
//! The joint search never reuses a top-k ranking in a plan with panes
//! ([`combinable`]).

use std::collections::hash_map::DefaultHasher;
use std::collections::{BTreeSet, HashMap, HashSet};
use std::hash::{Hash, Hasher};
use std::rc::Rc;

use asap_logical_asap_optimization::accuracy::estimators::size_for_all_readers;
use asap_logical_asap_optimization::asap_aware_cse::{
    pane_candidates, rollup_candidates, topk_reuse_candidates, window_composition_pass2_candidates,
    with_pane_chain,
};
use asap_logical_asap_optimization::per_computation_candidates::PerComputationCandidateError;
use asap_types::ir::flat::flatten;
use asap_types::ir::schema::sketch::SketchKind;
use asap_types::ir::schema::FieldDataType;
use asap_types::ir::{ASAPOp, OperatorNode, QueryRoot};
use asap_types::workload::{greatest_common_divisor, QueryWorkloadEntry};

use super::{
    CandidateOrigin, CouplingKind, CouplingSite, GroupId, ImplementationCandidate, Memo, MemoGroup,
};

/// One threshold of a shared pane chain: its grid, the longest window it
/// covers and the size it is built at. A reader no stricter than it may
/// read it ([`may_read`]).
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ChainThreshold {
    /// The width of one pane.
    pub pane_width_ms: u64,
    /// The window the chain covers: the longest window among its readers.
    pub longest_window_ms: u64,
    /// The family and size the chain is built at: the size for its
    /// strictest reader (`size_for_all_readers`).
    pub family: FieldDataType,
}

/// A candidate's read of a shared pane chain: its own panes, window and
/// size, and the `SummaryMerge` that reads them.
#[derive(Debug, Clone)]
pub(crate) struct ChainReader {
    /// The chain's [`CouplingSite::key`].
    pub key: u64,
    /// The width of the reader's own panes: the gcd of its window and its
    /// readers' grids.
    pub pane_width_ms: u64,
    /// The reader's window.
    pub window_ms: u64,
    /// The family and size the reader needs.
    pub family: FieldDataType,
    /// The merge over the reader's own chain, which a threshold replaces.
    pub merge: Rc<OperatorNode>,
}

/// Every transformation of this module, in order: window composition,
/// rollup, exact top-k reuse. `entries[q]` is workload query `q`'s entry.
pub(crate) fn add_pass2_candidates(memo: &mut Memo, entries: &[QueryWorkloadEntry]) {
    add_window_composition(memo, entries);
    add_rollup(memo);
    add_topk_reuse(memo);
}

/// Whether the joint search can choose `chosen` together: never an exact
/// top-k reuse with panes (Pass 2 never applies window composition after
/// top-k reuse, nor top-k reuse after it).
pub(crate) fn combinable<'a>(
    chosen: impl IntoIterator<Item = &'a ImplementationCandidate>,
) -> bool {
    let (mut panes, mut topk) = (false, false);
    for candidate in chosen {
        panes |= candidate.origin == CandidateOrigin::Panes;
        topk |= candidate.origin == CandidateOrigin::TopKReuse;
    }
    !(panes && topk)
}

/// Window composition: for each group whose every query repeats on a grid,
/// each Pass 1 (or shared-build) candidate with a summary build over a
/// range window gets a [`CandidateOrigin::Panes`] candidate per non-empty
/// set of its builds in panes (`window_composition_pass2_candidates` on
/// the candidate alone). Each build another group also builds (same
/// family at any size, same input below the range, grouping, item and
/// weight, filter and names) is also offered reading a shared chain: the
/// same node with a [`CouplingKind::PaneChain`] site per such build, whose
/// chain the site's threshold sets ([`read_at_threshold`]).
pub(crate) fn add_window_composition(memo: &mut Memo, entries: &[QueryWorkloadEntry]) {
    let group_of = group_of_node(&memo.groups);
    let mut own: Vec<(usize, ImplementationCandidate)> = Vec::new();
    for (g, group) in memo.groups.iter().enumerate() {
        let Some(grid) = group_grid_ms(group, entries) else {
            continue;
        };
        for candidate in &memo.candidates[g] {
            if !matches!(
                candidate.origin,
                CandidateOrigin::Pass1 | CandidateOrigin::SharedBuild
            ) {
                continue;
            }
            let roots = [QueryRoot::Operator(Rc::clone(&candidate.node))];
            let panes = pane_candidates(&roots, &[Some(grid)]);
            if panes.is_empty() {
                continue;
            }
            let Ok(pass2_candidates) = window_composition_pass2_candidates(&roots, &panes) else {
                continue;
            };
            for pass2_candidate in pass2_candidates {
                let Ok(rewritten) = pass2_candidate.roots else {
                    continue;
                };
                let [QueryRoot::Operator(node)] = &rewritten[..] else {
                    continue;
                };
                own.push((
                    g,
                    rewritten_candidate(
                        &group_of,
                        candidate,
                        Rc::clone(node),
                        CandidateOrigin::Panes,
                    ),
                ));
            }
        }
    }
    // The chain keys each group builds.
    let mut builders: HashMap<u64, BTreeSet<usize>> = HashMap::new();
    for (g, candidate) in &own {
        for reader in chain_readers(&candidate.node) {
            builders.entry(reader.key).or_default().insert(*g);
        }
    }
    for (g, candidate) in own {
        let mut keys: Vec<u64> = chain_readers(&candidate.node)
            .into_iter()
            .map(|reader| reader.key)
            .filter(|key| builders[key].len() >= 2)
            .collect();
        keys.sort_unstable();
        keys.dedup();
        if !keys.is_empty() {
            let mut shared = candidate.clone();
            shared
                .coupling_sites
                .extend(keys.into_iter().map(|key| CouplingSite {
                    key,
                    kind: CouplingKind::PaneChain,
                }));
            push_new(memo, g, shared);
        }
        push_new(memo, g, candidate);
    }
}

/// The reads of a pane chain in `node`: each `SummaryMerge` over an
/// [`ASAPOp::PaneChain`], with the chain's key.
pub(crate) fn chain_readers(node: &Rc<OperatorNode>) -> Vec<ChainReader> {
    let mut seen = HashSet::new();
    let mut readers = Vec::new();
    for merge in OperatorNode::reachable(node) {
        if !seen.insert(Rc::as_ptr(&merge)) {
            continue;
        }
        let Some(ASAPOp::SummaryMerge { children, .. }) = merge.asap() else {
            continue;
        };
        let [chain] = &children[..] else {
            continue;
        };
        let Some(ASAPOp::PaneChain {
            child,
            family,
            input,
            reduction,
            grouping,
            filter,
            pane_width_ms,
            pane_count,
        }) = chain.asap()
        else {
            continue;
        };
        let below = serde_json::to_string(&flatten(&[QueryRoot::Operator(Rc::clone(child))]).0)
            .unwrap_or_default();
        let names: Vec<(&String, &Option<String>)> = chain
            .schema
            .fields
            .iter()
            .map(|f| (&f.name, &f.table))
            .collect();
        let key = hash_of(
            &serde_json::json!({
                "family": family_without_size(family),
                "below": below,
                "input": input,
                "reduction": reduction,
                "grouping": grouping,
                "filter": filter,
                "names": names,
            })
            .to_string(),
        );
        readers.push(ChainReader {
            key,
            pane_width_ms: *pane_width_ms,
            window_ms: pane_width_ms * u64::from(*pane_count),
            family: family.clone(),
            merge: Rc::clone(&merge),
        });
    }
    readers
}

/// The thresholds of the shared pane chain `site`: its grids are the
/// closure of its readers' pane widths under gcd; for each grid, every
/// distinct (longest window, strictest size) pair that some set of the
/// readers whose widths the grid divides has, as the readers no stricter
/// than it ([`may_read`]). Ordered by grid, window, then size.
pub(crate) fn chain_thresholds(memo: &Memo, site: &CouplingSite) -> Vec<ChainThreshold> {
    let mut readers: Vec<ChainReader> = Vec::new();
    for candidate in memo.candidates.iter().flatten() {
        if !candidate.coupling_sites.contains(site) {
            continue;
        }
        for reader in chain_readers(&candidate.node) {
            let known = readers.iter().any(|r| {
                r.pane_width_ms == reader.pane_width_ms
                    && r.window_ms == reader.window_ms
                    && r.family == reader.family
            });
            if reader.key == site.key && !known {
                readers.push(reader);
            }
        }
    }
    let mut grids: BTreeSet<u64> = readers.iter().map(|r| r.pane_width_ms).collect();
    loop {
        let more: Vec<u64> = grids
            .iter()
            .flat_map(|&a| grids.iter().map(move |&b| greatest_common_divisor(a, b)))
            .filter(|w| !grids.contains(w))
            .collect();
        if more.is_empty() {
            break;
        }
        grids.extend(more);
    }
    let mut thresholds = Vec::new();
    for &pane_width_ms in &grids {
        let eligible: Vec<&ChainReader> = readers
            .iter()
            .filter(|r| r.pane_width_ms.is_multiple_of(pane_width_ms))
            .collect();
        let windows: BTreeSet<u64> = eligible.iter().map(|r| r.window_ms).collect();
        // The sizes: the readers' own, closed under the shared size.
        let mut sizes: Vec<FieldDataType> = Vec::new();
        for reader in &eligible {
            if !sizes.contains(&reader.family) {
                sizes.push(reader.family.clone());
            }
        }
        let mut i = 0;
        while i < sizes.len() {
            for j in 0..i {
                if let Some(joined) = shared_size(&sizes[i], &sizes[j]) {
                    if !sizes.contains(&joined) {
                        sizes.push(joined);
                    }
                }
            }
            i += 1;
        }
        for &longest_window_ms in &windows {
            for family in &sizes {
                let threshold = ChainThreshold {
                    pane_width_ms,
                    longest_window_ms,
                    family: family.clone(),
                };
                let admitted: Vec<&&ChainReader> = eligible
                    .iter()
                    .filter(|r| may_read(&threshold, r))
                    .collect();
                let window = admitted.iter().map(|r| r.window_ms).max();
                let size = admitted.iter().try_fold(None::<FieldDataType>, |acc, r| {
                    Some(Some(match acc {
                        None => r.family.clone(),
                        Some(acc) => shared_size(&acc, &r.family)?,
                    }))
                });
                if window == Some(longest_window_ms) && size == Some(Some(family.clone())) {
                    thresholds.push(threshold);
                }
            }
        }
    }
    thresholds
}

/// Whether `reader` may read a chain at `threshold`: its panes are
/// multiples of the chain's, its window is no longer than the chain's, and
/// its size is no stricter (the chain's size serves it).
pub(crate) fn may_read(threshold: &ChainThreshold, reader: &ChainReader) -> bool {
    reader.pane_width_ms.is_multiple_of(threshold.pane_width_ms)
        && reader.window_ms <= threshold.longest_window_ms
        && shared_size(&threshold.family, &reader.family).as_ref() == Some(&threshold.family)
}

/// `candidate`'s node with each of its chain reads of `site`'s key reading
/// the newest panes of the chain at `threshold`, which each may read
/// ([`may_read`]); every guarantee composed again.
pub(crate) fn read_at_threshold(
    candidate: &ImplementationCandidate,
    site: &CouplingSite,
    threshold: &ChainThreshold,
) -> Result<Rc<OperatorNode>, PerComputationCandidateError> {
    let pane_count = u32::try_from(threshold.longest_window_ms / threshold.pane_width_ms)
        .expect("a chain's pane count fits");
    let mut roots = vec![QueryRoot::Operator(Rc::clone(&candidate.node))];
    loop {
        // Each rewrite rebuilds the node: find the next unread merge again.
        let QueryRoot::Operator(node) = &roots[0] else {
            unreachable!("an operator root");
        };
        let next = chain_readers(node).into_iter().find(|reader| {
            reader.key == site.key
                && !(reader.pane_width_ms == threshold.pane_width_ms
                    && matches!(reader.merge.asap(), Some(ASAPOp::SummaryMerge { children, .. })
                        if matches!(children[0].asap(), Some(ASAPOp::PaneChain { pane_count: n, family, .. })
                            if *n == pane_count && *family == threshold.family)))
        });
        let Some(reader) = next else {
            break;
        };
        roots = with_pane_chain(
            &roots,
            &reader.merge,
            threshold.family.clone(),
            threshold.pane_width_ms,
            pane_count,
        )?;
    }
    let [QueryRoot::Operator(node)] = &roots[..] else {
        unreachable!("an operator root");
    };
    Ok(Rc::clone(node))
}

/// Rollup: each coarse exact accumulator of a group's Pass 1 (or
/// shared-build) candidate that another group's candidate can answer by
/// regrouping its finalized values (`rollup_candidates`), as a
/// [`CandidateOrigin::Rollup`] candidate that reads that group: the finer
/// value is the finer group's output, so the finer candidate must be that
/// group's whole implementation. Sketch rollups, which merge a finer
/// build's states rather than a group's output, are not offered.
pub(crate) fn add_rollup(memo: &mut Memo) {
    let group_of = group_of_node(&memo.groups);
    let mut added: Vec<(usize, ImplementationCandidate)> = Vec::new();
    let base = |c: &ImplementationCandidate| {
        matches!(
            c.origin,
            CandidateOrigin::Pass1 | CandidateOrigin::SharedBuild
        )
    };
    for (coarse_group, coarse_candidates) in memo.candidates.iter().enumerate() {
        for coarse in coarse_candidates.iter().filter(|c| base(c)) {
            let reached: HashSet<*const OperatorNode> = OperatorNode::reachable(&coarse.node)
                .iter()
                .map(Rc::as_ptr)
                .collect();
            for (finer_group, finer_candidates) in memo.candidates.iter().enumerate() {
                if finer_group == coarse_group {
                    continue;
                }
                for finer in finer_candidates.iter().filter(|c| base(c)) {
                    let roots = [
                        QueryRoot::Operator(Rc::clone(&coarse.node)),
                        QueryRoot::Operator(Rc::clone(&finer.node)),
                    ];
                    for rollup in rollup_candidates(&roots) {
                        let exact = matches!(
                            rollup.finer.asap(),
                            Some(ASAPOp::FinalizeExactAccumulator { .. })
                        );
                        if !exact
                            || !Rc::ptr_eq(&rollup.finer, &finer.node)
                            || !reached.contains(&Rc::as_ptr(&rollup.coarse))
                        {
                            continue;
                        }
                        // As `rollup_pass2_candidates` rewrites it, with the finer
                        // value read from the finer group: every finalized value
                        // of the coarse build replaced by the rollup.
                        let finer_output = &memo.groups[finer_group].node;
                        let to_group = HashMap::from([(Rc::as_ptr(&finer.node), finer_output)]);
                        let Ok(rolled) = replace(&rollup.rollup, &to_group, &mut HashMap::new())
                        else {
                            continue;
                        };
                        let finalized: Vec<Rc<OperatorNode>> = OperatorNode::reachable(
                            &coarse.node,
                        )
                        .into_iter()
                        .filter(|n| {
                            matches!(n.asap(), Some(ASAPOp::FinalizeExactAccumulator { child })
                                    if Rc::ptr_eq(child, &rollup.coarse))
                        })
                        .collect();
                        let by_rollup: HashMap<*const OperatorNode, &Rc<OperatorNode>> =
                            finalized.iter().map(|n| (Rc::as_ptr(n), &rolled)).collect();
                        let Ok(node) = replace(&coarse.node, &by_rollup, &mut HashMap::new())
                        else {
                            continue;
                        };
                        added.push((
                            coarse_group,
                            rewritten_candidate(&group_of, coarse, node, CandidateOrigin::Rollup),
                        ));
                    }
                }
            }
        }
    }
    for (g, candidate) in added {
        push_new(memo, g, candidate);
    }
}

/// Exact top-k reuse: the groups whose Pass 1 candidates are exact top-k
/// computations of one input and partition with at least two different
/// `k` (`topk_reuse_candidates` over the candidates' nodes, which carry
/// their guarantees) each get a [`CandidateOrigin::TopKReuse`] candidate
/// reading the ranking group, `Limit(k_max)` over a descending `Sort` of
/// the input group (the top-k group's child). The ranking's two groups
/// are added when the memo has no equal ones, each with its one candidate
/// (also of origin `TopKReuse`). The rule reads an exact input: the input
/// group's chosen implementation must be exact.
pub(crate) fn add_topk_reuse(memo: &mut Memo) {
    let mut group_of_candidate: HashMap<*const OperatorNode, usize> = HashMap::new();
    let mut roots: Vec<QueryRoot> = Vec::new();
    for (g, candidates) in memo.candidates.iter().enumerate() {
        for candidate in candidates {
            if candidate.origin == CandidateOrigin::Pass1 {
                group_of_candidate.insert(Rc::as_ptr(&candidate.node), g);
                roots.push(QueryRoot::Operator(Rc::clone(&candidate.node)));
            }
        }
    }
    for candidate in topk_reuse_candidates(&roots) {
        let mut members: Vec<usize> = candidate
            .members
            .iter()
            .filter_map(|m| group_of_candidate.get(&Rc::as_ptr(m)).copied())
            .collect();
        members.sort_unstable();
        members.dedup();
        if members.len() < 2 {
            continue;
        }
        let queries: Vec<usize> = members
            .iter()
            .flat_map(|&g| memo.groups[g].queries.iter().copied())
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect();
        // The ranking over the input group's node.
        let Some(&input) = memo.groups[members[0]].node.children().first() else {
            continue;
        };
        let input = Rc::clone(input);
        let sort = candidate.ranking.children()[0];
        let Ok(sort) = sort.with_new_children(|_| Rc::clone(&input)) else {
            continue;
        };
        let sort = Rc::new(sort);
        let input_group = find_group(&memo.groups, &input);
        let sort_group = group_for(memo, &sort, input_group.into_iter().collect(), &queries);
        let sort = Rc::clone(&memo.groups[sort_group.0 as usize].node);
        let ranking = match candidate.ranking.with_new_children(|_| Rc::clone(&sort)) {
            Ok(ranking) => Rc::new(ranking),
            Err(_) => continue,
        };
        let ranking_group = group_for(memo, &ranking, vec![sort_group], &queries);
        let ranking = Rc::clone(&memo.groups[ranking_group.0 as usize].node);
        for &g in &members {
            let Ok(node) = memo.groups[g]
                .node
                .with_new_children(|_| Rc::clone(&ranking))
            else {
                continue;
            };
            let reuse = ImplementationCandidate {
                implements: vec![GroupId(g as u32)],
                node: Rc::new(node),
                children: vec![ranking_group],
                origin: CandidateOrigin::TopKReuse,
                coupling_sites: Vec::new(),
                budget_share: 1,
            };
            push_new(memo, g, reuse);
        }
    }
}

/// The group whose node equals `node`, or a new one over `children`, read
/// by `queries`, with `node` itself as its one candidate.
fn group_for(
    memo: &mut Memo,
    node: &Rc<OperatorNode>,
    children: Vec<GroupId>,
    queries: &[usize],
) -> GroupId {
    if let Some(found) = find_group(&memo.groups, node) {
        return found;
    }
    let id = GroupId(memo.groups.len() as u32);
    for child in &children {
        memo.groups[child.0 as usize].parents.push(id);
    }
    memo.groups.push(MemoGroup {
        node: Rc::clone(node),
        children: children.clone(),
        parents: Vec::new(),
        queries: queries.to_vec(),
        roots_of: Vec::new(),
    });
    memo.candidates.push(vec![ImplementationCandidate {
        implements: vec![id],
        node: Rc::clone(node),
        children,
        origin: CandidateOrigin::TopKReuse,
        coupling_sites: Vec::new(),
        budget_share: 1,
    }]);
    id
}

/// The group whose node is `node` (the same node, or an equal one).
fn find_group(groups: &[MemoGroup], node: &Rc<OperatorNode>) -> Option<GroupId> {
    groups
        .iter()
        .position(|g| Rc::ptr_eq(&g.node, node))
        .or_else(|| groups.iter().position(|g| *g.node == **node))
        .map(|g| GroupId(g as u32))
}

/// Each group's node, by address.
fn group_of_node(groups: &[MemoGroup]) -> HashMap<*const OperatorNode, usize> {
    groups
        .iter()
        .enumerate()
        .map(|(g, group)| (Rc::as_ptr(&group.node), g))
        .collect()
}

/// `from` rewritten to `node`: the groups it implements and its sites
/// kept, its child groups those `node` reaches (the groups' nodes below
/// its root), or `from`'s when `from`'s node does not reach its children
/// by their nodes.
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
        budget_share: from.budget_share,
    }
}

/// The groups other than `implements` whose nodes `node` reaches below its
/// root without passing through another group's node, in discovery order.
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

/// Add `candidate` to group `g` unless an equal one (same node, sites and
/// origin) is there.
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

/// `node` with each node of `replaced` (by address) swapped.
fn replace(
    node: &Rc<OperatorNode>,
    replaced: &HashMap<*const OperatorNode, &Rc<OperatorNode>>,
    done: &mut HashMap<*const OperatorNode, Rc<OperatorNode>>,
) -> Result<Rc<OperatorNode>, asap_types::ir::SchemaDerivationError> {
    if let Some(found) = replaced.get(&Rc::as_ptr(node)) {
        return Ok(Rc::clone(found));
    }
    if let Some(found) = done.get(&Rc::as_ptr(node)) {
        return Ok(Rc::clone(found));
    }
    let mut children = HashMap::new();
    let mut changed = false;
    for child in node.children() {
        let new = replace(child, replaced, done)?;
        changed |= !Rc::ptr_eq(&new, child);
        children.insert(Rc::as_ptr(child), new);
    }
    let out = if changed {
        Rc::new(node.with_new_children(|c| Rc::clone(&children[&Rc::as_ptr(c)]))?)
    } else {
        Rc::clone(node)
    };
    done.insert(Rc::as_ptr(node), Rc::clone(&out));
    Ok(out)
}

/// The grid every query of `group` is evaluated on: the gcd of their
/// grids; `None` when one does not repeat at a fixed interval.
fn group_grid_ms(group: &MemoGroup, entries: &[QueryWorkloadEntry]) -> Option<u64> {
    group
        .queries
        .iter()
        .map(|&q| entries[q].evaluation_grid_ms())
        .try_fold(0, |grid, query| Some(greatest_common_divisor(grid, query?)))
        .filter(|&grid| grid > 0)
}

/// The size that serves readers of families `a` and `b`: either, for one
/// exact accumulator; a sketch of their algorithm sized for both; `None`
/// when they cannot be one build.
fn shared_size(a: &FieldDataType, b: &FieldDataType) -> Option<FieldDataType> {
    match (a, b) {
        (FieldDataType::Sketch(a, grouping), FieldDataType::Sketch(b, grouping_b))
            if a.algorithm() == b.algorithm() && grouping == grouping_b =>
        {
            let params = size_for_all_readers(&[a.params(), b.params()])?;
            Some(FieldDataType::Sketch(
                SketchKind::new(a.algorithm().clone(), params),
                grouping.clone(),
            ))
        }
        (FieldDataType::Sketch(..), _) | (_, FieldDataType::Sketch(..)) => None,
        _ => (a == b).then(|| a.clone()),
    }
}

/// `family` without its size: a sketch's algorithm and grouping, or the
/// family itself.
fn family_without_size(family: &FieldDataType) -> String {
    match family {
        FieldDataType::Sketch(kind, grouping) => format!("{:?}/{grouping:?}", kind.algorithm()),
        other => serde_json::to_string(other).unwrap_or_default(),
    }
}

fn hash_of<T: Hash>(value: &T) -> u64 {
    let mut hasher = DefaultHasher::new();
    value.hash(&mut hasher);
    hasher.finish()
}

#[cfg(test)]
mod tests {
    use super::*;
    use asap_logical_asap_optimization::per_computation_candidates::generate_per_computation_candidates;
    use asap_logical_asap_optimization::test_support::{entry_with, lower_promql};
    use asap_logical_asap_optimization::workload_candidates::stage1_logical_asap_optimization;
    use asap_types::ir::properties::AccuracyTarget;
    use asap_types::ir::schema::schema_support::with_promql_series_identity;
    use asap_types::ir::NonASAPOp;
    use asap_types::workload::{QueryRecurrence, RepeatedDemand, RepetitionInterval};

    fn repeating(interval_ms: u32) -> QueryWorkloadEntry {
        entry_with(
            AccuracyTarget::exact(),
            QueryRecurrence::Repeated(RepeatedDemand::FixedInterval(RepetitionInterval(
                interval_ms,
            ))),
        )
    }

    /// A memo by hand: one group per query (`queries[q]` repeating every
    /// `intervals_ms[q]`), its node the lowered query, its candidates every
    /// Pass 1 candidate of the query alone, each wholly private.
    fn memo(queries: &[&str], intervals_ms: &[u32]) -> (Memo, Vec<QueryWorkloadEntry>) {
        let entries: Vec<QueryWorkloadEntry> =
            intervals_ms.iter().map(|&ms| repeating(ms)).collect();
        let mut memo = Memo::default();
        for (q, query) in queries.iter().enumerate() {
            let node = lower_promql(query, AccuracyTarget::exact());
            let node = with_promql_series_identity(&node).unwrap();
            let inventory = generate_per_computation_candidates(vec![(
                q,
                QueryRoot::Operator(Rc::clone(&node)),
            )])
            .unwrap();
            let id = GroupId(q as u32);
            let candidates = stage1_logical_asap_optimization(&inventory, &entries[q..=q])
                .filter(|c| c.pass2.is_none())
                .filter_map(|c| match c.roots.ok()?.pop()? {
                    QueryRoot::Operator(node) => Some(node),
                    QueryRoot::Scalar(_) => None,
                })
                .map(|node| ImplementationCandidate {
                    implements: vec![id],
                    node,
                    children: Vec::new(),
                    origin: CandidateOrigin::Pass1,
                    coupling_sites: Vec::new(),
                    budget_share: 1,
                })
                .collect();
            memo.groups.push(MemoGroup {
                node,
                children: Vec::new(),
                parents: Vec::new(),
                queries: vec![q],
                roots_of: vec![q],
            });
            memo.candidates.push(candidates);
        }
        (memo, entries)
    }

    /// The pane-chain sites of group `g`'s candidates.
    fn chain_sites(memo: &Memo, g: usize) -> BTreeSet<CouplingSite> {
        memo.candidates[g]
            .iter()
            .flat_map(|c| c.coupling_sites.iter().copied())
            .filter(|s| s.kind == CouplingKind::PaneChain)
            .collect()
    }

    /// Sums over 5 m and 1 h, both every 60 s: each group gets its own pane
    /// candidate and one reading a shared chain, the two at one site,
    /// whose only grid is 60 s; the 1-hour threshold serves both, and a
    /// chain read at it merges the newest 5 of 60 panes.
    #[test]
    fn nested_windows_share_one_chain_site_on_one_grid() {
        let (mut memo, entries) = memo(
            &["sum_over_time(m[5m])", "sum_over_time(m[1h])"],
            &[60_000, 60_000],
        );
        add_window_composition(&mut memo, &entries);
        for g in 0..2 {
            assert!(memo.candidates[g]
                .iter()
                .any(|c| c.origin == CandidateOrigin::Panes && c.coupling_sites.is_empty()));
        }
        let site = chain_sites(&memo, 0);
        assert_eq!(site.len(), 1);
        assert_eq!(site, chain_sites(&memo, 1));
        let site = *site.first().unwrap();
        let thresholds = chain_thresholds(&memo, &site);
        let grids: BTreeSet<u64> = thresholds.iter().map(|t| t.pane_width_ms).collect();
        assert_eq!(grids, BTreeSet::from([60_000]));
        let hour = thresholds
            .iter()
            .find(|t| t.longest_window_ms == 3_600_000)
            .unwrap();
        let five_minutes = memo.candidates[0]
            .iter()
            .find(|c| c.coupling_sites.contains(&site))
            .unwrap();
        let reader = &chain_readers(&five_minutes.node)[0];
        assert!(may_read(hour, reader));
        let read = read_at_threshold(five_minutes, &site, hour).unwrap();
        let newest: Vec<(Option<u32>, u32)> = OperatorNode::reachable(&read)
            .iter()
            .filter_map(|n| match n.asap() {
                Some(ASAPOp::SummaryMerge {
                    children,
                    newest_panes,
                    ..
                }) => match children[0].asap() {
                    Some(ASAPOp::PaneChain { pane_count, .. }) => {
                        Some((*newest_panes, *pane_count))
                    }
                    _ => None,
                },
                _ => None,
            })
            .collect();
        assert_eq!(newest, vec![(Some(5), 60)]);
    }

    /// A 5-minute sum every 60 s (panes of 60 s) and a 1-hour sum every
    /// 300 s (panes of 300 s): grids 60 s and 300 s, and only at 60 s does
    /// a threshold serve both.
    #[test]
    fn readers_at_60_and_300_seconds_meet_on_the_60_second_grid() {
        let (mut memo, entries) = memo(
            &["sum_over_time(m[5m])", "sum_over_time(m[1h])"],
            &[60_000, 300_000],
        );
        add_window_composition(&mut memo, &entries);
        let site = *chain_sites(&memo, 0).first().unwrap();
        let thresholds = chain_thresholds(&memo, &site);
        let grids: BTreeSet<u64> = thresholds.iter().map(|t| t.pane_width_ms).collect();
        assert_eq!(grids, BTreeSet::from([60_000, 300_000]));
        let readers: Vec<ChainReader> = (0..2)
            .flat_map(|g| memo.candidates[g].iter())
            .filter(|c| c.coupling_sites.contains(&site))
            .flat_map(|c| chain_readers(&c.node))
            .collect();
        let both = |t: &ChainThreshold| readers.iter().all(|r| may_read(t, r));
        assert!(thresholds
            .iter()
            .any(|t| t.pane_width_ms == 60_000 && both(t)));
        assert!(!thresholds
            .iter()
            .any(|t| t.pane_width_ms == 300_000 && both(t)));
    }

    /// Sums over 5 m by `(job, instance)` and by `job`: the per-job group
    /// gets a rollup candidate that reads the finer group's output.
    #[test]
    fn a_rollup_candidate_reads_the_finer_group() {
        let (mut memo, entries) = memo(
            &[
                "sum by (job, instance) (sum_over_time(m[5m]))",
                "sum by (job) (sum_over_time(m[5m]))",
            ],
            &[60_000, 60_000],
        );
        add_pass2_candidates(&mut memo, &entries);
        let rollups: Vec<&ImplementationCandidate> = memo.candidates[1]
            .iter()
            .filter(|c| c.origin == CandidateOrigin::Rollup)
            .collect();
        assert!(!rollups.is_empty());
        for rollup in rollups {
            assert_eq!(rollup.children, vec![GroupId(0)]);
            assert!(OperatorNode::reachable(&rollup.node)
                .iter()
                .any(|n| Rc::ptr_eq(n, &memo.groups[0].node)));
        }
        assert!(memo.candidates[0]
            .iter()
            .all(|c| c.origin != CandidateOrigin::Rollup));
    }

    /// Exact top-5 and top-10 of one per-job sum: the ranking (`Limit(10)`
    /// over a `Sort`) is added as a group, and each top-k gets a candidate
    /// reading it; it cannot be chosen with a pane candidate.
    #[test]
    fn a_topk_reuse_candidate_reads_the_ranking() {
        let (mut memo, entries) = memo(
            &[
                "topk(5, sum by (job) (sum_over_time(m[5m])))",
                "topk(10, sum by (job) (sum_over_time(m[5m])))",
            ],
            &[60_000, 60_000],
        );
        add_pass2_candidates(&mut memo, &entries);
        let ranking = memo
            .groups
            .iter()
            .position(|g| {
                matches!(
                    g.node.non_asap(),
                    Some(NonASAPOp::Limit { n: Some(10), .. })
                )
            })
            .expect("a ranking group");
        for g in 0..2 {
            let reuse = memo.candidates[g]
                .iter()
                .find(|c| c.origin == CandidateOrigin::TopKReuse)
                .expect("a top-k reuse candidate");
            assert_eq!(reuse.children, vec![GroupId(ranking as u32)]);
            assert!(Rc::ptr_eq(
                reuse.node.children()[0],
                &memo.groups[ranking].node
            ));
            let panes = ImplementationCandidate {
                origin: CandidateOrigin::Panes,
                ..reuse.clone()
            };
            assert!(!combinable([reuse, &panes]));
            assert!(combinable([reuse]));
        }
    }
}

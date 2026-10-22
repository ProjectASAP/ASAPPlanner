//! The window-composition, rollup and exact top-k reuse transformations
//! (planner-layering design, "The stages over a memo of sub-expressions"):
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
//!   regrouping a finer group's values, or a coarse sketch by merging a
//!   finer group's build states (`rollup_candidates`' conditions).
//! - **Exact top-k reuse** ([`add_topk_reuse`]): a top-k answered from a
//!   ranking group, `Limit(k_max)` over a descending `Sort` of the input,
//!   added to the memo when missing.
//!
//! What the joint rules require across groups, the search checks on a
//! choice with [`requirements_met`]: no top-k reuse with panes, exact
//! inputs where a rule reads exact values, and each rollup reading the
//! build the joint rule would choose.

use std::collections::hash_map::DefaultHasher;
use std::collections::{BTreeSet, HashMap, HashSet};
use std::hash::{Hash, Hasher};
use std::rc::Rc;

use asap_logical_asap_optimization::accuracy::estimators::size_for_all_readers;
use asap_logical_asap_optimization::asap_aware_cse::{
    pane_candidates, rollup_candidates, rollup_pass2_candidates, topk_reuse_candidates,
    window_composition_pass2_candidates, with_pane_chain, with_resized_build, RollupCandidate,
};
use asap_logical_asap_optimization::per_computation_candidates::PerComputationCandidateError;
use asap_types::ir::flat::flatten;
use asap_types::ir::properties::AccuracyGuarantee;
use asap_types::ir::schema::sketch::SketchKind;
use asap_types::ir::schema::FieldDataType;
use asap_types::ir::{ASAPOp, OperatorNode, QueryRoot};
use asap_types::workload::{greatest_common_divisor, QueryWorkloadEntry};

use super::{
    CandidateOrigin, CouplingKind, CouplingSite, GroupId, ImplementationCandidate, Memo, MemoGroup,
    RollupRead,
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

/// Every transformation of this module, in order: window composition, a
/// coarser grouping reading a finer grouping's chain, rollup, exact top-k
/// reuse. `entries[q]` is workload query `q`'s entry.
pub(crate) fn add_pass2_candidates(memo: &mut Memo, entries: &[QueryWorkloadEntry]) {
    add_window_composition(memo, entries);
    add_rollup_windows(memo, entries);
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
        let Some(ASAPOp::SummaryMerge {
            children,
            newest_panes,
            ..
        }) = merge.asap()
        else {
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
            window_ms: pane_width_ms * u64::from(newest_panes.unwrap_or(*pane_count)),
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

/// `candidate`'s node (a sketch rollup reading the state of `site`, or
/// the finer group's candidate exposing it) with that build resized to
/// `threshold` ([`state_size_thresholds`], which the candidate may read:
/// [`may_read_state`]); every guarantee composed again.
pub(crate) fn resize_state_at(
    candidate: &ImplementationCandidate,
    site: &CouplingSite,
    threshold: &FieldDataType,
) -> Result<Rc<OperatorNode>, PerComputationCandidateError> {
    let build = match candidate
        .rollup_of
        .as_ref()
        .and_then(|r| r.finer_state.as_ref())
    {
        Some(state) => Some(Rc::clone(state)),
        None => OperatorNode::reachable(&candidate.node)
            .into_iter()
            .find(|n| is_sketch_build(n) && state_key(n) == site.key),
    };
    let Some(build) = build else {
        return Ok(Rc::clone(&candidate.node));
    };
    let roots = [QueryRoot::Operator(Rc::clone(&candidate.node))];
    let resized = with_resized_build(&roots, &build, threshold.clone())?;
    let [QueryRoot::Operator(node)] = &resized[..] else {
        unreachable!("an operator root");
    };
    Ok(Rc::clone(node))
}

/// A coarser grouping reading a finer grouping's pane chain (the
/// window-composition rule's rollup groups): for each Pass 1 (or
/// shared-build) candidate of a group with a sketch build over a SQL
/// table's rows whose readers keep a merged guarantee, and each such
/// candidate of another group building the same family, input, grouping
/// and filter over the same rows grouped by more keys over a window at
/// least as long, a [`CandidateOrigin::Panes`] candidate of the coarser
/// group that regroups the newest panes of the finer build's chain
/// (`window_composition_pass2_candidates` on the two, the coarser group's
/// pane width the finer build's grid). It carries the finer chain's
/// [`CouplingKind::PaneChain`] site, whose threshold sizes the chain for
/// every reader, and reads the finer group, which must build that chain:
/// the finer group gets a copy of its own pane candidate with the site
/// ([`ImplementationCandidate::requires_sites`]).
pub(crate) fn add_rollup_windows(memo: &mut Memo, entries: &[QueryWorkloadEntry]) {
    let group_of = group_of_node(&memo.groups);
    let grids: Vec<Option<u64>> = memo
        .groups
        .iter()
        .map(|group| group_grid_ms(group, entries))
        .collect();
    let base = |c: &ImplementationCandidate| {
        matches!(
            c.origin,
            CandidateOrigin::Pass1 | CandidateOrigin::SharedBuild
        )
    };
    let mut added: Vec<(usize, ImplementationCandidate)> = Vec::new();
    // Per finer group, the chain keys its pane candidates must expose.
    let mut hosts: Vec<(usize, u64)> = Vec::new();
    for (coarse_group, coarse_candidates) in memo.candidates.iter().enumerate() {
        let Some(coarse_grid) = grids[coarse_group] else {
            continue;
        };
        for coarse in coarse_candidates.iter().filter(|c| base(c)) {
            let coarse_root = [QueryRoot::Operator(Rc::clone(&coarse.node))];
            // The coarser build's own pane width.
            let own = pane_candidates(&coarse_root, &[Some(coarse_grid)]);
            let [own] = &own[..] else {
                continue;
            };
            for (finer_group, finer_candidates) in memo.candidates.iter().enumerate() {
                if finer_group == coarse_group || grids[finer_group].is_none() {
                    continue;
                }
                for finer in finer_candidates.iter().filter(|c| base(c)) {
                    let roots = [
                        QueryRoot::Operator(Rc::clone(&coarse.node)),
                        QueryRoot::Operator(Rc::clone(&finer.node)),
                    ];
                    let grids_of_pair = [Some(coarse_grid), Some(own.pane_width_ms)];
                    let panes = pane_candidates(&roots, &grids_of_pair);
                    if panes.len() != 2 {
                        continue;
                    }
                    let Ok(pass2_candidates) = window_composition_pass2_candidates(&roots, &panes)
                    else {
                        continue;
                    };
                    for pass2_candidate in pass2_candidates {
                        if pass2_candidate.pane_candidates.len() != 2
                            || pass2_candidate.shared_chains.is_empty()
                        {
                            continue;
                        }
                        let Ok(rewritten) = pass2_candidate.roots else {
                            continue;
                        };
                        let [QueryRoot::Operator(node), QueryRoot::Operator(finer_node)] =
                            &rewritten[..]
                        else {
                            continue;
                        };
                        // The finer build's chain, which the coarser one
                        // regroups.
                        let [finer_chain] = &chain_readers(finer_node)[..] else {
                            continue;
                        };
                        let [coarse_read] = &chain_readers(node)[..] else {
                            continue;
                        };
                        if coarse_read.key != finer_chain.key || !regroups(node, &coarse_read.merge)
                        {
                            continue;
                        }
                        let site = CouplingSite {
                            key: finer_chain.key,
                            kind: CouplingKind::PaneChain,
                        };
                        let mut candidate = rewritten_candidate(
                            &group_of,
                            coarse,
                            Rc::clone(node),
                            CandidateOrigin::Panes,
                        );
                        let finer_id = GroupId(finer_group as u32);
                        if !candidate.children.contains(&finer_id) {
                            candidate.children.push(finer_id);
                        }
                        candidate.coupling_sites.push(site);
                        candidate.requires_sites.push((finer_id, site));
                        added.push((coarse_group, candidate));
                        hosts.push((finer_group, site.key));
                    }
                }
            }
        }
    }
    for (g, candidate) in added {
        push_new(memo, g, candidate);
    }
    // Each finer group's own pane candidates building a host chain, with
    // its site.
    hosts.sort_unstable();
    hosts.dedup();
    for (g, key) in hosts {
        let site = CouplingSite {
            key,
            kind: CouplingKind::PaneChain,
        };
        let exposing: Vec<ImplementationCandidate> = memo.candidates[g]
            .iter()
            .filter(|c| c.origin == CandidateOrigin::Panes && !c.coupling_sites.contains(&site))
            .filter(|c| c.requires_sites.is_empty())
            .filter(|c| chain_readers(&c.node).iter().any(|r| r.key == key))
            .cloned()
            .map(|mut c| {
                c.coupling_sites
                    .retain(|s| s.kind != CouplingKind::PaneChain);
                c.coupling_sites.push(site);
                c
            })
            .collect();
        for candidate in exposing {
            push_new(memo, g, candidate);
        }
    }
}

/// Whether `merge` (over a pane chain) is read by a regrouping merge in
/// `node`: a coarser grouping's read of a finer chain.
fn regroups(node: &Rc<OperatorNode>, merge: &Rc<OperatorNode>) -> bool {
    OperatorNode::reachable(node).iter().any(|n| {
        matches!(n.asap(), Some(ASAPOp::SummaryMerge { children, group_by: Some(_), .. })
            if Rc::ptr_eq(&children[0], merge))
    })
}

/// Rollup: each coarse build of a group's Pass 1 (or shared-build)
/// candidate that another group's candidate can answer
/// (`rollup_candidates` over the two) gets a [`CandidateOrigin::Rollup`]
/// candidate of the coarse group:
///
/// - an exact accumulator regroups the finer group's finalized values: the
///   finer value is the finer group's output, which must be exact
///   ([`ImplementationCandidate::requires_exact_children`]);
/// - a sketch merges the finer build's states by the coarse keys: it reads
///   that state ([`ImplementationCandidate::rollup_of`]), and the
///   finer build is sized for both groups' readers, a
///   [`CouplingKind::SharedBuildSize`] site ([`state_size_thresholds`])
///   that the finer group's candidate also gets a copy with. The joint
///   rule rolls a coarse build up only from the finest build over its rows,
///   and every coarse build of one finest build together
///   ([`requirements_met`] checks both).
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
            let in_coarse = reached(&coarse.node);
            for (finer_group, finer_candidates) in memo.candidates.iter().enumerate() {
                if finer_group == coarse_group {
                    continue;
                }
                for finer in finer_candidates.iter().filter(|c| base(c)) {
                    let roots = [
                        QueryRoot::Operator(Rc::clone(&coarse.node)),
                        QueryRoot::Operator(Rc::clone(&finer.node)),
                    ];
                    let in_finer = reached(&finer.node);
                    for rollup in rollup_candidates(&roots) {
                        if !in_coarse.contains(&Rc::as_ptr(&rollup.coarse))
                            || in_coarse.contains(&Rc::as_ptr(&rollup.finer))
                            || !in_finer.contains(&Rc::as_ptr(&rollup.finer))
                        {
                            continue;
                        }
                        let made = if is_sketch_build(&rollup.finer) {
                            sketch_rollup(&group_of, coarse, finer_group, finer, &roots, rollup)
                        } else {
                            exact_rollup(
                                &memo.groups,
                                &group_of,
                                coarse,
                                finer_group,
                                finer,
                                &rollup,
                            )
                        };
                        added.extend(made.into_iter().map(|(g, c)| {
                            (
                                if g == FINER {
                                    finer_group
                                } else {
                                    coarse_group
                                },
                                c,
                            )
                        }));
                    }
                }
            }
        }
    }
    for (g, candidate) in added {
        push_new(memo, g, candidate);
    }
}

/// Marks a candidate [`sketch_rollup`] adds to the finer group.
const FINER: usize = usize::MAX;

/// The exact rollup of `rollup` (over `coarse`'s and `finer`'s nodes): the
/// coarse candidate with every finalized value of the coarse build
/// replaced by the regrouping of the finer group's output, as
/// `rollup_pass2_candidates` rewrites it. `finer` must be the finer
/// group's whole implementation (its node the finalized finer value).
fn exact_rollup(
    groups: &[MemoGroup],
    group_of: &HashMap<*const OperatorNode, usize>,
    coarse: &ImplementationCandidate,
    finer_group: usize,
    finer: &ImplementationCandidate,
    rollup: &RollupCandidate,
) -> Vec<(usize, ImplementationCandidate)> {
    if !Rc::ptr_eq(&rollup.finer, &finer.node) {
        return Vec::new();
    }
    let finer_output = &groups[finer_group].node;
    let to_group = HashMap::from([(Rc::as_ptr(&finer.node), finer_output)]);
    let Ok(rolled) = replace(&rollup.rollup, &to_group, &mut HashMap::new()) else {
        return Vec::new();
    };
    let by_rollup: HashMap<*const OperatorNode, &Rc<OperatorNode>> =
        OperatorNode::reachable(&coarse.node)
            .into_iter()
            .filter(|n| {
                matches!(n.asap(), Some(ASAPOp::FinalizeExactAccumulator { child })
                    if Rc::ptr_eq(child, &rollup.coarse))
            })
            .map(|n| (Rc::as_ptr(&n), &rolled))
            .collect();
    let Ok(node) = replace(&coarse.node, &by_rollup, &mut HashMap::new()) else {
        return Vec::new();
    };
    let mut candidate = rewritten_candidate(group_of, coarse, node, CandidateOrigin::Rollup);
    let finer_id = GroupId(finer_group as u32);
    if !candidate.children.contains(&finer_id) {
        candidate.children.push(finer_id);
    }
    candidate.requires_exact_children = vec![finer_id];
    candidate.rollup_of = Some(RollupRead {
        finer_group: finer_id,
        finer_state: None,
        coarse_candidate: Rc::clone(&coarse.node),
        coarse_build: Rc::clone(&rollup.coarse),
    });
    vec![(0, candidate)]
}

/// The sketch rollup of `rollup` (over `roots`: `coarse`'s and `finer`'s
/// nodes): the coarse candidate as `rollup_pass2_candidates` rewrites it
/// (the coarse build replaced by the regrouping merge of the finer build,
/// sized for both), reading the finer build's state, and the finer
/// candidate's copy that exposes that state; both with the build's
/// [`CouplingKind::SharedBuildSize`] site.
fn sketch_rollup(
    group_of: &HashMap<*const OperatorNode, usize>,
    coarse: &ImplementationCandidate,
    finer_group: usize,
    finer: &ImplementationCandidate,
    roots: &[QueryRoot],
    rollup: RollupCandidate,
) -> Vec<(usize, ImplementationCandidate)> {
    let site = CouplingSite {
        key: state_key(&rollup.finer),
        kind: CouplingKind::SharedBuildSize,
    };
    let coarse_build = Rc::clone(&rollup.coarse);
    let Ok(mut pass2) = rollup_pass2_candidates(roots, &[rollup]) else {
        return Vec::new();
    };
    let Some(Ok(rolled)) = pass2.pop().map(|p| p.roots) else {
        return Vec::new();
    };
    let QueryRoot::Operator(node) = &rolled[0] else {
        return Vec::new();
    };
    // The resized finer build the regrouping merge reads.
    let Some(finer_build) =
        OperatorNode::reachable(node)
            .into_iter()
            .find_map(|n| match n.asap() {
                Some(ASAPOp::SummaryMerge {
                    children,
                    group_by: Some(_),
                    ..
                }) if is_sketch_build(&children[0]) => Some(Rc::clone(&children[0])),
                _ => None,
            })
    else {
        return Vec::new();
    };
    let mut candidate =
        rewritten_candidate(group_of, coarse, Rc::clone(node), CandidateOrigin::Rollup);
    // Its private nodes stop at the finer build, which the finer group
    // builds: the walk below it found the finer group's children.
    candidate.children = coarse.children.clone();
    let finer_id = GroupId(finer_group as u32);
    candidate.children.push(finer_id);
    candidate.coupling_sites.push(site);
    candidate.rollup_of = Some(RollupRead {
        finer_group: finer_id,
        finer_state: Some(finer_build),
        coarse_candidate: Rc::clone(&coarse.node),
        coarse_build,
    });
    candidate.requires_sites.push((finer_id, site));
    let mut exposed = finer.clone();
    exposed.coupling_sites.push(site);
    vec![(0, candidate), (FINER, exposed)]
}

/// Whether `node` is a sketch `SummaryAgg`.
fn is_sketch_build(node: &OperatorNode) -> bool {
    matches!(
        node.asap(),
        Some(ASAPOp::SummaryAgg {
            family: FieldDataType::Sketch(..),
            ..
        })
    )
}

/// The key of a sketch build's state, its size masked: the build with its
/// family's algorithm and grouping only.
fn state_key(build: &OperatorNode) -> u64 {
    let mut value = serde_json::to_value(&build.operator).unwrap_or_default();
    if let (Some(ASAPOp::SummaryAgg { family, .. }), Some(agg)) = (
        build.asap(),
        value
            .pointer_mut("/ASAP/SummaryAgg")
            .and_then(|v| v.as_object_mut()),
    ) {
        agg.insert(
            "family".to_string(),
            serde_json::Value::String(family_without_size(family)),
        );
    }
    hash_of(&format!("rollup state {value}"))
}

/// The sizes the finer build of a sketch rollup's `site` may take: the
/// shared size ([`size_for_all_readers`]) of every set of its readers'
/// sizes (the finer group's own build and each rolled-up coarse build),
/// without one larger than the shared size of the readers it admits.
pub(crate) fn state_size_thresholds(memo: &Memo, site: &CouplingSite) -> Vec<FieldDataType> {
    let readers: Vec<FieldDataType> = memo
        .candidates
        .iter()
        .flatten()
        .filter(|c| c.coupling_sites.contains(site))
        .filter_map(|c| state_reader_size(c, site))
        .collect();
    let mut sizes: Vec<FieldDataType> = Vec::new();
    for reader in &readers {
        let mut grown: Vec<FieldDataType> = sizes
            .iter()
            .filter_map(|size| shared_size(size, reader))
            .collect();
        grown.push(reader.clone());
        for size in grown {
            if !sizes.iter().any(|known| same_size(known, &size)) {
                sizes.push(size);
            }
        }
    }
    sizes
        .into_iter()
        .filter(|threshold| {
            let joined = readers
                .iter()
                .filter(|r| no_stricter(r, threshold))
                .try_fold(None::<FieldDataType>, |acc, r| {
                    Some(Some(match acc {
                        None => r.clone(),
                        Some(acc) => shared_size(&acc, r)?,
                    }))
                });
            matches!(joined, Some(Some(joined)) if same_size(&joined, threshold))
        })
        .collect()
}

/// Whether `site` is a sketch rollup's state ([`sketch_rollup`]): a
/// rollup candidate of `memo` reads it.
pub(crate) fn is_rollup_state(memo: &Memo, site: &CouplingSite) -> bool {
    memo.candidates.iter().flatten().any(|c| {
        c.rollup_of
            .as_ref()
            .is_some_and(|r| r.finer_state.is_some())
            && c.coupling_sites.contains(site)
    })
}

/// Whether `candidate`, a reader of the sketch rollup state `site`, may
/// read it at size `threshold`: its size is no stricter.
pub(crate) fn may_read_state(
    threshold: &FieldDataType,
    candidate: &ImplementationCandidate,
    site: &CouplingSite,
) -> bool {
    state_reader_size(candidate, site).is_some_and(|size| no_stricter(&size, threshold))
}

/// The size a reader of the rollup state `site` needs: a rollup's coarse
/// build's, or the finer group's own build's.
fn state_reader_size(
    candidate: &ImplementationCandidate,
    site: &CouplingSite,
) -> Option<FieldDataType> {
    let build = match &candidate.rollup_of {
        Some(read) => Rc::clone(&read.coarse_build),
        None => OperatorNode::reachable(&candidate.node)
            .into_iter()
            .find(|n| is_sketch_build(n) && state_key(n) == site.key)?,
    };
    match build.asap() {
        Some(ASAPOp::SummaryAgg { family, .. }) => Some(family.clone()),
        _ => None,
    }
}

/// Whether a build of size `size` serves a reader needing `required`.
fn no_stricter(required: &FieldDataType, size: &FieldDataType) -> bool {
    shared_size(size, required).is_some_and(|joined| same_size(&joined, size))
}

/// Equal families and sizes, the recorded failure probabilities and errors
/// included (which `SketchParams` equality ignores).
fn same_size(a: &FieldDataType, b: &FieldDataType) -> bool {
    a == b && format!("{a:?}") == format!("{b:?}")
}

/// Whether the candidates `chosen` (one per group, with every group they
/// name and every group with a summary build over the same rows as a
/// rolled-up build, in workload order) meet each other's requirements:
///
/// - never an exact top-k reuse with panes ([`combinable`]);
/// - every group in a candidate's `requires_exact_children` chose an exact
///   implementation ([`is_exact`]), and every group in its
///   `requires_sites` one carrying that site;
/// - every rollup reads the build the joint rule would roll its coarse
///   build up from, over the chosen plan with each rollup undone: an exact
///   accumulator's coarsest finer build, a sketch's finest, in its finer
///   group's chosen implementation;
/// - a sketch rollup's finer group chose a candidate exposing the state at
///   the same site, and every chosen coarse build whose finest build that is
///   rolls up too (the joint rule's choice of one finest build).
pub(crate) fn requirements_met(chosen: &[(GroupId, &ImplementationCandidate)]) -> bool {
    if !combinable(chosen.iter().map(|(_, c)| *c)) {
        return false;
    }
    let of = |g: GroupId| chosen.iter().find(|(h, _)| *h == g).map(|(_, c)| *c);
    for (_, candidate) in chosen {
        for &g in &candidate.requires_exact_children {
            if !of(g).is_some_and(is_exact) {
                return false;
            }
        }
        for (g, site) in &candidate.requires_sites {
            if !of(*g).is_some_and(|c| c.coupling_sites.contains(site)) {
                return false;
            }
        }
    }
    let rolled: Vec<(&ImplementationCandidate, &RollupRead)> = chosen
        .iter()
        .filter_map(|(_, c)| Some((*c, c.rollup_of.as_ref()?)))
        .collect();
    if rolled.is_empty() {
        return true;
    }
    // The joint rule's rollups over the chosen plan, each rollup undone.
    let roots: Vec<QueryRoot> = chosen
        .iter()
        .map(|(_, c)| match &c.rollup_of {
            Some(read) => QueryRoot::Operator(Rc::clone(&read.coarse_candidate)),
            None => QueryRoot::Operator(Rc::clone(&c.node)),
        })
        .collect();
    let source: Vec<(*const OperatorNode, Rc<OperatorNode>)> = rollup_candidates(&roots)
        .into_iter()
        .map(|r| (Rc::as_ptr(&r.coarse), r.finer))
        .collect();
    let mut used_state: HashSet<*const OperatorNode> = HashSet::new();
    let mut rolled_coarse: HashSet<*const OperatorNode> = HashSet::new();
    for &(reader, read) in &rolled {
        let Some(finer) = of(read.finer_group) else {
            return false;
        };
        let Some((_, build)) = source
            .iter()
            .find(|(coarse, _)| *coarse == Rc::as_ptr(&read.coarse_build))
        else {
            return false;
        };
        if !reached(&finer.node).contains(&Rc::as_ptr(build)) {
            return false;
        }
        rolled_coarse.insert(Rc::as_ptr(&read.coarse_build));
        if read.finer_state.is_none() {
            continue;
        }
        // The state the reader reads is the joint rule's finer build's: its
        // site (keyed by the memo's build, as [`sketch_rollup`] keys it, not
        // by the resized copy, whose recomposed guarantees change its key),
        // which the finer group's chosen candidate exposes.
        let site = CouplingSite {
            key: state_key(build),
            kind: CouplingKind::SharedBuildSize,
        };
        if !reader.requires_sites.contains(&(read.finer_group, site))
            || !finer.coupling_sites.contains(&site)
        {
            return false;
        }
        used_state.insert(Rc::as_ptr(build));
    }
    source
        .iter()
        .filter(|(_, build)| used_state.contains(&Rc::as_ptr(build)))
        .all(|(coarse, _)| rolled_coarse.contains(coarse))
}

/// Whether `candidate` computes exactly: no node it reaches builds an
/// approximate summary (every summary an exact accumulator).
pub(crate) fn is_exact(candidate: &ImplementationCandidate) -> bool {
    OperatorNode::reachable(&candidate.node).iter().all(|n| {
        !matches!(
            n.asap(),
            Some(
                ASAPOp::SummaryAgg { family, .. } | ASAPOp::PaneChain { family, .. }
            ) if !matches!(family, FieldDataType::ExactAggregate(..))
        )
    })
}

/// The addresses of every node `node` reaches.
fn reached(node: &Rc<OperatorNode>) -> HashSet<*const OperatorNode> {
    OperatorNode::reachable(node)
        .iter()
        .map(Rc::as_ptr)
        .collect()
}

/// Exact top-k reuse: the groups that are top-k computations of one input
/// group and partition with at least two different `k`
/// (`topk_reuse_candidates` over the groups' nodes) each get a [`CandidateOrigin::TopKReuse`] candidate
/// reading the ranking group, `Limit(k_max)` over a descending `Sort` of
/// the input group (the top-k group's child). The ranking's two groups
/// are added when the memo has no equal ones, each with its one candidate
/// (also of origin `TopKReuse`). The rule reads an exact input: the input
/// group's chosen implementation must be exact
/// ([`ImplementationCandidate::requires_exact_children`])..
/// Appended groups come after their readers; [`Memo::topological_order`]
/// orders them.
pub(crate) fn add_topk_reuse(memo: &mut Memo) {
    // The groups' nodes, each input marked exact: the rule reads an exact
    // input, which `requires_exact_children` asks of the input group.
    let mut group_of_copy: HashMap<*const OperatorNode, usize> = HashMap::new();
    let mut roots: Vec<QueryRoot> = Vec::new();
    for (g, group) in memo.groups.iter().enumerate() {
        let [input] = &group.node.children()[..] else {
            continue;
        };
        let exact_input = Rc::new(OperatorNode::clone(input).with_guarantee(Some(
            AccuracyGuarantee::exact("the input group chosen exact"),
        )));
        let Ok(copy) = group.node.with_new_children(|_| Rc::clone(&exact_input)) else {
            continue;
        };
        let copy = Rc::new(copy);
        group_of_copy.insert(Rc::as_ptr(&copy), g);
        roots.push(QueryRoot::Operator(copy));
    }
    for candidate in topk_reuse_candidates(&roots) {
        let mut members: Vec<usize> = candidate
            .members
            .iter()
            .filter_map(|m| group_of_copy.get(&Rc::as_ptr(m)).copied())
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
        let Some(&input_group) = memo.groups[members[0]].children.first() else {
            continue;
        };
        let input = Rc::clone(&memo.groups[input_group.0 as usize].node);
        let sort = candidate.ranking.children()[0];
        let Ok(sort) = sort.with_new_children(|_| Rc::clone(&input)) else {
            continue;
        };
        let sort = Rc::new(sort);
        let sort_group = group_for(memo, &sort, vec![input_group], &queries);
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
                materializations: Vec::new(),
                budget_share: 1,
                requires_exact_children: vec![input_group],
                rollup_of: None,
                requires_sites: Vec::new(),
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
        materializations: Vec::new(),
        budget_share: 1,
        requires_exact_children: Vec::new(),
        rollup_of: None,
        requires_sites: Vec::new(),
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
        materializations: from.materializations.clone(),
        budget_share: from.budget_share,
        requires_exact_children: Vec::new(),
        rollup_of: None,
        requires_sites: Vec::new(),
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
        let exact: Vec<(&str, AccuracyTarget)> = queries
            .iter()
            .map(|&q| (q, AccuracyTarget::exact()))
            .collect();
        memo_with(&exact, intervals_ms)
    }

    /// [`memo`] with each query's own accuracy target.
    fn memo_with(
        queries: &[(&str, AccuracyTarget)],
        intervals_ms: &[u32],
    ) -> (Memo, Vec<QueryWorkloadEntry>) {
        let nodes = queries
            .iter()
            .map(|(query, target)| {
                with_promql_series_identity(&lower_promql(query, target.clone())).unwrap()
            })
            .collect();
        memo_of_nodes(nodes, intervals_ms)
    }

    /// [`memo`] of query roots `nodes`, already lowered.
    fn memo_of_nodes(
        nodes: Vec<Rc<OperatorNode>>,
        intervals_ms: &[u32],
    ) -> (Memo, Vec<QueryWorkloadEntry>) {
        let entries: Vec<QueryWorkloadEntry> =
            intervals_ms.iter().map(|&ms| repeating(ms)).collect();
        let mut memo = Memo::default();
        for (q, node) in nodes.into_iter().enumerate() {
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
                    materializations: Vec::new(),
                    budget_share: 1,
                    requires_exact_children: Vec::new(),
                    rollup_of: None,
                    requires_sites: Vec::new(),
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

    /// Exact top-5 and top-10 of one per-job sum (the memo of the
    /// workload): the ranking (`Limit(10)` over a `Sort` of the sum group)
    /// is added as a group before both top-k groups, and each top-k gets a
    /// candidate reading it that requires the sum group exact; it cannot be
    /// chosen with a pane candidate.
    #[test]
    fn a_topk_reuse_candidate_reads_the_ranking() {
        let queries = [
            "topk(5, sum by (job) (sum_over_time(m[5m])))",
            "topk(10, sum by (job) (sum_over_time(m[5m])))",
        ];
        let roots = queries
            .iter()
            .map(|q| QueryRoot::Operator(lower_promql(q, AccuracyTarget::exact())))
            .enumerate()
            .collect();
        let mut memo =
            super::super::groups::memo_of(&generate_per_computation_candidates(roots).unwrap());
        let entries = vec![repeating(60_000); 2];
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
        let topk: Vec<usize> = (0..memo.groups.len())
            .filter(|&g| {
                memo.candidates[g]
                    .iter()
                    .any(|c| c.origin == CandidateOrigin::TopKReuse && g != ranking)
                    && !memo.groups[g].roots_of.is_empty()
            })
            .collect();
        assert_eq!(topk.len(), 2);
        for g in topk {
            let order = memo.topological_order().unwrap();
            let at = |g: usize| order.iter().position(|x| x.0 as usize == g).unwrap();
            assert!(at(ranking) < at(g), "children before parents");
            let reuse = memo.candidates[g]
                .iter()
                .find(|c| c.origin == CandidateOrigin::TopKReuse)
                .unwrap();
            assert_eq!(reuse.children, vec![GroupId(ranking as u32)]);
            assert!(Rc::ptr_eq(
                reuse.node.children()[0],
                &memo.groups[ranking].node
            ));
            let input = memo.groups[g].children[0];
            assert_eq!(reuse.requires_exact_children, vec![input]);
            assert!(at(input.0 as usize) < at(ranking));
            let panes = ImplementationCandidate {
                origin: CandidateOrigin::Panes,
                ..reuse.clone()
            };
            assert!(!combinable([reuse, &panes]));
            let exact_input = memo.candidates[input.0 as usize]
                .iter()
                .find(|c| is_exact(c))
                .unwrap();
            assert!(requirements_met(&[
                (GroupId(g as u32), reuse),
                (input, exact_input)
            ]));
            assert!(!requirements_met(&[(GroupId(g as u32), reuse)]));
        }
    }

    /// An exact rollup requires the finer group exact and implemented by
    /// the build it rolls up from: met with the finer group's grouped
    /// exact accumulator, not with its per-series one (no rollup source),
    /// nor with a sketch implementation chosen.
    #[test]
    fn an_exact_rollup_requires_the_finer_group_exact() {
        let (mut memo, _) = memo(
            &[
                "sum by (job, instance) (sum_over_time(m[5m]))",
                "sum by (job) (sum_over_time(m[5m]))",
            ],
            &[60_000, 60_000],
        );
        add_rollup(&mut memo);
        let rollup = memo.candidates[1]
            .iter()
            .find(|c| c.origin == CandidateOrigin::Rollup)
            .unwrap();
        assert_eq!(rollup.requires_exact_children, vec![GroupId(0)]);
        let met: Vec<bool> = memo.candidates[0]
            .iter()
            .map(|finer| requirements_met(&[(GroupId(0), finer), (GroupId(1), rollup)]))
            .collect();
        assert!(met.contains(&true) && met.contains(&false), "{met:?}");
        let source = &memo.candidates[0][met.iter().position(|&m| m).unwrap()];
        assert!(is_exact(source));
        // A KLL quantile stands in for an approximate implementation.
        let (sketched, _) = memo_with(
            &[(
                "quantile_over_time(0.9, m[5m])",
                AccuracyTarget::max_error(0.01),
            )],
            &[60_000],
        );
        let approximate = sketched.candidates[0]
            .iter()
            .find(|c| !is_exact(c))
            .unwrap();
        assert!(!requirements_met(&[
            (GroupId(0), approximate),
            (GroupId(1), rollup)
        ]));
    }

    /// KLL quantiles by `(host, job)` at ε 0.01 and by `job` at 0.001: the
    /// per-job group gets a rollup reading the finer group's KLL state, the
    /// finer group a copy exposing it, both at one size site whose largest
    /// threshold is the coarse reader's (the larger `k`); the requirement
    /// holds only with the exposing copy chosen.
    #[test]
    fn a_sketch_rollup_reads_the_finer_groups_state() {
        let (mut memo, _) = memo_with(
            &[
                (
                    "quantile by (host, job) (0.5, data)",
                    AccuracyTarget::max_error(0.01),
                ),
                (
                    "quantile by (job) (0.5, data)",
                    AccuracyTarget::max_error(0.001),
                ),
            ],
            &[60_000, 60_000],
        );
        add_rollup(&mut memo);
        let rollup = memo.candidates[1]
            .iter()
            .find(|c| {
                c.rollup_of
                    .as_ref()
                    .is_some_and(|r| r.finer_state.is_some())
            })
            .expect("a sketch rollup");
        let read = rollup.rollup_of.as_ref().unwrap();
        assert_eq!(read.finer_group, GroupId(0));
        assert!(rollup.children.contains(&GroupId(0)));
        let site = *rollup
            .coupling_sites
            .iter()
            .find(|s| s.kind == CouplingKind::SharedBuildSize)
            .unwrap();
        let exposed = memo.candidates[0]
            .iter()
            .find(|c| c.coupling_sites.contains(&site))
            .expect("the finer group exposes the state");
        let plain = memo.candidates[0]
            .iter()
            .find(|c| c.coupling_sites.is_empty() && Rc::ptr_eq(&c.node, &exposed.node))
            .unwrap();
        assert!(requirements_met(&[
            (GroupId(0), exposed),
            (GroupId(1), rollup)
        ]));
        assert!(!requirements_met(&[
            (GroupId(0), plain),
            (GroupId(1), rollup)
        ]));
        let k = |family: &FieldDataType| match family {
            FieldDataType::Sketch(kind, _) => format!("{:?}", kind.params()),
            _ => String::new(),
        };
        let thresholds = state_size_thresholds(&memo, &site);
        let coarse_size = state_reader_size(rollup, &site).unwrap();
        assert!(thresholds.iter().any(|t| k(t) == k(&coarse_size)));
        assert!(thresholds.iter().all(|t| may_read_state(t, exposed, &site)
            || !no_stricter(&state_reader_size(exposed, &site).unwrap(), t)));
        let largest = thresholds
            .iter()
            .find(|t| may_read_state(t, rollup, &site) && may_read_state(t, exposed, &site))
            .expect("one size serves both");
        assert_eq!(k(largest), k(&coarse_size));
    }

    /// Three KLL groupings `job` ⊂ `(job, instance)` ⊂ `(job, instance,
    /// zone)`: the joint rule rolls both coarse builds up from the finest
    /// together. Rolling one alone, or rolling from the middle build while
    /// the finest is chosen, fails the requirement.
    #[test]
    fn sketch_rollups_follow_the_finest_build_all_or_nothing() {
        let target = AccuracyTarget::max_error(0.01);
        let (mut memo, _) = memo_with(
            &[
                ("quantile by (job) (0.9, m)", target.clone()),
                ("quantile by (job, instance) (0.9, m)", target.clone()),
                ("quantile by (job, instance, zone) (0.9, m)", target),
            ],
            &[60_000, 60_000, 60_000],
        );
        add_rollup(&mut memo);
        let (job, instance, zone) = (0, 1, 2);
        let order = memo.topological_order().unwrap();
        let at = |g: usize| order.iter().position(|x| x.0 as usize == g).unwrap();
        assert!(
            at(zone) < at(instance) && at(instance) < at(job),
            "finer first"
        );
        let rolled_from = |g: usize, from: usize| {
            memo.candidates[g]
                .iter()
                .find(|c| {
                    c.rollup_of.as_ref().is_some_and(|r| {
                        r.finer_state.is_some() && r.finer_group == GroupId(from as u32)
                    })
                })
                .unwrap_or_else(|| panic!("group {g} rolls up from {from}"))
        };
        let exposing = |g: usize, rollup: &ImplementationCandidate| {
            let site = rollup
                .coupling_sites
                .iter()
                .find(|s| s.kind == CouplingKind::SharedBuildSize)
                .unwrap();
            memo.candidates[g]
                .iter()
                .find(|c| c.rollup_of.is_none() && c.coupling_sites.contains(site))
                .unwrap()
        };
        let id = |g: usize| GroupId(g as u32);
        let job_from_zone = rolled_from(job, zone);
        let instance_from_zone = rolled_from(instance, zone);
        let zone_exposed = exposing(zone, job_from_zone);
        assert!(requirements_met(&[
            (id(job), job_from_zone),
            (id(instance), instance_from_zone),
            (id(zone), zone_exposed),
        ]));
        // The `(job, instance)` build itself, sketched, not rolled up.
        let instance_own = memo.candidates[instance]
            .iter()
            .find(|c| c.origin == CandidateOrigin::Pass1 && !is_exact(c))
            .unwrap();
        assert!(!requirements_met(&[
            (id(job), job_from_zone),
            (id(instance), instance_own),
            (id(zone), zone_exposed),
        ]));
        // From the middle build while the finest is chosen.
        let job_from_instance = rolled_from(job, instance);
        let instance_exposed = exposing(instance, job_from_instance);
        let zone_own = memo.candidates[zone]
            .iter()
            .find(|c| c.origin == CandidateOrigin::Pass1 && !is_exact(c))
            .unwrap();
        assert!(!requirements_met(&[
            (id(job), job_from_instance),
            (id(instance), instance_exposed),
            (id(zone), zone_own),
        ]));
        // Without the finest build, the middle one is the finest.
        assert!(requirements_met(&[
            (id(job), job_from_instance),
            (id(instance), instance_exposed),
        ]));
    }

    /// The workload of #878 over a SQL table `metrics(ts, city, value)`,
    /// every 10 s at ε = 0.01: the global median over 5 minutes and the
    /// per-city p90 over 20 minutes. The global group gets a candidate that
    /// regroups the newest 30 of the per-city chain's 120 panes of 10 s,
    /// at the per-city chain's site, which the per-city group must build;
    /// one threshold (10 s, 20 minutes) serves both.
    #[test]
    fn a_coarser_grouping_reads_the_finer_groupings_chain() {
        use asap_logical_asap_optimization::test_support::{agg, scan_from, time_range};
        use asap_types::ir::operator::{AggIntent, Source};
        use asap_types::ir::schema::{DataType, Field, Schema};
        use std::time::Duration;
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
        let quantile = |q: f64| AggIntent::Quantile {
            col: Some(2),
            q,
            accuracy: AccuracyTarget::max_error(0.01),
        };
        let minutes = |m: u64| Duration::from_secs(60 * m);
        let global = agg(vec![], quantile(0.5), time_range(minutes(5), scan()));
        let per_city = agg(vec![1], quantile(0.9), time_range(minutes(20), scan()));
        let (mut memo, entries) = memo_of_nodes(vec![global, per_city], &[10_000, 10_000]);
        add_window_composition(&mut memo, &entries);
        add_rollup_windows(&mut memo, &entries);
        let reads: Vec<&ImplementationCandidate> = memo.candidates[0]
            .iter()
            .filter(|c| !c.requires_sites.is_empty())
            .collect();
        assert!(!reads.is_empty());
        for read in &reads {
            assert_eq!(read.origin, CandidateOrigin::Panes);
            let (host, site) = read.requires_sites[0];
            assert_eq!(host, GroupId(1));
            assert!(read.children.contains(&GroupId(1)));
            assert!(read.coupling_sites.contains(&site));
            let [reader] = &chain_readers(&read.node)[..] else {
                panic!("one chain read");
            };
            assert!(regroups(&read.node, &reader.merge));
            assert_eq!((reader.pane_width_ms, reader.window_ms), (10_000, 300_000));
            let host_candidate = memo.candidates[1]
                .iter()
                .find(|c| c.coupling_sites.contains(&site) && chain_readers(&c.node).len() == 1)
                .expect("the per-city group builds the chain");
            assert!(requirements_met(&[
                (GroupId(0), read),
                (GroupId(1), host_candidate)
            ]));
            let own = memo.candidates[1]
                .iter()
                .find(|c| c.origin == CandidateOrigin::Pass1)
                .unwrap();
            assert!(!requirements_met(&[(GroupId(0), read), (GroupId(1), own)]));
            let threshold = chain_thresholds(&memo, &site)
                .into_iter()
                .find(|t| {
                    t.pane_width_ms == 10_000
                        && t.longest_window_ms == 1_200_000
                        && may_read(t, reader)
                        && chain_readers(&host_candidate.node)
                            .iter()
                            .all(|r| may_read(t, r))
                })
                .expect("one threshold serves both");
            let at = read_at_threshold(read, &site, &threshold).unwrap();
            let [at_reader] = &chain_readers(&at)[..] else {
                panic!("one chain read");
            };
            assert_eq!(at_reader.window_ms, 300_000);
            assert!(regroups(&at, &at_reader.merge));
        }
    }

    /// A sketch rollup and its finer group's exposing candidate, resized to
    /// a size threshold, both build the finer KLL at that size.
    #[test]
    fn resizing_the_rolled_up_state_to_a_threshold() {
        let (mut memo, _) = memo_with(
            &[
                (
                    "quantile by (host, job) (0.5, data)",
                    AccuracyTarget::max_error(0.01),
                ),
                (
                    "quantile by (job) (0.5, data)",
                    AccuracyTarget::max_error(0.001),
                ),
            ],
            &[60_000, 60_000],
        );
        add_rollup(&mut memo);
        let rollup = memo.candidates[1]
            .iter()
            .find(|c| {
                c.rollup_of
                    .as_ref()
                    .is_some_and(|r| r.finer_state.is_some())
            })
            .unwrap();
        let site = rollup.requires_sites[0].1;
        let exposed = memo.candidates[0]
            .iter()
            .find(|c| c.coupling_sites.contains(&site))
            .unwrap();
        let sizes = |node: &Rc<OperatorNode>| -> Vec<String> {
            OperatorNode::reachable(node)
                .iter()
                .filter(|n| is_sketch_build(n) && state_key(n) == site.key)
                .map(|n| match n.asap() {
                    Some(ASAPOp::SummaryAgg { family, .. }) => format!("{family:?}"),
                    _ => unreachable!(),
                })
                .collect()
        };
        let mut resized = 0;
        for threshold in state_size_thresholds(&memo, &site) {
            let expected = vec![format!("{threshold:?}")];
            for candidate in [exposed, rollup] {
                if may_read_state(&threshold, candidate, &site) {
                    let node = resize_state_at(candidate, &site, &threshold).unwrap();
                    assert_eq!(sizes(&node), expected);
                    resized += 1;
                }
            }
        }
        // The larger threshold serves both, the smaller only the finer group.
        assert_eq!(resized, 3);
    }
}

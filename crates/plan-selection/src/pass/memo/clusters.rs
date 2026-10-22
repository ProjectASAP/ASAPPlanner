//! Sharing clusters and the exact joint choice in each (steps 4–6).
//!
//! Two groups are in one cluster when a kept candidate of each exposes a
//! site with one meeting key: a Pass 2 rule could make them one node. A
//! cluster of one group selects its cheapest kept candidate. A larger
//! cluster tries every combination of its groups' kept candidates (their
//! roots; Stage 2 chooses every materialization again): each combination's
//! roots, with the cross-group window-composition, rollup and top-k reuse
//! rewrites ([`derived_variants`]), each followed by the cross-group
//! identical-expression and summary-capability sharing
//! ([`shared_variants`]), is implemented in every Stage 2 materialization
//! and priced by Stage 3 over the cluster's queries; the cheapest wins.

use std::collections::HashMap;

use asap_physical_asap_optimization::{optimize_physical_asap, PhysicalCandidate};
use asap_types::ir::flat::flatten;
use asap_types::ir::QueryRoot;
use asap_types::workload::{DataWorkload, QueryWorkloadEntry};

use super::derivations::derived_variants;
use super::groups::MemoGroups;
use super::sharing::shared_variants;
use super::{ClusterSelection, GroupCandidate};
use crate::{PlanSelector, PlanningModels, SelectionError};

/// The clusters of `groups` whose kept candidates (`candidates[g]`, priced,
/// undominated) have sites that can meet, each selected exactly. A cluster
/// whose groups have no valid combination is an error.
pub(crate) fn select_clusters(
    groups: &MemoGroups,
    candidates: &[Vec<GroupCandidate>],
    entries: &[QueryWorkloadEntry],
    data: &DataWorkload,
    models: PlanningModels<'_>,
) -> Result<Vec<ClusterSelection>, SelectionError> {
    clusters(candidates)
        .into_iter()
        .map(|cluster| select_cluster(groups, &cluster, candidates, entries, data, models))
        .collect()
}

/// The groups linked by a common meeting key among their kept candidates'
/// sites, each cluster in group order, ordered by its first group.
pub(crate) fn clusters(candidates: &[Vec<GroupCandidate>]) -> Vec<Vec<usize>> {
    let mut parent: Vec<usize> = (0..candidates.len()).collect();
    fn find(parent: &mut [usize], g: usize) -> usize {
        let mut root = g;
        while parent[root] != root {
            root = parent[root];
        }
        let mut g = g;
        while parent[g] != root {
            let next = parent[g];
            parent[g] = root;
            g = next;
        }
        root
    }
    let mut first_with_key: HashMap<u64, usize> = HashMap::new();
    for (group, kept) in candidates.iter().enumerate() {
        for site in kept.iter().flat_map(|candidate| &candidate.signature.0) {
            let other = *first_with_key.entry(site.meeting_key).or_insert(group);
            let (a, b) = (find(&mut parent, group), find(&mut parent, other));
            parent[a.max(b)] = a.min(b);
        }
    }
    let mut clusters: Vec<Vec<usize>> = Vec::new();
    let mut index_of: HashMap<usize, usize> = HashMap::new();
    for group in 0..candidates.len() {
        let root = find(&mut parent, group);
        let index = *index_of.entry(root).or_insert_with(|| {
            clusters.push(Vec::new());
            clusters.len() - 1
        });
        clusters[index].push(group);
    }
    clusters
}

/// The cheapest plan of the cluster of groups `cluster`.
fn select_cluster(
    groups: &MemoGroups,
    cluster: &[usize],
    candidates: &[Vec<GroupCandidate>],
    entries: &[QueryWorkloadEntry],
    data: &DataWorkload,
    models: PlanningModels<'_>,
) -> Result<ClusterSelection, SelectionError> {
    let mut queries: Vec<usize> = cluster
        .iter()
        .flat_map(|&g| groups.groups[g].queries.iter().copied())
        .collect();
    queries.sort_unstable();
    let cluster_entries: Vec<QueryWorkloadEntry> =
        queries.iter().map(|&q| entries[q].clone()).collect();
    // Each group's kept candidates by distinct roots: Stage 2 chooses every
    // materialization again over the cluster.
    let options: Vec<Vec<&GroupCandidate>> = cluster
        .iter()
        .map(|&g| {
            let mut seen = Vec::new();
            candidates[g]
                .iter()
                .filter(|candidate| {
                    let flat = flatten(&candidate.roots).0;
                    let new = !seen.contains(&flat);
                    if new {
                        seen.push(flat);
                    }
                    new
                })
                .collect()
        })
        .collect();
    let mut selector = PlanSelector::new(&cluster_entries, data, models, false)?;
    // The physical candidates offered, by id, with the group candidates
    // they combine.
    let mut offered: HashMap<String, (Vec<usize>, Vec<QueryRoot>)> = HashMap::new();
    let mut digits = vec![0usize; cluster.len()];
    let mut number = 0usize;
    'combinations: loop {
        if options.iter().all(|kept| !kept.is_empty()) {
            let chosen: Vec<&GroupCandidate> = digits
                .iter()
                .zip(&options)
                .map(|(&d, kept)| kept[d])
                .collect();
            for roots in cluster_roots(groups, cluster, &chosen, entries) {
                number += 1;
                let id = format!("C{number}");
                let Ok(implemented) = optimize_physical_asap(
                    &id,
                    &roots,
                    &cluster_entries,
                    data,
                    models.deployment_capabilities,
                ) else {
                    continue;
                };
                for (k, mut physical) in implemented.into_iter().enumerate() {
                    physical.id = format!("{id}-m{k}");
                    selector.offer(&physical)?;
                    if selector.keeps_from_logical(&id) {
                        offered.insert(physical.id.clone(), (digits.clone(), roots.clone()));
                    }
                }
            }
        }
        for (digit, kept) in digits.iter_mut().zip(&options).rev() {
            *digit += 1;
            if *digit < kept.len() {
                continue 'combinations;
            }
            *digit = 0;
        }
        break;
    }
    let (_, physical) = selector.finish()?;
    let (combined, roots) = offered
        .remove(&physical.id)
        .expect("the selection was kept");
    let selected = cluster
        .iter()
        .zip(&combined)
        .map(|(&g, &d)| {
            let chosen = options[cluster.iter().position(|&c| c == g).expect("in cluster")][d];
            candidates[g]
                .iter()
                .position(|candidate| std::ptr::eq(candidate, chosen))
                .expect("kept")
        })
        .collect();
    Ok(ClusterSelection {
        groups: cluster.to_vec(),
        selected,
        queries,
        roots,
        physical,
    })
}

/// The cluster roots, in the order of the cluster's queries, of every way
/// to apply the cross-group rules to `chosen` (one candidate per group of
/// `cluster`): each derived variant, each followed by its shared variants.
/// A cluster of one group is its candidate's roots.
fn cluster_roots(
    groups: &MemoGroups,
    cluster: &[usize],
    chosen: &[&GroupCandidate],
    entries: &[QueryWorkloadEntry],
) -> Vec<Vec<QueryRoot>> {
    if cluster.len() == 1 {
        return vec![chosen[0].roots.clone()];
    }
    let members: Vec<_> = cluster.iter().map(|&g| groups.groups[g].clone()).collect();
    let mut out: Vec<Vec<QueryRoot>> = Vec::new();
    let mut seen = Vec::new();
    for derived in derived_variants(&members, chosen, entries) {
        // The derived roots, back in each group's candidate.
        let regrouped: Vec<GroupCandidate> = cluster
            .iter()
            .zip(chosen)
            .map(|(&g, candidate)| {
                let roots = groups.groups[g]
                    .queries
                    .iter()
                    .map(|q| {
                        let at = derived
                            .queries
                            .iter()
                            .position(|d| d == q)
                            .expect("in cluster");
                        derived.roots[at].clone()
                    })
                    .collect();
                GroupCandidate {
                    roots,
                    ..(*candidate).clone()
                }
            })
            .collect();
        let regrouped: Vec<&GroupCandidate> = regrouped.iter().collect();
        for shared in shared_variants(groups, &regrouped) {
            let flat = flatten(&shared.roots).0;
            if !seen.contains(&flat) {
                seen.push(flat);
                out.push(shared.roots);
            }
        }
    }
    out
}

/// The whole workload's physical candidate composed of `selections` (one
/// per cluster, covering every query once): each cluster's roots at its
/// queries' positions, each build materialized as its cluster's selection
/// materialized it. `None` when Stage 2 does not rebuild it.
pub(crate) fn composed(
    selections: &[ClusterSelection],
    query_count: usize,
    entries: &[QueryWorkloadEntry],
    data: &DataWorkload,
    models: PlanningModels<'_>,
) -> Option<PhysicalCandidate> {
    let mut roots: Vec<Option<QueryRoot>> = vec![None; query_count];
    let mut cluster_of = vec![0usize; query_count];
    for (c, selection) in selections.iter().enumerate() {
        for (&q, root) in selection.queries.iter().zip(&selection.roots) {
            roots[q] = Some(root.clone());
            cluster_of[q] = c;
        }
    }
    let roots: Vec<QueryRoot> = roots.into_iter().collect::<Option<_>>()?;
    let mut assignments: Vec<std::vec::IntoIter<_>> = selections
        .iter()
        .map(|s| s.physical.assignment.clone().into_iter())
        .collect();
    let mut choose = |readers: &[usize]| assignments[cluster_of[*readers.first()?]].next();
    let built = optimize_physical_asap_assigned_roots(&roots, entries, data, models, &mut choose)?;
    assignments
        .iter_mut()
        .all(|rest| rest.next().is_none())
        .then_some(built)
}

/// Stage 2's physical candidate of `roots` with the materialization
/// `choose` gives each candidate.
fn optimize_physical_asap_assigned_roots(
    roots: &[QueryRoot],
    entries: &[QueryWorkloadEntry],
    data: &DataWorkload,
    models: PlanningModels<'_>,
    choose: &mut dyn FnMut(&[usize]) -> Option<asap_types::physical::Materialization>,
) -> Option<PhysicalCandidate> {
    asap_physical_asap_optimization::optimize_physical_asap_assigned(
        "M",
        roots,
        entries,
        data,
        models.deployment_capabilities,
        choose,
    )
    .ok()
    .flatten()
}

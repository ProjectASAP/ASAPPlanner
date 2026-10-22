//! The cross-group Pass 2 rewrites of a cluster's chosen candidates
//! (joint-planning-search design, "Memo-group search", step 5): window
//! composition across groups (one pane chain shared by several groups'
//! builds: nested windows read as the newest panes, chains shared across
//! repeat intervals, gcd-width chains), and derivation edges (a coarse
//! build of one group answered from a finer build of another, an exact
//! top-k from another group's ranking).
//!
//! Each group candidate already holds the Pass 2 rewrites that touch only
//! its own nodes, its own pane chains included. The rewrites here are the
//! ones the current Pass 2 rules find on the composed roots, kept only
//! when each of them links nodes of two or more groups: a rewrite inside
//! one group is a candidate of that group instead.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::rc::Rc;

use asap_logical_asap_optimization::asap_aware_cse::{
    pane_candidates, rollup_candidates, rollup_choices, rollup_pass2_candidates,
    topk_reuse_candidates, topk_reuse_pass2_candidates, window_composition_pass2_candidates,
    MAX_REUSE_CANDIDATES,
};
use asap_types::ir::flat::{flatten, FlatDag};
use asap_types::ir::operator::AggIntent;
use asap_types::ir::{ASAPOp, NonASAPOp, OperatorNode, QueryRoot};
use asap_types::workload::QueryWorkloadEntry;

use super::super::components::root_nodes;
use super::{GroupCandidate, MemoGroup, SharingSite};

/// One way to apply the cross-group window-composition, rollup and top-k
/// reuse rules to a cluster's chosen candidates: the cluster's roots (each
/// query of its groups, in workload order) rewritten, with the sites it
/// joins.
#[derive(Debug, Clone)]
pub(crate) struct DerivedVariant {
    /// The cluster's queries, by index in the workload, in workload order.
    pub queries: Vec<usize>,
    /// Their roots, in the order of `queries`.
    pub roots: Vec<QueryRoot>,
    /// The chosen candidates' sites the rewrites join, sorted: for each
    /// rewrite, the sites of each group it links whose meeting key is
    /// another linked group's too.
    pub joined_sites: Vec<SharingSite>,
}

/// Every [`DerivedVariant`] of `chosen` (one candidate per group, in
/// `groups` order): no derivation first, then each combination the current
/// Pass 2 rules (`window_composition_pass2_candidates`' shared chains,
/// `rollup_pass2_candidates`, `topk_reuse_pass2_candidates`, with their
/// combination limits: exact top-k reuse is never combined with panes)
/// would enumerate on the composed roots, restricted to rewrites that span
/// two or more groups. `entries[q]` is workload query `q`'s entry. A
/// variant equal to an earlier one is listed once; a rewrite whose
/// composition fails, or a rule over its enumeration cap, lists nothing.
pub(crate) fn derived_variants(
    groups: &[MemoGroup],
    chosen: &[&GroupCandidate],
    entries: &[QueryWorkloadEntry],
) -> Vec<DerivedVariant> {
    let mut by_query: Vec<(usize, usize, QueryRoot)> = groups
        .iter()
        .zip(chosen)
        .enumerate()
        .flat_map(|(position, (group, candidate))| {
            group
                .queries
                .iter()
                .zip(&candidate.roots)
                .map(move |(&q, root)| (q, position, root.clone()))
        })
        .collect();
    by_query.sort_by_key(|(q, _, _)| *q);
    let queries: Vec<usize> = by_query.iter().map(|(q, _, _)| *q).collect();
    let owner: Vec<usize> = by_query.iter().map(|(_, g, _)| *g).collect();
    let roots: Vec<QueryRoot> = by_query.into_iter().map(|(_, _, root)| root).collect();
    let grids: Vec<Option<u64>> = queries
        .iter()
        .map(|&q| entries[q].evaluation_grid_ms())
        .collect();
    let mut found = Variants {
        queries,
        owner,
        grids,
        chosen,
        seen: vec![flatten(&roots).0],
        out: vec![],
    };
    found.out.push(DerivedVariant {
        queries: found.queries.clone(),
        roots: roots.clone(),
        joined_sites: Vec::new(),
    });
    found.window_composition(&roots, &[]);
    found.reuse(&roots);
    found.out
}

/// The variants found so far, and what classifying their rewrites needs.
struct Variants<'a> {
    queries: Vec<usize>,
    /// Per query of `queries`, its group's position in the cluster.
    owner: Vec<usize>,
    /// Per query of `queries`, its evaluation grid.
    grids: Vec<Option<u64>>,
    chosen: &'a [&'a GroupCandidate],
    /// The flattened DAG of every variant listed.
    seen: Vec<FlatDag>,
    out: Vec<DerivedVariant>,
}

/// A rewrite's linked groups, by position in the cluster.
type Link = BTreeSet<usize>;

impl Variants<'_> {
    /// List `roots`, rewritten by rewrites linking `links`, unless an equal
    /// variant is listed.
    fn push(&mut self, roots: Vec<QueryRoot>, links: &[Link]) {
        let flat = flatten(&roots).0;
        if self.seen.contains(&flat) {
            return;
        }
        self.seen.push(flat);
        let mut joined: Vec<SharingSite> = Vec::new();
        for link in links {
            for &g in link {
                let others: HashSet<u64> = link
                    .iter()
                    .filter(|&&h| h != g)
                    .flat_map(|&h| self.chosen[h].signature.0.iter().map(|s| s.meeting_key))
                    .collect();
                joined.extend(
                    self.chosen[g]
                        .signature
                        .0
                        .iter()
                        .filter(|site| others.contains(&site.meeting_key)),
                );
            }
        }
        joined.sort_unstable();
        joined.dedup();
        self.out.push(DerivedVariant {
            queries: self.queries.clone(),
            roots,
            joined_sites: joined,
        });
    }

    /// The groups whose queries reach each node of `roots`.
    fn readers(&self, roots: &[QueryRoot]) -> HashMap<*const OperatorNode, Link> {
        let mut readers: HashMap<*const OperatorNode, Link> = HashMap::new();
        for (root, &g) in roots.iter().zip(&self.owner) {
            for node in root_nodes(root) {
                readers.entry(Rc::as_ptr(&node)).or_default().insert(g);
            }
        }
        readers
    }

    /// The window-composition variants of `roots` (rewritten by rewrites
    /// linking `links`) in which every build in panes is read by two or
    /// more groups, or reads a chain shared with another group's build.
    /// None when `roots` reuse a top-k ranking: the rules never pane
    /// those.
    fn window_composition(&mut self, roots: &[QueryRoot], links: &[Link]) {
        if reads_a_topk_ranking(roots) {
            return;
        }
        let panes = pane_candidates(roots, &self.grids);
        if panes.is_empty() {
            return;
        }
        let Ok(pass2_candidates) = window_composition_pass2_candidates(roots, &panes) else {
            return;
        };
        let readers = self.readers(roots);
        for pass2_candidate in pass2_candidates {
            let Ok(rewritten) = pass2_candidate.roots else {
                continue;
            };
            let build_groups: Vec<Link> = pass2_candidate
                .pane_candidates
                .iter()
                .map(|p| readers[&Rc::as_ptr(&p.build)].clone())
                .collect();
            let mut spans = vec![false; build_groups.len()];
            let mut added: Vec<Link> = links.to_vec();
            for chain in &pass2_candidate.shared_chains {
                let link: Link = chain
                    .members
                    .iter()
                    .flat_map(|&m| build_groups[m].iter().copied())
                    .collect();
                if link.len() >= 2 {
                    chain.members.iter().for_each(|&m| spans[m] = true);
                    added.push(link);
                }
            }
            for (i, link) in build_groups.iter().enumerate() {
                if !spans[i] && link.len() >= 2 {
                    spans[i] = true;
                    added.push(link.clone());
                }
            }
            if spans.iter().all(|&s| s) {
                self.push(rewritten, &added);
            }
        }
    }

    /// The reuse variants of `roots`, in the order of the Pass 2 rules:
    /// the exact top-k reuse variants, then each rollup variant followed
    /// by its own exact top-k reuse and window-composition variants. More
    /// than `MAX_REUSE_CANDIDATES` rollup choices and top-k reuse
    /// candidates together list none.
    fn reuse(&mut self, roots: &[QueryRoot]) {
        let candidates = rollup_candidates(roots);
        let counted = rollup_choices(&candidates).len() + topk_reuse_candidates(roots).len();
        if counted > MAX_REUSE_CANDIDATES {
            return;
        }
        self.topk_reuse(roots, &[]);
        let Ok(pass2_candidates) = rollup_pass2_candidates(roots, &candidates) else {
            return;
        };
        let readers = self.readers(roots);
        for pass2_candidate in pass2_candidates {
            let Ok(rolled) = pass2_candidate.roots else {
                continue;
            };
            // A sketch choice (every coarse build of one finest build) is
            // applied whole: it spans when one of its rollups does.
            let mut links = Vec::new();
            let mut spans = true;
            for choice in rollup_choices(&pass2_candidate.rollup_candidates) {
                let choice_links: Vec<Link> = choice
                    .iter()
                    .map(|c| {
                        let mut link = readers[&Rc::as_ptr(&c.coarse)].clone();
                        link.extend(&readers[&Rc::as_ptr(&c.finer)]);
                        link
                    })
                    .filter(|link| link.len() >= 2)
                    .collect();
                spans &= !choice_links.is_empty();
                links.extend(choice_links);
            }
            if !spans {
                continue;
            }
            self.push(rolled.clone(), &links);
            self.topk_reuse(&rolled, &links);
            self.window_composition(&rolled, &links);
        }
    }

    /// The exact top-k reuse variants of `roots` (rewritten by rewrites
    /// linking `links`) in which each ranking is read by two or more
    /// groups. None when `roots` have pane chains: the rules never reuse a
    /// top-k ranking with panes.
    fn topk_reuse(&mut self, roots: &[QueryRoot], links: &[Link]) {
        if has_a_pane_chain(roots) {
            return;
        }
        let candidates = topk_reuse_candidates(roots);
        let Ok(pass2_candidates) = topk_reuse_pass2_candidates(roots, &candidates) else {
            return;
        };
        let readers = self.readers(roots);
        for pass2_candidate in pass2_candidates {
            let Ok(rewritten) = pass2_candidate.roots else {
                continue;
            };
            let mut added = links.to_vec();
            let mut spans = true;
            for candidate in &pass2_candidate.topk_reuse_candidates {
                let link: Link = candidate
                    .members
                    .iter()
                    .flat_map(|m| readers[&Rc::as_ptr(m)].iter().copied())
                    .collect();
                spans &= link.len() >= 2;
                added.push(link);
            }
            if spans {
                self.push(rewritten, &added);
            }
        }
    }
}

/// Whether `roots` build anything in panes.
fn has_a_pane_chain(roots: &[QueryRoot]) -> bool {
    roots
        .iter()
        .flat_map(root_nodes)
        .any(|node| matches!(node.asap(), Some(ASAPOp::PaneChain { .. })))
}

/// Whether `roots` answer an exact top-k from a ranking (a top-k over a
/// `Limit`, which the exact top-k reuse rule introduces).
fn reads_a_topk_ranking(roots: &[QueryRoot]) -> bool {
    roots.iter().flat_map(root_nodes).any(|node| {
        matches!(
            node.non_asap(),
            Some(NonASAPOp::Aggregate { child, measures, .. })
                if matches!(measures.as_slice(), [AggIntent::TopK { .. }])
                    && matches!(child.non_asap(), Some(NonASAPOp::Limit { .. }))
        )
    })
}

#[cfg(test)]
mod tests {
    use super::super::groups::{group_candidates, memo_groups};
    use super::*;
    use crate::PlanningModels;
    use asap_logical_asap_optimization::per_computation_candidates::generate_per_computation_candidates;
    use asap_logical_asap_optimization::test_support::{entry_with, lower_promql};
    use asap_types::ir::properties::AccuracyTarget;
    use asap_types::ir::schema::schema_support::with_promql_series_identity;
    use asap_types::workload::{
        DataArrival, DataWorkload, QueryRecurrence, RepeatedDemand, RepetitionInterval, SourceFacts,
    };

    /// Data arriving at 10 000 rows per second, each source holding 10 000
    /// series.
    fn data() -> DataWorkload {
        DataWorkload {
            arrival: DataArrival::Ingesting {
                rows_per_second: 10_000.0,
            },
            sources: ["m", "x"]
                .iter()
                .map(|source| {
                    let facts = SourceFacts {
                        series_count: Some(10_000),
                        ..SourceFacts::default()
                    };
                    (source.to_string(), facts)
                })
                .collect(),
        }
    }

    /// PromQL `queries` under `target`, each repeating every 60 s: their
    /// memo groups and each group's candidates.
    fn planned(
        queries: &[&str],
        target: AccuracyTarget,
    ) -> (
        Vec<MemoGroup>,
        Vec<Vec<GroupCandidate>>,
        Vec<QueryWorkloadEntry>,
    ) {
        let models = PlanningModels::builtin();
        let roots: Vec<(usize, QueryRoot)> = queries
            .iter()
            .map(|q| {
                let root = lower_promql(q, target.clone());
                QueryRoot::Operator(with_promql_series_identity(&root).unwrap())
            })
            .enumerate()
            .collect();
        let recurrence =
            QueryRecurrence::Repeated(RepeatedDemand::FixedInterval(RepetitionInterval(60_000)));
        let entries: Vec<QueryWorkloadEntry> = queries
            .iter()
            .map(|_| entry_with(target.clone(), recurrence.clone()))
            .collect();
        let inventory = generate_per_computation_candidates(roots)
            .unwrap()
            .without_unsupported_summaries(models.deployment_capabilities);
        let groups = memo_groups(&inventory, &entries, &models);
        let candidates = (0..groups.groups.len())
            .map(|g| group_candidates(&inventory, &groups, g, &entries, &data(), models))
            .collect();
        (groups.groups, candidates, entries)
    }

    /// Each group's first candidate: its Pass 1 candidate of choice 0,
    /// without Pass 2 rewrites.
    fn firsts(candidates: &[Vec<GroupCandidate>]) -> Vec<&GroupCandidate> {
        candidates.iter().map(|c| &c[0]).collect()
    }

    /// Every node a variant's roots reach.
    fn nodes(variant: &DerivedVariant) -> Vec<Rc<OperatorNode>> {
        variant.roots.iter().flat_map(root_nodes).collect()
    }

    fn reads_one_node(variant: &DerivedVariant, matches: impl Fn(&OperatorNode) -> bool) -> bool {
        let first: HashSet<*const OperatorNode> = root_nodes(&variant.roots[0])
            .iter()
            .filter(|n| matches(n))
            .map(Rc::as_ptr)
            .collect();
        root_nodes(&variant.roots[1])
            .iter()
            .any(|n| first.contains(&Rc::as_ptr(n)))
    }

    /// Two sums over 5 m and 1 h of one metric, every minute, in two
    /// groups: a variant reads one chain of one-minute panes from both, and
    /// joins a site of each. Each group's own pane variant is already one of
    /// its candidates.
    #[test]
    fn nested_windows_share_one_chain_across_groups() {
        let (groups, candidates, entries) = planned(
            &["sum_over_time(m[5m])", "sum_over_time(m[1h])"],
            AccuracyTarget::exact(),
        );
        assert_eq!(groups.len(), 2);
        for own in &candidates {
            assert!(own.iter().any(|c| c
                .roots
                .iter()
                .flat_map(root_nodes)
                .any(|n| matches!(n.asap(), Some(ASAPOp::PaneChain { .. })))));
        }
        let variants = derived_variants(&groups, &firsts(&candidates), &entries);
        assert!(variants[0].joined_sites.is_empty());
        let shared: Vec<&DerivedVariant> = variants
            .iter()
            .filter(|v| reads_one_node(v, |n| matches!(n.asap(), Some(ASAPOp::PaneChain { .. }))))
            .collect();
        assert!(!shared.is_empty());
        assert!(shared.iter().all(|v| !v.joined_sites.is_empty()));
        // A variant in which only one group's build is in panes is that
        // group's candidate, not a derivation.
        assert!(variants[1..]
            .iter()
            .all(|v| v.roots.iter().all(|root| root_nodes(root)
                .iter()
                .any(|n| matches!(n.asap(), Some(ASAPOp::PaneChain { .. }))))));
    }

    /// Sums of one metric over 5 m by `(job, instance)` and by `job`, in two
    /// groups: a variant answers the per-job sum from the finer one. (A
    /// per-series sum, `sum_over_time(m[5m])`, is built per entity, not
    /// grouped by keys, and the rollup rule does not read it.)
    #[test]
    fn a_coarse_sum_rolls_up_from_another_groups_finer_sum() {
        let (groups, candidates, entries) = planned(
            &[
                "sum by (job, instance) (sum_over_time(m[5m]))",
                "sum by (job) (sum_over_time(m[5m]))",
            ],
            AccuracyTarget::exact(),
        );
        assert_eq!(groups.len(), 2);
        // The rule reads a coarse build over the window grouped by keys,
        // which only some of each group's Pass 1 candidates have.
        let mut rolled = Vec::new();
        for a in &candidates[0] {
            for b in &candidates[1] {
                rolled.extend(
                    derived_variants(&groups, &[a, b], &entries)
                        .into_iter()
                        .filter(|v| {
                            reads_one_node(v, |n| {
                                matches!(n.asap(), Some(ASAPOp::FinalizeExactAccumulator { .. }))
                            })
                        }),
                );
            }
        }
        assert!(!rolled.is_empty());
        assert!(rolled.iter().all(|v| !v.joined_sites.is_empty()));
    }

    /// Two exact top-k with different `k` over one input, in two groups: a
    /// variant reads one ranking from both, and no variant reuses a ranking
    /// with panes.
    #[test]
    fn exact_topk_reuses_another_groups_ranking_never_with_panes() {
        let (groups, candidates, entries) = planned(
            &[
                "topk(5, sum by (job) (sum_over_time(m[5m])))",
                "topk(10, sum by (job) (sum_over_time(m[5m])))",
            ],
            AccuracyTarget::exact(),
        );
        assert_eq!(groups.len(), 2);
        let variants = derived_variants(&groups, &firsts(&candidates), &entries);
        let is_ranking = |n: &OperatorNode| matches!(n.non_asap(), Some(NonASAPOp::Limit { .. }));
        let reused: Vec<&DerivedVariant> = variants
            .iter()
            .filter(|v| reads_one_node(v, is_ranking))
            .collect();
        assert!(!reused.is_empty(), "{} variants", variants.len());
        for variant in &variants {
            let ranked = nodes(variant).iter().any(|n| is_ranking(n));
            let panes = has_a_pane_chain(&variant.roots);
            assert!(!(ranked && panes));
        }
        // Not over chosen candidates already in panes either.
        for own in candidates
            .iter()
            .flatten()
            .filter(|c| has_a_pane_chain(&c.roots))
        {
            let other = &candidates[1 - own.group][0];
            let chosen: Vec<&GroupCandidate> = if own.group == 0 {
                vec![own, other]
            } else {
                vec![other, own]
            };
            for variant in derived_variants(&groups, &chosen, &entries) {
                assert!(!nodes(&variant).iter().any(|n| is_ranking(n)));
            }
        }
    }
}

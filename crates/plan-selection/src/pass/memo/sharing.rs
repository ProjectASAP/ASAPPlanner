//! Sharing across the groups of a cluster (step 5's composition): the
//! chosen candidates' roots composed in workload order, and every way the
//! identical-expression and summary-capability rules make nodes of two or
//! more groups one node.
//!
//! The rules run unchanged on the composed roots; only what they would
//! make one node across groups is kept. Sharing inside one group is
//! already in its candidates (a group candidate is either shared or apart
//! there), so a node set with two nodes of one group is not offered again:
//! sharing it would repeat a candidate of that group.

use std::collections::{HashMap, HashSet};
use std::rc::Rc;

use asap_logical_asap_optimization::asap_aware_cse::{
    capability_sharing_candidates, identical_expression_pass2_candidates, sharing_candidates,
    summary_capability_pass2_candidates, with_union_scan_labels, SharingCandidate,
};
use asap_types::ir::flat::{flatten, FlatDag};
use asap_types::ir::{NonASAPOp, OperatorNode, QueryRoot};

use super::super::components::root_nodes;
use super::groups::MemoGroups;
use super::{GroupCandidate, SharingSite};

/// One way to share the meeting sites of a cluster's chosen candidates:
/// the cluster's roots (each query of its groups, in workload order) with
/// some identical sites made one node and some capability subsets made
/// one build.
#[derive(Debug, Clone)]
pub(crate) struct SharedVariant {
    /// The cluster's queries, by index in the workload, in workload order.
    pub queries: Vec<usize>,
    /// Their roots, in the order of [`Self::queries`].
    pub roots: Vec<QueryRoot>,
    /// The sites two or more groups read as one node in this variant and
    /// not in the composed roots, sorted; empty for the variant that shares
    /// nothing. Each site's cost key hashes the node without its
    /// materialization, which Stage 2 chooses.
    pub shared_sites: Vec<SharingSite>,
}

/// Every [`SharedVariant`] of `chosen` (one candidate per group of the
/// cluster, of `groups`): the composed roots sharing nothing first, then
/// every distinct candidate the identical-expression rule (with scans
/// widened to one label set, when that shares a scan) and the
/// summary-capability rule (each choice followed by its own
/// identical-expression candidates) enumerate on the composed roots, in
/// Stage 1's order, keeping only the node sets that span two or more
/// groups with one node of each. A summary-capability choice whose
/// guarantees do not compose is left out.
pub(crate) fn shared_variants(
    groups: &MemoGroups,
    chosen: &[&GroupCandidate],
) -> Vec<SharedVariant> {
    let mut queries: Vec<(usize, usize, QueryRoot)> = chosen
        .iter()
        .flat_map(|candidate| {
            let members = &groups.groups[candidate.group].queries;
            members
                .iter()
                .zip(&candidate.roots)
                .map(|(&q, root)| (q, candidate.group, root.clone()))
        })
        .collect();
    queries.sort_by_key(|(q, _, _)| *q);
    let group_of_root: Vec<usize> = queries.iter().map(|(_, g, _)| *g).collect();
    let roots: Vec<QueryRoot> = queries.iter().map(|(_, _, root)| root.clone()).collect();
    let mut variants = Variants {
        groups,
        group_of_root: &group_of_root,
        queries: queries.iter().map(|(q, _, _)| *q).collect(),
        already_shared: Vec::new(),
        seen: vec![flatten(&roots).0],
        out: Vec::new(),
        widened: Vec::new(),
    };
    variants.already_shared = variants.cross_group_sites(&roots);
    variants.out.push(SharedVariant {
        queries: variants.queries.clone(),
        roots: roots.clone(),
        shared_sites: Vec::new(),
    });
    variants.identical_expression(&roots);
    let capability = capability_sharing_candidates(&roots);
    // More members than the rule enumerates: no capability sharing.
    for pass2 in summary_capability_pass2_candidates(&roots, &capability).unwrap_or_default() {
        let Ok(shared_roots) = pass2.roots else {
            continue;
        };
        if !pass2
            .shared_builds
            .iter()
            .all(|builds| variants.one_per_group(&roots, builds))
        {
            continue;
        }
        variants.push(&shared_roots);
        variants.identical_expression(&shared_roots);
    }
    let mut out = variants.out;
    out.append(&mut variants.widened);
    out
}

/// The variants found so far, each distinct one once.
struct Variants<'a> {
    groups: &'a MemoGroups,
    /// Each composed root's group, by index.
    group_of_root: &'a [usize],
    queries: Vec<usize>,
    /// The sites the composed roots already read as one node across
    /// groups (nodes the groups' candidates have in common).
    already_shared: Vec<SharingSite>,
    seen: Vec<FlatDag>,
    out: Vec<SharedVariant>,
    /// The variants over widened scans, listed last, as Stage 1 does.
    widened: Vec<SharedVariant>,
}

impl Variants<'_> {
    /// The groups whose roots (of `roots`, composed in workload order)
    /// reach each node.
    fn groups_reaching(&self, roots: &[QueryRoot]) -> HashMap<*const OperatorNode, Vec<usize>> {
        let mut reaching: HashMap<*const OperatorNode, Vec<usize>> = HashMap::new();
        for (root, &group) in roots.iter().zip(self.group_of_root) {
            for node in root_nodes(root) {
                let groups = reaching.entry(Rc::as_ptr(&node)).or_default();
                if !groups.contains(&group) {
                    groups.push(group);
                }
            }
        }
        reaching
    }

    /// Whether `nodes` of `roots` are one node of each of two or more
    /// groups.
    fn one_per_group(&self, roots: &[QueryRoot], nodes: &[Rc<OperatorNode>]) -> bool {
        let reaching = self.groups_reaching(roots);
        let mut seen = HashSet::new();
        for node in nodes {
            for &group in reaching.get(&Rc::as_ptr(node)).into_iter().flatten() {
                if !seen.insert(group) {
                    return false;
                }
            }
        }
        seen.len() >= 2
    }

    /// The sites among the nodes two or more groups of `roots` reach.
    fn cross_group_sites(&self, roots: &[QueryRoot]) -> Vec<SharingSite> {
        let reaching = self.groups_reaching(roots);
        let mut nodes = Vec::new();
        let mut seen = HashSet::new();
        for node in roots.iter().flat_map(root_nodes) {
            if reaching[&Rc::as_ptr(&node)].len() >= 2 && seen.insert(Rc::as_ptr(&node)) {
                nodes.push(node);
            }
        }
        let mut sites = self.groups.sites_of_nodes(&nodes);
        sites.sort_unstable();
        sites.dedup();
        sites
    }

    /// The variant with roots `roots`, unless an equal one is listed.
    fn variant(&mut self, roots: &[QueryRoot]) -> Option<SharedVariant> {
        let flat = flatten(roots).0;
        if self.seen.contains(&flat) {
            return None;
        }
        self.seen.push(flat);
        let shared_sites = self
            .cross_group_sites(roots)
            .into_iter()
            .filter(|site| !self.already_shared.contains(site))
            .collect();
        Some(SharedVariant {
            queries: self.queries.clone(),
            roots: roots.to_vec(),
            shared_sites,
        })
    }

    fn push(&mut self, roots: &[QueryRoot]) {
        if let Some(variant) = self.variant(roots) {
            self.out.push(variant);
        }
    }

    /// The identical-expression variants of `roots` whose node sets cross
    /// groups; then, when a metric's scans project different labels, those
    /// of `roots` with the scans widened to one label set that share a
    /// scan (listed last).
    fn identical_expression(&mut self, roots: &[QueryRoot]) {
        for pass2 in self.cross_group_identical(roots) {
            self.push(&pass2);
        }
        // Widening that fails to compose shares nothing.
        let Ok(Some(widened)) = with_union_scan_labels(roots) else {
            return;
        };
        let candidates: Vec<SharingCandidate> = sharing_candidates(&widened)
            .into_iter()
            .filter(|c| self.one_per_group(&widened, &c.nodes))
            .collect();
        for pass2 in identical_expression_pass2_candidates(&widened, &candidates) {
            let shares_a_scan = pass2
                .sharing_candidates
                .iter()
                .any(|c| matches!(c.nodes[0].non_asap(), Some(NonASAPOp::Scan { .. })));
            if shares_a_scan {
                if let Some(variant) = self.variant(&pass2.roots) {
                    self.widened.push(variant);
                }
            }
        }
    }

    /// The roots of every identical-expression Pass 2 candidate of `roots`
    /// over the sharing candidates with one node of each of two or more
    /// groups.
    fn cross_group_identical(&self, roots: &[QueryRoot]) -> Vec<Vec<QueryRoot>> {
        let candidates: Vec<SharingCandidate> = sharing_candidates(roots)
            .into_iter()
            .filter(|c| self.one_per_group(roots, &c.nodes))
            .collect();
        identical_expression_pass2_candidates(roots, &candidates)
            .into_iter()
            .map(|pass2| pass2.roots)
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pass::memo::groups::{group_candidates, memo_groups};
    use crate::PlanningModels;
    use asap_logical_asap_optimization::per_computation_candidates::generate_per_computation_candidates;
    use asap_logical_asap_optimization::test_support::{entry_with, lower_promql};
    use asap_types::ir::properties::AccuracyTarget;
    use asap_types::ir::schema::sketch::{SketchAlgorithm, SketchParams};
    use asap_types::ir::schema::FieldDataType;
    use asap_types::ir::ASAPOp;
    use asap_types::workload::{
        DataArrival, DataWorkload, QueryRecurrence, QueryWorkloadEntry, RepeatedDemand,
        RepetitionInterval, SourceFacts,
    };

    fn repeating(interval_ms: u32) -> QueryRecurrence {
        QueryRecurrence::Repeated(RepeatedDemand::FixedInterval(RepetitionInterval(
            interval_ms,
        )))
    }

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

    /// PromQL `queries[q]` within error `errors[q]`, repeating every minute:
    /// the memo groups and each group's candidates.
    fn planned(
        queries: &[&str],
        errors: &[f64],
        intervals_ms: &[u32],
    ) -> (MemoGroups, Vec<Vec<GroupCandidate>>) {
        let targets: Vec<AccuracyTarget> = errors
            .iter()
            .map(|&e| {
                if e == 0.0 {
                    AccuracyTarget::exact()
                } else {
                    AccuracyTarget::max_error(e)
                }
            })
            .collect();
        let roots = queries
            .iter()
            .zip(&targets)
            .map(|(q, t)| QueryRoot::Operator(lower_promql(q, t.clone())))
            .collect::<Vec<_>>();
        let entries: Vec<QueryWorkloadEntry> = targets
            .iter()
            .zip(intervals_ms)
            .map(|(t, &interval_ms)| entry_with(t.clone(), repeating(interval_ms)))
            .collect();
        let models = PlanningModels::builtin();
        let inventory =
            generate_per_computation_candidates(roots.into_iter().enumerate().collect())
                .unwrap()
                .without_unsupported_summaries(models.deployment_capabilities);
        let groups = memo_groups(&inventory, &entries, &models);
        let candidates = (0..groups.groups.len())
            .map(|g| group_candidates(&inventory, &groups, g, &entries, &data(), models))
            .collect();
        (groups, candidates)
    }

    /// The KLL builds (`SummaryAgg`, not pane chains) `roots` reach, once
    /// each.
    fn kll_builds(roots: &[QueryRoot]) -> Vec<Rc<OperatorNode>> {
        let mut seen = HashSet::new();
        roots
            .iter()
            .flat_map(root_nodes)
            .filter(|node| {
                matches!(
                    node.asap(),
                    Some(ASAPOp::SummaryAgg { family: FieldDataType::Sketch(kind, _), .. })
                        if *kind.algorithm() == SketchAlgorithm::Kll
                )
            })
            .filter(|node| seen.insert(Rc::as_ptr(node)))
            .collect()
    }

    /// The first candidate of `candidates` whose roots read `builds` KLL
    /// builds and no pane chain.
    fn with_kll_builds(candidates: &[GroupCandidate], builds: usize) -> &GroupCandidate {
        candidates
            .iter()
            .find(|c| {
                kll_builds(&c.roots).len() == builds
                    && !c
                        .roots
                        .iter()
                        .flat_map(root_nodes)
                        .any(|n| matches!(n.asap(), Some(ASAPOp::PaneChain { .. })))
            })
            .expect("a candidate with that many KLL builds")
    }

    /// The parameter `k` of a KLL build.
    fn kll_k(build: &OperatorNode) -> u32 {
        match build.asap() {
            Some(ASAPOp::SummaryAgg {
                family: FieldDataType::Sketch(kind, _),
                ..
            }) => match kind.params() {
                SketchParams::Kll { k, .. } => *k,
                other => panic!("not a KLL build: {other:?}"),
            },
            _ => panic!("not a summary build"),
        }
    }

    /// One quantile in two groups (the same query, evaluated every minute
    /// and every two minutes), each a KLL sketch of one size: the
    /// identical-expression rule shares the nested identical nodes (scan,
    /// window, build, estimate), each sharing also sharing everything below
    /// it. The variants are the composed roots apart, then one per closed
    /// set: the chain shared up to each level.
    #[test]
    fn identical_queries_in_two_groups_are_apart_or_shared() {
        let quantile = "quantile_over_time(0.9, m[5m])";
        let (groups, candidates) =
            planned(&[quantile, quantile], &[0.01, 0.01], &[60_000, 120_000]);
        assert_eq!(groups.groups.len(), 2);
        let chosen = [
            with_kll_builds(&candidates[0], 1),
            with_kll_builds(&candidates[1], 1),
        ];
        let variants = shared_variants(&groups, &chosen);
        assert_eq!(variants.len(), 5);
        for variant in &variants {
            assert_eq!(variant.queries, vec![0, 1]);
            assert_eq!(variant.roots.len(), 2);
        }
        let apart = &variants[0];
        assert!(apart.shared_sites.is_empty());
        assert_eq!(kll_builds(&apart.roots).len(), 2);
        // Each level shares one more node: the shared sites grow by one.
        for (level, variant) in variants.iter().enumerate().skip(1) {
            assert_eq!(variant.shared_sites.len(), level);
        }
        assert_eq!(kll_builds(&variants[4].roots).len(), 1);
    }

    /// One quantile within 1% and within 2% error, in two groups: two KLL
    /// builds of different `k`. The summary-capability rule makes them one
    /// build sized for the strictest reader (the larger `k`); its two
    /// estimates are then identical, and one more variant shares them.
    #[test]
    fn kll_builds_of_two_groups_share_one_build_at_the_larger_k() {
        let quantile = "quantile_over_time(0.9, m[5m])";
        let (groups, candidates) = planned(&[quantile, quantile], &[0.01, 0.02], &[60_000, 60_000]);
        assert_eq!(groups.groups.len(), 2);
        let chosen = [
            with_kll_builds(&candidates[0], 1),
            with_kll_builds(&candidates[1], 1),
        ];
        let sizes: Vec<u32> = chosen
            .iter()
            .map(|c| kll_k(&kll_builds(&c.roots)[0]))
            .collect();
        assert_ne!(sizes[0], sizes[1]);
        let variants = shared_variants(&groups, &chosen);
        assert_eq!(variants.len(), 3);
        assert!(variants[0].shared_sites.is_empty());
        assert_eq!(kll_builds(&variants[0].roots).len(), 2);
        for shared in &variants[1..] {
            let builds = kll_builds(&shared.roots);
            assert_eq!(builds.len(), 1);
            assert_eq!(kll_k(&builds[0]), sizes[0].max(sizes[1]));
            assert!(!shared.shared_sites.is_empty());
        }
    }

    /// A ratio of two quantiles (one group) and one of the quantiles at a
    /// looser error (another group). The ratio's candidate with its two
    /// identical builds apart is chosen: no variant makes them one build
    /// (that is the group's other candidate), alone or together with the
    /// other group's build; each of them may share with the other group's.
    #[test]
    fn sharing_inside_a_group_is_not_enumerated_again() {
        let (groups, candidates) = planned(
            &[
                "quantile_over_time(0.9, m[5m]) / quantile_over_time(0.5, m[5m])",
                "quantile_over_time(0.9, m[5m])",
            ],
            &[0.01, 0.02],
            &[60_000, 60_000],
        );
        assert_eq!(groups.groups.len(), 2);
        let ratio = with_kll_builds(&candidates[0], 2);
        let chosen = [ratio, with_kll_builds(&candidates[1], 1)];
        let variants = shared_variants(&groups, &chosen);
        assert!(variants.len() > 1);
        let mut capability_shared = 0;
        for variant in &variants {
            // The ratio (query 0) still reads two builds.
            assert_eq!(kll_builds(&variant.roots[..1]).len(), 2);
            if kll_builds(&variant.roots).len() == 2 {
                capability_shared += 1;
            }
        }
        assert!(capability_shared > 0);
    }
}

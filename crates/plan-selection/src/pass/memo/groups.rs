//! Memo groups and their candidates (steps 1–2).
//!
//! A memo group is planned alone: its candidates are the Stage 1
//! candidates of the workload of its queries alone, each implemented in
//! every Stage 2 materialization. What another group can share with one of
//! them is its sharing signature: the nodes a Pass 2 rule could make one
//! node with a node of another group's candidate.
//!
//! The sites are found once for the workload ([`Sites`]): for every two
//! queries of different groups over a common source and every choice of
//! their own Pass 1 candidates, the nodes the Pass 2 rules make both read
//! ([`meetings`]). Each such node's meeting key is a hash of its sub-DAG with
//! what sharing may change masked ([`NodeKeys::meeting`]); the keys of nodes
//! that met are joined, so that equal meeting keys meet. A node of a group
//! candidate is a site when its masked hash is one that met a node of
//! another group.

use std::collections::hash_map::DefaultHasher;
use std::collections::{HashMap, HashSet};
use std::hash::{Hash, Hasher};
use std::rc::Rc;

use asap_logical_asap_optimization::per_computation_candidates::{
    compose_logical_candidate, PerComputationCandidates,
};
use asap_logical_asap_optimization::workload_candidates::stage1_logical_asap_optimization_skipping;
use asap_physical_asap_optimization::{optimize_physical_asap, PhysicalCandidate};
use asap_types::ir::flat::flatten;
use asap_types::ir::operator::Source;
use asap_types::ir::{ASAPOp, NonASAPOp, Operator, OperatorNode, QueryRoot, ScalarExpr};
use asap_types::workload::{DataWorkload, QueryWorkloadEntry};

use super::super::components::{meetings, root_nodes};
use super::super::interchangeable::{mask_quantile_ranks, same_entry, Classes};
use super::super::query_candidates::{next_digits, query_digits};
use super::{GroupCandidate, MemoGroup, SharingSignature, SharingSite};
use crate::PlanningModels;

/// A workload's memo groups, with what planning each alone needs: the
/// classes of interchangeable queries and the sharing sites between groups.
pub(crate) struct MemoGroups {
    /// The groups, ordered by their first query.
    pub groups: Vec<MemoGroup>,
    classes: Classes,
    sites: Sites,
}

/// The memo groups of a workload: identical queries and classes of
/// interchangeable queries are one group each, and so are queries that
/// read a common Pass 1 target. `per_computation` is the workload's Pass 1
/// inventory, already pruned for the deployment's capabilities;
/// `entries[q]` is query `q`'s workload entry.
pub(crate) fn memo_groups<Id: Clone>(
    per_computation: &PerComputationCandidates<Id>,
    entries: &[QueryWorkloadEntry],
    models: &PlanningModels<'_>,
) -> MemoGroups {
    let classes = Classes::of(per_computation, entries, models);
    let digits: Vec<Vec<usize>> = per_computation
        .roots
        .iter()
        .map(|(_, root)| query_digits(per_computation, root))
        .collect();
    let plans: Vec<Option<String>> = per_computation
        .roots
        .iter()
        .map(|(_, root)| serde_json::to_string(&flatten(std::slice::from_ref(root)).0).ok())
        .collect();
    let queries = per_computation.roots.len();
    let mut parent: Vec<usize> = (0..queries).collect();
    for a in 0..queries {
        for b in a + 1..queries {
            let identical =
                plans[a].is_some() && plans[a] == plans[b] && same_entry(&entries[a], &entries[b]);
            let common_target = digits[a].iter().any(|d| digits[b].contains(d));
            if identical || common_target {
                union(&mut parent, a, b);
            }
        }
    }
    for class in classes.members() {
        for pair in class.windows(2) {
            union(&mut parent, pair[0], pair[1]);
        }
    }
    let mut groups: Vec<MemoGroup> = Vec::new();
    let mut group_of_root: HashMap<usize, usize> = HashMap::new();
    for (q, own) in digits.iter().enumerate() {
        let root = find(&mut parent, q);
        let g = *group_of_root.entry(root).or_insert_with(|| {
            groups.push(MemoGroup {
                queries: Vec::new(),
                targets: Vec::new(),
            });
            groups.len() - 1
        });
        groups[g].queries.push(q);
        groups[g].targets.extend(own);
    }
    for group in &mut groups {
        group.targets.sort_unstable();
        group.targets.dedup();
    }
    let sites = Sites::of(per_computation, &groups, &digits, entries);
    MemoGroups {
        groups,
        classes,
        sites,
    }
}

/// Every candidate of group `group` of `groups`, planned alone, unpriced,
/// each with the sharing sites it exposes to the workload's other groups:
/// every Stage 1 candidate of the group's queries alone (a class of
/// interchangeable queries reading one set of builds, as the workload's
/// search requires), in enumeration order, each in every Stage 2
/// materialization. `per_computation` and `entries` are those
/// [`memo_groups`] split.
pub(crate) fn group_candidates<Id: Clone>(
    per_computation: &PerComputationCandidates<Id>,
    groups: &MemoGroups,
    group: usize,
    entries: &[QueryWorkloadEntry],
    data: &DataWorkload,
    models: PlanningModels<'_>,
) -> Vec<GroupCandidate> {
    let members = &groups.groups[group];
    let alone = PerComputationCandidates {
        roots: members
            .queries
            .iter()
            .map(|&q| per_computation.roots[q].clone())
            .collect(),
        targets: per_computation.targets.clone(),
    };
    let alone_entries: Vec<QueryWorkloadEntry> = members
        .queries
        .iter()
        .map(|&q| entries[q].clone())
        .collect();
    let classes = groups.classes.restricted_to(&members.queries);
    let in_group: Vec<bool> = (0..per_computation.targets.len())
        .map(|t| members.targets.binary_search(&t).is_ok())
        .collect();
    // Only the group's targets vary: every other digit stays 0.
    let invalid_prefix = |choice: &[usize]| {
        let outside = (0..choice.len()).find(|&d| !in_group[d] && choice[d] != 0);
        match (outside.map(|d| d + 1), classes.invalid_prefix(choice)) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        }
    };
    let stage1 = stage1_logical_asap_optimization_skipping(
        &alone,
        &alone_entries,
        invalid_prefix,
        |_| true,
        |roots| classes.shared_first(roots, &alone_entries),
    );
    let mut candidates = Vec::new();
    for (n, logical) in stage1.enumerate() {
        let Ok(roots) = logical.roots else {
            continue;
        };
        if !classes.reads_shared_builds(&roots) {
            continue;
        }
        let id = format!("G{}-L{}", group + 1, n + 1);
        let Ok(physical) = optimize_physical_asap(
            &id,
            &roots,
            &alone_entries,
            data,
            models.deployment_capabilities,
        ) else {
            continue;
        };
        let choice: Vec<usize> = members.targets.iter().map(|&t| logical.choice[t]).collect();
        for implemented in physical {
            candidates.push(GroupCandidate {
                group,
                choice: choice.clone(),
                signature: groups.sites.signature(group, &roots, &implemented),
                roots: roots.clone(),
                materialization: implemented.assignment,
                usage: None,
            });
        }
    }
    candidates
}

/// Where the workload's groups can meet: per group, the masked hashes of
/// its nodes a Pass 2 rule could make one node with a node of another
/// group's candidate, and each such hash's meeting key.
struct Sites {
    /// Per group, the masked hashes ([`NodeKeys::meeting`]) of its nodes
    /// that meet another group's.
    exposed: Vec<HashSet<u64>>,
    /// Each exposed masked hash's meeting key: the smallest hash it is
    /// joined with, through nodes that met.
    meeting_key: HashMap<u64, u64>,
}

impl Sites {
    /// The sites of `groups` of `per_computation`'s workload: every two
    /// queries of different groups that read a common source, with every
    /// choice of their own Pass 1 candidates (`digits[q]` are query `q`'s
    /// digits), composed together, and every node set a Pass 2 rule makes
    /// both read ([`meetings`]).
    fn of<Id: Clone>(
        per_computation: &PerComputationCandidates<Id>,
        groups: &[MemoGroup],
        digits: &[Vec<usize>],
        entries: &[QueryWorkloadEntry],
    ) -> Self {
        let queries = per_computation.roots.len();
        let mut group_of = vec![0; queries];
        for (g, group) in groups.iter().enumerate() {
            for &q in &group.queries {
                group_of[q] = g;
            }
        }
        let sources: Vec<HashSet<String>> = per_computation
            .roots
            .iter()
            .map(|(_, root)| source_names(root))
            .collect();
        let grids: Vec<Option<u64>> = entries
            .iter()
            .map(QueryWorkloadEntry::evaluation_grid_ms)
            .collect();
        let mut exposed = vec![HashSet::new(); groups.len()];
        let mut joined: HashMap<u64, u64> = HashMap::new();
        for a in 0..queries {
            for b in a + 1..queries {
                let (ga, gb) = (group_of[a], group_of[b]);
                if ga == gb || sources[a].is_disjoint(&sources[b]) {
                    continue;
                }
                let pair = PerComputationCandidates {
                    roots: vec![
                        per_computation.roots[a].clone(),
                        per_computation.roots[b].clone(),
                    ],
                    targets: per_computation.targets.clone(),
                };
                let pair_digits: Vec<usize> = digits[a].iter().chain(&digits[b]).copied().collect();
                let mut choice = vec![0; per_computation.targets.len()];
                loop {
                    if !per_computation.absorbed_choice_varies(&choice) {
                        if let Ok(composed) = compose_logical_candidate(&pair, &choice) {
                            let roots: Vec<QueryRoot> =
                                composed.into_iter().map(|(_, root)| root).collect();
                            let mut keys = NodeKeys::default();
                            for meeting in meetings(&roots, &[grids[a], grids[b]], false) {
                                let first: Vec<u64> =
                                    meeting.first.iter().map(|n| keys.meeting(n)).collect();
                                let second: Vec<u64> =
                                    meeting.second.iter().map(|n| keys.meeting(n)).collect();
                                exposed[ga].extend(&first);
                                exposed[gb].extend(&second);
                                let mut all = first.into_iter().chain(second);
                                let head = all.next().expect("a meeting has nodes");
                                joined.entry(head).or_insert(head);
                                for key in all {
                                    join(&mut joined, head, key);
                                }
                            }
                        }
                    }
                    if !next_digits(per_computation, &pair_digits, &mut choice) {
                        break;
                    }
                }
            }
        }
        // Each joined set's smallest hash is its meeting key.
        let keys: Vec<u64> = joined.keys().copied().collect();
        let mut smallest: HashMap<u64, u64> = HashMap::new();
        for &key in &keys {
            let root = find_key(&mut joined, key);
            let entry = smallest.entry(root).or_insert(key);
            *entry = (*entry).min(key);
        }
        let meeting_key = keys
            .into_iter()
            .map(|key| (key, smallest[&find_key(&mut joined, key)]))
            .collect();
        Self {
            exposed,
            meeting_key,
        }
    }

    /// The sharing signature of a candidate of group `group` with Stage 1
    /// roots `roots`, implemented as `physical`: each node of `roots` whose
    /// masked hash met another group's node, with its meeting key and a
    /// hash of the node as built (sizes, ranks, panes and windows
    /// included) and of the materializations `physical` gives its copies.
    fn signature(
        &self,
        group: usize,
        roots: &[QueryRoot],
        physical: &PhysicalCandidate,
    ) -> SharingSignature {
        let exposed = &self.exposed[group];
        if exposed.is_empty() {
            return SharingSignature::default();
        }
        let mut keys = NodeKeys::default();
        // Each physical node's materialization, by its hash as built; a node
        // built at two timings has two copies.
        let mut materialized: HashMap<u64, Vec<String>> = HashMap::new();
        for node in unique_nodes(&physical.roots) {
            let exact = keys.exact(&node);
            materialized
                .entry(exact)
                .or_default()
                .push(format!("{:?}", node.materialization));
        }
        let mut sites = Vec::new();
        for node in unique_nodes(roots) {
            let masked = keys.meeting(&node);
            if !exposed.contains(&masked) {
                continue;
            }
            let exact = keys.exact(&node);
            let mut materializations = materialized.get(&exact).cloned().unwrap_or_default();
            materializations.sort();
            sites.push(SharingSite {
                meeting_key: self.meeting_key[&masked],
                cost_key: hash_of(&(exact, materializations)),
            });
        }
        sites.sort_unstable();
        SharingSignature(sites)
    }
}

/// Hashes of operator nodes by their sub-DAGs, memoized per node (the memo
/// keeps each node alive, so its address is not reused).
#[derive(Default)]
struct NodeKeys {
    meeting: HashMap<*const OperatorNode, (Rc<OperatorNode>, u64)>,
    exact: HashMap<*const OperatorNode, (Rc<OperatorNode>, u64)>,
}

impl NodeKeys {
    /// The node as built: its operator over its children's exact hashes,
    /// without timing, materialization, guarantee or schema, which Stage 2
    /// and the checks derive.
    fn exact(&mut self, node: &Rc<OperatorNode>) -> u64 {
        if let Some((_, key)) = self.exact.get(&Rc::as_ptr(node)) {
            return *key;
        }
        let children: HashMap<*const OperatorNode, u64> = node
            .children()
            .into_iter()
            .map(|child| (Rc::as_ptr(child), self.exact(child)))
            .collect();
        let operator = node.operator.map_children(|c| children[&Rc::as_ptr(c)]);
        let key = hash_of(&serde_json::to_string(&operator).unwrap_or_default());
        self.exact.insert(Rc::as_ptr(node), (Rc::clone(node), key));
        key
    }

    /// The node with what a Pass 2 rule may change in sharing it masked,
    /// over its children's masked hashes: summary sizes and accuracy
    /// targets (summary capability sizes for the strictest reader),
    /// quantile ranks and top-k `k` (a ranking serves every `k`), a range
    /// window's length, a pane chain's panes and a merge's newest panes (a
    /// pane chain is the build over its window, so nested windows meet),
    /// and the labels a scan projects, a column-only `Project` passing its
    /// child through (scans widened to one label set).
    fn meeting(&mut self, node: &Rc<OperatorNode>) -> u64 {
        if let Some((_, key)) = self.meeting.get(&Rc::as_ptr(node)) {
            return *key;
        }
        let key = match &node.operator {
            Operator::NonASAP(NonASAPOp::Project { cols, child, .. })
                if cols
                    .iter()
                    .all(|item| matches!(item.expr, ScalarExpr::Column(_))) =>
            {
                self.meeting(child)
            }
            Operator::ASAP(ASAPOp::PaneChain { child, .. }) => {
                // The chain's input as the whole build's range window reads it.
                let window = masked_hash(serde_json::json!({
                    "NonASAP": {"TimeRange": {"kind": "range", "child": self.meeting(child)}}
                }));
                let operator = node.operator.map_children(|_| window);
                let mut value = serde_json::to_value(&operator).unwrap_or_default();
                if let Some(asap) = value.get_mut("ASAP").and_then(|v| v.as_object_mut()) {
                    if let Some(chain) = asap.remove("PaneChain") {
                        asap.insert("SummaryAgg".to_string(), chain);
                    }
                }
                masked_hash(value)
            }
            operator => {
                let children: HashMap<*const OperatorNode, u64> = node
                    .children()
                    .into_iter()
                    .map(|child| (Rc::as_ptr(child), self.meeting(child)))
                    .collect();
                let operator = operator.map_children(|c| children[&Rc::as_ptr(c)]);
                masked_hash(serde_json::to_value(&operator).unwrap_or_default())
            }
        };
        self.meeting
            .insert(Rc::as_ptr(node), (Rc::clone(node), key));
        key
    }
}

/// The hash of `value`, a serialized operator over its children's masked
/// hashes, with what sharing may change masked ([`NodeKeys::meeting`]).
fn masked_hash(mut value: serde_json::Value) -> u64 {
    fn mask(value: &mut serde_json::Value) {
        match value {
            serde_json::Value::Object(map) => {
                // A range window's length; an instant lookback stays.
                if map.get("kind").and_then(serde_json::Value::as_str) == Some("range") {
                    map.remove("range");
                }
                for field in [
                    "schema",
                    "params",
                    "accuracy",
                    "k",
                    "pane_width_ms",
                    "pane_count",
                    "newest_panes",
                ] {
                    map.remove(field);
                }
                // Families whose size is the second of a pair.
                for family in ["Sample", "Wavelet", "StatModel"] {
                    if let Some(serde_json::Value::Array(pair)) = map.get_mut(family) {
                        pair.truncate(1);
                    }
                }
                map.values_mut().for_each(mask);
            }
            serde_json::Value::Array(items) => items.iter_mut().for_each(mask),
            _ => {}
        }
    }
    mask_quantile_ranks(&mut value);
    mask(&mut value);
    hash_of(&value.to_string())
}

fn hash_of<T: Hash>(value: &T) -> u64 {
    let mut hasher = DefaultHasher::new();
    value.hash(&mut hasher);
    hasher.finish()
}

/// Every node `roots` reach, once each.
fn unique_nodes(roots: &[QueryRoot]) -> Vec<Rc<OperatorNode>> {
    let mut seen = HashSet::new();
    roots
        .iter()
        .flat_map(root_nodes)
        .filter(|node| seen.insert(Rc::as_ptr(node)))
        .collect()
}

/// The sources `root` scans, by name.
fn source_names(root: &QueryRoot) -> HashSet<String> {
    root_nodes(root)
        .iter()
        .filter_map(|node| match node.non_asap() {
            Some(NonASAPOp::Scan { source, .. }) => Some(match source {
                Source::TimeSeries { metric } => metric.clone(),
                Source::Table { table_ref } => table_ref.clone(),
            }),
            _ => None,
        })
        .collect()
}

fn find(parent: &mut [usize], q: usize) -> usize {
    let mut root = q;
    while parent[root] != root {
        root = parent[root];
    }
    parent[q] = root;
    root
}

fn union(parent: &mut [usize], a: usize, b: usize) {
    let (ra, rb) = (find(parent, a), find(parent, b));
    parent[ra.max(rb)] = ra.min(rb);
}

fn find_key(parent: &mut HashMap<u64, u64>, key: u64) -> u64 {
    let mut root = key;
    while parent[&root] != root {
        root = parent[&root];
    }
    parent.insert(key, root);
    root
}

fn join(parent: &mut HashMap<u64, u64>, a: u64, b: u64) {
    parent.entry(a).or_insert(a);
    parent.entry(b).or_insert(b);
    let (ra, rb) = (find_key(parent, a), find_key(parent, b));
    if ra != rb {
        parent.insert(ra.max(rb), ra.min(rb));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use asap_logical_asap_optimization::per_computation_candidates::generate_per_computation_candidates;
    use asap_logical_asap_optimization::test_support::{agg, entry_with, lower_promql, scan_from};
    use asap_types::ir::operator::AggIntent;
    use asap_types::ir::properties::AccuracyTarget;
    use asap_types::ir::schema::{DataType, Field, Schema};
    use asap_types::workload::{
        DataArrival, QueryRecurrence, RepeatedDemand, RepetitionInterval, SourceFacts,
    };

    /// A query repeating every `interval_ms`.
    fn repeating(interval_ms: u32) -> QueryRecurrence {
        QueryRecurrence::Repeated(RepeatedDemand::FixedInterval(RepetitionInterval(
            interval_ms,
        )))
    }

    /// Data arriving at 10 000 rows per second, each source holding 10 000
    /// series.
    fn data() -> DataWorkload {
        DataWorkload {
            arrival: DataArrival::Ingesting {
                rows_per_second: 10_000.0,
            },
            sources: ["m", "x", "t"]
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

    /// The memo groups of `roots` (query `q` with `entries[q]`) under the
    /// built-in models, and each group's candidates.
    fn planned(
        roots: Vec<QueryRoot>,
        entries: &[QueryWorkloadEntry],
    ) -> (MemoGroups, Vec<Vec<GroupCandidate>>) {
        let models = PlanningModels::builtin();
        let inventory =
            generate_per_computation_candidates(roots.into_iter().enumerate().collect())
                .unwrap()
                .without_unsupported_summaries(models.deployment_capabilities);
        let groups = memo_groups(&inventory, entries, &models);
        let candidates = (0..groups.groups.len())
            .map(|g| group_candidates(&inventory, &groups, g, entries, &data(), models))
            .collect();
        (groups, candidates)
    }

    /// PromQL `queries`, each within 1% error, repeating every `interval_ms`.
    fn promql(queries: &[&str], interval_ms: u32) -> (Vec<QueryRoot>, Vec<QueryWorkloadEntry>) {
        let target = AccuracyTarget::max_error(0.01);
        let roots = queries
            .iter()
            .map(|q| QueryRoot::Operator(lower_promql(q, target.clone())))
            .collect();
        let entries = queries
            .iter()
            .map(|_| entry_with(target.clone(), repeating(interval_ms)))
            .collect();
        (roots, entries)
    }

    fn queries(groups: &MemoGroups) -> Vec<Vec<usize>> {
        groups.groups.iter().map(|g| g.queries.clone()).collect()
    }

    /// The meeting keys of `candidate`'s sites.
    fn meeting_keys(candidate: &GroupCandidate) -> HashSet<u64> {
        candidate
            .signature
            .0
            .iter()
            .map(|site| site.meeting_key)
            .collect()
    }

    /// Whether some candidate of each of two groups has a site that meets
    /// one of the other's.
    fn meet(a: &[GroupCandidate], b: &[GroupCandidate]) -> bool {
        a.iter().any(|x| {
            let keys = meeting_keys(x);
            b.iter().any(|y| !keys.is_disjoint(&meeting_keys(y)))
        })
    }

    /// Two identical queries are one group: each candidate gives both the
    /// same choice (they are interchangeable under cost model 1), and a
    /// query over another metric is a group of its own that shares nothing.
    #[test]
    fn identical_queries_form_one_group() {
        let (roots, entries) = promql(
            &[
                "quantile_over_time(0.9, m[5m])",
                "quantile_over_time(0.9, m[5m])",
                "sum by (job) (rate(x[5m]))",
            ],
            60_000,
        );
        let (groups, candidates) = planned(roots, &entries);
        assert_eq!(queries(&groups), vec![vec![0, 1], vec![2]]);
        assert_eq!(groups.groups[0].targets.len(), 2);
        assert!(!candidates[0].is_empty());
        for candidate in &candidates[0] {
            assert_eq!(candidate.roots.len(), 2);
            assert_eq!(candidate.choice[0], candidate.choice[1]);
        }
        assert!(candidates
            .iter()
            .flatten()
            .all(|c| c.signature.0.is_empty()));
    }

    /// `q0.9 / q0.5` and the two quantiles: three groups (the ratio reads
    /// its own targets, and shares a rank node with each quantile, so the
    /// two are not interchangeable), each pair with a site that meets: the
    /// ratio's exact or sketched quantiles meet each quantile's (identical
    /// expression, or one quantile sketch over the same input at any size),
    /// and the two quantiles' sketches meet each other.
    #[test]
    fn a_ratio_of_quantiles_meets_each_quantile() {
        let (roots, entries) = promql(
            &[
                "quantile_over_time(0.9, m[5m]) / quantile_over_time(0.5, m[5m])",
                "quantile_over_time(0.9, m[5m])",
                "quantile_over_time(0.5, m[5m])",
            ],
            60_000,
        );
        let (groups, candidates) = planned(roots, &entries);
        assert_eq!(queries(&groups), vec![vec![0], vec![1], vec![2]]);
        assert_eq!(groups.groups[0].targets.len(), 2);
        assert!(meet(&candidates[0], &candidates[1]));
        assert!(meet(&candidates[0], &candidates[2]));
        assert!(meet(&candidates[1], &candidates[2]));
        // Every candidate exposes a site, and the quantiles' candidates
        // with the same choice expose the same meeting keys: rank is masked.
        assert!(candidates
            .iter()
            .flatten()
            .all(|c| !c.signature.0.is_empty()));
        for (one, other) in candidates[1].iter().zip(&candidates[2]) {
            assert_eq!(one.choice, other.choice);
            assert_eq!(meeting_keys(one), meeting_keys(other));
        }
        // Exact, each quantile shares only the scan and its window: no rule
        // makes the ratio's exact quantile one node with the query's. With a
        // sketch, the estimate is a site too, and its cost key tells the 0.9
        // quantile from the 0.5 one.
        for (one, other) in candidates[1].iter().zip(&candidates[2]) {
            match one.choice[..] {
                [0] => assert_eq!(one.signature, other.signature),
                _ => assert_ne!(one.signature, other.signature),
            }
        }
    }

    /// Two windows over one metric, evaluated every minute, meet: one pane
    /// chain of one-minute panes serves the 5-minute and the 1-hour window.
    /// Evaluated once, they have no evaluation grid, so no pane chain, and
    /// their builds over different windows share nothing.
    #[test]
    fn windows_meet_only_through_a_nested_pane_chain() {
        let windows = ["sum_over_time(m[5m])", "sum_over_time(m[1h])"];
        let (roots, entries) = promql(&windows, 60_000);
        let (groups, candidates) = planned(roots.clone(), &entries);
        assert_eq!(queries(&groups), vec![vec![0], vec![1]]);
        assert!(meet(&candidates[0], &candidates[1]));

        let once: Vec<QueryWorkloadEntry> = entries
            .into_iter()
            .map(|mut entry| {
                entry.recurrence = QueryRecurrence::OneTime {
                    invocations: 1,
                    execute_at: None,
                };
                entry
            })
            .collect();
        let (groups, candidates) = planned(roots, &once);
        assert_eq!(queries(&groups), vec![vec![0], vec![1]]);
        assert!(candidates
            .iter()
            .flatten()
            .all(|c| c.signature.0.is_empty()));
    }

    /// SQL sums over one table grouped by different columns: two groups,
    /// no site. The table scan has no unique key, so it is never made one
    /// node, and neither grouping rolls up into the other.
    #[test]
    fn sql_queries_with_different_groupings_expose_no_site() {
        let table = || {
            scan_from(
                Source::Table {
                    table_ref: "t".into(),
                },
                Schema::with_time_index(
                    vec![
                        Field::plain("ts", DataType::Timestamp, false),
                        Field::plain("value", DataType::Float64, false),
                        Field::plain("a", DataType::Utf8, true),
                        Field::plain("b", DataType::Utf8, true),
                    ],
                    0,
                    vec![],
                ),
            )
        };
        let sum = || AggIntent::Sum { col: Some(1) };
        // Each query lowered on its own reads its own scan.
        let roots = vec![
            QueryRoot::Operator(agg(vec![2], sum(), table())),
            QueryRoot::Operator(agg(vec![3], sum(), table())),
        ];
        let entries = vec![entry_with(AccuracyTarget::exact(), repeating(60_000)); 2];
        let (groups, candidates) = planned(roots, &entries);
        assert_eq!(queries(&groups), vec![vec![0], vec![1]]);
        assert!(candidates.iter().all(|c| !c.is_empty()));
        assert!(candidates
            .iter()
            .flatten()
            .all(|c| c.signature.0.is_empty()));
    }
}

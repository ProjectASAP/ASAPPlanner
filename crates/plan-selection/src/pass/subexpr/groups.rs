//! The hash-consed group DAG and Pass 1's implementation candidates per
//! group (design steps 1 and 2, Pass 1 part).
//!
//! Groups are the workload's logical DAG interned by
//! [`share_common_sub_dags`], the identical-expression rule's legality
//! included: structurally equal nodes are one group when sharing them is
//! legal (an ASAP node, or one whose schema has a unique key), so the
//! unkeyed scans of two queries stay two groups while a keyed aggregate
//! over them is one.
//!
//! A Pass 1 target's aggregate group gets one candidate per combination of
//! its measures' Pass 1 candidates: the aggregate alone composed by
//! [`compose_logical_candidate`] with that choice, every target beneath it
//! exact. Its private nodes are the composed nodes above the first node
//! equal to one of the group's descendant groups, which are its children.
//! Summaries are sized for their measure's whole budget, as composing the
//! aggregate alone does.

use std::collections::hash_map::DefaultHasher;
use std::collections::{HashMap, HashSet};
use std::hash::{Hash, Hasher};
use std::rc::Rc;

use asap_logical_asap_optimization::per_computation_candidates::{
    compose_logical_candidate, PerComputationCandidates,
};
use asap_types::ir::cse::share_common_sub_dags;
use asap_types::ir::{OperatorNode, QueryRoot};

use super::{CandidateOrigin, GroupId, ImplementationCandidate, Memo, MemoGroup};

/// The memo of `per_computation`'s workload: its roots hash-consed into
/// groups, children before parents, and each group's Pass 1 candidates.
/// `per_computation` is the workload's Pass 1 inventory, pruned for the
/// deployment's capabilities.
pub(crate) fn memo_of<Id: Clone>(per_computation: &PerComputationCandidates<Id>) -> Memo {
    let (groups, group_of) = hash_consed(&per_computation.roots);
    let candidates = pass1_candidates(per_computation, &groups, &group_of);
    Memo { groups, candidates }
}

/// Each original node (of `roots`), by address, to its group.
type GroupOf = HashMap<*const OperatorNode, GroupId>;

/// The groups of `roots`, children before parents, and each original node's
/// group. A scalar query's roots are the operator nodes its expression
/// reads.
fn hash_consed<Id>(roots: &[(Id, QueryRoot)]) -> (Vec<MemoGroup>, GroupOf) {
    let originals: Vec<(usize, Rc<OperatorNode>)> = roots
        .iter()
        .enumerate()
        .flat_map(|(query, (_, root))| {
            operator_roots(root)
                .into_iter()
                .map(move |node| (query, node))
        })
        .collect();
    let interned = share_common_sub_dags(originals.clone());
    // Number the interned nodes children first.
    let mut groups: Vec<MemoGroup> = Vec::new();
    let mut id_of: HashMap<*const OperatorNode, GroupId> = HashMap::new();
    for (_, root) in &interned {
        number(root, &mut groups, &mut id_of);
    }
    for (query, root) in &interned {
        let g = id_of[&Rc::as_ptr(root)];
        let roots_of = &mut groups[g.0 as usize].roots_of;
        if !roots_of.contains(query) {
            roots_of.push(*query);
        }
        let mut reached = HashSet::new();
        reach(&groups, g, &mut reached);
        for reached in reached {
            let queries = &mut groups[reached.0 as usize].queries;
            if !queries.contains(query) {
                queries.push(*query);
            }
        }
    }
    for group in &mut groups {
        group.queries.sort_unstable();
    }
    // Interning rebuilds every node over its children in order, so the two
    // DAGs are walked side by side.
    let mut group_of = GroupOf::new();
    for ((_, original), (_, root)) in originals.iter().zip(&interned) {
        pair(original, root, &id_of, &mut group_of);
    }
    (groups, group_of)
}

/// The operator nodes a query root reads.
fn operator_roots(root: &QueryRoot) -> Vec<Rc<OperatorNode>> {
    match root {
        QueryRoot::Operator(node) => vec![Rc::clone(node)],
        QueryRoot::Scalar(expr) => expr.operator_refs().into_iter().cloned().collect(),
    }
}

/// Number `node` (interned) and its descendants, children first.
fn number(
    node: &Rc<OperatorNode>,
    groups: &mut Vec<MemoGroup>,
    id_of: &mut HashMap<*const OperatorNode, GroupId>,
) -> GroupId {
    if let Some(&id) = id_of.get(&Rc::as_ptr(node)) {
        return id;
    }
    let children: Vec<GroupId> = node
        .children()
        .into_iter()
        .map(|child| number(child, groups, id_of))
        .collect();
    let id = GroupId(groups.len() as u32);
    for &child in &children {
        let parents = &mut groups[child.0 as usize].parents;
        if !parents.contains(&id) {
            parents.push(id);
        }
    }
    groups.push(MemoGroup {
        node: Rc::clone(node),
        children,
        parents: Vec::new(),
        queries: Vec::new(),
        roots_of: Vec::new(),
    });
    id_of.insert(Rc::as_ptr(node), id);
    id
}

/// `g` and every group below it.
fn reach(groups: &[MemoGroup], g: GroupId, reached: &mut HashSet<GroupId>) {
    if reached.insert(g) {
        for &child in &groups[g.0 as usize].children {
            reach(groups, child, reached);
        }
    }
}

/// Map `original` and its descendants to the groups of `interned`, its
/// interned copy.
fn pair(
    original: &Rc<OperatorNode>,
    interned: &Rc<OperatorNode>,
    id_of: &HashMap<*const OperatorNode, GroupId>,
    group_of: &mut GroupOf,
) {
    if group_of
        .insert(Rc::as_ptr(original), id_of[&Rc::as_ptr(interned)])
        .is_some()
    {
        return;
    }
    for (child, interned_child) in original.children().into_iter().zip(interned.children()) {
        pair(child, interned_child, id_of, group_of);
    }
}

/// Every group's Pass 1 candidates, by group index.
fn pass1_candidates<Id: Clone>(
    per_computation: &PerComputationCandidates<Id>,
    groups: &[MemoGroup],
    group_of: &GroupOf,
) -> Vec<Vec<ImplementationCandidate>> {
    // Each target aggregate group's targets, one per measure (identical
    // aggregates of several queries are one group with one target each).
    let mut targets_of: HashMap<GroupId, Vec<usize>> = HashMap::new();
    for (t, target) in per_computation.targets.iter().enumerate() {
        let g = group_of[&Rc::as_ptr(&target.aggregate)];
        let targets = targets_of.entry(g).or_default();
        if targets
            .iter()
            .all(|&other| per_computation.targets[other].measure != target.measure)
        {
            targets.push(t);
        }
    }
    let own = |g: usize| ImplementationCandidate {
        implements: vec![GroupId(g as u32)],
        node: Rc::clone(&groups[g].node),
        children: groups[g].children.clone(),
        origin: CandidateOrigin::Pass1,
        coupling_sites: Vec::new(),
    };
    (0..groups.len())
        .map(|g| match targets_of.get(&GroupId(g as u32)) {
            None => vec![own(g)],
            Some(targets) => target_candidates(
                per_computation,
                groups,
                group_of,
                GroupId(g as u32),
                targets,
            )
            .unwrap_or_else(|| vec![own(g)]),
        })
        .collect()
}

/// The candidates of target aggregate group `g`, whose measures' targets
/// are `targets`: one per combination of their Pass 1 candidates (the last
/// target varying fastest), each the aggregate composed alone with that
/// choice. `None` when none composes.
fn target_candidates<Id: Clone>(
    per_computation: &PerComputationCandidates<Id>,
    groups: &[MemoGroup],
    group_of: &GroupOf,
    g: GroupId,
    targets: &[usize],
) -> Option<Vec<ImplementationCandidate>> {
    let aggregate = Rc::clone(&per_computation.targets[targets[0]].aggregate);
    // The groups a candidate's private nodes may bottom out at, by key.
    let mut below = HashSet::new();
    for &child in &groups[g.0 as usize].children {
        reach(groups, child, &mut below);
    }
    let mut keys = NodeKeys::default();
    let mut descendant: HashMap<u64, GroupId> = HashMap::new();
    let mut ordered: Vec<GroupId> = below.into_iter().collect();
    ordered.sort_unstable();
    for d in ordered {
        descendant
            .entry(keys.key(&groups[d.0 as usize].node))
            .or_insert(d);
    }
    let own_key = keys.key(&groups[g.0 as usize].node);
    let mut candidates = Vec::new();
    let mut choice = vec![0; per_computation.targets.len()];
    loop {
        if let Ok(composed) = compose_alone(per_computation, &aggregate, targets, &choice) {
            if let Some((_, QueryRoot::Operator(root))) = composed.into_iter().next() {
                let mut implements = vec![g];
                for &t in targets {
                    if let Some((_, Some(absorbs))) =
                        per_computation.targets[t].computation_for(choice[t])
                    {
                        for &a in absorbs {
                            let absorbed =
                                group_of[&Rc::as_ptr(&per_computation.targets[a].aggregate)];
                            if !implements.contains(&absorbed) {
                                implements.push(absorbed);
                            }
                        }
                    }
                }
                let candidate = if keys.key(&root) == own_key {
                    ImplementationCandidate {
                        implements,
                        node: Rc::clone(&groups[g.0 as usize].node),
                        children: groups[g.0 as usize].children.clone(),
                        origin: CandidateOrigin::Pass1,
                        coupling_sites: Vec::new(),
                    }
                } else {
                    let mut children = Vec::new();
                    boundary(
                        &root,
                        &mut keys,
                        &descendant,
                        &mut children,
                        &mut HashSet::new(),
                    );
                    ImplementationCandidate {
                        implements,
                        node: root,
                        children,
                        origin: CandidateOrigin::Pass1,
                        coupling_sites: Vec::new(),
                    }
                };
                candidates.push(candidate);
            }
        }
        // Mixed-radix over this group's targets only.
        let mut advanced = false;
        for &t in targets.iter().rev() {
            choice[t] += 1;
            if choice[t] < per_computation.targets[t].choice_count() {
                advanced = true;
                break;
            }
            choice[t] = 0;
        }
        if !advanced {
            break;
        }
    }
    (!candidates.is_empty()).then_some(candidates)
}

/// `aggregate` composed alone with `choice[t]` for each of its measures'
/// `targets`, every other target left exact: the inventory holds only
/// `targets`, and the targets a chosen fused computation absorbs (not
/// computed then).
fn compose_alone<Id: Clone>(
    per_computation: &PerComputationCandidates<Id>,
    aggregate: &Rc<OperatorNode>,
    targets: &[usize],
    choice: &[usize],
) -> Result<
    Vec<(usize, QueryRoot)>,
    asap_logical_asap_optimization::per_computation_candidates::PerComputationCandidateError,
> {
    let mut kept: Vec<usize> = targets.to_vec();
    for &t in targets {
        if let Some((_, Some(absorbs))) = per_computation.targets[t].computation_for(choice[t]) {
            for &a in absorbs {
                if !kept.contains(&a) {
                    kept.push(a);
                }
            }
        }
    }
    let position = |t: usize| kept.iter().position(|&k| k == t);
    let alone = PerComputationCandidates {
        roots: vec![(0usize, QueryRoot::Operator(Rc::clone(aggregate)))],
        targets: kept
            .iter()
            .map(|&t| {
                let mut target = per_computation.targets[t].clone();
                if let Some(fused) = &mut target.fused {
                    // Absorbed targets not kept are never absorbed here: the
                    // fusion is kept only when chosen, with all of them.
                    match fused.absorbs.iter().map(|&a| position(a)).collect() {
                        Some(absorbs) => fused.absorbs = absorbs,
                        None => target.fused = None,
                    }
                }
                target
            })
            .collect(),
    };
    let alone_choice: Vec<usize> = kept
        .iter()
        .map(|&t| if targets.contains(&t) { choice[t] } else { 0 })
        .collect();
    compose_logical_candidate(&alone, &alone_choice)
}

/// The groups `node`'s private nodes read: walking down from `node`'s
/// children, the first node on each path equal to a group in `descendant`.
fn boundary(
    node: &Rc<OperatorNode>,
    keys: &mut NodeKeys,
    descendant: &HashMap<u64, GroupId>,
    children: &mut Vec<GroupId>,
    seen: &mut HashSet<*const OperatorNode>,
) {
    for child in node.children() {
        if !seen.insert(Rc::as_ptr(child)) {
            continue;
        }
        match descendant.get(&keys.key(child)) {
            Some(&group) => {
                if !children.contains(&group) {
                    children.push(group);
                }
            }
            None => boundary(child, keys, descendant, children, seen),
        }
    }
}

/// Structural hashes of nodes: the operator over its children's hashes,
/// without the schema, guarantee, timing and materialization composition
/// derives. Memoized per node (the memo keeps each node alive, so its
/// address is not reused).
#[derive(Default)]
struct NodeKeys {
    memo: HashMap<*const OperatorNode, (Rc<OperatorNode>, u64)>,
}

impl NodeKeys {
    fn key(&mut self, node: &Rc<OperatorNode>) -> u64 {
        if let Some((_, key)) = self.memo.get(&Rc::as_ptr(node)) {
            return *key;
        }
        let children: HashMap<*const OperatorNode, u64> = node
            .children()
            .into_iter()
            .map(|child| (Rc::as_ptr(child), self.key(child)))
            .collect();
        let operator = node.operator.map_children(|c| children[&Rc::as_ptr(c)]);
        let mut hasher = DefaultHasher::new();
        serde_json::to_string(&operator)
            .unwrap_or_default()
            .hash(&mut hasher);
        let key = hasher.finish();
        self.memo.insert(Rc::as_ptr(node), (Rc::clone(node), key));
        key
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use asap_logical_asap_optimization::per_computation_candidates::generate_per_computation_candidates;
    use asap_logical_asap_optimization::test_support::lower_promql;
    use asap_types::ir::properties::AccuracyTarget;

    use asap_types::ir::ASAPOp;

    /// The memo of PromQL `queries`, each with accuracy target `target`.
    fn memo(queries: &[&str], target: AccuracyTarget) -> Memo {
        let roots = queries
            .iter()
            .map(|q| QueryRoot::Operator(lower_promql(q, target.clone())))
            .enumerate()
            .collect();
        memo_of(&generate_per_computation_candidates(roots).unwrap())
    }

    /// The group of the first node of `kind` reached from query `q`'s root,
    /// walking down the groups' first children.
    fn group_of_kind(memo: &Memo, q: usize, kind: &str) -> usize {
        let mut g = memo
            .groups
            .iter()
            .position(|group| group.roots_of.contains(&q))
            .unwrap();
        while memo.groups[g].node.operator.kind_name() != kind {
            g = memo.groups[g].children[0].0 as usize;
        }
        g
    }

    /// The summary algorithms a candidate's private nodes build.
    fn algorithms(candidate: &ImplementationCandidate) -> Vec<String> {
        OperatorNode::reachable(&candidate.node)
            .iter()
            .filter_map(|node| match node.asap() {
                Some(ASAPOp::SummaryAgg { family, .. }) => Some(format!("{family:?}")),
                _ => None,
            })
            .collect()
    }

    /// The per-job rate sum both queries compute is one group, reached by
    /// both, the root of the first and read by the second's arithmetic.
    #[test]
    fn identical_sub_expressions_of_two_queries_are_one_group() {
        let memo = memo(
            &[
                "sum by (job) (rate(m[5m]))",
                "sum by (job) (rate(m[5m])) * 2",
            ],
            AccuracyTarget::exact(),
        );
        let sum = group_of_kind(&memo, 0, "Aggregate");
        assert_eq!(group_of_kind(&memo, 1, "Aggregate"), sum);
        let group = &memo.groups[sum];
        assert_eq!(group.queries, vec![0, 1]);
        assert_eq!(group.roots_of, vec![0]);
        let product = memo
            .groups
            .iter()
            .position(|group| group.roots_of == vec![1])
            .unwrap();
        assert_eq!(group.parents, vec![GroupId(product as u32)]);
        assert_eq!(memo.groups[product].children, vec![GroupId(sum as u32)]);
        // Children come before parents.
        for (g, group) in memo.groups.iter().enumerate() {
            assert!(group.children.iter().all(|c| (c.0 as usize) < g));
        }
    }

    /// A quantile's aggregate group has an exact candidate (its own node over
    /// its input's group) and a KLL and a DDSketch candidate over the same
    /// input group.
    #[test]
    fn a_quantile_target_group_has_exact_kll_and_ddsketch_candidates() {
        let memo = memo(
            &["quantile_over_time(0.9, m[5m])"],
            AccuracyTarget::max_error(0.01),
        );
        let quantile = group_of_kind(&memo, 0, "Aggregate");
        let input = memo.groups[quantile].children.clone();
        let candidates = &memo.candidates[quantile];
        assert_eq!(candidates.len(), 3);
        assert!(Rc::ptr_eq(&candidates[0].node, &memo.groups[quantile].node));
        assert!(algorithms(&candidates[0]).is_empty());
        assert!(algorithms(&candidates[1])[0].contains("Kll"));
        assert!(algorithms(&candidates[2])[0].contains("DDSketch"));
        for candidate in candidates {
            assert_eq!(candidate.implements, vec![GroupId(quantile as u32)]);
            assert_eq!(candidate.children, input);
            assert_eq!(candidate.origin, CandidateOrigin::Pass1);
        }
    }

    /// The whole-expression top-k heap over the per-job sums implements both
    /// aggregates' groups, and reads the sums' input group; the exact top-k
    /// reads the sums' group.
    #[test]
    fn a_fused_candidate_lists_both_groups_it_implements() {
        let memo = memo(
            &["topk by (job) (10, sum_over_time(m[1m]))"],
            AccuracyTarget::max_error(0.01).failure_probability(0.001),
        );
        let topk = group_of_kind(&memo, 0, "Aggregate");
        let sums = memo.groups[topk].children[0];
        let sums_input = memo.groups[sums.0 as usize].children.clone();
        let candidates = &memo.candidates[topk];
        assert_eq!(candidates[0].children, vec![sums]);
        let fused: Vec<_> = candidates
            .iter()
            .filter(|c| c.implements.len() > 1)
            .collect();
        assert!(!fused.is_empty());
        for candidate in fused {
            assert_eq!(candidate.implements, vec![GroupId(topk as u32), sums]);
            assert_eq!(candidate.children, sums_input);
            assert!(algorithms(candidate)[0].contains("Heap"));
        }
    }
}

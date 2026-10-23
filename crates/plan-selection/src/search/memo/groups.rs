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
//! The private nodes read each child group through its node (the same
//! `Rc`), and the candidate outputs the group's schema, so a parent reads
//! it in the group's place (a query's top-k root, which no parent reads,
//! may output the ranked value beside the key).
//!
//! A query whose value combines `n` approximate inputs splits its budget
//! into `n` equal shares, so a summary's size depends on its readers: each
//! sketch candidate is offered once per share `1..=n`, `n` the most
//! approximate inputs a query reaching the group can have
//! ([`ImplementationCandidate::budget_share`]). A plan is valid with the
//! share composition gives: the most approximate inputs among the queries
//! that read the summary, 1 when such a query's value carries no
//! guarantee. A smaller share fails the reader's accuracy check, a larger
//! one costs more.

use std::collections::hash_map::DefaultHasher;
use std::collections::{HashMap, HashSet};
use std::hash::{Hash, Hasher};
use std::rc::Rc;

use asap_logical_asap_optimization::per_computation_candidates::{
    compose_logical_candidate_with_budget_share, ComputationCandidate, PerComputationCandidates,
};
use asap_types::ir::cse::share_common_sub_dags;
use asap_types::ir::schema::FieldDataType;
use asap_types::ir::{ASAPOp, OperatorNode, QueryRoot};

use crate::search::memo::{CandidateOrigin, GroupId, ImplementationCandidate, Memo, MemoGroup};

/// The memo of `per_computation`'s workload: its roots hash-consed into
/// groups, children before parents, and each group's Pass 1 candidates.
/// `per_computation` is the workload's Pass 1 inventory, pruned for the
/// deployment's capabilities.
pub(crate) fn memo_of<Id: Clone>(per_computation: &PerComputationCandidates<Id>) -> Memo {
    let (groups, group_of) = hash_consed(&per_computation.roots);
    let candidates = pass1_candidates(per_computation, &groups, &group_of);
    Memo {
        groups,
        candidates,
        roots: per_computation
            .roots
            .iter()
            .map(|(_, root)| root.clone())
            .collect(),
    }
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
        materializations: Vec::new(),
        budget_share: 1,
        requires_exact_children: Vec::new(),
        rollup_of: None,
        requires_sites: Vec::new(),
    };
    // Each query's most approximate inputs: its targets that offer a
    // summary, each one sketch state at most.
    let offers_a_summary = |t: usize| {
        let target = &per_computation.targets[t];
        let summary = |c: &ComputationCandidate| matches!(c, ComputationCandidate::Summary(_));
        target.choices.candidates.iter().any(summary)
            || target
                .fused
                .as_ref()
                .is_some_and(|fused| fused.candidates.iter().any(summary))
    };
    let mut most_approximate_inputs = vec![0usize; per_computation.roots.len()];
    for (g, targets) in &targets_of {
        let summaries = targets.iter().filter(|&&t| offers_a_summary(t)).count();
        for &q in &groups[g.0 as usize].queries {
            most_approximate_inputs[q] += summaries;
        }
    }
    (0..groups.len())
        .map(|g| match targets_of.get(&GroupId(g as u32)) {
            None => vec![own(g)],
            Some(targets) => target_candidates(
                per_computation,
                groups,
                group_of,
                GroupId(g as u32),
                targets,
                groups[g]
                    .queries
                    .iter()
                    .map(|&q| most_approximate_inputs[q])
                    .max()
                    .unwrap_or(1)
                    .max(1),
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
    max_budget_share: usize,
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
        let mut implements = vec![g];
        for &t in targets {
            if let Some((_, Some(absorbs))) = per_computation.targets[t].computation_for(choice[t])
            {
                for &a in absorbs {
                    let absorbed = group_of[&Rc::as_ptr(&per_computation.targets[a].aggregate)];
                    if !implements.contains(&absorbed) {
                        implements.push(absorbed);
                    }
                }
            }
        }
        private_absorbed(groups, &mut implements);
        for budget_share in 1..=max_budget_share {
            let Ok(composed) =
                compose_alone(per_computation, &aggregate, targets, &choice, budget_share)
            else {
                continue;
            };
            let Some((_, QueryRoot::Operator(root))) = composed.into_iter().next() else {
                continue;
            };
            if budget_share > 1 && !builds_a_sketch(&root) {
                break;
            }
            let (node, children) = if keys.key(&root) == own_key {
                (
                    Rc::clone(&groups[g.0 as usize].node),
                    groups[g.0 as usize].children.clone(),
                )
            } else {
                let mut children = Vec::new();
                let node = over_groups(
                    &root,
                    groups,
                    &mut keys,
                    &descendant,
                    &mut children,
                    &mut HashMap::new(),
                );
                // A parent reads the candidate in the group's place, so it
                // outputs the group's schema when it has the same columns,
                // with the group's metadata (composition may derive a closed
                // label set where the frontend's node leaves it open). A
                // query's top-k root may output other columns (the ranked
                // value beside the key): it keeps its own.
                let group_node = &groups[g.0 as usize].node;
                let same_columns = node.schema.fields == group_node.schema.fields
                    && node.schema.time_index == group_node.schema.time_index;
                let node = if !same_columns || node.schema == group_node.schema {
                    node
                } else {
                    Rc::new(OperatorNode {
                        schema: group_node.schema.clone(),
                        ..(*node).clone()
                    })
                };
                (node, children)
            };
            candidates.push(ImplementationCandidate {
                implements: implements.clone(),
                node,
                children,
                origin: CandidateOrigin::Pass1,
                coupling_sites: Vec::new(),
                materializations: Vec::new(),
                budget_share,
                requires_exact_children: Vec::new(),
                rollup_of: None,
                requires_sites: Vec::new(),
            });
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

/// Keeps in `implements` (the group, then the groups its fused computation
/// absorbs) only the absorbed groups no other reader needs: no query's root,
/// and every parent in `implements`. Pass 1 fuses an aggregate only into its
/// single consumer within one query; the memo merges equal nodes across
/// queries, so an absorbed group may also be another query's root or
/// another group's child. Such a group (and, in turn, what only it reads)
/// is computed privately inside the fused candidate and stays its own group
/// for its other readers, so e.g. `topk(5, sum by (job) (…))` by a heap
/// never answers a second query's `sum by (job) (…)`.
fn private_absorbed(groups: &[MemoGroup], implements: &mut Vec<GroupId>) {
    loop {
        let shared = implements[1..].iter().position(|a| {
            let group = &groups[a.0 as usize];
            !group.roots_of.is_empty() || group.parents.iter().any(|p| !implements.contains(p))
        });
        match shared {
            Some(i) => {
                implements.remove(i + 1);
            }
            None => break,
        }
    }
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
    budget_share: usize,
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
    compose_logical_candidate_with_budget_share(&alone, &alone_choice, budget_share)
}

/// `node` with each first node on a path down from its children that
/// equals a group in `descendant` replaced by that group's node (the same
/// `Rc`), and those groups pushed to `children` in the order met: the
/// candidate's private nodes over its child groups.
fn over_groups(
    node: &Rc<OperatorNode>,
    groups: &[MemoGroup],
    keys: &mut NodeKeys,
    descendant: &HashMap<u64, GroupId>,
    children: &mut Vec<GroupId>,
    rebuilt: &mut HashMap<*const OperatorNode, Rc<OperatorNode>>,
) -> Rc<OperatorNode> {
    let mut replaced: HashMap<*const OperatorNode, Rc<OperatorNode>> = HashMap::new();
    for child in node.children() {
        let key = Rc::as_ptr(child);
        if replaced.contains_key(&key) {
            continue;
        }
        let new = match descendant.get(&keys.key(child)) {
            Some(&group) => {
                if !children.contains(&group) {
                    children.push(group);
                }
                Rc::clone(&groups[group.0 as usize].node)
            }
            None => match rebuilt.get(&key) {
                Some(done) => Rc::clone(done),
                None => {
                    let done = over_groups(child, groups, keys, descendant, children, rebuilt);
                    rebuilt.insert(key, Rc::clone(&done));
                    done
                }
            },
        };
        replaced.insert(key, new);
    }
    if node
        .children()
        .into_iter()
        .all(|child| Rc::ptr_eq(child, &replaced[&Rc::as_ptr(child)]))
    {
        return Rc::clone(node);
    }
    Rc::new(OperatorNode {
        operator: node
            .operator
            .map_children(|child| Rc::clone(&replaced[&Rc::as_ptr(child)])),
        ..(**node).clone()
    })
}

/// Whether `node`'s private nodes build a sketch, which a budget share sizes.
fn builds_a_sketch(node: &Rc<OperatorNode>) -> bool {
    OperatorNode::reachable(node).iter().any(|n| {
        matches!(
            n.asap(),
            Some(
                ASAPOp::SummaryAgg {
                    family: FieldDataType::Sketch(..),
                    ..
                } | ASAPOp::PaneChain {
                    family: FieldDataType::Sketch(..),
                    ..
                }
            )
        )
    })
}

/// Structural hashes of nodes: the operator over its children's hashes,
/// without the schema, guarantee, timing and materialization composition
/// derives. Memoized per node (the memo keeps each node alive, so its
/// address is not reused).
#[derive(Debug, Default)]
pub(crate) struct NodeKeys {
    memo: HashMap<*const OperatorNode, (Rc<OperatorNode>, u64)>,
}

impl NodeKeys {
    pub(crate) fn key(&mut self, node: &Rc<OperatorNode>) -> u64 {
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

impl Memo {
    /// The groups, each after every group it or one of its candidates
    /// reads, the lower index first among those ready: the order the
    /// dynamic program visits them, whatever order transformations appended
    /// groups in. `None` when those reads form a cycle.
    pub(crate) fn topological_order(&self) -> Option<Vec<GroupId>> {
        let count = self.groups.len();
        let mut reads: Vec<HashSet<usize>> = vec![HashSet::new(); count];
        for (g, group) in self.groups.iter().enumerate() {
            let candidates = self.candidates.get(g).map_or(&[][..], Vec::as_slice);
            for child in group
                .children
                .iter()
                .chain(candidates.iter().flat_map(|c| &c.children))
            {
                if child.0 as usize != g {
                    reads[g].insert(child.0 as usize);
                }
            }
        }
        let mut readers: Vec<Vec<usize>> = vec![Vec::new(); count];
        let mut waiting: Vec<usize> = reads.iter().map(HashSet::len).collect();
        for (g, read) in reads.iter().enumerate() {
            for &child in read {
                readers[child].push(g);
            }
        }
        let mut ready: std::collections::BinaryHeap<std::cmp::Reverse<usize>> = (0..count)
            .filter(|&g| waiting[g] == 0)
            .map(std::cmp::Reverse)
            .collect();
        let mut order = Vec::with_capacity(count);
        while let Some(std::cmp::Reverse(g)) = ready.pop() {
            order.push(GroupId(g as u32));
            for &reader in &readers[g] {
                waiting[reader] -= 1;
                if waiting[reader] == 0 {
                    ready.push(std::cmp::Reverse(reader));
                }
            }
        }
        (order.len() == count).then_some(order)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use asap_logical_asap_optimization::per_computation_candidates::{
        compose_logical_candidate, generate_per_computation_candidates,
    };
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

    /// Every candidate reads its child groups through their nodes (the same
    /// `Rc`), and outputs its group's schema, so a parent can read it in
    /// the group's place.
    #[test]
    fn candidates_read_child_group_nodes_and_output_the_group_schema() {
        for (queries, target) in [
            (
                vec!["quantile_over_time(0.9, m[5m]) / quantile_over_time(0.5, m[5m])"],
                AccuracyTarget::max_error(0.01),
            ),
            (
                vec!["topk by (job) (10, sum_over_time(m[1m]))"],
                AccuracyTarget::max_error(0.01).failure_probability(0.001),
            ),
            (vec!["sum by (job) (rate(m[5m]))"], AccuracyTarget::exact()),
        ] {
            let memo = memo(&queries, target);
            for (g, candidates) in memo.candidates.iter().enumerate() {
                for candidate in candidates {
                    let reached: HashSet<*const OperatorNode> =
                        OperatorNode::reachable(&candidate.node)
                            .iter()
                            .map(Rc::as_ptr)
                            .collect();
                    for child in &candidate.children {
                        let node = &memo.groups[child.0 as usize].node;
                        assert!(reached.contains(&Rc::as_ptr(node)), "{queries:?} g{g}");
                    }
                    assert_eq!(candidate.node.schema, memo.groups[g].node.schema);
                }
            }
        }
    }

    /// The sketch summaries of the first query of `queries` composed with
    /// every target at its `algorithm` summary, by [`compose_logical_candidate`]:
    /// as the exhaustive search sizes them.
    fn composed_sketches(
        queries: &[&str],
        target: &AccuracyTarget,
        algorithm: &str,
    ) -> Vec<String> {
        let roots = queries
            .iter()
            .map(|q| QueryRoot::Operator(lower_promql(q, target.clone())))
            .enumerate()
            .collect();
        let inventory = generate_per_computation_candidates(roots).unwrap();
        let choice: Vec<usize> = inventory
            .targets
            .iter()
            .map(|t| {
                t.choices
                    .candidates
                    .iter()
                    .position(|c| {
                        matches!(c, ComputationCandidate::Summary(a) if format!("{a:?}") == algorithm)
                    })
                    .unwrap()
            })
            .collect();
        let composed = compose_logical_candidate(&inventory, &choice).unwrap();
        let QueryRoot::Operator(root) = &composed[0].1 else {
            unreachable!("an operator root")
        };
        OperatorNode::reachable(root)
            .iter()
            .filter_map(|n| match n.asap() {
                Some(ASAPOp::SummaryAgg { family, .. }) => Some(format!("{family:?}")),
                _ => None,
            })
            .collect()
    }

    /// The sketch candidates of `memo`'s group `g`: each one's summary and
    /// budget share.
    fn sketch_shares(memo: &Memo, g: usize) -> Vec<(String, usize)> {
        memo.candidates[g]
            .iter()
            .filter(|c| !algorithms(c).is_empty())
            .map(|c| (algorithms(c)[0].clone(), c.budget_share))
            .collect()
    }

    /// `q0.9 / q0.5` beside a plain `q0.9`: each of the ratio's quantile
    /// groups offers every sketch for the whole budget and for half of it,
    /// since its query has two inputs that may be approximate; the plain
    /// query's quantile group (its own: its unkeyed window is not shared)
    /// offers only the whole budget. A ratio of quantiles carries no
    /// guarantee, so composition does not split its budget: the
    /// whole-budget candidate is the exhaustive search's sizing, and the
    /// half-budget one, larger, is never the cheaper valid one.
    #[test]
    fn a_ratio_operand_has_a_candidate_per_budget_share() {
        let queries = [
            "quantile_over_time(0.9, m[5m]) / quantile_over_time(0.5, m[5m])",
            "quantile_over_time(0.9, m[5m])",
        ];
        let target = AccuracyTarget::max_error(0.01);
        let memo = memo(&queries, target.clone());
        let ratio = memo
            .groups
            .iter()
            .position(|group| group.roots_of == vec![0])
            .unwrap();
        let operand = memo.groups[ratio].children[0].0 as usize;
        for operand in &memo.groups[ratio].children {
            let shares: Vec<usize> = sketch_shares(&memo, operand.0 as usize)
                .into_iter()
                .map(|(_, share)| share)
                .collect();
            assert_eq!(shares, vec![1, 2, 1, 2]);
        }
        let plain = group_of_kind(&memo, 1, "Aggregate");
        let plain_shares: Vec<usize> = sketch_shares(&memo, plain)
            .into_iter()
            .map(|(_, share)| share)
            .collect();
        assert_eq!(plain_shares, vec![1, 1]);
        let whole_kll = &sketch_shares(&memo, operand)[0];
        assert_eq!(whole_kll.1, 1);
        assert_eq!(
            composed_sketches(&queries[..1], &target, "Kll"),
            vec![whole_kll.0.clone(); 2]
        );
    }

    /// A difference of two Count-Min counts splits its budget: each
    /// operand's half-budget candidate is the Count-Min composition sizes
    /// for the difference with both counts sketched.
    #[test]
    fn a_split_budget_matches_its_half_budget_candidate() {
        let queries = ["count by (job) (a) - count by (job) (b)"];
        let target = AccuracyTarget::max_error(0.01).failure_probability(0.001);
        let memo = memo(&queries, target.clone());
        let difference = memo
            .groups
            .iter()
            .position(|group| group.roots_of == vec![0])
            .unwrap();
        let operand = memo.groups[difference].children[0].0 as usize;
        let half = sketch_shares(&memo, operand)
            .into_iter()
            .find(|(family, share)| family.contains("Cms") && *share == 2)
            .unwrap();
        assert_eq!(composed_sketches(&queries, &target, "Cms"), vec![half.0; 2]);
    }

    /// A group appended after its readers (as a transformation appends a
    /// ranking group) is ordered before every group whose candidate reads
    /// it.
    #[test]
    fn topological_order_puts_appended_groups_before_their_readers() {
        let mut memo = memo(
            &["quantile_over_time(0.9, m[5m])"],
            AccuracyTarget::max_error(0.01),
        );
        assert_eq!(
            memo.topological_order().unwrap(),
            (0..memo.groups.len() as u32)
                .map(GroupId)
                .collect::<Vec<_>>()
        );
        let quantile = group_of_kind(&memo, 0, "Aggregate");
        let appended = GroupId(memo.groups.len() as u32);
        memo.groups.push(memo.groups[1].clone());
        memo.candidates.push(memo.candidates[1].clone());
        let mut reader = memo.candidates[quantile][0].clone();
        reader.children = vec![appended];
        memo.candidates[quantile].push(reader);
        let order = memo.topological_order().unwrap();
        let position = |g: GroupId| order.iter().position(|&x| x == g).unwrap();
        assert!(position(appended) < position(GroupId(quantile as u32)));
        // A cycle has no order.
        memo.candidates[1][0]
            .children
            .push(GroupId(quantile as u32));
        assert_eq!(memo.topological_order(), None);
    }

    /// The workload generator of `joint_search_equivalence`: a seeded
    /// xorshift generator.
    struct Rng(u64);

    impl Rng {
        fn below(&mut self, n: usize) -> usize {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            (self.0 % n as u64) as usize
        }

        fn pick<'a, T>(&mut self, items: &'a [T]) -> &'a T {
            &items[self.below(items.len())]
        }
    }

    /// One random PromQL query of `joint_search_equivalence`.
    fn random_query(rng: &mut Rng, gauges: &[&str]) -> String {
        let m = rng.pick(gauges);
        let group = rng.pick(&["job", "job, instance", "instance"]);
        let window = rng.pick(&["1m", "5m", "10m"]);
        let phi = rng.pick(&["0.5", "0.9", "0.99"]);
        let k = rng.pick(&["3", "5"]);
        match rng.below(10) {
            0 => format!("sum by ({group}) (sum_over_time({m}[{window}]))"),
            1 => format!("avg by ({group}) (quantile_over_time({phi}, {m}[{window}]))"),
            2 => format!("quantile_over_time({phi}, {m}[{window}])"),
            3 => format!("topk by ({group}) ({k}, sum_over_time({m}[{window}]))"),
            4 => format!("sum by ({group}) (rate(c_total[{window}]))"),
            5 => format!("max by ({group}) (max_over_time({m}[{window}]))"),
            6 => format!("count_over_time({m}[{window}])"),
            7 => format!("sum by ({group}) (increase(c_total[{window}]))"),
            8 => format!("stddev_over_time({m}[{window}])"),
            _ => format!("topk({k}, sum by ({group}) (rate(c_total[{window}])))"),
        }
    }

    /// The roots of `joint_search_equivalence`'s random workload `seed`
    /// over `gauges`, lowered as the planner lowers PromQL (with series
    /// identity).
    fn random_roots(seed: u64, gauges: &[&str]) -> Vec<QueryRoot> {
        use asap_types::ir::properties::AccuracyMetric;
        use asap_types::workload::{
            DataArrival, DataWorkload, DurationMs, MetricType, PlanningWorkload, Predictability,
            Query, QueryLanguage, QueryRequirements, QueryWorkload, RepeatedDemand, RepeatingEntry,
            RepetitionInterval, SourceFacts,
        };
        let mut rng = Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1);
        let queries = 2 + rng.below(3);
        let entries = (0..queries)
            .map(|_| RepeatingEntry {
                query: Query(random_query(&mut rng, gauges)),
                demand: RepeatedDemand::FixedInterval(RepetitionInterval(
                    *rng.pick(&[60_000u32, 300_000]),
                )),
                requirements: QueryRequirements {
                    accuracy: rng
                        .pick(&[
                            AccuracyTarget::exact(),
                            AccuracyTarget::max_error(0.05),
                            AccuracyTarget::max_error(0.01).failure_probability(0.01),
                            AccuracyTarget::max_error(0.05).accuracy_metric(AccuracyMetric::Rank),
                        ])
                        .clone(),
                    max_latency_ms: None,
                },
                predictability: Predictability::Predictable { known_at: None },
                range_evaluation: None,
            })
            .collect();
        let source = |metric_type| SourceFacts {
            series_count: Some(1_000),
            metric_type: Some(metric_type),
            sample_interval: Some(DurationMs(15_000)),
            ..SourceFacts::default()
        };
        let workload = PlanningWorkload {
            query_workload: QueryWorkload {
                language: QueryLanguage::PromQL,
                query_batch: None,
                repeating_queries: Some(entries),
            },
            data_workload: Some(DataWorkload {
                arrival: DataArrival::Ingesting {
                    rows_per_second: 2_000.0 / 15.0,
                },
                sources: gauges
                    .iter()
                    .map(|g| (g.to_string(), source(MetricType::Gauge)))
                    .chain([("c_total".to_string(), source(MetricType::Counter))])
                    .collect(),
            }),
        };
        asap_frontend_promql::lower_promql_query_workload(&workload)
            .unwrap()
            .into_iter()
            .map(|root| match root {
                QueryRoot::Operator(node) => QueryRoot::Operator(
                    asap_types::ir::schema::schema_support::with_promql_series_identity(&node)
                        .unwrap(),
                ),
                scalar => scalar,
            })
            .collect()
    }

    /// Whether a composed sub-DAG is some combination of the memo's
    /// per-group candidates: a candidate's private nodes equal the composed
    /// nodes operator by operator (schemas and guarantees aside, which
    /// composition derives), and each child group's node is matched by a
    /// candidate of that group in turn.
    struct Combination<'a> {
        memo: &'a Memo,
        matched: HashMap<(usize, *const OperatorNode), bool>,
        own: HashMap<*const OperatorNode, (Rc<OperatorNode>, serde_json::Value)>,
    }

    impl Combination<'_> {
        fn own_fields(&mut self, node: &Rc<OperatorNode>) -> serde_json::Value {
            self.own
                .entry(Rc::as_ptr(node))
                .or_insert_with(|| {
                    let operator = node.operator.map_children(|_| ());
                    (Rc::clone(node), serde_json::to_value(&operator).unwrap())
                })
                .1
                .clone()
        }

        fn group(&mut self, g: usize, composed: &Rc<OperatorNode>) -> bool {
            let key = (g, Rc::as_ptr(composed));
            if let Some(&done) = self.matched.get(&key) {
                return done;
            }
            self.matched.insert(key, false);
            let memo = self.memo;
            let found = memo.candidates[g]
                .iter()
                .any(|c| self.private(&c.node, composed, &c.children));
            self.matched.insert(key, found);
            found
        }

        fn private(
            &mut self,
            node: &Rc<OperatorNode>,
            composed: &Rc<OperatorNode>,
            children: &[GroupId],
        ) -> bool {
            let memo = self.memo;
            if let Some(child) = children
                .iter()
                .find(|c| Rc::ptr_eq(&memo.groups[c.0 as usize].node, node))
            {
                return self.group(child.0 as usize, composed);
            }
            let (ours, theirs) = (node.children(), composed.children());
            ours.len() == theirs.len()
                && self.own_fields(node) == self.own_fields(composed)
                && ours
                    .iter()
                    .zip(&theirs)
                    .all(|(a, b)| self.private(a, b, children))
        }
    }

    /// For the random workloads of `joint_search_equivalence` (over one
    /// gauge and over two), every whole-workload Pass 1 choice composes,
    /// for each query, a plan that is a combination of the memo's per-group
    /// candidates: the memo misses no Pass 1 candidate. Every target's
    /// group has the target's own and fused candidates, also a target a
    /// fused computation absorbs; every candidate of a group a parent reads
    /// outputs the group's schema.
    #[test]
    fn every_composed_pass1_candidate_is_a_combination_of_group_candidates() {
        let mut checked = 0;
        let mut missing = Vec::new();
        let mut other_schema = Vec::new();
        for gauges in [&["m"][..], &["m", "n"]] {
            for seed in 1..=60u64 {
                let roots = random_roots(seed, gauges);
                let inventory = generate_per_computation_candidates(
                    roots.iter().cloned().enumerate().collect(),
                )
                .unwrap();
                if inventory.combinations() > 400 {
                    continue;
                }
                let memo = memo_of(&inventory);
                // Each target's own and fused candidates, absorbed or not.
                let group_of = |t: usize| {
                    memo.groups
                        .iter()
                        .position(|group| *group.node == *inventory.targets[t].aggregate)
                        .unwrap()
                };
                for (t, target) in inventory.targets.iter().enumerate() {
                    let g = group_of(t);
                    if inventory.targets.iter().enumerate().any(|(u, other)| {
                        u != t && group_of(u) == g && other.measure != target.measure
                    }) {
                        continue;
                    }
                    let whole_budget = |fused: bool| {
                        memo.candidates[g]
                            .iter()
                            .filter(|c| c.budget_share == 1 && (c.implements.len() > 1) == fused)
                            .count()
                    };
                    let fused = target.fused.as_ref().map_or(0, |f| f.candidates.len());
                    if whole_budget(false) != target.choices.candidates.len()
                        || whole_budget(true) != fused
                    {
                        missing.push(format!("{gauges:?} seed {seed} target {t} candidates"));
                    }
                }
                for (g, group) in memo.groups.iter().enumerate() {
                    for candidate in &memo.candidates[g] {
                        if !group.parents.is_empty() && candidate.node.schema != group.node.schema {
                            other_schema.push(format!("{gauges:?} seed {seed} g{g}"));
                        }
                    }
                }
                let root_group = |q: usize| {
                    memo.groups
                        .iter()
                        .position(|group| group.roots_of.contains(&q))
                        .unwrap()
                };
                let mut combination = Combination {
                    memo: &memo,
                    matched: HashMap::new(),
                    own: HashMap::new(),
                };
                let mut choice = vec![0; inventory.targets.len()];
                loop {
                    if !inventory.absorbed_choice_varies(&choice) {
                        if let Ok(composed) = compose_logical_candidate(&inventory, &choice) {
                            for (q, (_, root)) in composed.iter().enumerate() {
                                let QueryRoot::Operator(node) = root else {
                                    continue;
                                };
                                checked += 1;
                                if !combination.group(root_group(q), node) {
                                    missing.push(format!("{gauges:?} seed {seed} q{q} {choice:?}"));
                                }
                            }
                        }
                    }
                    let mut advanced = false;
                    for t in (0..choice.len()).rev() {
                        choice[t] += 1;
                        if choice[t] < inventory.targets[t].choice_count() {
                            advanced = true;
                            break;
                        }
                        choice[t] = 0;
                    }
                    if !advanced {
                        break;
                    }
                }
            }
        }
        assert!(checked > 100, "{checked} plans checked");
        assert!(
            missing.is_empty(),
            "{} of {checked} missing: {missing:#?}",
            missing.len()
        );
    }

    /// Two SQL queries over one table, each lowered with its own scan, read
    /// one scan group: equal scans are shared, a table's keyless rows
    /// included. Their aggregates, grouped differently, stay two groups.
    #[test]
    fn equal_sql_table_scans_are_one_group() {
        use asap_logical_asap_optimization::test_support::{agg, scan_from};
        use asap_types::ir::operator::{AggIntent, Source};
        use asap_types::ir::schema::{DataType, Field, Schema};
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
        let roots = vec![
            (0, QueryRoot::Operator(agg(vec![2], sum(), table()))),
            (1, QueryRoot::Operator(agg(vec![3], sum(), table()))),
        ];
        let memo = memo_of(&generate_per_computation_candidates(roots).unwrap());
        assert_eq!(memo.groups.len(), 3);
        assert_eq!(memo.groups[0].node.operator.kind_name(), "Scan");
        assert_eq!(memo.groups[0].queries, vec![0, 1]);
        assert_eq!(memo.groups[0].parents, vec![GroupId(1), GroupId(2)]);
    }
}

//! The hash-consed group DAG and Pass 1's implementation candidates per
//! group (design steps 1 and 2, Pass 1 part).
//!
//! Groups are the workload's logical DAG interned by
//! [`crate::pass2::cse::share_common_sub_dags`], the identical-expression rule's legality
//! included: structurally equal nodes are one group when sharing them is
//! legal: an ASAP node, or a non-ASAP node that returns the same rows for
//! the same input rows (`NonASAPOp::is_shareable_when_equal`).
//! A unique key is not needed; an unordered `LIMIT` stays one group per
//! reader.
//!
//! A Pass 1 target's aggregate group gets one candidate per combination of
//! its measures' Pass 1 candidates: the aggregate alone composed by
//! [`crate::pass1::per_computation_candidates::compose_logical_candidate`] with that choice, every target beneath it
//! exact. Its local nodes are the composed nodes above the first node
//! equal to one of the group's descendant groups, which are its children.
//! The local nodes read each child group through its node (the same
//! `Rc`), and the candidate outputs the group's schema, so a parent reads
//! it in the group's place (a query's top-k root, which no parent reads,
//! may output the ranked value beside the key).
//!
//! A query whose value combines `n` approximate inputs splits its budget
//! into `n` equal shares, so a summary's size depends on its readers: each
//! sketch candidate is offered once per share `1..=n`, `n` the most
//! approximate inputs a query reaching the group can have
//! ([`CandidateForMemoGroup::accuracy_budget_parts`]). This records the sizing
//! configuration used during generation; the concrete sketch parameters carry
//! the resulting error contract. The connection planner may select a conservative
//! larger configuration, and validates the recomposed query-root guarantee.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::rc::Rc;

use super::DagOfLogicalMemoGroups;
use crate::pass1::per_computation_candidates::{
    compose_logical_candidate_with_budget_share, HowToComputeAggregateMeasure,
    PerComputationCandidates,
};
use asap_types::ir::schema::FieldDataType;
use asap_types::ir::{ASAPOp, OperatorNode, QueryRoot};

use super::candidates::{
    CandidateForMemoGroup, CandidateMemo, CandidatePopulationError, OriginOfCandidateForMemoGroup,
};
use super::{MemoGroup, MemoGroupId};

/// Populate implementation candidates without changing initial group identities.
/// The inventory must retain the original ordered root dependencies. This phase
/// consumes existing Pass 1 choices; it does not yet generate rules per group.
pub fn try_populate_pass1<Id: Clone>(
    initial: DagOfLogicalMemoGroups,
    per_computation: &PerComputationCandidates<Id>,
) -> Result<CandidateMemo, CandidatePopulationError> {
    if !initial.matches_root_dependencies(&per_computation.roots) {
        return Err(CandidatePopulationError::RootDependenciesChanged);
    }
    for (index, target) in per_computation.targets.iter().enumerate() {
        if initial.group_for_node(&target.aggregate).is_none()
            || !matches!(target.aggregate.non_asap(), Some(asap_types::ir::NonASAPOp::Aggregate { measures, .. }) if target.measure_index < measures.len())
        {
            return Err(CandidatePopulationError::InvalidTarget { target: index });
        }
        if let Some(fused) = &target.fused_computation {
            if let Some(&absorbed) = fused
                .absorbed_targets
                .iter()
                .find(|&&absorbed| absorbed >= per_computation.targets.len())
            {
                return Err(CandidatePopulationError::InvalidAbsorption {
                    target: index,
                    absorbed,
                });
            }
        }
    }
    let candidates = pass1_candidates(per_computation, &initial.groups, &initial);
    Ok(CandidateMemo {
        groups: initial.groups,
        query_roots: initial.query_roots,
        candidates,
    })
}

/// `g` and every group below it.
fn reach(groups: &[MemoGroup], g: MemoGroupId, reached: &mut HashSet<MemoGroupId>) {
    if reached.insert(g) {
        for &child in &groups[g.0 as usize].child_groups {
            reach(groups, child, reached);
        }
    }
}

/// Every group's Pass 1 candidates, by group index.
fn pass1_candidates<Id: Clone>(
    per_computation: &PerComputationCandidates<Id>,
    groups: &[MemoGroup],
    group_of: &DagOfLogicalMemoGroups,
) -> Vec<Vec<CandidateForMemoGroup>> {
    // Each target aggregate group's targets, one per measure (identical
    // aggregates of several queries are one group with one target each).
    let mut targets_of: HashMap<MemoGroupId, Vec<usize>> = HashMap::new();
    for (t, target) in per_computation.targets.iter().enumerate() {
        let g = group_of
            .group_for_node(&target.aggregate)
            .expect("target belongs to logical memo");
        let targets = targets_of.entry(g).or_default();
        if targets
            .iter()
            .all(|&other| per_computation.targets[other].measure_index != target.measure_index)
        {
            targets.push(t);
        }
    }
    let own = |g: usize| CandidateForMemoGroup {
        implements: vec![MemoGroupId(g as u32)],
        root_node: Rc::clone(&groups[g].logical_node),
        input_groups: groups[g].child_groups.clone(),
        origin: OriginOfCandidateForMemoGroup::Pass1,
        accuracy_budget_parts: 1,
    };
    // Each query's most approximate inputs: its targets that offer a
    // summary, each one sketch state at most.
    let offers_a_summary = |t: usize| {
        let target = &per_computation.targets[t];
        let summary = |c: &HowToComputeAggregateMeasure| {
            matches!(c, HowToComputeAggregateMeasure::Summary(_))
        };
        target.choices.candidates.iter().any(summary)
            || target
                .fused_computation
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
        .map(|g| match targets_of.get(&MemoGroupId(g as u32)) {
            None => vec![own(g)],
            Some(targets) => target_candidates(
                per_computation,
                groups,
                group_of,
                MemoGroupId(g as u32),
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
    group_of: &DagOfLogicalMemoGroups,
    g: MemoGroupId,
    targets: &[usize],
    max_budget_share: usize,
) -> Option<Vec<CandidateForMemoGroup>> {
    let aggregate = Rc::clone(&per_computation.targets[targets[0]].aggregate);
    // The groups a candidate's private nodes may bottom out at, by key.
    let mut below = HashSet::new();
    for &child in &groups[g.0 as usize].child_groups {
        reach(groups, child, &mut below);
    }
    let mut structural_ids = NodeToStructuralIdCache::default();
    let mut descendant: HashMap<u64, MemoGroupId> = HashMap::new();
    let mut ordered: Vec<MemoGroupId> = below.into_iter().collect();
    ordered.sort_unstable();
    for d in ordered {
        descendant
            .entry(structural_ids.structural_id(&groups[d.0 as usize].logical_node))
            .or_insert(d);
    }
    let own_key = structural_ids.structural_id(&groups[g.0 as usize].logical_node);
    let root_groups = super::root_groups(&group_of.query_roots);
    let mut candidates = Vec::new();
    let mut choice = vec![0; per_computation.targets.len()];
    loop {
        let mut implements = vec![g];
        for &t in targets {
            if let Some((_, Some(absorbs))) = per_computation.targets[t].computation_for(choice[t])
            {
                for &a in absorbs {
                    let absorbed = group_of
                        .group_for_node(&per_computation.targets[a].aggregate)
                        .expect("absorbed target belongs to logical memo");
                    if !implements.contains(&absorbed) {
                        implements.push(absorbed);
                    }
                }
            }
        }
        private_absorbed(groups, &root_groups, &mut implements);
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
            let (node, children) = if structural_ids.structural_id(&root) == own_key {
                (
                    Rc::clone(&groups[g.0 as usize].logical_node),
                    groups[g.0 as usize].child_groups.clone(),
                )
            } else {
                let mut children = Vec::new();
                let node = over_groups(
                    &root,
                    groups,
                    &mut structural_ids,
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
                let group_node = &groups[g.0 as usize].logical_node;
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
            candidates.push(CandidateForMemoGroup {
                implements: implements.clone(),
                root_node: node,
                input_groups: children,
                origin: OriginOfCandidateForMemoGroup::Pass1,
                accuracy_budget_parts: budget_share,
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
fn private_absorbed(
    groups: &[MemoGroup],
    root_groups: &BTreeSet<MemoGroupId>,
    implements: &mut Vec<MemoGroupId>,
) {
    loop {
        let shared = implements[1..].iter().position(|a| {
            root_groups.contains(a)
                || groups[a.0 as usize]
                    .parents
                    .iter()
                    .any(|p| !implements.contains(p))
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
    crate::pass1::per_computation_candidates::PerComputationCandidateError,
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
                if let Some(fused) = &mut target.fused_computation {
                    // Absorbed targets not kept are never absorbed here: the
                    // fusion is kept only when chosen, with all of them.
                    match fused
                        .absorbed_targets
                        .iter()
                        .map(|&a| position(a))
                        .collect()
                    {
                        Some(absorbs) => fused.absorbed_targets = absorbs,
                        None => target.fused_computation = None,
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
    structural_ids: &mut NodeToStructuralIdCache,
    descendant: &HashMap<u64, MemoGroupId>,
    children: &mut Vec<MemoGroupId>,
    rebuilt: &mut HashMap<*const OperatorNode, Rc<OperatorNode>>,
) -> Rc<OperatorNode> {
    let mut replaced: HashMap<*const OperatorNode, Rc<OperatorNode>> = HashMap::new();
    for child in node.children() {
        let key = Rc::as_ptr(child);
        if replaced.contains_key(&key) {
            continue;
        }
        let new = match descendant.get(&structural_ids.structural_id(child)) {
            Some(&group) => {
                if !children.contains(&group) {
                    children.push(group);
                }
                Rc::clone(&groups[group.0 as usize].logical_node)
            }
            None => match rebuilt.get(&key) {
                Some(done) => Rc::clone(done),
                None => {
                    let done =
                        over_groups(child, groups, structural_ids, descendant, children, rebuilt);
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

/// Interned structural identities: the operator over its children's identities,
/// without the schema, guarantee, timing and materialization composition
/// derives. Memoized per node (the cache keeps each node alive, so its
/// address is not reused).
#[derive(Default)]
pub struct NodeToStructuralIdCache {
    id_of_node: HashMap<*const OperatorNode, (Rc<OperatorNode>, u64)>,
    id_of_encoded_operator: HashMap<Vec<u8>, u64>,
}

impl NodeToStructuralIdCache {
    pub fn structural_id(&mut self, node: &Rc<OperatorNode>) -> u64 {
        if let Some((_, key)) = self.id_of_node.get(&Rc::as_ptr(node)) {
            return *key;
        }
        let children: HashMap<*const OperatorNode, u64> = node
            .children()
            .into_iter()
            .map(|child| (Rc::as_ptr(child), self.structural_id(child)))
            .collect();
        let operator = node.operator.map_children(|c| children[&Rc::as_ptr(c)]);
        // MessagePack preserves non-finite floats and signed zero. HashMap
        // compares the complete byte key, so a hash collision cannot merge
        // different computations. Cloned/recomposed NaN literals retain identity.
        let encoded =
            rmp_serde::to_vec(&operator).expect("an IR operator over child IDs must serialize");
        let next = self.id_of_encoded_operator.len() as u64;
        let key = *self.id_of_encoded_operator.entry(encoded).or_insert(next);
        self.id_of_node
            .insert(Rc::as_ptr(node), (Rc::clone(node), key));
        key
    }
}

impl CandidateMemo {
    /// The groups, each after every group it or one of its candidates
    /// reads, the lower index first among those ready: a dependency order for
    /// candidate traversal, whatever order transformations appended
    /// groups in. `None` when those reads form a cycle.
    pub fn topological_order(&self) -> Option<Vec<MemoGroupId>> {
        let count = self.groups.len();
        let mut reads: Vec<HashSet<usize>> = vec![HashSet::new(); count];
        for (g, group) in self.groups.iter().enumerate() {
            let candidates = self.candidates.get(g).map_or(&[][..], Vec::as_slice);
            for child in group
                .child_groups
                .iter()
                .chain(candidates.iter().flat_map(|c| &c.input_groups))
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
            order.push(MemoGroupId(g as u32));
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

    #[test]
    fn structural_keys_preserve_nonfinite_and_signed_zero_literals() {
        let input = crate::test_support::lower_promql(
            "m",
            asap_types::ir::properties::AccuracyTarget::exact(),
        );
        let literal = |value| {
            OperatorNode::new_shared(asap_types::ir::Operator::NonASAP(
                asap_types::ir::NonASAPOp::Project {
                    cols: vec![asap_types::ir::ProjectItem {
                        alias: Some("value".into()),
                        expr: asap_types::ir::ScalarExpr::literal_f64(value),
                    }],
                    qualifier: None,
                    child: Rc::clone(&input),
                },
            ))
            .unwrap()
        };
        let mut structural_ids = NodeToStructuralIdCache::default();
        for (left, right) in [
            (f64::NAN, f64::INFINITY),
            (f64::INFINITY, f64::NEG_INFINITY),
            (0.0, -0.0),
        ] {
            assert_ne!(
                structural_ids.structural_id(&literal(left)),
                structural_ids.structural_id(&literal(right))
            );
        }
        assert_eq!(
            structural_ids.structural_id(&literal(1.0)),
            structural_ids.structural_id(&literal(1.0))
        );
        let node = literal(f64::NAN);
        assert_eq!(
            structural_ids.structural_id(&node),
            structural_ids.structural_id(&literal(f64::NAN))
        );
    }

    fn populate_pass1<Id: Clone>(
        initial: DagOfLogicalMemoGroups,
        inventory: &PerComputationCandidates<Id>,
    ) -> CandidateMemo {
        try_populate_pass1(initial, inventory).unwrap()
    }

    fn memo_of<Id: Clone>(inventory: &PerComputationCandidates<Id>) -> CandidateMemo {
        populate_pass1(
            DagOfLogicalMemoGroups::from_roots(&inventory.roots),
            inventory,
        )
    }
    use crate::pass1::per_computation_candidates::{
        compose_logical_candidate, generate_per_computation_candidates,
    };
    use crate::test_support::lower_promql;
    use asap_types::ir::properties::AccuracyTarget;

    use asap_types::ir::ASAPOp;

    /// The cross-stage API rejects mismatched root identities without panicking.
    #[test]
    fn population_rejects_an_inventory_from_other_roots() {
        let roots = vec![(
            0,
            QueryRoot::Operator(lower_promql(
                "quantile_over_time(0.95, m[5m])",
                AccuracyTarget::max_error(0.01),
            )),
        )];
        let initial = DagOfLogicalMemoGroups::from_roots(&roots);
        let other_roots = vec![(
            0,
            QueryRoot::Operator(lower_promql(
                "quantile_over_time(0.95, m[5m])",
                AccuracyTarget::max_error(0.01),
            )),
        )];
        let inventory = generate_per_computation_candidates(other_roots).unwrap();
        assert!(matches!(
            try_populate_pass1(initial, &inventory),
            Err(CandidatePopulationError::RootDependenciesChanged)
        ));
    }

    /// Invalid target and fusion indices fail at the stage boundary, not in composition.
    #[test]
    fn population_checks_target_and_absorption_indices() {
        let roots = vec![(
            0,
            QueryRoot::Operator(lower_promql(
                "quantile_over_time(0.95, m[5m])",
                AccuracyTarget::max_error(0.01),
            )),
        )];
        let initial = DagOfLogicalMemoGroups::from_roots(&roots);
        let inventory = generate_per_computation_candidates(roots).unwrap();
        let mut invalid_measure = inventory.clone();
        invalid_measure.targets[0].measure_index = usize::MAX;
        assert!(matches!(
            try_populate_pass1(initial.clone(), &invalid_measure),
            Err(CandidatePopulationError::InvalidTarget { target: 0 })
        ));
        let mut invalid_absorption = inventory;
        let missing = invalid_absorption.targets.len();
        invalid_absorption.targets[0].fused_computation =
            Some(crate::pass1::per_computation_candidates::FusedComputation {
                absorbed_targets: vec![missing],
                candidates: Vec::new(),
                pruned: Vec::new(),
            });
        assert!(matches!(try_populate_pass1(initial, &invalid_absorption),
            Err(CandidatePopulationError::InvalidAbsorption { target: 0, absorbed }) if absorbed == missing));
    }

    /// Capability pruning must change implementations, not logical identities.
    #[test]
    fn initial_groups_survive_pass1_expansion_and_pruning() {
        let roots: Vec<_> = [
            "quantile_over_time(0.95, m[5m])",
            "quantile_over_time(0.99, m[5m])",
        ]
        .iter()
        .enumerate()
        .map(|(query, text)| {
            (
                query,
                QueryRoot::Operator(lower_promql(text, AccuracyTarget::max_error(0.01))),
            )
        })
        .collect();
        let initial = DagOfLogicalMemoGroups::from_roots(&roots);
        let original_groups = initial.groups.clone();
        let original_roots = initial.query_roots.clone();
        let inventory = generate_per_computation_candidates(roots).unwrap();
        let expanded = populate_pass1(initial.clone(), &inventory);
        let restricted_inventory = inventory.without_unsupported_summaries(Some(&[]));
        let restricted = populate_pass1(initial, &restricted_inventory);
        for memo in [&expanded, &restricted] {
            assert_eq!(memo.groups.len(), original_groups.len());
            for (before, after) in original_groups.iter().zip(&memo.groups) {
                assert!(Rc::ptr_eq(&before.logical_node, &after.logical_node));
                assert_eq!(before.child_groups, after.child_groups);
                assert_eq!(before.parents, after.parents);
                assert_eq!(before.queries, after.queries);
            }
            assert_eq!(memo.query_roots, original_roots);
            assert_ne!(
                group_of_kind(memo, 0, "Aggregate"),
                group_of_kind(memo, 1, "Aggregate")
            );
            assert!(memo.candidates.iter().all(|c| !c.is_empty()));
        }
        assert!(
            expanded.candidates.iter().map(Vec::len).sum::<usize>()
                > restricted.candidates.iter().map(Vec::len).sum::<usize>()
        );
    }

    /// The memo of PromQL `queries`, each with accuracy target `target`.
    fn memo(queries: &[&str], target: AccuracyTarget) -> CandidateMemo {
        let roots = queries
            .iter()
            .map(|q| QueryRoot::Operator(lower_promql(q, target.clone())))
            .enumerate()
            .collect();
        memo_of(&generate_per_computation_candidates(roots).unwrap())
    }

    /// The group of the first node of `kind` reached from query `q`'s root,
    /// walking down the groups' first children.
    fn group_of_kind(memo: &CandidateMemo, q: usize, kind: &str) -> usize {
        let mut g = memo.query_roots[q].as_operator().unwrap().0 as usize;
        while memo.groups[g].logical_node.operator.kind_name() != kind {
            g = memo.groups[g].child_groups[0].0 as usize;
        }
        g
    }

    /// The summary algorithms a candidate's private nodes build.
    fn algorithms(candidate: &CandidateForMemoGroup) -> Vec<String> {
        OperatorNode::reachable(&candidate.root_node)
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
        assert_eq!(
            super::super::queries_rooted_at(&memo.query_roots, MemoGroupId(sum as u32)),
            vec![0]
        );
        let product = memo.query_roots[1].as_operator().unwrap().0 as usize;
        assert_eq!(group.parents, vec![MemoGroupId(product as u32)]);
        assert_eq!(
            memo.groups[product].child_groups,
            vec![MemoGroupId(sum as u32)]
        );
        // Children come before parents.
        for (g, group) in memo.groups.iter().enumerate() {
            assert!(group.child_groups.iter().all(|c| (c.0 as usize) < g));
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
        let input = memo.groups[quantile].child_groups.clone();
        let candidates = &memo.candidates[quantile];
        assert_eq!(candidates.len(), 3);
        assert!(Rc::ptr_eq(
            &candidates[0].root_node,
            &memo.groups[quantile].logical_node
        ));
        assert!(algorithms(&candidates[0]).is_empty());
        assert!(algorithms(&candidates[1])[0].contains("Kll"));
        assert!(algorithms(&candidates[2])[0].contains("DDSketch"));
        for candidate in candidates {
            assert_eq!(candidate.implements, vec![MemoGroupId(quantile as u32)]);
            assert_eq!(candidate.input_groups, input);
            assert_eq!(candidate.origin, OriginOfCandidateForMemoGroup::Pass1);
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
        let sums = memo.groups[topk].child_groups[0];
        let sums_input = memo.groups[sums.0 as usize].child_groups.clone();
        let candidates = &memo.candidates[topk];
        assert_eq!(candidates[0].input_groups, vec![sums]);
        let fused: Vec<_> = candidates
            .iter()
            .filter(|c| c.implements.len() > 1)
            .collect();
        assert!(!fused.is_empty());
        for candidate in fused {
            assert_eq!(candidate.implements, vec![MemoGroupId(topk as u32), sums]);
            assert_eq!(candidate.input_groups, sums_input);
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
                        OperatorNode::reachable(&candidate.root_node)
                            .iter()
                            .map(Rc::as_ptr)
                            .collect();
                    for child in &candidate.input_groups {
                        let node = &memo.groups[child.0 as usize].logical_node;
                        assert!(reached.contains(&Rc::as_ptr(node)), "{queries:?} g{g}");
                    }
                    assert_eq!(
                        candidate.root_node.schema,
                        memo.groups[g].logical_node.schema
                    );
                }
            }
        }
    }

    /// The sketch summaries of the first query of `queries` composed with
    /// every target at its `algorithm` summary, by [`crate::pass1::per_computation_candidates::compose_logical_candidate`]:
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
                        matches!(c, HowToComputeAggregateMeasure::Summary(a) if format!("{a:?}") == algorithm)
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
    fn sketch_shares(memo: &CandidateMemo, g: usize) -> Vec<(String, usize)> {
        memo.candidates[g]
            .iter()
            .filter(|c| !algorithms(c).is_empty())
            .map(|c| (algorithms(c)[0].clone(), c.accuracy_budget_parts))
            .collect()
    }

    /// `q0.9 / q0.5` beside a plain `q0.9`: each of the ratio's quantile
    /// groups offers every sketch for the whole budget and for half of it,
    /// since its query has two inputs that may be approximate. The plain
    /// query's quantile is the numerator's group: equal per-series quantiles
    /// return the same rows, so they are one group with both readers. A ratio of quantiles carries no
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
        let ratio = memo.query_roots[0].as_operator().unwrap().0 as usize;
        let operand = memo.groups[ratio].child_groups[0].0 as usize;
        for operand in &memo.groups[ratio].child_groups {
            let shares: Vec<usize> = sketch_shares(&memo, operand.0 as usize)
                .into_iter()
                .map(|(_, share)| share)
                .collect();
            assert_eq!(shares, vec![1, 2, 1, 2]);
        }
        let plain = memo.query_roots[1].as_operator().unwrap().0 as usize;
        assert_eq!(plain, operand);
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
        let difference = memo.query_roots[0].as_operator().unwrap().0 as usize;
        let operand = memo.groups[difference].child_groups[0].0 as usize;
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
                .map(MemoGroupId)
                .collect::<Vec<_>>()
        );
        let quantile = group_of_kind(&memo, 0, "Aggregate");
        let appended = MemoGroupId(memo.groups.len() as u32);
        memo.groups.push(memo.groups[1].clone());
        memo.candidates.push(memo.candidates[1].clone());
        let mut reader = memo.candidates[quantile][0].clone();
        reader.input_groups = vec![appended];
        memo.candidates[quantile].push(reader);
        let order = memo.topological_order().unwrap();
        let position = |g: MemoGroupId| order.iter().position(|&x| x == g).unwrap();
        assert!(position(appended) < position(MemoGroupId(quantile as u32)));
        // A cycle has no order.
        memo.candidates[1][0]
            .input_groups
            .push(MemoGroupId(quantile as u32));
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
        memo: &'a CandidateMemo,
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
                .any(|c| self.private(&c.root_node, composed, &c.input_groups));
            self.matched.insert(key, found);
            found
        }

        fn private(
            &mut self,
            node: &Rc<OperatorNode>,
            composed: &Rc<OperatorNode>,
            children: &[MemoGroupId],
        ) -> bool {
            let memo = self.memo;
            if let Some(child) = children
                .iter()
                .find(|c| Rc::ptr_eq(&memo.groups[c.0 as usize].logical_node, node))
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
                        .position(|group| *group.logical_node == *inventory.targets[t].aggregate)
                        .unwrap()
                };
                for (t, target) in inventory.targets.iter().enumerate() {
                    let g = group_of(t);
                    if inventory.targets.iter().enumerate().any(|(u, other)| {
                        u != t && group_of(u) == g && other.measure_index != target.measure_index
                    }) {
                        continue;
                    }
                    let whole_budget = |fused: bool| {
                        memo.candidates[g]
                            .iter()
                            .filter(|c| {
                                c.accuracy_budget_parts == 1 && (c.implements.len() > 1) == fused
                            })
                            .count()
                    };
                    let fused = target
                        .fused_computation
                        .as_ref()
                        .map_or(0, |f| f.candidates.len());
                    if whole_budget(false) != target.choices.candidates.len()
                        || whole_budget(true) != fused
                    {
                        missing.push(format!("{gauges:?} seed {seed} target {t} candidates"));
                    }
                }
                for (g, group) in memo.groups.iter().enumerate() {
                    for candidate in &memo.candidates[g] {
                        if !group.parents.is_empty()
                            && candidate.root_node.schema != group.logical_node.schema
                        {
                            other_schema.push(format!("{gauges:?} seed {seed} g{g}"));
                        }
                    }
                }
                let root_group = |q: usize| memo.query_roots[q].as_operator().unwrap().0 as usize;
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
        use crate::test_support::{agg, scan_from};
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
        assert_eq!(memo.groups[0].logical_node.operator.kind_name(), "Scan");
        assert_eq!(memo.groups[0].queries, vec![0, 1]);
        assert_eq!(memo.groups[0].parents, vec![MemoGroupId(1), MemoGroupId(2)]);
    }
}

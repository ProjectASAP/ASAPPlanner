//! Stage 1 (logical ASAP-aware optimization), Pass 2: ASAP-aware
//! common-subexpression elimination, as defined in
//! `docs/design_docs/proposals/planner-layering.md` (Algorithm 2).
//!
//! Its three rules are implemented: the identical-expression rule, the
//! summary-capability rule and, for tumbling panes of one query, the
//! window-composition rule.
//!
//! # Identical-expression rule
//!
//! A *sharing group* of a
//! whole-workload candidate is a set of two or more of its nodes with
//! identical input and computation semantics that are legal to share: the
//! `ir::cse` interning merges only ASAP nodes and nodes with a unique key.
//! [`identical_expression_variants`] returns one candidate per non-empty
//! subset of the groups (Algorithm 2, line 31): each group in the subset
//! becomes one node serving every consumer, and every other group stays
//! separate. The caller keeps the independent candidate beside them, so plan
//! selection compares all of them; nothing is capped.
//!
//! Sharing a node shares the sub-DAG below it. A subset that leaves out a
//! group whose nodes all lie below the nodes of a group it shares gives the
//! same candidate as the subset with that group; such a candidate is
//! returned once, under the subset of the groups it actually shares.
//!
//! # Summary-capability rule
//!
//! Composition sizes each consumer's summary build alone, for its own
//! accuracy target. The *members* of a [`CapabilityGroup`] are summary
//! builds of one candidate that chose the same family (same
//! [`SketchAlgorithm`]) over the same input sub-DAG (window, `WHERE` and
//! time range included), item and weight, grouping and filter, and whose
//! every reader is an estimate the family answers
//! ([`FamilyCapability::answers`]); they differ only in their sizes.
//! [`summary_capability_variants`] returns one candidate per choice, for
//! each group, of no sharing or one subset of two or more of its members
//! whose sizes differ (Q1: partial sharing too): the subset becomes one
//! build serving all its readers, sized with each parameter's largest value
//! among them ([`shared_size_params`]: each consumer sized alone, the shared
//! build the per-parameter maximum, a KLL's failure probability the
//! minimum), and every guarantee above it is composed again, so plan
//! selection checks each consumer against the shared state. Members of
//! equal size are the identical-expression rule's; builds that are not
//! shared are untouched, and a consumer never changes family.
//!
//! # Window-composition rule
//!
//! A query repeating every `E` over a window `W` can build its summary as
//! back-to-back tumbling panes of width `w = gcd(W, E)` (Algorithm 2's
//! `Tumbling(L)` for one query), merged before each read. A *pane site*
//! ([`pane_sites`]) is a `SummaryAgg` over a range `TimeRange(W)` whose
//! family merges in the family-capability table ([`FamilyCapability`]):
//! exact accumulators included, and also families whose merged readouts
//! are only approximate (heap top-k), whose readers then carry no
//! guarantee and are left to plan selection's evidence. A family without a
//! merge (IRate) has no site. [`window_composition_variants`] returns one
//! candidate per non-empty subset of the sites: each site in the subset
//! becomes a `SummaryMerge` over one [`ASAPOp::PaneChain`] of `W / w` panes,
//! a compact node whatever the pane count, with the build's family and
//! size (a mergeable family's merged state has one build's guarantee), and
//! every guarantee above is composed again. Nothing is materialized here:
//! the panes are rebuilt at each evaluation until Stage 2 keeps them. A pane
//! width shared by several queries, sliding windows and Exponential
//! Histograms are not implemented.

use std::collections::HashMap;
#[cfg(test)]
use std::collections::HashSet;
use std::rc::Rc;

use asap_types::ir::cse::share_common_sub_dags;
use asap_types::ir::{
    ASAPOp, NonASAPOp, Operator, OperatorNode, OperatorResultKind, QueryRoot, TimeRangeKind,
};
use asap_types::post_asap::{
    FamilyCapability, FieldDataType, GroupingStrategy, SketchAlgorithm, SketchKind,
};

use crate::accuracy::estimators::shared_size_params;
use crate::per_computation_candidates::{with_composed_guarantees, PerComputationCandidateError};

/// A Pass 2 candidate: the candidate's roots with the sharing groups
/// `shared_groups` (indices into [`sharing_groups`]) shared.
#[derive(Debug, Clone)]
pub struct IdenticalExpressionVariant {
    pub shared_groups: Vec<usize>,
    pub roots: Vec<QueryRoot>,
}

/// One sharing group: the identical nodes, in the order a children-first
/// walk of the roots meets them.
#[derive(Debug, Clone)]
pub struct SharingGroup {
    pub nodes: Vec<Rc<OperatorNode>>,
}

/// The sharing groups of the candidate with query roots `roots`, children
/// before parents.
pub fn sharing_groups(roots: &[QueryRoot]) -> Vec<SharingGroup> {
    let operators: Vec<Rc<OperatorNode>> = roots.iter().flat_map(root_operators).cloned().collect();
    let interned = share_common_sub_dags(operators.iter().cloned().enumerate().collect());
    // Each node's interned representative: interning rebuilds every node
    // over its children in order, so the two DAGs are walked side by side.
    let mut representative: HashMap<*const OperatorNode, *const OperatorNode> = HashMap::new();
    let mut order = Vec::new();
    for (original, (_, merged)) in operators.iter().zip(&interned) {
        pair(original, merged, &mut representative, &mut order);
    }
    let mut members: HashMap<*const OperatorNode, Vec<Rc<OperatorNode>>> = HashMap::new();
    let mut first_seen = Vec::new();
    for node in order {
        let key = representative[&Rc::as_ptr(&node)];
        let entry = members.entry(key).or_default();
        if entry.is_empty() {
            first_seen.push(key);
        }
        entry.push(node);
    }
    first_seen
        .into_iter()
        .filter_map(|key| {
            let nodes = members.remove(&key)?;
            (nodes.len() >= 2).then_some(SharingGroup { nodes })
        })
        .collect()
}

/// Record `original`'s representative `merged` and recurse into the
/// children, then list `original` (children first, each node once).
fn pair(
    original: &Rc<OperatorNode>,
    merged: &Rc<OperatorNode>,
    representative: &mut HashMap<*const OperatorNode, *const OperatorNode>,
    order: &mut Vec<Rc<OperatorNode>>,
) {
    if representative.contains_key(&Rc::as_ptr(original)) {
        return;
    }
    representative.insert(Rc::as_ptr(original), Rc::as_ptr(merged));
    for (child, merged_child) in original.children().into_iter().zip(merged.children()) {
        pair(child, merged_child, representative, order);
    }
    order.push(Rc::clone(original));
}

/// Every Pass 2 candidate of the identical-expression rule for the
/// candidate with query roots `roots` and sharing groups `groups`
/// ([`sharing_groups`]): one per non-empty subset of the groups that gives a
/// distinct candidate, in binary-counter order of the subsets (the first
/// group the lowest bit). Empty when nothing is identical.
///
/// Sharing a set of groups also shares every group whose remaining nodes it
/// leaves as one; that closure maps each subset to the candidate it gives,
/// and the distinct candidates are the closed sets. They are enumerated
/// directly (Ganter's Next Closure, one closure per step and group) rather
/// than by trying all `2^g` subsets, which repeat when groups nest.
pub fn identical_expression_variants(
    roots: &[QueryRoot],
    groups: &[SharingGroup],
) -> Vec<IdenticalExpressionVariant> {
    let n = groups.len();
    let mut variants = Vec::new();
    // The closed sets in lectic order, from the closure of the empty set
    // (which shares nothing).
    let mut closed = vec![false; n];
    'next: loop {
        for i in (0..n).rev() {
            if closed[i] {
                continue;
            }
            let mut candidate: Vec<bool> = (0..n).map(|g| g < i && closed[g]).collect();
            candidate[i] = true;
            let (shared_roots, merged) = apply(roots, groups, &candidate);
            // Lectic successor: adds nothing below `i` that `closed` lacks.
            if (0..i).all(|g| !merged[g] || closed[g]) {
                // `merged` is closed: applying it gives the same candidate.
                variants.push((merged.clone(), shared_roots));
                closed = merged;
                continue 'next;
            }
        }
        break;
    }
    // Binary-counter order of the subsets: compare from the highest group.
    variants.sort_by(|(a, _), (b, _)| a.iter().rev().cmp(b.iter().rev()));
    variants
        .into_iter()
        .map(|(subset, roots)| IdenticalExpressionVariant {
            shared_groups: (0..n).filter(|&g| subset[g]).collect(),
            roots,
        })
        .collect()
}

/// `roots` with the groups in `subset` shared, and the groups that leaves
/// shared: those whose every node still read became one node (a node below
/// a shared node other than the one kept is no longer read).
fn apply(
    roots: &[QueryRoot],
    groups: &[SharingGroup],
    subset: &[bool],
) -> (Vec<QueryRoot>, Vec<bool>) {
    let group_of: HashMap<*const OperatorNode, usize> = groups
        .iter()
        .enumerate()
        .flat_map(|(index, group)| group.nodes.iter().map(move |n| (Rc::as_ptr(n), index)))
        .collect();
    let mut sharing = Sharing {
        subset,
        group_of: &group_of,
        rebuilt: HashMap::new(),
        shared: HashMap::new(),
    };
    let shared_roots: Vec<QueryRoot> = roots.iter().map(|r| sharing.root(r)).collect();
    let merged = groups
        .iter()
        .map(|group| {
            let mut nodes = group.nodes.iter().filter_map(|node| {
                let (_, rebuilt) = sharing.rebuilt.get(&Rc::as_ptr(node))?;
                Some(Rc::as_ptr(rebuilt))
            });
            let first = nodes.next();
            nodes.all(|node| Some(node) == first)
        })
        .collect();
    (shared_roots, merged)
}

/// Binary increment of `subset`, the first entry the lowest bit; `false`
/// once every subset has been visited.
#[cfg(test)]
fn next_subset(subset: &mut [bool]) -> bool {
    for bit in subset.iter_mut() {
        *bit = !*bit;
        if *bit {
            return true;
        }
    }
    false
}

/// One application of the rule: the groups in `subset` shared.
struct Sharing<'a> {
    subset: &'a [bool],
    group_of: &'a HashMap<*const OperatorNode, usize>,
    /// Original node → (original, rebuilt). The original is kept alive so
    /// its address cannot be reused while it is a key.
    rebuilt: HashMap<*const OperatorNode, (Rc<OperatorNode>, Rc<OperatorNode>)>,
    /// Group → the one node every member of a shared group becomes.
    shared: HashMap<usize, Rc<OperatorNode>>,
}

impl Sharing<'_> {
    fn root(&mut self, root: &QueryRoot) -> QueryRoot {
        match root {
            QueryRoot::Operator(node) => QueryRoot::Operator(self.node(node)),
            QueryRoot::Scalar(expr) => {
                QueryRoot::Scalar(expr.map_operator_refs(&mut |node| self.node(node)))
            }
        }
    }

    fn node(&mut self, node: &Rc<OperatorNode>) -> Rc<OperatorNode> {
        if let Some((_, rebuilt)) = self.rebuilt.get(&Rc::as_ptr(node)) {
            return Rc::clone(rebuilt);
        }
        let group = self
            .group_of
            .get(&Rc::as_ptr(node))
            .copied()
            .filter(|&g| self.subset[g]);
        let rebuilt = match group.and_then(|g| self.shared.get(&g)) {
            Some(shared) => Rc::clone(shared),
            None => {
                let rebuilt = self.over_rebuilt_children(node);
                if let Some(g) = group {
                    self.shared.insert(g, Rc::clone(&rebuilt));
                }
                rebuilt
            }
        };
        self.rebuilt
            .insert(Rc::as_ptr(node), (Rc::clone(node), Rc::clone(&rebuilt)));
        rebuilt
    }

    /// `node` over its rebuilt children; the same `Rc` when none changed.
    /// Every child is replaced by an identical node, so `node`'s schema,
    /// guarantee, timing and materialization stay valid.
    fn over_rebuilt_children(&mut self, node: &Rc<OperatorNode>) -> Rc<OperatorNode> {
        let mut changed = false;
        let operator = node.operator.map_children(|child| {
            let rebuilt = self.node(child);
            changed |= !Rc::ptr_eq(&rebuilt, child);
            rebuilt
        });
        if !changed {
            return Rc::clone(node);
        }
        Rc::new(
            OperatorNode::with_schema(operator, node.schema.clone())
                .with_guarantee(node.guarantee.clone())
                .with_timing(node.timing)
                .with_materialization(node.materialization),
        )
    }
}

/// One group of the summary-capability rule: summary builds of one family
/// over the same input data that differ only in size, in the order a
/// children-first walk of the roots meets them.
#[derive(Debug, Clone)]
pub struct CapabilityGroup {
    pub family: SketchAlgorithm,
    pub members: Vec<Rc<OperatorNode>>,
}

/// One shared build of a summary-capability candidate: the members
/// (indices into the group's [`CapabilityGroup::members`]) of group
/// `group` that read one state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SharedSummary {
    pub group: usize,
    pub members: Vec<usize>,
}

/// A Pass 2 candidate of the summary-capability rule: the candidate's
/// roots with the builds in `shared` made one build each, sized for all
/// their consumers, and every guarantee composed again; the error when
/// that composition fails, which leaves the other variants unaffected.
#[derive(Debug)]
pub struct SummaryCapabilityVariant {
    pub shared: Vec<SharedSummary>,
    pub roots: Result<Vec<QueryRoot>, PerComputationCandidateError>,
}

/// The summary-capability groups of the candidate with query roots
/// `roots`: groups of at least two builds whose sizes are not all equal.
pub fn capability_groups(roots: &[QueryRoot]) -> Vec<CapabilityGroup> {
    let mut nodes = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for node in roots.iter().flat_map(root_operators) {
        collect_children_first(node, &mut seen, &mut nodes);
    }
    // Every node that reads each build, by the build's address.
    let mut readers: HashMap<*const OperatorNode, Vec<&Rc<OperatorNode>>> = HashMap::new();
    for node in &nodes {
        for child in node.children() {
            readers.entry(Rc::as_ptr(child)).or_default().push(node);
        }
    }
    let mut groups: Vec<CapabilityGroup> = Vec::new();
    for node in &nodes {
        let Some(family) = shareable_family(node, readers.get(&Rc::as_ptr(node))) else {
            continue;
        };
        match groups
            .iter_mut()
            .find(|g| g.family == family && same_input_data(&g.members[0], node))
        {
            Some(group) => group.members.push(Rc::clone(node)),
            None => groups.push(CapabilityGroup {
                family,
                members: vec![Rc::clone(node)],
            }),
        }
    }
    groups.retain(|g| !all_equal(g.members.iter()));
    groups
}

fn collect_children_first(
    node: &Rc<OperatorNode>,
    seen: &mut std::collections::HashSet<*const OperatorNode>,
    out: &mut Vec<Rc<OperatorNode>>,
) {
    if !seen.insert(Rc::as_ptr(node)) {
        return;
    }
    for child in node.children() {
        collect_children_first(child, seen, out);
    }
    out.push(Rc::clone(node));
}

/// The family of a per-subpopulation sketch build whose every reader is an
/// estimate the family answers; `None` for any other node.
fn shareable_family(
    node: &OperatorNode,
    readers: Option<&Vec<&Rc<OperatorNode>>>,
) -> Option<SketchAlgorithm> {
    let Some(ASAPOp::SummaryAgg {
        family: family @ FieldDataType::Sketch(kind, GroupingStrategy::PerSubpopulationInstance),
        ..
    }) = node.asap()
    else {
        return None;
    };
    let capability = FamilyCapability::of(family);
    let answered = |reader: &&Rc<OperatorNode>| {
        matches!(
            reader.asap(),
            Some(ASAPOp::SummaryEstimate { query, .. }) if capability.answers(query)
        )
    };
    let readers = readers?;
    readers
        .iter()
        .all(answered)
        .then(|| kind.algorithm().clone())
}

/// Whether two builds of one family read the same data the same way: the
/// same input sub-DAG, item and weight, grouping and filter.
fn same_input_data(a: &OperatorNode, b: &OperatorNode) -> bool {
    match (a.asap(), b.asap()) {
        (
            Some(ASAPOp::SummaryAgg {
                child,
                input,
                reduction,
                grouping,
                filter,
                ..
            }),
            Some(ASAPOp::SummaryAgg {
                child: child_b,
                input: input_b,
                reduction: reduction_b,
                grouping: grouping_b,
                filter: filter_b,
                ..
            }),
        ) => {
            (Rc::ptr_eq(child, child_b) || child == child_b)
                && input == input_b
                && reduction == reduction_b
                && grouping == grouping_b
                && filter == filter_b
        }
        _ => false,
    }
}

fn all_equal<'a>(mut nodes: impl Iterator<Item = &'a Rc<OperatorNode>>) -> bool {
    let Some(first) = nodes.next() else {
        return true;
    };
    nodes.all(|node| Rc::ptr_eq(node, first) || **node == **first)
}

/// The most summary builds one summary-capability group may have: every
/// subset of two or more is a candidate, about a million at 20.
pub const MAX_GROUP_MEMBERS: usize = 20;

/// Every Pass 2 candidate of the summary-capability rule for the candidate
/// with query roots `roots` and groups `groups` ([`capability_groups`]):
/// one per choice, for each group, of no sharing or one subset of two or
/// more members whose sizes are not all equal, other than sharing nothing.
/// An error only for a group of more than [`MAX_GROUP_MEMBERS`] members; a variant whose
/// composition fails carries its own error.
/// The first group varies slowest; within a group, subsets come in
/// binary-counter order (the first member the lowest bit).
pub fn summary_capability_variants(
    roots: &[QueryRoot],
    groups: &[CapabilityGroup],
) -> Result<Vec<SummaryCapabilityVariant>, PerComputationCandidateError> {
    // Every subset of a group is a candidate: 2^n of them. Past
    // MAX_GROUP_MEMBERS that is no longer a list plan selection can price.
    if groups
        .iter()
        .any(|group| group.members.len() > MAX_GROUP_MEMBERS)
    {
        return Err(PerComputationCandidateError::Unsupported(
            "a summary-capability group of more than 20 summary builds (2^n sharing subsets)",
        ));
    }
    // Each group's options: `None` (no sharing), then its subsets.
    let options: Vec<Vec<Option<Vec<usize>>>> = groups
        .iter()
        .map(|group| {
            let n = group.members.len();
            let mut options = vec![None];
            for mask in 1u64..(1u64 << n) {
                let members: Vec<usize> = (0..n).filter(|&m| mask >> m & 1 == 1).collect();
                if members.len() >= 2 && !all_equal(members.iter().map(|&m| &group.members[m])) {
                    options.push(Some(members));
                }
            }
            options
        })
        .collect();
    let mut choice = vec![0usize; groups.len()];
    let mut variants = Vec::new();
    loop {
        // Mixed-radix increment, the last group varying fastest.
        let mut carried = true;
        for (digit, group_options) in choice.iter_mut().zip(&options).rev() {
            *digit += 1;
            if *digit < group_options.len() {
                carried = false;
                break;
            }
            *digit = 0;
        }
        if carried {
            break;
        }
        let shared: Vec<SharedSummary> = choice
            .iter()
            .enumerate()
            .filter_map(|(group, &option)| {
                options[group][option]
                    .clone()
                    .map(|members| SharedSummary { group, members })
            })
            .collect();
        match share_summaries(roots, groups, &shared) {
            Ok(None) => {}
            Ok(Some(roots)) => variants.push(SummaryCapabilityVariant {
                shared,
                roots: Ok(roots),
            }),
            Err(error) => variants.push(SummaryCapabilityVariant {
                shared,
                roots: Err(error),
            }),
        }
    }
    Ok(variants)
}

/// `roots` with each of `shared` made one build sized for all its members,
/// and every guarantee composed again; `None` when the members' sizes have
/// no shared size.
fn share_summaries(
    roots: &[QueryRoot],
    groups: &[CapabilityGroup],
    shared: &[SharedSummary],
) -> Result<Option<Vec<QueryRoot>>, PerComputationCandidateError> {
    let mut replaced: HashMap<*const OperatorNode, Rc<OperatorNode>> = HashMap::new();
    for summary in shared {
        let members: Vec<&Rc<OperatorNode>> = summary
            .members
            .iter()
            .map(|&m| &groups[summary.group].members[m])
            .collect();
        let Some(build) = sized_for_all(&members)? else {
            return Ok(None);
        };
        for member in members {
            replaced.insert(Rc::as_ptr(member), Rc::clone(&build));
        }
    }
    let mut rebuilt: HashMap<*const OperatorNode, Rc<OperatorNode>> = HashMap::new();
    let mut rebuild = |node: &Rc<OperatorNode>| rebuild(node, &replaced, &mut rebuilt);
    let mut out = Vec::new();
    for root in roots {
        out.push(match root {
            QueryRoot::Operator(node) => QueryRoot::Operator(rebuild(node)?),
            QueryRoot::Scalar(expr) => {
                let mut nodes = HashMap::new();
                for node in expr.operator_refs() {
                    nodes.insert(Rc::as_ptr(node), rebuild(node)?);
                }
                QueryRoot::Scalar(
                    expr.map_operator_refs(&mut |node| nodes[&Rc::as_ptr(node)].clone()),
                )
            }
        });
    }
    Ok(Some(with_composed_guarantees(&out)?))
}

/// One build serving every member: the first member with the members'
/// shared size ([`shared_size_params`]).
fn sized_for_all(
    members: &[&Rc<OperatorNode>],
) -> Result<Option<Rc<OperatorNode>>, PerComputationCandidateError> {
    let kinds: Vec<&SketchKind> = members
        .iter()
        .filter_map(|member| match member.asap() {
            Some(ASAPOp::SummaryAgg {
                family: FieldDataType::Sketch(kind, _),
                ..
            }) => Some(kind),
            _ => None,
        })
        .collect();
    let params: Vec<_> = kinds.iter().map(|kind| kind.params()).collect();
    let Some(params) = shared_size_params(&params) else {
        return Ok(None);
    };
    let first = members[0];
    let Some(ASAPOp::SummaryAgg {
        child,
        family: FieldDataType::Sketch(kind, grouping_strategy),
        input,
        reduction,
        grouping,
        filter,
    }) = first.asap()
    else {
        return Ok(None);
    };
    let operator = Operator::ASAP(ASAPOp::SummaryAgg {
        child: Rc::clone(child),
        family: FieldDataType::Sketch(
            SketchKind::new(kind.algorithm().clone(), params),
            grouping_strategy.clone(),
        ),
        input: input.clone(),
        reduction: reduction.clone(),
        grouping: grouping.clone(),
        filter: filter.clone(),
    });
    let mut build = OperatorNode::new(operator)?;
    // Keep the member's explicit output names.
    for (field, retained) in build.schema.fields.iter_mut().zip(&first.schema.fields) {
        field.name = retained.name.clone();
        field.table = retained.table.clone();
    }
    Ok(Some(Rc::new(build)))
}

/// One summary build the window-composition rule can build as tumbling
/// panes: a `SummaryAgg` over a range `TimeRange(W)` whose family merges
/// (the family-capability table: exact accumulators included, and families
/// whose merged readouts are only approximate), read only by queries that
/// repeat at a fixed interval. Its panes are `pane_count = W / w` back-to-back
/// windows of `w = gcd(W, E)` over the queries' intervals `E`, at least two.
#[derive(Debug, Clone)]
pub struct PaneSite {
    pub build: Rc<OperatorNode>,
    /// The queries (indices into the roots) that read the build.
    pub readers: Vec<usize>,
    pub pane_width_ms: u64,
    pub pane_count: u32,
}

/// A Pass 2 candidate of the window-composition rule: the candidate's roots
/// with each build in `sites` (indices into [`pane_sites`]) built as its
/// tumbling panes, merged before every read, and every guarantee composed
/// again; the error when that fails, which leaves the other variants
/// unaffected.
#[derive(Debug)]
pub struct WindowCompositionVariant {
    pub sites: Vec<usize>,
    pub roots: Result<Vec<QueryRoot>, PerComputationCandidateError>,
}

/// The most pane sites one candidate may have: every non-empty subset is a
/// candidate, about a million at 20.
pub const MAX_PANE_SITES: usize = 20;

/// The pane sites of the candidate with query roots `roots`, children
/// first. `interval_ms[q]` is how often query `q` repeats, `None` when it
/// does not repeat at a fixed interval.
pub fn pane_sites(roots: &[QueryRoot], interval_ms: &[Option<u64>]) -> Vec<PaneSite> {
    let mut nodes = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for node in roots.iter().flat_map(root_operators) {
        collect_children_first(node, &mut seen, &mut nodes);
    }
    let reach: Vec<std::collections::HashSet<*const OperatorNode>> = roots
        .iter()
        .map(|root| {
            root_operators(root)
                .into_iter()
                .flat_map(OperatorNode::reachable)
                .map(|node| Rc::as_ptr(&node))
                .collect()
        })
        .collect();
    nodes
        .into_iter()
        .filter_map(|build| {
            let window_ms = pane_window_ms(&build)?;
            let readers: Vec<usize> = (0..roots.len())
                .filter(|&q| reach[q].contains(&Rc::as_ptr(&build)))
                .collect();
            let mut width = window_ms;
            for &q in &readers {
                width = gcd(width, interval_ms.get(q).copied().flatten()?);
            }
            let pane_count = u32::try_from(window_ms / width).ok()?;
            (!readers.is_empty() && pane_count >= 2).then_some(PaneSite {
                build,
                readers,
                pane_width_ms: width,
                pane_count,
            })
        })
        .collect()
}

/// The window `W` of a build the rule can split: a `SummaryAgg` whose
/// family merges, over a range `TimeRange(W)` over an instant vector read
/// at the query's time or a fixed offset from it.
fn pane_window_ms(build: &OperatorNode) -> Option<u64> {
    let Some(ASAPOp::SummaryAgg { child, family, .. }) = build.asap() else {
        return None;
    };
    FamilyCapability::of(family).merge?;
    let Some(NonASAPOp::TimeRange {
        range,
        kind: TimeRangeKind::Range,
        child: input,
    }) = child.non_asap()
    else {
        return None;
    };
    if input.result_kind != OperatorResultKind::InstantVector
        || matches!(input.non_asap(), Some(NonASAPOp::TimeShift { shift, .. }) if shift.at.is_some())
    {
        return None;
    }
    u64::try_from(range.as_millis()).ok().filter(|&ms| ms > 0)
}

fn gcd(a: u64, b: u64) -> u64 {
    if b == 0 {
        a
    } else {
        gcd(b, a % b)
    }
}

/// Every Pass 2 candidate of the window-composition rule for the candidate
/// with query roots `roots` and pane sites `sites` ([`pane_sites`]): one per
/// non-empty subset of the sites, in binary-counter order (the first site
/// the lowest bit). Each build in the subset becomes a `SummaryMerge` over
/// one [`ASAPOp::PaneChain`] of its panes; every other build is untouched.
/// An error only for more than [`MAX_PANE_SITES`] sites; a variant whose
/// composition fails carries its own error.
pub fn window_composition_variants(
    roots: &[QueryRoot],
    sites: &[PaneSite],
) -> Result<Vec<WindowCompositionVariant>, PerComputationCandidateError> {
    if sites.len() > MAX_PANE_SITES {
        return Err(PerComputationCandidateError::Unsupported(
            "more than 20 summary builds that could be built in tumbling panes (2^n candidates)",
        ));
    }
    let mut variants = Vec::new();
    for mask in 1u64..(1u64 << sites.len()) {
        let chosen: Vec<usize> = (0..sites.len()).filter(|&i| mask >> i & 1 == 1).collect();
        variants.push(WindowCompositionVariant {
            roots: in_panes(roots, chosen.iter().map(|&i| &sites[i])),
            sites: chosen,
        });
    }
    Ok(variants)
}

/// `roots` with each of `sites` built as its tumbling panes, and every
/// guarantee composed again.
fn in_panes<'a>(
    roots: &[QueryRoot],
    sites: impl Iterator<Item = &'a PaneSite>,
) -> Result<Vec<QueryRoot>, PerComputationCandidateError> {
    let mut replaced: HashMap<*const OperatorNode, Rc<OperatorNode>> = HashMap::new();
    for site in sites {
        replaced.insert(Rc::as_ptr(&site.build), merged_panes(site)?);
    }
    let mut rebuilt: HashMap<*const OperatorNode, Rc<OperatorNode>> = HashMap::new();
    let mut out = Vec::new();
    for root in roots {
        out.push(match root {
            QueryRoot::Operator(node) => {
                QueryRoot::Operator(rebuild(node, &replaced, &mut rebuilt)?)
            }
            QueryRoot::Scalar(expr) => {
                let mut nodes = HashMap::new();
                for node in expr.operator_refs() {
                    nodes.insert(Rc::as_ptr(node), rebuild(node, &replaced, &mut rebuilt)?);
                }
                QueryRoot::Scalar(
                    expr.map_operator_refs(&mut |node| nodes[&Rc::as_ptr(node)].clone()),
                )
            }
        });
    }
    with_composed_guarantees(&out)
}

/// `SummaryMerge` over the [`ASAPOp::PaneChain`] of `site`'s panes, with
/// the build's output names.
fn merged_panes(site: &PaneSite) -> Result<Rc<OperatorNode>, PerComputationCandidateError> {
    let Some(ASAPOp::SummaryAgg {
        child: range,
        family,
        input,
        reduction,
        grouping,
        filter,
    }) = site.build.asap()
    else {
        unreachable!("a pane site is a SummaryAgg");
    };
    let mut chain = OperatorNode::new(Operator::ASAP(ASAPOp::PaneChain {
        child: Rc::clone(range.children()[0]),
        family: family.clone(),
        input: input.clone(),
        reduction: reduction.clone(),
        grouping: grouping.clone(),
        filter: filter.clone(),
        pane_width_ms: site.pane_width_ms,
        pane_count: site.pane_count,
    }))?;
    for (field, kept) in chain
        .schema
        .fields
        .iter_mut()
        .zip(&site.build.schema.fields)
    {
        field.name = kept.name.clone();
        field.table = kept.table.clone();
    }
    Ok(Rc::new(OperatorNode::new(Operator::ASAP(
        ASAPOp::SummaryMerge {
            children: vec![Rc::new(chain)],
        },
    ))?))
}

/// `node` with every replaced build below it swapped for its shared build;
/// the same `Rc` when nothing below changed.
fn rebuild(
    node: &Rc<OperatorNode>,
    replaced: &HashMap<*const OperatorNode, Rc<OperatorNode>>,
    rebuilt: &mut HashMap<*const OperatorNode, Rc<OperatorNode>>,
) -> Result<Rc<OperatorNode>, PerComputationCandidateError> {
    if let Some(done) = rebuilt.get(&Rc::as_ptr(node)) {
        return Ok(Rc::clone(done));
    }
    let out = match replaced.get(&Rc::as_ptr(node)) {
        Some(build) => Rc::clone(build),
        None => {
            let mut changed = false;
            let mut children = HashMap::new();
            for child in node.children() {
                let new = rebuild(child, replaced, rebuilt)?;
                changed |= !Rc::ptr_eq(&new, child);
                children.insert(Rc::as_ptr(child), new);
            }
            if changed {
                Rc::new(node.with_new_children(|child| children[&Rc::as_ptr(child)].clone())?)
            } else {
                Rc::clone(node)
            }
        }
    };
    rebuilt.insert(Rc::as_ptr(node), Rc::clone(&out));
    Ok(out)
}

fn root_operators(root: &QueryRoot) -> Vec<&Rc<OperatorNode>> {
    match root {
        QueryRoot::Operator(node) => vec![node],
        QueryRoot::Scalar(expr) => expr.operator_refs(),
    }
}

/// Distinct nodes (by identity) reachable from `roots`.
#[cfg(test)]
fn distinct_nodes(roots: &[QueryRoot]) -> usize {
    roots
        .iter()
        .flat_map(root_operators)
        .flat_map(OperatorNode::reachable)
        .map(|node| Rc::as_ptr(&node))
        .collect::<HashSet<_>>()
        .len()
}

/// Nodes reachable from both `a` and `b`.
#[cfg(test)]
fn common_nodes(a: &QueryRoot, b: &QueryRoot) -> HashSet<*const OperatorNode> {
    let reach = |root: &QueryRoot| -> HashSet<*const OperatorNode> {
        root_operators(root)
            .into_iter()
            .flat_map(OperatorNode::reachable)
            .map(|node| Rc::as_ptr(&node))
            .collect()
    };
    reach(a).intersection(&reach(b)).copied().collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::lower_promql;
    use asap_types::ir::schema_support::with_promql_series_identity;
    use asap_types::types::RequestedAccuracy;

    fn roots(queries: &[&str]) -> Vec<QueryRoot> {
        queries
            .iter()
            .map(|q| {
                let root = lower_promql(q, RequestedAccuracy::exact());
                QueryRoot::Operator(with_promql_series_identity(&root).unwrap())
            })
            .collect()
    }

    fn kinds(group: &SharingGroup) -> &'static str {
        group.nodes[0].operator.kind_name()
    }

    /// Two queries over the same range selector have two sharing groups,
    /// the scans and the time ranges. Sharing the time range shares the
    /// scan below it, so there are two variants: the scan alone shared, and
    /// both shared. Each query's value is unchanged.
    #[test]
    fn identical_range_selector_gives_one_variant_per_distinct_subset() {
        let roots = roots(&[
            "sum by (job) (rate(m[1m]))",
            "topk by (job) (10, sum_over_time(m[1m]))",
        ]);
        let groups = sharing_groups(&roots);
        assert_eq!(
            groups.iter().map(kinds).collect::<Vec<_>>(),
            ["Scan", "TimeRange"]
        );
        let variants = identical_expression_variants(&roots, &sharing_groups(&roots));
        assert_eq!(
            variants
                .iter()
                .map(|v| v.shared_groups.clone())
                .collect::<Vec<_>>(),
            [vec![0], vec![0, 1]]
        );
        for variant in &variants {
            assert_eq!(variant.roots, roots, "values are unchanged");
            assert!(distinct_nodes(&variant.roots) < distinct_nodes(&roots));
        }
        assert_eq!(
            common_nodes(&variants[0].roots[0], &variants[0].roots[1]).len(),
            1
        );
        assert_eq!(
            common_nodes(&variants[1].roots[0], &variants[1].roots[1]).len(),
            2
        );
    }

    /// Two pairs of queries over different metrics: each pair has a scan
    /// group and an instant-selector group, with two distinct ways to share
    /// (the scan, or the scan and the selector) or none, so 3 × 3 − 1 = 8
    /// Pass 2 candidates; each pair is shared alone or with the other.
    #[test]
    fn independent_groups_give_every_non_empty_subset() {
        let roots = roots(&["sum(a)", "max(a)", "sum(b)", "max(b)"]);
        assert_eq!(
            sharing_groups(&roots).iter().map(kinds).collect::<Vec<_>>(),
            ["Scan", "TimeRange", "Scan", "TimeRange"]
        );
        let variants = identical_expression_variants(&roots, &sharing_groups(&roots));
        assert_eq!(variants.len(), 8);
        let shares = |v: &IdenticalExpressionVariant| {
            (
                !common_nodes(&v.roots[0], &v.roots[1]).is_empty(),
                !common_nodes(&v.roots[2], &v.roots[3]).is_empty(),
            )
        };
        assert_eq!(
            variants.iter().map(shares).collect::<HashSet<_>>(),
            HashSet::from([(true, false), (false, true), (true, true)])
        );
        assert!(variants.iter().all(|v| v.roots == roots));
    }

    /// Queries with nothing in common have no Pass 2 candidate.
    #[test]
    fn nothing_identical_gives_no_variant() {
        assert!({
            let roots = roots(&["sum(rate(a[1m]))", "sum(rate(b[1m]))"]);
            identical_expression_variants(&roots, &sharing_groups(&roots))
        }
        .is_empty());
    }

    /// Every non-empty subset of the groups, applied, kept when it gives a
    /// distinct candidate: the definition the direct enumeration follows.
    fn brute_force(roots: &[QueryRoot]) -> Vec<Vec<usize>> {
        let groups = sharing_groups(roots);
        let mut subset = vec![false; groups.len()];
        let mut out = Vec::new();
        while next_subset(&mut subset) {
            let (_, merged) = apply(roots, &groups, &subset);
            if merged == subset {
                out.push((0..groups.len()).filter(|&g| subset[g]).collect());
            }
        }
        out
    }

    /// The direct enumeration gives exactly the brute force's candidates, in
    /// the same order, on nested and independent groups.
    #[test]
    fn direct_enumeration_matches_the_brute_force() {
        for queries in [
            &[
                "sum by (job) (rate(m[1m]))",
                "topk by (job) (10, sum_over_time(m[1m]))",
            ][..],
            &["sum(a)", "max(a)", "sum(b)", "max(b)"],
            &["sum(abs(a))", "max(abs(a))", "min(a)", "sum(b)", "max(b)"],
            &["sum(abs(ceil(a)))", "max(abs(ceil(a)))", "min(ceil(a))"],
        ] {
            let roots = roots(queries);
            let groups = sharing_groups(&roots);
            let direct: Vec<_> = identical_expression_variants(&roots, &groups)
                .into_iter()
                .map(|v| v.shared_groups)
                .collect();
            assert_eq!(direct, brute_force(&roots), "{queries:?}");
        }
    }

    /// Twenty nested groups (two identical chains of a keyed scan and 19
    /// filters) have 20 distinct candidates, one per depth shared, not 2^20
    /// subsets to try.
    #[test]
    fn twenty_nested_groups_enumerate_quickly() {
        use crate::test_support::{filter, metric_scan_with_keys};
        use asap_types::ir::ScalarExpr;
        use asap_types::pre_asap::expr_ir::CompareOpKind;
        let chain = || {
            let mut node = metric_scan_with_keys(&["job"], vec![vec![0, 2]]);
            for i in 0..19 {
                node = filter(
                    ScalarExpr::Compare {
                        left: Box::new(ScalarExpr::Column(1)),
                        op: CompareOpKind::Gt,
                        right: Box::new(ScalarExpr::literal_f64(i as f64)),
                        semantics: asap_types::ir::ExprSemantics::Promql,
                    },
                    node,
                );
            }
            QueryRoot::Operator(node)
        };
        let roots = [chain(), chain()];
        let groups = sharing_groups(&roots);
        assert_eq!(groups.len(), 20);
        let start = std::time::Instant::now();
        let variants = identical_expression_variants(&roots, &groups);
        assert_eq!(variants.len(), 20);
        assert!(
            start.elapsed() < std::time::Duration::from_secs(5),
            "{:?}",
            start.elapsed()
        );
    }

    /// The composed candidate of `queries` (each at its own ε, δ = 0.01)
    /// in which every target chose `family`.
    fn chose(queries: &[(&str, f64)], family: SketchAlgorithm) -> Vec<QueryRoot> {
        use crate::per_computation_candidates::ComputationCandidate;
        chose_where(queries, |c| {
            *c == ComputationCandidate::Summary(family.clone())
        })
    }

    /// The composed candidate of `queries` (each at its own ε, δ = 0.01)
    /// in which every target chose the first candidate `pick` accepts.
    fn chose_where(
        queries: &[(&str, f64)],
        pick: impl Fn(&crate::per_computation_candidates::ComputationCandidate) -> bool,
    ) -> Vec<QueryRoot> {
        use crate::per_computation_candidates::{
            compose_logical_candidate, generate_per_computation_candidates,
        };
        let roots = queries
            .iter()
            .enumerate()
            .map(|(i, (q, epsilon))| {
                let root = lower_promql(
                    q,
                    asap_types::types::RequestedAccuracy::epsilon_delta(*epsilon, 0.01)
                        .with_metric(asap_types::types::ErrorMetric::Rank),
                );
                (
                    i,
                    QueryRoot::Operator(with_promql_series_identity(&root).unwrap()),
                )
            })
            .collect();
        let inventory = generate_per_computation_candidates(roots).unwrap();
        let choice: Vec<usize> = inventory
            .targets
            .iter()
            .map(|t| {
                t.choices
                    .candidates
                    .iter()
                    .position(&pick)
                    .expect("the family is a candidate")
            })
            .collect();
        compose_logical_candidate(&inventory, &choice)
            .unwrap()
            .into_iter()
            .map(|(_, root)| root)
            .collect()
    }

    /// The summary builds `roots` read, by address, with each one's KLL `k`.
    fn kll_builds(roots: &[QueryRoot]) -> Vec<(*const OperatorNode, u32)> {
        let mut out: Vec<(*const OperatorNode, u32)> = Vec::new();
        for node in roots
            .iter()
            .flat_map(root_operators)
            .flat_map(OperatorNode::reachable)
        {
            if let Some(ASAPOp::SummaryAgg {
                family: FieldDataType::Sketch(kind, _),
                ..
            }) = node.asap()
            {
                let asap_types::post_asap::SketchParams::Kll { k, .. } = kind.params() else {
                    panic!("{kind:?}")
                };
                if !out.iter().any(|(p, _)| *p == Rc::as_ptr(&node)) {
                    out.push((Rc::as_ptr(&node), *k));
                }
            }
        }
        out
    }

    fn root_bound(root: &QueryRoot) -> f64 {
        let QueryRoot::Operator(node) = root else {
            panic!("operator root")
        };
        node.guarantee
            .as_ref()
            .and_then(|g| g.spec.error.evaluate())
            .expect("a constant bound")
    }

    /// p50 at ε = 0.01 and p99 at ε = 0.001 over one window, each with its
    /// own KLL: the rule shares one KLL with the larger `k`, and p50's
    /// guarantee is the shared KLL's, tighter than its own.
    #[test]
    fn quantiles_at_different_targets_share_one_kll_with_the_larger_k() {
        let roots = chose(
            &[
                ("quantile_over_time(0.5, lat[5m])", 0.01),
                ("quantile_over_time(0.99, lat[5m])", 0.001),
            ],
            SketchAlgorithm::Kll,
        );
        let own = kll_builds(&roots);
        assert_eq!(own.len(), 2);
        assert!(own[0].1 < own[1].1);
        let groups = capability_groups(&roots);
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].family, SketchAlgorithm::Kll);
        let variants = summary_capability_variants(&roots, &groups).unwrap();
        assert_eq!(variants.len(), 1);
        assert_eq!(
            variants[0].shared,
            [SharedSummary {
                group: 0,
                members: vec![0, 1]
            }]
        );
        let shared = kll_builds(variants[0].roots.as_ref().unwrap());
        assert_eq!(shared.len(), 1, "one KLL serves both");
        assert_eq!(shared[0].1, own[1].1);
        assert!(root_bound(&variants[0].roots.as_ref().unwrap()[0]) < root_bound(&roots[0]));
        assert_eq!(
            root_bound(&variants[0].roots.as_ref().unwrap()[1]),
            root_bound(&roots[1])
        );
    }

    /// Three consumers at three targets: every subset of two or more of
    /// them shares one build (three pairs and all three), the others keep
    /// their own.
    #[test]
    fn every_subset_of_two_or_more_consumers_shares_one_build() {
        let roots = chose(
            &[
                ("quantile_over_time(0.5, lat[5m])", 0.05),
                ("quantile_over_time(0.9, lat[5m])", 0.01),
                ("quantile_over_time(0.99, lat[5m])", 0.001),
            ],
            SketchAlgorithm::Kll,
        );
        let groups = capability_groups(&roots);
        let variants = summary_capability_variants(&roots, &groups).unwrap();
        assert_eq!(
            variants
                .iter()
                .map(|v| v.shared[0].members.clone())
                .collect::<Vec<_>>(),
            [vec![0, 1], vec![0, 2], vec![1, 2], vec![0, 1, 2]]
        );
        for variant in &variants {
            let builds = kll_builds(variant.roots.as_ref().unwrap());
            assert_eq!(builds.len(), 4 - variant.shared[0].members.len());
        }
    }

    /// A group of more than 20 members (over a million sharing subsets)
    /// is an error, not a silent skip or an endless enumeration.
    #[test]
    fn a_group_past_20_members_is_an_error() {
        let roots = chose(
            &[
                ("quantile_over_time(0.5, lat[5m])", 0.01),
                ("quantile_over_time(0.99, lat[5m])", 0.001),
            ],
            SketchAlgorithm::Kll,
        );
        let mut groups = capability_groups(&roots);
        let members = groups[0].members.clone();
        groups[0].members = members
            .iter()
            .cycle()
            .take(MAX_GROUP_MEMBERS + 1)
            .cloned()
            .collect();
        assert!(matches!(
            summary_capability_variants(&roots, &groups),
            Err(PerComputationCandidateError::Unsupported(_))
        ));
    }

    /// Equal targets give identical builds, which are the
    /// identical-expression rule's; a different window or family is a
    /// different input or capability, never shared.
    #[test]
    fn equal_sizes_other_windows_and_other_families_are_not_capability_groups() {
        let equal = chose(
            &[
                ("quantile_over_time(0.5, lat[5m])", 0.01),
                ("quantile_over_time(0.99, lat[5m])", 0.01),
            ],
            SketchAlgorithm::Kll,
        );
        assert!(capability_groups(&equal).is_empty());
        assert!(!identical_expression_variants(&equal, &sharing_groups(&equal)).is_empty());
        let windows = chose(
            &[
                ("quantile_over_time(0.5, lat[5m])", 0.01),
                ("quantile_over_time(0.99, lat[10m])", 0.001),
            ],
            SketchAlgorithm::Kll,
        );
        assert!(capability_groups(&windows).is_empty());
        let families = chose(
            &[
                ("quantile_over_time(0.5, lat[5m])", 0.01),
                ("quantile_over_time(0.99, lat[5m])", 0.001),
            ],
            SketchAlgorithm::DDSketch,
        );
        assert_eq!(capability_groups(&families).len(), 1);
        assert_eq!(
            capability_groups(&families)[0].family,
            SketchAlgorithm::DDSketch
        );
    }

    /// Each candidate's label (the groups it shares) and its roots agree:
    /// applying the label gives the same roots, sharing exactly the
    /// labelled groups.
    #[test]
    fn labels_and_roots_agree() {
        for queries in [
            &[
                "sum by (job) (rate(m[1m]))",
                "topk by (job) (10, sum_over_time(m[1m]))",
            ][..],
            &["sum(a)", "max(a)", "sum(b)", "max(b)"],
            &["sum(abs(a))", "max(abs(a))", "min(a)", "sum(b)", "max(b)"],
        ] {
            let roots = roots(queries);
            let groups = sharing_groups(&roots);
            for variant in identical_expression_variants(&roots, &groups) {
                let label: Vec<bool> = (0..groups.len())
                    .map(|g| variant.shared_groups.contains(&g))
                    .collect();
                let (applied, merged) = apply(&roots, &groups, &label);
                assert_eq!(merged, label, "{queries:?}");
                assert_eq!(applied, variant.roots, "{queries:?}");
                assert_eq!(
                    distinct_nodes(&applied),
                    distinct_nodes(&variant.roots),
                    "{queries:?} {:?}",
                    variant.shared_groups
                );
                for (a, b) in [(0, 1), (2, 3)] {
                    if b < roots.len() {
                        assert_eq!(
                            common_nodes(&applied[a], &applied[b]).len(),
                            common_nodes(&variant.roots[a], &variant.roots[b]).len()
                        );
                    }
                }
            }
        }
    }

    /// The pane sites of `roots` as (pane width, pane count).
    fn panes(roots: &[QueryRoot], interval_ms: &[Option<u64>]) -> Vec<(u64, u32)> {
        pane_sites(roots, interval_ms)
            .iter()
            .map(|site| (site.pane_width_ms, site.pane_count))
            .collect()
    }

    /// A 5-min window evaluated every 2 min, 90 s or 1 min is built in panes
    /// of gcd(window, interval); a query evaluated once per window, or not
    /// at a fixed interval, has no panes.
    #[test]
    fn pane_width_is_the_gcd_of_the_window_and_the_interval() {
        let roots = chose(
            &[("quantile_over_time(0.99, m[5m])", 0.01)],
            SketchAlgorithm::Kll,
        );
        assert_eq!(panes(&roots, &[Some(120_000)]), [(60_000, 5)]);
        assert_eq!(panes(&roots, &[Some(90_000)]), [(30_000, 10)]);
        assert_eq!(panes(&roots, &[Some(60_000)]), [(60_000, 5)]);
        assert!(panes(&roots, &[Some(300_000)]).is_empty());
        assert!(panes(&roots, &[None]).is_empty());
    }

    /// Exact accumulators whose states merge get panes; IRate, which reads
    /// the last two samples of the window, does not.
    #[test]
    fn exact_accumulators_that_merge_get_panes() {
        use crate::per_computation_candidates::ComputationCandidate;
        let accumulator =
            |c: &ComputationCandidate| matches!(c, ComputationCandidate::ExactAccumulator { .. });
        for query in [
            "rate(m[5m])",
            "increase(m[5m])",
            "sum_over_time(m[5m])",
            "max_over_time(m[5m])",
        ] {
            let roots = chose_where(&[(query, 0.01)], accumulator);
            assert_eq!(panes(&roots, &[Some(60_000)]), [(60_000, 5)], "{query}");
        }
        let roots = chose_where(&[("irate(m[5m])", 0.01)], accumulator);
        assert!(panes(&roots, &[Some(60_000)]).is_empty());
    }

    /// The rule's candidate reads a `SummaryMerge` over one `PaneChain` of
    /// the build's family and size over the range's input, keeps the
    /// build's output names, and keeps the whole build's guarantee (a KLL
    /// merges into the state of one build over the union).
    #[test]
    fn panes_replace_the_build_with_a_merged_chain() {
        let roots = chose(
            &[("quantile_over_time(0.99, m[5m])", 0.01)],
            SketchAlgorithm::Kll,
        );
        let sites = pane_sites(&roots, &[Some(60_000)]);
        let variants = window_composition_variants(&roots, &sites).unwrap();
        let [variant] = variants.as_slice() else {
            panic!("one variant: {variants:?}")
        };
        assert_eq!(variant.sites, [0]);
        let panes = variant.roots.as_ref().unwrap();
        let QueryRoot::Operator(root) = &panes[0] else {
            panic!("operator root")
        };
        root.validate_structure().unwrap();
        let QueryRoot::Operator(whole) = &roots[0] else {
            panic!("operator root")
        };
        assert_eq!(root.guarantee, whole.guarantee);
        assert!(root.guarantee.is_some());
        let merge = OperatorNode::reachable(root)
            .into_iter()
            .find(|n| matches!(n.asap(), Some(ASAPOp::SummaryMerge { .. })))
            .expect("a merge");
        let Some(ASAPOp::SummaryMerge { children }) = merge.asap() else {
            unreachable!()
        };
        let [chain] = children.as_slice() else {
            panic!("one chain")
        };
        let Some(ASAPOp::PaneChain {
            child,
            family,
            pane_width_ms: 60_000,
            pane_count: 5,
            ..
        }) = chain.asap()
        else {
            panic!("{:?}", chain.operator)
        };
        let Some(ASAPOp::SummaryAgg {
            child: range,
            family: whole_family,
            ..
        }) = sites[0].build.asap()
        else {
            unreachable!()
        };
        assert_eq!(family, whole_family);
        assert_eq!(child.operator, range.children()[0].operator);
        assert_eq!(chain.schema, sites[0].build.schema);
        assert_eq!(merge.coverage(), sites[0].build.coverage());
    }

    /// Two builds that could be split give three candidates: each alone and
    /// both, in binary-counter order.
    #[test]
    fn every_non_empty_subset_of_the_sites_is_a_candidate() {
        let roots = chose(
            &[
                ("quantile_over_time(0.99, m[5m])", 0.01),
                ("quantile_over_time(0.5, m[2m])", 0.01),
            ],
            SketchAlgorithm::Kll,
        );
        let sites = pane_sites(&roots, &[Some(60_000), Some(60_000)]);
        assert_eq!(sites.len(), 2);
        let subsets: Vec<_> = window_composition_variants(&roots, &sites)
            .unwrap()
            .into_iter()
            .map(|v| {
                assert!(v.roots.is_ok());
                v.sites
            })
            .collect();
        assert_eq!(subsets, [vec![0], vec![1], vec![0, 1]]);
    }
}

//! Implementation candidates attached to memo groups.

use std::collections::{HashMap, HashSet};
use std::rc::Rc;

use super::{MemoGroup, MemoGroupId, NodeToStructuralIdCache};
use crate::ir::{OperatorNode, QueryRoot};

/// The memo: the memo groups, each query's root over them, and each group's
/// candidate implementations.
#[derive(Debug, Clone, Default)]
pub struct Memo {
    pub groups: Vec<MemoGroup>,
    /// Each query's root over groups, by index in the workload (the
    /// initial logical memo's query roots); groups added later are never a
    /// query's root.
    pub query_roots: Vec<QueryRoot<MemoGroupId>>,
    /// Each group's implementation candidates, by group index.
    pub candidates: Vec<Vec<CandidateForMemoGroup>>,
    /// The hash-consing index [`Self::intern`] looks groups up in.
    group_index: MemoGroupIndex,
}

/// The groups by logical node and by structural id, over
/// `groups[..indexed_groups]`: [`Memo::intern`] indexes groups appended
/// since its last call first.
#[derive(Debug, Clone, Default)]
struct MemoGroupIndex {
    indexed_groups: usize,
    /// Each group's logical node, by address.
    group_of_node: HashMap<*const OperatorNode, MemoGroupId>,
    /// The lowest group of each structural id whose logical node is legal
    /// to share when equal (Stage 0's rule: an ASAP node, or a non-ASAP node
    /// `NonASAPOp::is_shareable_when_equal`).
    group_of_structural_id: HashMap<u64, MemoGroupId>,
    structural_ids: NodeToStructuralIdCache,
    /// The groups [`Memo::intern`] made.
    made_by_intern: HashSet<MemoGroupId>,
}

/// One way to implement a group: its node as Stage 2 implements it, over
/// child groups.
#[derive(Debug, Clone)]
pub struct CandidateForMemoGroup {
    /// The groups a fused computation also implements, beyond the group
    /// whose candidate it is (its owner, implicit from its position in
    /// [`Memo::candidates`]); empty otherwise.
    pub fused_groups: Vec<MemoGroupId>,
    /// The implementation's own node(s): its root node, whose descendants
    /// down to [`Self::input_groups`] are its private nodes.
    pub root_node: Rc<OperatorNode>,
    /// The groups it reads, whose implementations are chosen separately.
    pub input_groups: Vec<MemoGroupId>,
    /// How it was made: by Pass 1, or by which Pass 2 transformation.
    pub origin: OriginOfCandidateForMemoGroup,
    /// The number of equal shares of its measures' accuracy budget its
    /// summaries are sized for: 1 for the whole budget, `n` when it is one
    /// of `n` approximate inputs of a query (a ratio's operand). This positive
    /// generation parameter is already encoded in concrete sketch parameters;
    /// it is not an equality constraint on the selected number of readers or
    /// approximate inputs. Recompose and validate accuracy at query roots.
    /// Larger allocations may be valid but costlier than necessary.
    pub accuracy_budget_parts: usize,
}

/// Whether two structurally equal copies of `node` compute the same rows,
/// so they may be one group (Stage 0's legality rule).
fn shareable_when_equal(node: &OperatorNode) -> bool {
    node.is_asap()
        || node
            .non_asap()
            .is_some_and(crate::ir::NonASAPOp::is_shareable_when_equal)
}

/// Where an implementation candidate came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OriginOfCandidateForMemoGroup {
    /// Pass 1: exact, exact accumulator, or a summary family and size; or
    /// the operator of an intermediate result Pass 1 interned as a group.
    Pass1,
    /// One build serving several reader groups (summary capability).
    SharedBuild,
    /// A summary readout factored from a composite implementation.
    SummaryResult,
    /// Canonical value preprocessing feeding a summary build.
    SummaryInput,
    /// Tumbling windows, or the newest tumbling windows of another group's chain.
    TumblingWindows,
    /// A coarse group regrouping a finer group.
    Rollup,
    /// A top-k read from another group's ranking.
    TopKReuse,
    /// Its metric's scans widened to a shared label set, read through a
    /// `Project` (scan widening).
    WidenedScans,
}

impl Memo {
    /// A memo of `groups`, the queries' roots over them and each group's
    /// candidates, by group index.
    pub fn new(
        groups: Vec<MemoGroup>,
        query_roots: Vec<QueryRoot<MemoGroupId>>,
        candidates: Vec<Vec<CandidateForMemoGroup>>,
    ) -> Self {
        Self {
            groups,
            query_roots,
            candidates,
            group_index: MemoGroupIndex::default(),
        }
    }

    /// The group of `node`, hash-consed as Stage 0 builds the groups: the
    /// group whose logical node is `node`, else, when sharing `node` is legal,
    /// the lowest group with its structural id, else a new group. A new
    /// group's logical node is `node` over its children's groups, interned
    /// first; its one candidate is that node over those groups, made by
    /// `origin` and sized for `accuracy_budget_parts`; `queries` reach it.
    ///
    /// A found group and every group below it are reached by `queries` too.
    /// When `intern` made the found group, its candidate made by `origin` is
    /// sized for the larger of its and `accuracy_budget_parts`: the node is
    /// the same, so it meets both sizings.
    pub fn intern(
        &mut self,
        node: &Rc<OperatorNode>,
        queries: &[usize],
        origin: &OriginOfCandidateForMemoGroup,
        accuracy_budget_parts: usize,
    ) -> MemoGroupId {
        if let Some(group) = self.group_of(node) {
            self.reach_by_queries(group, queries);
            if !self.group_index.made_by_intern.contains(&group) {
                return group;
            }
            let own = Rc::clone(&self.groups[group.0 as usize].logical_node);
            for candidate in &mut self.candidates[group.0 as usize] {
                if candidate.origin == *origin && Rc::ptr_eq(&candidate.root_node, &own) {
                    candidate.accuracy_budget_parts =
                        candidate.accuracy_budget_parts.max(accuracy_budget_parts);
                }
            }
            return group;
        }
        let mut child_of: HashMap<*const OperatorNode, MemoGroupId> = HashMap::new();
        let mut child_groups = Vec::new();
        for child in node.children() {
            let group = match child_of.get(&Rc::as_ptr(child)) {
                Some(&group) => group,
                None => {
                    let group = self.intern(child, queries, origin, accuracy_budget_parts);
                    child_of.insert(Rc::as_ptr(child), group);
                    group
                }
            };
            child_groups.push(group);
        }
        let reads_group_nodes = node.children().into_iter().all(|child| {
            Rc::ptr_eq(
                child,
                &self.groups[child_of[&Rc::as_ptr(child)].0 as usize].logical_node,
            )
        });
        let logical_node = if reads_group_nodes {
            Rc::clone(node)
        } else {
            Rc::new(OperatorNode {
                operator: node.operator.map_children(|child| {
                    Rc::clone(&self.groups[child_of[&Rc::as_ptr(child)].0 as usize].logical_node)
                }),
                ..(**node).clone()
            })
        };
        let id = MemoGroupId(
            u32::try_from(self.groups.len()).expect("a memo has at most u32::MAX groups"),
        );
        let mut parents_updated = HashSet::new();
        for &child in &child_groups {
            if parents_updated.insert(child) {
                self.groups[child.0 as usize].parents.push(id);
            }
        }
        let mut queries = queries.to_vec();
        queries.sort_unstable();
        queries.dedup();
        self.groups.push(MemoGroup {
            logical_node: Rc::clone(&logical_node),
            child_groups: child_groups.clone(),
            parents: Vec::new(),
            queries,
        });
        self.candidates.push(vec![CandidateForMemoGroup {
            fused_groups: Vec::new(),
            root_node: logical_node,
            input_groups: child_groups,
            origin: origin.clone(),
            accuracy_budget_parts,
        }]);
        self.group_index.made_by_intern.insert(id);
        // `node` itself maps to the new group too, when it was rebuilt; the
        // structural-id cache keeps it alive, so its address is not reused.
        self.group_index.group_of_node.insert(Rc::as_ptr(node), id);
        self.group_index.structural_ids.structural_id(node);
        self.index_appended_groups();
        id
    }

    /// The group [`Self::intern`] finds for `node`, without adding one: the
    /// group whose logical node is `node`, else, when sharing `node` is
    /// legal, the lowest group with its structural id.
    pub fn group_of(&mut self, node: &Rc<OperatorNode>) -> Option<MemoGroupId> {
        self.index_appended_groups();
        let index = &mut self.group_index;
        if let Some(&group) = index.group_of_node.get(&Rc::as_ptr(node)) {
            return Some(group);
        }
        if !shareable_when_equal(node) {
            return None;
        }
        let id = index.structural_ids.structural_id(node);
        index.group_of_structural_id.get(&id).copied()
    }

    /// Index the groups appended since the last call (by [`Self::intern`] or
    /// directly).
    fn index_appended_groups(&mut self) {
        let index = &mut self.group_index;
        for g in index.indexed_groups..self.groups.len() {
            let id = MemoGroupId(g as u32);
            let node = &self.groups[g].logical_node;
            index.group_of_node.entry(Rc::as_ptr(node)).or_insert(id);
            if shareable_when_equal(node) {
                let structural_id = index.structural_ids.structural_id(node);
                index
                    .group_of_structural_id
                    .entry(structural_id)
                    .or_insert(id);
            }
        }
        index.indexed_groups = self.groups.len();
    }

    /// Add `queries` to `group`'s and every group below it.
    fn reach_by_queries(&mut self, group: MemoGroupId, queries: &[usize]) {
        let mut pending = vec![group];
        let mut seen = HashSet::new();
        while let Some(g) = pending.pop() {
            if !seen.insert(g) {
                continue;
            }
            let reached = &mut self.groups[g.0 as usize].queries;
            if queries.iter().all(|q| reached.contains(q)) {
                continue;
            }
            reached.extend_from_slice(queries);
            reached.sort_unstable();
            reached.dedup();
            pending.extend(self.groups[g.0 as usize].child_groups.iter().copied());
        }
    }

    /// The groups, each after every group it or one of its candidates
    /// reads, the lower index first among those ready: a dependency order for
    /// candidate traversal, whatever order transformations appended
    /// groups in. `None` when those reads form a cycle, a group reading
    /// itself included.
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
                // A group reading itself never becomes ready: a cycle.
                reads[g].insert(child.0 as usize);
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

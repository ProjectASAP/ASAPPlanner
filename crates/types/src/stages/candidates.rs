//! Implementation candidates attached to memo groups.

use std::collections::{HashMap, HashSet};
use std::rc::Rc;

use super::hash_consing::HashConsingTable;
use super::{DagOfLogicalMemoGroups, MemoGroup, MemoGroupId};
use crate::ir::{OperatorNode, QueryRoot};

/// The memo: the memo groups, each query's root over them, and each group's
/// candidate implementations.
#[derive(Debug, Clone, Default)]
pub struct Memo {
    /// The groups, by id ([`Self::groups`]); private so every group added
    /// ([`Self::push_group`]) is in [`Self::group_lookup`].
    groups: Vec<MemoGroup>,
    /// Each query's root over groups, by index in the workload (the
    /// initial logical memo's query roots); groups added later are never a
    /// query's root.
    pub query_roots: Vec<QueryRoot<MemoGroupId>>,
    /// Each group's implementation candidates, by group index.
    pub candidates: Vec<Vec<CandidateForMemoGroup>>,
    /// Every group, by its logical node: [`Self::group_whose_node_is`] and
    /// [`Self::group_with_equal_node`] look groups up in it.
    group_lookup: MemoGroupLookup,
    /// The number of groups Stage 0 produced from the queries' DAG
    /// ([`Self::new`]): groups `0..stage0_group_count`. Groups added later
    /// come after them ([`Self::is_stage0_group`]).
    stage0_group_count: usize,
}

/// Every group of a memo, by the address of its logical node and by node
/// equality ([`HashConsingTable`], Stage 0's equality).
#[derive(Debug, Clone, Default)]
struct MemoGroupLookup {
    /// Each group's logical node, by address (the memo keeps it alive).
    group_whose_node_is: HashMap<*const OperatorNode, MemoGroupId>,
    /// Each group's logical node, by equality.
    group_with_equal_node: HashConsingTable<MemoGroupId>,
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

/// Where an implementation candidate came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OriginOfCandidateForMemoGroup {
    /// Pass 1: exact, exact accumulator, or a summary family and size; or
    /// the operator of an intermediate result Pass 1 added as a group.
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
    /// The memo of Stage 0's groups and query roots, every group without
    /// candidates yet.
    pub fn new(dag: DagOfLogicalMemoGroups) -> Self {
        let mut memo = Self {
            groups: Vec::with_capacity(dag.groups.len()),
            query_roots: dag.query_roots,
            candidates: vec![Vec::new(); dag.groups.len()],
            group_lookup: MemoGroupLookup::default(),
            stage0_group_count: dag.groups.len(),
        };
        for group in dag.groups {
            memo.index_and_push(group);
        }
        memo
    }

    /// The groups, by id.
    pub fn groups(&self) -> &[MemoGroup] {
        &self.groups
    }

    /// Whether `group` is one Stage 0 produced from the queries' DAG, not
    /// one Pass 1 or Pass 2 added beneath them.
    pub fn is_stage0_group(&self, group: MemoGroupId) -> bool {
        (group.0 as usize) < self.stage0_group_count
    }

    /// The groups, by id, to change in a test (a malformed reference, a
    /// different logical node); a changed logical node is not looked up again.
    #[cfg(any(test, feature = "test-support"))]
    pub fn groups_mut(&mut self) -> &mut [MemoGroup] {
        &mut self.groups
    }

    /// Adds `group` with its `candidates` and returns its id.
    pub fn push_group(
        &mut self,
        group: MemoGroup,
        candidates: Vec<CandidateForMemoGroup>,
    ) -> MemoGroupId {
        let id = self.index_and_push(group);
        self.candidates.push(candidates);
        id
    }

    /// Indexes `group` by its logical node and adds it; its id.
    fn index_and_push(&mut self, group: MemoGroup) -> MemoGroupId {
        let id = MemoGroupId(
            u32::try_from(self.groups.len()).expect("a memo has at most u32::MAX groups"),
        );
        let node = Rc::clone(&group.logical_node);
        self.group_lookup
            .group_whose_node_is
            .entry(Rc::as_ptr(&node))
            .or_insert(id);
        let equal_nodes = &mut self.group_lookup.group_with_equal_node;
        if equal_nodes.find(&node).is_none() {
            equal_nodes.insert(node, id);
        }
        self.groups.push(group);
        id
    }

    /// The group whose logical node is `node` itself (the same `Rc`).
    pub fn group_whose_node_is(&self, node: &Rc<OperatorNode>) -> Option<MemoGroupId> {
        self.group_lookup
            .group_whose_node_is
            .get(&Rc::as_ptr(node))
            .copied()
    }

    /// The group whose logical node is `node`, else the lowest group whose
    /// logical node equals `node` by Stage 0's equality when sharing `node`
    /// is legal ([`HashConsingTable::find`]).
    pub fn group_with_equal_node(&self, node: &Rc<OperatorNode>) -> Option<MemoGroupId> {
        self.group_whose_node_is(node)
            .or_else(|| self.group_lookup.group_with_equal_node.find(node))
    }

    /// The groups `group` reads: its logical child groups and its
    /// candidates' input groups.
    fn groups_read_by(&self, group: MemoGroupId) -> impl Iterator<Item = MemoGroupId> + '_ {
        self.groups[group.0 as usize]
            .child_groups
            .iter()
            .chain(
                self.candidates
                    .get(group.0 as usize)
                    .into_iter()
                    .flatten()
                    .flat_map(|candidate| &candidate.input_groups),
            )
            .copied()
    }

    /// Adds `queries` to `group` and to every group below it, through
    /// logical child groups and candidates' input groups. Below `group`, a
    /// group that already has every query is not walked further: the groups
    /// below it have them already, or get them when this is called for it.
    pub fn add_queries(&mut self, group: MemoGroupId, queries: &[usize]) {
        let mut pending: Vec<MemoGroupId> = self.groups_read_by(group).collect();
        let own = &mut self.groups[group.0 as usize].queries;
        own.extend_from_slice(queries);
        own.sort_unstable();
        own.dedup();
        let mut seen = HashSet::from([group]);
        while let Some(next) = pending.pop() {
            if !seen.insert(next) {
                continue;
            }
            let reached = &mut self.groups[next.0 as usize].queries;
            if queries.iter().all(|query| reached.contains(query)) {
                continue;
            }
            reached.extend_from_slice(queries);
            reached.sort_unstable();
            reached.dedup();
            pending.extend(self.groups_read_by(next));
        }
    }

    /// Sets every group's parents to the groups that read it: through a
    /// logical child group or a candidate's input group.
    pub fn rebuild_parents(&mut self) {
        for group in &mut self.groups {
            group.parents.clear();
        }
        for group in 0..self.groups.len() {
            let reader = MemoGroupId(group as u32);
            let read: HashSet<MemoGroupId> = self.groups_read_by(reader).collect();
            let mut read: Vec<MemoGroupId> = read.into_iter().collect();
            read.sort_unstable();
            for child in read {
                self.groups[child.0 as usize].parents.push(reader);
            }
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

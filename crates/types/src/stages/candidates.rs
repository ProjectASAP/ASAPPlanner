//! Implementation candidates attached to memo groups.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::rc::Rc;

use super::hash_consing::HashConsingTable;
use super::{MemoGroup, MemoGroupId};
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
    /// Every group, by its logical node: [`Self::group_of`] looks groups up
    /// in it.
    group_lookup: MemoGroupLookup,
}

/// Every group of a memo, by the address of its logical node and by node
/// equality ([`HashConsingTable`], Stage 0's equality).
#[derive(Debug, Clone, Default)]
struct MemoGroupLookup {
    /// Each group's logical node, by address (the memo keeps it alive).
    group_with_node: HashMap<*const OperatorNode, MemoGroupId>,
    /// Each group's logical node, by equality; a cell because looking a
    /// node up memoizes its structural hash.
    group_with_equal_node: RefCell<HashConsingTable<MemoGroupId>>,
}

/// What [`Memo::find_or_add_group`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GroupFoundOrAdded {
    /// An existing group's node equals the node.
    Found(MemoGroupId),
    /// No group's node equals the node: this group was added for it.
    Added(MemoGroupId),
}

impl GroupFoundOrAdded {
    /// The group found or added.
    pub fn group(self) -> MemoGroupId {
        match self {
            Self::Found(group) | Self::Added(group) => group,
        }
    }
}

/// The groups some inputs read through the groups added beneath the
/// workload's groups ([`Memo::groups_read_through_added_groups`]).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GroupsReadThroughAddedGroups {
    /// The added groups passed through, in the order first reached.
    pub added_groups: Vec<MemoGroupId>,
    /// The workload groups reached, in the order first reached.
    pub workload_groups: Vec<MemoGroupId>,
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
    /// A memo of `groups`, the queries' roots over them and each group's
    /// candidates, by group index.
    pub fn new(
        groups: Vec<MemoGroup>,
        query_roots: Vec<QueryRoot<MemoGroupId>>,
        candidates: Vec<Vec<CandidateForMemoGroup>>,
    ) -> Self {
        let mut memo = Self {
            groups: Vec::with_capacity(groups.len()),
            query_roots,
            candidates,
            group_lookup: MemoGroupLookup::default(),
        };
        for group in groups {
            memo.index_and_push(group);
        }
        memo
    }

    /// The groups, by id.
    pub fn groups(&self) -> &[MemoGroup] {
        &self.groups
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
            .group_with_node
            .entry(Rc::as_ptr(&node))
            .or_insert(id);
        let mut equal_nodes = self.group_lookup.group_with_equal_node.borrow_mut();
        if equal_nodes.find(&node).is_none() {
            equal_nodes.insert(node, id);
        }
        drop(equal_nodes);
        self.groups.push(group);
        id
    }

    /// The group whose logical node is `node` itself (the same `Rc`).
    pub fn group_with_node(&self, node: &Rc<OperatorNode>) -> Option<MemoGroupId> {
        self.group_lookup
            .group_with_node
            .get(&Rc::as_ptr(node))
            .copied()
    }

    /// The group [`Self::find_or_add_group`] finds for `node`: the group whose
    /// logical node is `node`, else the lowest group whose logical node
    /// equals `node` by Stage 0's equality when sharing `node` is legal
    /// ([`HashConsingTable::find`]).
    pub fn group_of(&self, node: &Rc<OperatorNode>) -> Option<MemoGroupId> {
        self.group_with_node(node).or_else(|| {
            self.group_lookup
                .group_with_equal_node
                .borrow_mut()
                .find(node)
        })
    }

    /// Returns the existing group whose node equals `node` ([`Self::group_of`],
    /// Stage 0's equality), after adding `queries` to it and to every group
    /// below it ([`Self::add_queries`]). Otherwise adds a group for `node`,
    /// and for each of its children the same way, and returns that group.
    /// It never copies a group. An added group's logical node is `node` over
    /// its children's groups' nodes, `queries` reach it, and its one
    /// candidate is that node over those groups, made by `origin` and sized
    /// for `accuracy_budget_parts`.
    pub fn find_or_add_group(
        &mut self,
        node: &Rc<OperatorNode>,
        queries: &[usize],
        origin: &OriginOfCandidateForMemoGroup,
        accuracy_budget_parts: usize,
    ) -> GroupFoundOrAdded {
        if let Some(group) = self.group_of(node) {
            self.add_queries(group, queries);
            return GroupFoundOrAdded::Found(group);
        }
        let mut group_of_child: HashMap<*const OperatorNode, MemoGroupId> = HashMap::new();
        let mut child_groups = Vec::new();
        for child in node.children() {
            let group = match group_of_child.get(&Rc::as_ptr(child)) {
                Some(&group) => group,
                None => {
                    let group = self
                        .find_or_add_group(child, queries, origin, accuracy_budget_parts)
                        .group();
                    group_of_child.insert(Rc::as_ptr(child), group);
                    group
                }
            };
            child_groups.push(group);
        }
        let group_node = |child: &Rc<OperatorNode>| {
            Rc::clone(&self.groups[group_of_child[&Rc::as_ptr(child)].0 as usize].logical_node)
        };
        let logical_node = if node
            .children()
            .into_iter()
            .all(|child| Rc::ptr_eq(child, &group_node(child)))
        {
            Rc::clone(node)
        } else {
            Rc::new(OperatorNode {
                operator: node.operator.map_children(group_node),
                ..(**node).clone()
            })
        };
        let mut queries = queries.to_vec();
        queries.sort_unstable();
        queries.dedup();
        let candidate = CandidateForMemoGroup {
            fused_groups: Vec::new(),
            root_node: Rc::clone(&logical_node),
            input_groups: child_groups.clone(),
            origin: origin.clone(),
            accuracy_budget_parts,
        };
        GroupFoundOrAdded::Added(self.push_group(
            MemoGroup {
                logical_node,
                child_groups,
                parents: Vec::new(),
                queries,
            },
            vec![candidate],
        ))
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

    /// The groups of the workload's logical DAG: the query roots' groups and
    /// every group below them through logical child groups. The groups
    /// added beneath them, which only candidates read, are not among them.
    pub fn workload_groups(&self) -> HashSet<MemoGroupId> {
        let mut pending: Vec<MemoGroupId> =
            super::root_groups(&self.query_roots).into_iter().collect();
        let mut reached = HashSet::new();
        while let Some(group) = pending.pop() {
            if reached.insert(group) {
                pending.extend(self.groups[group.0 as usize].child_groups.iter().copied());
            }
        }
        reached
    }

    /// The groups `inputs` read, walking through every group not in
    /// `workload_groups` (each read through all its candidates' input
    /// groups) and stopping at workload groups.
    pub fn groups_read_through_added_groups(
        &self,
        inputs: &[MemoGroupId],
        workload_groups: &HashSet<MemoGroupId>,
    ) -> GroupsReadThroughAddedGroups {
        let mut read = GroupsReadThroughAddedGroups::default();
        let mut pending: Vec<MemoGroupId> = inputs.iter().rev().copied().collect();
        let mut seen = HashSet::new();
        while let Some(group) = pending.pop() {
            if !seen.insert(group) {
                continue;
            }
            if workload_groups.contains(&group) {
                read.workload_groups.push(group);
                continue;
            }
            read.added_groups.push(group);
            let below: Vec<MemoGroupId> = self.candidates[group.0 as usize]
                .iter()
                .flat_map(|candidate| &candidate.input_groups)
                .copied()
                .collect();
            pending.extend(below.into_iter().rev());
        }
        read
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

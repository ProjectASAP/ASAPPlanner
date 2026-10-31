//! Implementation candidates attached to memo groups.

use std::collections::HashSet;
use std::rc::Rc;

use super::{MemoGroup, MemoGroupId};
use crate::ir::{OperatorNode, QueryRoot};

/// The hash-consed workload: its groups, children before parents.
#[derive(Debug, Clone, Default)]
pub struct MemoCandidates {
    pub groups: Vec<MemoGroup>,
    /// Each query's root over groups, by index in the workload (the
    /// initial logical memo's query roots); groups added later are never a
    /// query's root.
    pub query_roots: Vec<QueryRoot<MemoGroupId>>,
    /// Each group's implementation candidates, by group index.
    pub candidates: Vec<Vec<CandidateForMemoGroup>>,
}

/// One way to implement a group: its node as Stage 2 implements it, over
/// child groups.
#[derive(Debug, Clone)]
pub struct CandidateForMemoGroup {
    /// The groups a fused computation also implements, beyond the group
    /// whose candidate it is (its owner, implicit from its position in
    /// [`MemoCandidates::candidates`]); empty otherwise.
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
    /// Pass 1: exact, exact accumulator, or a summary family and size.
    Pass1,
    /// One build serving several reader groups (summary capability).
    SharedBuild,
    /// A summary readout factored from a composite implementation.
    SummaryResult,
    /// Canonical value preprocessing feeding a summary build.
    SummaryInput,
    /// Tumbling panes, or the newest panes of another group's chain.
    Panes,
    /// A coarse group regrouping a finer group.
    Rollup,
    /// A top-k read from another group's ranking.
    TopKReuse,
    /// Its metric's scans widened to a shared label set, read through a
    /// `Project` (scan widening).
    WidenedScans,
}

impl MemoCandidates {
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

//! Memo groups: the distinct logical sub-expressions of the workload.

use std::collections::{BTreeSet, HashMap};
use std::rc::Rc;

use crate::ir::{OperatorNode, QueryRoot};

/// Candidate generation stopped before its finite catalog was complete.
/// Callers must discard the partial catalog and cannot certify optimality from it.
#[derive(Debug, Clone, Copy, thiserror::Error)]
#[error("candidate generation deadline exceeded")]
pub struct CandidateGenerationTimeout;

/// A group, by index in the memo's groups.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct MemoGroupId(pub u32);

/// One distinct logical sub-expression of the workload.
#[derive(Debug, Clone)]
pub struct MemoGroup {
    /// Its logical node as the frontends (and Pass 1 targets) see it; its
    /// children are other groups' nodes.
    pub logical_node: Rc<OperatorNode>,
    /// Its child groups, in the node's child order.
    pub child_groups: Vec<MemoGroupId>,
    /// The groups that read it ([`Self::parent_groups`]). Public only so
    /// Stage 1, which builds and extends the memo, can maintain it; later
    /// stages read it through [`Self::parent_groups`].
    pub parents: Vec<MemoGroupId>,
    /// The queries whose roots reach it ([`Self::reached_by_queries`]).
    /// Public only so Stage 1, which builds and extends the memo, can
    /// maintain it; later stages read it through
    /// [`Self::reached_by_queries`].
    pub queries: Vec<usize>,
}

impl MemoGroup {
    /// The groups that read it.
    pub fn parent_groups(&self) -> &[MemoGroupId] {
        &self.parents
    }

    /// The queries whose roots reach it, by index in the workload, ascending.
    pub fn reached_by_queries(&self) -> &[usize] {
        &self.queries
    }
}

/// Stage 0's output, Pass 1's input: the workload's memo groups, one per
/// distinct logical sub-expression, without implementation choices or
/// solver state. Stage 1 builds it by hash-consing the query roots.
#[derive(Debug, Clone)]
pub struct DagOfLogicalMemoGroups {
    pub groups: Vec<MemoGroup>,
    /// Each query's root over groups, by index in the workload: its
    /// operator's group, or its scalar expression with every operator
    /// reference replaced by that operator's group.
    pub query_roots: Vec<QueryRoot<MemoGroupId>>,
    /// Each node of the original workload, by address, to its group.
    group_of: HashMap<*const OperatorNode, MemoGroupId>,
}

impl DagOfLogicalMemoGroups {
    /// The memo groups `groups` (children before parents), each query's
    /// root over them, and `group_of`: each node of the original workload,
    /// by address, to its group.
    pub fn new(
        groups: Vec<MemoGroup>,
        query_roots: Vec<QueryRoot<MemoGroupId>>,
        group_of: HashMap<*const OperatorNode, MemoGroupId>,
    ) -> Self {
        Self {
            groups,
            query_roots,
            group_of,
        }
    }

    /// Group of a node from the original workload, not a rewritten candidate.
    pub fn group_for_node(&self, node: &Rc<OperatorNode>) -> Option<MemoGroupId> {
        self.group_of.get(&Rc::as_ptr(node)).copied()
    }

    /// Each query's root over the groups' logical nodes, in workload order:
    /// a group several queries read is one node.
    pub fn roots_over_groups(&self) -> Vec<QueryRoot> {
        let node = |g: &MemoGroupId| Rc::clone(&self.groups[g.0 as usize].logical_node);
        self.query_roots
            .iter()
            .map(|root| match root {
                QueryRoot::Operator(g) => QueryRoot::Operator(node(g)),
                QueryRoot::Scalar(expr) => {
                    QueryRoot::Scalar(expr.map_operator_refs(&mut |g| node(g)))
                }
            })
            .collect()
    }
}

/// The groups some query's root reads, ascending: the groups of
/// `query_roots` that are a query's result or a scalar query's operand.
pub fn root_groups(query_roots: &[QueryRoot<MemoGroupId>]) -> BTreeSet<MemoGroupId> {
    query_roots
        .iter()
        .flat_map(QueryRoot::operator_refs)
        .copied()
        .collect()
}

/// The queries whose root reads `group`, ascending.
pub fn queries_rooted_at(query_roots: &[QueryRoot<MemoGroupId>], group: MemoGroupId) -> Vec<usize> {
    (0..query_roots.len())
        .filter(|&query| query_roots[query].operator_refs().contains(&&group))
        .collect()
}

//! Memo groups: the distinct logical sub-expressions of the workload, each
//! with its candidates.

use std::collections::BTreeSet;
use std::rc::Rc;

use super::CandidateForMemoGroup;
use crate::ir::{OperatorNode, QueryRoot};

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
    /// Its candidates: empty after Stage 0, filled by Stage 1.
    pub candidates: Vec<CandidateForMemoGroup>,
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

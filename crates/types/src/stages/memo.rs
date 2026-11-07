//! Memo groups: the distinct logical sub-expressions of the workload, each
//! with its candidates.

use std::collections::BTreeSet;

use super::{CandidateForMemoGroup, Memo};
use crate::ir::operator::input::OperatorInput;
use crate::ir::properties::summary_coverage::SummaryCoverage;
use crate::ir::schema::Schema;
use crate::ir::{Operator, OperatorResultKind, QueryRoot};

/// A group, by index in the memo's groups.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct MemoGroupId(pub u32);

/// One distinct logical sub-expression of the workload: what it computes
/// and the output every candidate of it must produce.
#[derive(Debug, Clone)]
pub struct MemoGroup {
    /// What it computes: one operator, with its arguments. Each
    /// `MemoGroupId` in it is an input group, never this group's own id. θ
    /// is no argument of an operator, so it is not part of a group.
    pub expression: Operator<MemoGroupId>,
    /// Derived once when the group is created, from its input groups; never
    /// changes.
    pub group_properties: GroupProperties,
    /// The groups that read it ([`Self::parent_groups`]) through their
    /// expression or a candidate's.
    pub parents: Vec<MemoGroupId>,
    /// The queries whose roots reach it ([`Self::reached_by_queries`]),
    /// ascending.
    pub queries: Vec<usize>,
    /// Its candidates: empty after Stage 0, filled by Stage 1.
    pub candidates: Vec<CandidateForMemoGroup>,
}

/// What every candidate of a group produces, derived from the group's
/// expression and its input groups' properties by the rules that check
/// operator nodes.
#[derive(Debug, Clone)]
pub struct GroupProperties {
    pub schema: Schema,
    pub result_kind: OperatorResultKind,
    /// What an aggregate group covers: a summary state, a chain, a merge
    /// over a chain's windows, or an exact `Aggregate` result; `None` for
    /// any other group. Derived today for summary states only; exact
    /// `Aggregate` groups follow in #1207 PR 2.
    pub aggregate_coverage: Option<SummaryCoverage>,
    /// Whether two equal expressions return the same rows for the same
    /// input rows, so they may be one group (not an unordered `LIMIT`).
    pub shareable_when_equal: bool,
}

/// A memo group read as an operator's input: its `group_properties` and
/// its `expression`, whose inputs are read the same way.
#[derive(Clone, Copy)]
pub(crate) struct MemoGroupAsInput<'m> {
    pub(crate) memo: &'m Memo,
    pub(crate) group_id: MemoGroupId,
}

impl<'m> MemoGroupAsInput<'m> {
    fn group(&self) -> &'m MemoGroup {
        &self.memo.groups()[self.group_id.0 as usize]
    }
}

impl OperatorInput for MemoGroupAsInput<'_> {
    fn schema(&self) -> &Schema {
        &self.group().group_properties.schema
    }

    fn result_kind(&self) -> OperatorResultKind {
        self.group().group_properties.result_kind
    }

    fn operator(&self) -> Operator<Self> {
        self.group().expression.map_children(|&group_id| Self {
            memo: self.memo,
            group_id,
        })
    }

    fn aggregate_coverage(&self) -> Option<&SummaryCoverage> {
        self.group().group_properties.aggregate_coverage.as_ref()
    }
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

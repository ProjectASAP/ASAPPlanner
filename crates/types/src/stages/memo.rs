//! Memo groups: the distinct logical sub-expressions of the workload, each
//! with its candidates.

use std::collections::BTreeSet;

use super::{CandidateForMemoGroup, Memo, MemoQuery};
use crate::ir::operator::input::OperatorInput;
use crate::ir::operator::operator_properties::SourceColumn;
use crate::ir::properties::summary_coverage::SummaryCoverage;
use crate::ir::schema::Schema;
use crate::ir::{Operator, OperatorResultKind};

/// A group, by index in the memo's groups.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, serde::Serialize)]
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
    /// expression or a candidate's: updated by
    /// [`Memo::find_or_add_group`](super::Memo::find_or_add_group) and
    /// [`Memo::add_candidate`](super::Memo::add_candidate).
    pub parents: Vec<MemoGroupId>,
    /// The queries whose roots reach it ([`Self::reached_by_queries`]),
    /// ascending: updated by
    /// [`Memo::add_query`](super::Memo::add_query) and
    /// [`Memo::add_candidate`](super::Memo::add_candidate).
    pub queries: Vec<usize>,
    /// Its candidates: empty after Stage 0, filled by Stage 1.
    pub candidates: Vec<CandidateForMemoGroup>,
}

/// What every candidate of a group produces, derived from the group's
/// expression and its input groups' properties by the rules that check
/// operator nodes.
#[derive(Debug, Clone)]
pub struct GroupProperties {
    /// Fields: name, type, nullability; no table qualifier.
    pub schema: Schema,
    pub result_kind: OperatorResultKind,
    /// Per output column, in schema order: the source column its values
    /// are copied from, or `Computed`.
    pub column_lineage: Vec<ColumnLineage>,
    /// Per output column, in schema order: the sign of its values, from the
    /// input groups' and, for a `Scan`, the declared ones.
    pub column_sign: Vec<ValueSign>,
    /// Whether the result changes with the evaluation time: true for a
    /// group under a `TimeRange` relative to the evaluation time
    /// (`sum_over_time(m[5m])`) or reading it (`time()`, `now()`), false
    /// for one under a fixed `@ T` or reading no time. A result that does
    /// not depend on it is the same at every evaluation, so a stored copy
    /// delivers on every grid.
    pub depends_on_evaluation_time: bool,
    /// What an aggregate group covers: a summary state, a chain, a
    /// `TimeRange`/`TimeShift` over a chain or a merge over one, or an exact
    /// `Aggregate` result; `None` for any other group.
    pub aggregate_coverage: Option<SummaryCoverage>,
    /// Whether two equal expressions return the same rows for the same
    /// input rows, so they may be one group (not an unordered `LIMIT`).
    pub shareable_when_equal: bool,
    /// Whether every `Scan` below reads a time series (PromQL rows): a
    /// `Scan` of a `Source::TimeSeries`, or any other operator all of whose
    /// input groups do. A PromQL top-k ranks one row per series.
    pub reads_only_time_series: bool,
}

impl GroupProperties {
    /// The `column_sign` of a state group's state column (its first field
    /// that is not plain): the sign of the values its states were updated
    /// with. `None` for a group without a state column.
    pub fn state_column_sign(&self) -> Option<ValueSign> {
        let state = self
            .schema
            .fields
            .iter()
            .position(|field| !field.is_plain())?;
        self.column_sign.get(state).copied()
    }
}

/// Where an output column's values come from, a group property derived
/// from the input groups (concepts doc, section 3, "Column lineage"). It
/// names a column across groups where a schema field's name cannot: two
/// join sides' columns of one name have different lineage, and a renamed
/// column keeps its lineage.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ColumnLineage {
    /// Copied unchanged from a column of a source.
    SourceColumn(SourceColumn),
    /// Computed by an operator: a measure, an expression, a summary state or readout.
    Computed,
}

/// The sign of one column's values, a group property derived from the
/// input groups like lineage (concepts doc, section 3, "Value sign"); a
/// `Scan`'s are declared per column in the data workload's source facts.
/// Error contracts that hold only for values of one sign read it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ValueSign {
    NonNegative,
    NonPositive,
    Unknown,
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

    pub(crate) fn group_properties(&self) -> &'m GroupProperties {
        &self.group().group_properties
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

    fn reads_only_time_series(&self) -> bool {
        self.group().group_properties.reads_only_time_series
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

/// The groups some query's root reads, ascending: the groups of the
/// `queries`' roots that are a query's result or a scalar query's operand.
pub fn root_groups(queries: &[MemoQuery]) -> BTreeSet<MemoGroupId> {
    queries
        .iter()
        .flat_map(|query| query.root.operator_refs())
        .copied()
        .collect()
}

/// The queries whose root reads `group`, ascending.
pub fn queries_rooted_at(queries: &[MemoQuery], group: MemoGroupId) -> Vec<usize> {
    (0..queries.len())
        .filter(|&query| queries[query].root.operator_refs().contains(&&group))
        .collect()
}

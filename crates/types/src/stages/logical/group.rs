//! Group scope: memo groups, the distinct logical sub-expressions of the
//! workload, and the properties every candidate of a group shares.

use crate::ir::operator::input::OperatorInput;
use crate::ir::properties::summary_coverage::SummaryCoverage;
use crate::ir::schema::Schema;
use crate::ir::ASAPOp;
use crate::ir::SchemaDerivationError;
use crate::ir::{Operator, OperatorResultKind};
use crate::stages::{CandidateForMemoGroup, Memo};

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
    /// expression or a candidate's.
    pub parents: Vec<MemoGroupId>,
    /// The queries whose roots reach it ([`Self::reached_by_queries`]),
    /// ascending.
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

/// What every candidate of a group produces, derived from the group's
/// expression and its input groups' properties by the rules that check
/// operator nodes.
#[derive(Debug, Clone)]
pub struct GroupProperties {
    pub schema: Schema,
    pub result_kind: OperatorResultKind,
    /// Per output column, in schema order: the sign of its values.
    pub column_sign: Vec<ValueSign>,
    /// Whether the result changes with the evaluation time: true for a
    /// group under a `TimeRange` relative to the evaluation time
    /// (`sum_over_time(m[5m])`), false for one under a fixed `@ T` or
    /// reading no time. A result that does not depend on it is the same at
    /// every evaluation, so a stored copy delivers on every grid.
    pub depends_on_evaluation_time: bool,
    /// What an aggregate group covers: a summary state, a chain, a merge
    /// over a chain's windows, or an exact `Aggregate` result; `None` for
    /// any other group. Derived today for summary states only; exact
    /// `Aggregate` groups follow in #1207 PR 2.
    pub aggregate_coverage: Option<SummaryCoverage>,
    /// Whether two equal expressions return the same rows for the same
    /// input rows, so they may be one group (not an unordered `LIMIT`).
    pub shareable_when_equal: bool,
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

/// The properties of a group, or a candidate, computing `expression` over
/// its input groups: the rules that check operator nodes, reading each
/// input group's `group_properties` and `expression`
/// ([`MemoGroupAsInput`]), never a node tree. Fails when the expression
/// breaks them (schema derivation, input validation, merge check).
// `column_sign` and `depends_on_evaluation_time` are derived in #1207 PR 2;
// until then the result is unreachable.
#[allow(unreachable_code)]
pub(super) fn derive_group_properties(
    memo: &Memo,
    expression: &Operator<MemoGroupId>,
) -> Result<GroupProperties, SchemaDerivationError> {
    let operator = expression.map_children(|&group_id| MemoGroupAsInput { memo, group_id });
    let schema = operator.output_schema()?;
    operator.validate_inputs()?;
    let aggregate_coverage = match SummaryCoverage::derive_from_inputs(&operator) {
        Ok(coverage) => Some(coverage),
        // A merge is valid only over states of one definition it may merge.
        Err(error) if matches!(operator, Operator::ASAP(ASAPOp::SummaryMerge { .. })) => {
            return Err(error.into())
        }
        Err(_) => None,
    };
    Ok(GroupProperties {
        schema,
        result_kind: operator.output_kind(),
        aggregate_coverage,
        shareable_when_equal: operator.is_shareable_when_equal(),
        column_sign: todo!("#1207 PR 2: each output column's sign, per operator, from the input groups' and the sources' declared signs"),
        depends_on_evaluation_time: todo!("#1207 PR 2: whether the result changes with the evaluation time, from the operator and the input groups"),
    })
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

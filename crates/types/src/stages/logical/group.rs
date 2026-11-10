//! Group scope: memo groups, the distinct logical sub-expressions of the
//! workload, and the properties every candidate of a group shares.

use std::collections::BTreeSet;

use super::aggregate_coverage::{aggregate_coverage, AggregateCoverage, CoverageError};
use super::column_properties::{column_signs, ValueSign};
use crate::ir::operator::arguments::{AtModifier, Source, TimeShift};
use crate::ir::operator::input::OperatorInput;
use crate::ir::schema::Schema;
use crate::ir::ASAPOp;
use crate::ir::SchemaDerivationError;
use crate::ir::{NonASAPOp, ScalarExpr};
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
    /// expression or a candidate's: kept up to date by
    /// [`Memo::find_or_add_group`](super::Memo::find_or_add_group) and
    /// [`Memo::add_candidate`](super::Memo::add_candidate).
    pub(super) parents: Vec<MemoGroupId>,
    /// The queries whose roots reach it ([`Self::reached_by_queries`]),
    /// ascending: kept up to date by
    /// [`Memo::add_query`](super::Memo::add_query) and
    /// [`Memo::add_candidate`](super::Memo::add_candidate).
    pub(super) queries: Vec<usize>,
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
    /// Per output column, in schema order: the sign of its values, from the
    /// input groups' and, for a `Scan`, the declared ones.
    pub column_signs: Vec<ValueSign>,
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
    pub aggregate_coverage: Option<AggregateCoverage>,
    /// Whether two equal expressions return the same rows for the same
    /// input rows, so they may be one group (not an unordered `LIMIT`).
    pub shareable_when_equal: bool,
    /// The sources the `Scan`s below this group read: a `Scan`'s own, else
    /// the union over its input groups.
    pub scanned_sources: BTreeSet<Source>,
}

/// Why an expression is not valid over its input groups: what
/// [`Memo::find_or_add_group`] and, wrapped in
/// [`CandidateError::NotValidOverInputGroups`](crate::stages::CandidateError::NotValidOverInputGroups),
/// [`Memo::add_candidate`] return. The memo is unchanged.
#[derive(Debug, thiserror::Error)]
pub enum MemoGroupError {
    /// Its output schema does not derive, or an input fails validation.
    #[error(transparent)]
    Schema(#[from] SchemaDerivationError),
    /// A merge reads states it may not merge (different definitions, or
    /// selections not proven to relate as the family's merge requires).
    #[error("invalid aggregate coverage: {0}")]
    Coverage(#[from] CoverageError),
}

/// Whether `operator` is a valid expression over its input groups
/// ([`MemoGroupAsInput`]), never a node tree: its output schema derives, its
/// inputs pass validation, and a merge reads only states it may merge.
/// Returns the output schema and, for a merge, the coverage its check
/// derived.
pub(super) fn validate_expression_over_input_groups(
    operator: &Operator<MemoGroupAsInput<'_>>,
) -> Result<(Schema, Option<AggregateCoverage>), MemoGroupError> {
    let schema = operator.output_schema()?;
    operator.validate_inputs()?;
    // A merge is valid only over states of one definition it may merge.
    let merge_coverage = match operator {
        Operator::ASAP(ASAPOp::SummaryMerge { .. }) => Some(aggregate_coverage(operator)?),
        _ => None,
    };
    Ok((schema, merge_coverage))
}

/// The properties of a group computing `expression` over its input
/// groups, once it is valid over them
/// ([`validate_expression_over_input_groups`]), reading each input group's
/// `group_properties` and `expression`.
pub(super) fn derive_group_properties(
    memo: &Memo,
    expression: &Operator<MemoGroupId>,
) -> Result<GroupProperties, MemoGroupError> {
    let operator = expression.map_children(|&group_id| MemoGroupAsInput { memo, group_id });
    let (schema, merge_coverage) = validate_expression_over_input_groups(&operator)?;
    let aggregate_coverage = merge_coverage.or_else(|| aggregate_coverage(&operator).ok());
    Ok(GroupProperties {
        column_signs: column_signs(&operator, &schema, &memo.source_facts),
        depends_on_evaluation_time: depends_on_evaluation_time(&operator),
        schema,
        result_kind: operator.output_kind(),
        aggregate_coverage,
        shareable_when_equal: operator.is_shareable_when_equal(),
        scanned_sources: operator.scanned_sources(),
    })
}

/// Whether `operator`'s result changes with the evaluation time: a
/// `TimeRange` reads the window before it, and `time()` or `now()` read it
/// directly; `@ T` pins its subtree to T. Any other operator depends on it
/// when an input group does.
fn depends_on_evaluation_time(operator: &Operator<MemoGroupAsInput<'_>>) -> bool {
    fn reads_evaluation_time<C>(expr: &ScalarExpr<C>) -> bool {
        matches!(
            expr,
            ScalarExpr::EvalTimestamp | ScalarExpr::CurrentTimestamp
        ) || expr.children().into_iter().any(reads_evaluation_time)
    }
    let reads_it = match operator {
        Operator::NonASAP(NonASAPOp::TimeRange { .. }) => return true,
        Operator::NonASAP(NonASAPOp::TimeShift {
            shift:
                TimeShift {
                    at: Some(AtModifier::Timestamp(_)),
                    ..
                },
            ..
        }) => return false,
        Operator::NonASAP(op) => op.scalar_exprs().into_iter().any(reads_evaluation_time),
        Operator::ASAP(
            ASAPOp::SummaryAgg { filter, .. } | ASAPOp::SummaryTumblingWindowChain { filter, .. },
        ) => filter.iter().any(|pred| reads_evaluation_time(&pred.0)),
        Operator::ASAP(_) => false,
    };
    reads_it
        || operator
            .children()
            .into_iter()
            .any(|input| input.group_properties().depends_on_evaluation_time)
}

/// A memo group read as an operator's input: its `group_properties` and
/// its `expression`, whose inputs are read the same way.
#[derive(Clone, Copy)]
pub(super) struct MemoGroupAsInput<'m> {
    pub(super) memo: &'m Memo,
    pub(super) group_id: MemoGroupId,
}

impl<'m> MemoGroupAsInput<'m> {
    fn group(&self) -> &'m MemoGroup {
        &self.memo.groups()[self.group_id.0 as usize]
    }

    pub(super) fn group_properties(&self) -> &'m GroupProperties {
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

    fn scanned_sources(&self) -> BTreeSet<Source> {
        self.group().group_properties.scanned_sources.clone()
    }
}

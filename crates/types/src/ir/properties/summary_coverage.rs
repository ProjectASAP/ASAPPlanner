//! What a summary state covers: `definition` (what it computes) and
//! `selection` (which output rows of that computation it took). Design:
//! `docs/design_docs/proposals/asap-primitive-schema.md` §4.2, after
//! Goldstein & Larson's view matching.
//!
//! Coverage is derived once for each aggregate memo group (a summary state
//! or an exact `Aggregate`), from its input groups, never declared, and
//! stored in its `GroupProperties::aggregate_coverage`. Walking down from a
//! `SummaryAgg` or `Aggregate` through the input groups' expressions, a
//! predicate conjunct moves into `selection` when it can be lifted to the
//! top (through `Filter`, a range `TimeRange`, a `TimeShift` and
//! direct-column `Project` items) and it is a value set or an interval on
//! one column. The `TimeRange` and `TimeShift`s become one interval on the
//! timestamp column ([`IntervalConstraintLimit`]). Everything else stays in
//! `definition` as a residual, so two states are merged only when they
//! compute the same thing over disjoint rows. Node trees carry no coverage.
use std::cmp::Ordering;
use std::collections::BTreeMap;
use std::ops::Bound;

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::ir::operator::asap::{selected_from, summary_agg_schema, ASAPOp};
use crate::ir::operator::input::{InputOrTemporaryBuild, OperatorInput};
use crate::ir::operator::node::{Operator, OperatorResultKind};
use crate::ir::operator::non_asap::{NonASAPOp, TimeRangeKind};
use crate::ir::operator::operator_properties::{AtModifier, Reduction};
use crate::ir::scalar::expr_ir::{CompareOpKind, ScalarValue};
use crate::ir::scalar::{Predicate, ScalarExpr};
use crate::ir::schema::family_capability::{FamilyCapability, MergeInputRule};
use crate::ir::schema::sketch::GroupingStrategy;
use crate::ir::schema::{ColumnId, Schema};
use crate::ir::SchemaDerivationError;
use crate::stages::{MemoGroupAsInput, MemoGroupId};

/// What a summary state computes, with every lifted selection removed.
#[derive(Debug, Clone, PartialEq)]
pub struct SummaryStateDefinition {
    /// The group the unlifted operators read: the first input under the
    /// SummaryAgg that is not Filter / range TimeRange / TimeShift without @
    /// / direct-column Project. `None` when that input is a `Scan` with a
    /// liftable predicate (a PromQL label matcher): it is lifted too, and the
    /// `Scan` with the rest of its predicates is the last unlifted operator,
    /// so states over one source with different matchers may merge.
    pub input_group: Option<MemoGroupId>,
    /// The operators not lifted into the selection, listed top down from the
    /// SummaryAgg, each with only its residual arguments. `None` marks an
    /// operator's main input (the next unlifted operator, or input_group);
    /// `Some(g)` is another input group it reads, such as the group a scalar
    /// subquery in a Filter reads. Two states may merge only if input_group
    /// and these operators are equal.
    pub unlifted_operators: Vec<Operator<Option<MemoGroupId>>>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct SummaryCoverage {
    pub(crate) definition: SummaryStateDefinition,
    /// Union of boxes over the output rows of the unlifted operators' input.
    pub(crate) selection: Vec<SelectionBox>,
}

#[cfg(any(test, feature = "test-support"))]
impl SummaryCoverage {
    pub fn definition(&self) -> &SummaryStateDefinition {
        &self.definition
    }

    /// Union of boxes over the output rows of the unlifted operators' input.
    pub fn selection(&self) -> &[SelectionBox] {
        &self.selection
    }
}

/// A conjunction of per-column constraints. The timestamp column is one of
/// the columns; there is no separate time field. A column it does not
/// mention is unrestricted. JSON: `columns` is a list of
/// `{column, constraint}` (its keys are not strings), and a bound is
/// `{"included": v}`, `{"excluded": v}` or `"unbounded"`.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SelectionBox {
    #[serde(with = "columns_as_list")]
    pub(crate) columns: BTreeMap<ColumnIdentity, Constraint>,
}

/// `SelectionBox::columns` as a list of `{column, constraint}`.
mod columns_as_list {
    use super::{ColumnIdentity, Constraint};
    use serde::{Deserialize, Deserializer, Serialize, Serializer};
    use std::collections::BTreeMap;

    #[derive(Serialize, Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Entry {
        column: ColumnIdentity,
        constraint: Constraint,
    }

    pub(super) fn serialize<S: Serializer>(
        columns: &BTreeMap<ColumnIdentity, Constraint>,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        let entries: Vec<Entry> = columns
            .iter()
            .map(|(column, constraint)| Entry {
                column: column.clone(),
                constraint: constraint.clone(),
            })
            .collect();
        entries.serialize(serializer)
    }

    pub(super) fn deserialize<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<BTreeMap<ColumnIdentity, Constraint>, D::Error> {
        let entries = Vec::<Entry>::deserialize(deserializer)?;
        Ok(entries
            .into_iter()
            .map(|entry| (entry.column, entry.constraint))
            .collect())
    }
}

/// A `Bound` in JSON: `{"included": v}`, `{"excluded": v}` or `"unbounded"`.
#[derive(Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum WireBound<T> {
    Included(T),
    Excluded(T),
    Unbounded,
}

impl<T: Clone> From<&Bound<T>> for WireBound<T> {
    fn from(bound: &Bound<T>) -> Self {
        match bound {
            Bound::Included(v) => Self::Included(v.clone()),
            Bound::Excluded(v) => Self::Excluded(v.clone()),
            Bound::Unbounded => Self::Unbounded,
        }
    }
}

impl<T> From<WireBound<T>> for Bound<T> {
    fn from(bound: WireBound<T>) -> Self {
        match bound {
            WireBound::Included(v) => Bound::Included(v),
            WireBound::Excluded(v) => Bound::Excluded(v),
            WireBound::Unbounded => Bound::Unbounded,
        }
    }
}

/// A column of the definition child's output, by `(table, name)`. A
/// `Project` below renames its columns and sets their table to its
/// qualifier (none by default).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ColumnIdentity {
    pub(crate) table: Option<String>,
    pub(crate) name: String,
}

#[cfg(any(test, feature = "test-support"))]
impl ColumnIdentity {
    pub fn new(table: Option<String>, name: impl Into<String>) -> Self {
        Self {
            table,
            name: name.into(),
        }
    }
}

/// A constraint on one column of a selection.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Constraint {
    /// The value is one of these.
    In(Vec<ScalarValue>),
    /// The value is none of these.
    NotIn(Vec<ScalarValue>),
    /// The value lies between `lower` and `upper`; each end is `Included`,
    /// `Excluded` or `Unbounded`. Examples, on the timestamp column `ts`
    /// (ms) unless stated:
    ///
    /// | Query fragment | `lower` | `upper` |
    /// |---|---|---|
    /// | PromQL `m[5m]` | `Included(EvaluationTimePlusMs(-300_000))` | `Excluded(EvaluationTimePlusMs(0))` |
    /// | PromQL `m[5m] offset 1h` | `Included(EvaluationTimePlusMs(-3_900_000))` | `Excluded(EvaluationTimePlusMs(-3_600_000))` |
    /// | PromQL `m[5m] @ 1700000000` | `Included(FixedValue(Timestamp(1_699_999_700_000)))` | `Excluded(FixedValue(Timestamp(1_700_000_000_000)))` |
    /// | SQL `WHERE ts >= '2026-11-01'` | `Included(FixedValue(Timestamp(..)))` | `Unbounded` |
    /// | SQL `WHERE price > 10` (column `price`) | `Excluded(FixedValue(Float64(10.0)))` | `Unbounded` |
    Interval {
        #[serde(with = "limit_bound")]
        lower: Bound<IntervalConstraintLimit>,
        #[serde(with = "limit_bound")]
        upper: Bound<IntervalConstraintLimit>,
    },
}

/// An end of a [`Constraint::Interval`]. Two limits compare only when they
/// are of the same variant: a fixed value against a fixed value of the same
/// type, an offset from the evaluation time against another such offset.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IntervalConstraintLimit {
    /// A constant: `Float64(10.0)` for SQL `price > 10`;
    /// `Timestamp(1_700_000_000_000)` for PromQL `@ 1700000000` or a SQL
    /// timestamp literal.
    FixedValue(ScalarValue),
    /// The evaluation time t of a repeatedly evaluated query plus this many
    /// ms (negative: before t); only on the timestamp column. `m[5m]` is
    /// `[EvaluationTimePlusMs(-300_000), EvaluationTimePlusMs(0))`.
    EvaluationTimePlusMs(i64),
}

/// A `Constraint::Interval` bound as a [`WireBound`].
mod limit_bound {
    use super::{IntervalConstraintLimit, WireBound};
    use serde::{Deserialize, Deserializer, Serialize, Serializer};
    use std::ops::Bound;

    pub(super) fn serialize<S: Serializer>(
        bound: &Bound<IntervalConstraintLimit>,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        WireBound::from(bound).serialize(serializer)
    }

    pub(super) fn deserialize<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<Bound<IntervalConstraintLimit>, D::Error> {
        Ok(WireBound::deserialize(deserializer)?.into())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum CoverageError {
    /// The node has no coverage: it is no aggregate (`SummaryAgg`,
    /// `Aggregate`, `SummaryTumblingWindowChain`, `SummaryMerge`), or a
    /// `TimeRange`/`TimeShift` over a chain whose windows are not one
    /// interval on the timestamp column. A caller bug, or bad input when it
    /// is a merge's input.
    #[error("coverage is derived only for aggregates and one time interval over a chain")]
    NotSummary,
    /// A `SummaryMerge` without inputs: bad input.
    #[error("summary merge requires at least one input")]
    EmptyMerge,
    /// The inputs of a `SummaryMerge` compute different things: bad input.
    #[error("summary merge inputs compute different things")]
    DefinitionMismatch,
    /// The inputs of a `SummaryMerge` are not proven to relate as the
    /// family's merge requires (disjoint, or disjoint in time for counter
    /// states): bad input.
    #[error(
        "summary merge inputs are not proven {}, which the family's merge requires",
        match required {
            MergeInputRule::DisjointInTime => "disjoint in time",
            _ => "disjoint",
        }
    )]
    InputsOverlap { required: MergeInputRule },
    /// A regrouping `SummaryMerge` key that is not one of the group keys
    /// its input states are built by (a build grouped `without` keys has
    /// none): bad input.
    #[error("regrouping merge key {key} is not a group key of its input states")]
    NotAGroupKey { key: ColumnId },
    /// A regrouping `SummaryMerge` of states whose family does not merge
    /// different groups: counter states (`Rate`, `Increase`) merge
    /// consecutive windows of one series, a family without a merge merges
    /// nothing, and a Hydra's groups share one state. Bad input.
    #[error("the inputs' family cannot merge different groups")]
    FamilyCannotRegroup,
    /// The build a regrouping `SummaryMerge` stands for does not derive:
    /// bad input.
    #[error("the regrouped build does not derive: {reason}")]
    RegroupedBuild { reason: String },
}

/// A memo group read as an input, and the inputs a rule builds over such
/// groups.
type GroupInput<'m> = InputOrTemporaryBuild<MemoGroupAsInput<'m>>;

impl SummaryCoverage {
    /// Coverage of what a memo group computes with `operator` over its
    /// input groups: a `SummaryAgg`, an exact `Aggregate`, a
    /// `SummaryTumblingWindowChain` (a build over its input, the timestamp
    /// column unconstrained), a `TimeRange` or `TimeShift` over a chain (the
    /// chain's coverage with the windows' interval on the timestamp column),
    /// a `SummaryMerge`, or a `Filter` that keeps whole states of a grouped
    /// `SummaryAgg`. So `SummaryMerge(TimeRange(W, TimeShift(o, Chain[w](X))))`
    /// covers what `SummaryAgg(TimeRange(W, TimeShift(o, X)))` does. A merge
    /// reads its inputs' stored coverage and fails
    /// unless every input has the same definition and their selections are
    /// pairwise disjoint; a family whose merge is idempotent
    /// ([`MergeInputRule::MayOverlap`] in the family-capability table) may
    /// also merge overlapping selections, and a counter family
    /// ([`MergeInputRule::DisjointInTime`]) needs disjoint time windows.
    pub(crate) fn derive_from_inputs<'m>(
        operator: &Operator<MemoGroupAsInput<'m>>,
    ) -> Result<Self, CoverageError> {
        let as_built = |op: &ASAPOp<MemoGroupAsInput<'m>>| -> ASAPOp<GroupInput<'m>> {
            op.map_children(|input| InputOrTemporaryBuild::Input(*input))
        };
        match operator {
            Operator::NonASAP(NonASAPOp::Aggregate { child, .. }) => {
                let top =
                    unlifted(&operator.map_children(|input| InputOrTemporaryBuild::Input(*input)));
                of_aggregate(&InputOrTemporaryBuild::Input(*child), None, |_| top)
            }
            Operator::NonASAP(NonASAPOp::TimeRange { .. } | NonASAPOp::TimeShift { .. })
                if operator.output_kind() == OperatorResultKind::State =>
            {
                of_window_selection(operator)
            }
            Operator::NonASAP(_) => match selected_build(operator)? {
                Some(build) => of_summary_agg(&build),
                None => Err(CoverageError::NotSummary),
            },
            Operator::ASAP(op @ ASAPOp::SummaryAgg { .. }) => of_summary_agg(&as_built(op)),
            Operator::ASAP(op @ ASAPOp::SummaryTumblingWindowChain { .. }) => {
                let ASAPOp::SummaryTumblingWindowChain {
                    child,
                    family,
                    input,
                    reduction,
                    grouping,
                    filter,
                    ..
                } = as_built(op)
                else {
                    unreachable!("matched a chain");
                };
                of_summary_agg(&ASAPOp::SummaryAgg {
                    child,
                    family,
                    input,
                    reduction,
                    grouping,
                    filter,
                })
            }
            Operator::ASAP(ASAPOp::SummaryMerge {
                children, group_by, ..
            }) => {
                let inputs = children
                    .iter()
                    .map(|child| {
                        child
                            .aggregate_coverage()
                            .cloned()
                            .ok_or(CoverageError::NotSummary)
                    })
                    .collect::<Result<Vec<_>, _>>()?;
                let merged = of_merge(&inputs, time_column_of_build_input(&children[0]).as_ref())?;
                match group_by {
                    None => Ok(merged),
                    Some(keys) => regrouped(merged, &children[0], keys),
                }
            }
            Operator::ASAP(_) => Err(CoverageError::NotSummary),
        }
    }

    /// A stable identity of `definition`: the 128-bit FNV-1a hash, in hex,
    /// of its input group and unlifted operators. Two equal definitions
    /// (the equality a merge requires) of one memo get equal ids, so a
    /// summary store can key states by this id and a concrete selection.
    pub(crate) fn definition_id(&self) -> String {
        const OFFSET: u128 = 0x6c62272e07bb014262b821756295c58d;
        const PRIME: u128 = 0x0000000001000000000000000000013b;
        let mut hash = OFFSET;
        for byte in format!("{:?}", self.definition).bytes() {
            hash ^= u128::from(byte);
            hash = hash.wrapping_mul(PRIME);
        }
        format!("{hash:032x}")
    }
}

/// For a `Filter` that keeps whole states of a grouped `SummaryAgg` by
/// their key columns, the build that computes exactly the kept states: the
/// same build over its input rows filtered by the same predicate on the
/// input columns those keys name. `None` for any other operator.
fn selected_build<'m>(
    operator: &Operator<MemoGroupAsInput<'m>>,
) -> Result<Option<ASAPOp<GroupInput<'m>>>, CoverageError> {
    let Operator::NonASAP(NonASAPOp::Filter {
        pred,
        child: states,
    }) = operator
    else {
        return Ok(None);
    };
    if operator.output_kind() != OperatorResultKind::State {
        return Ok(None);
    }
    let Operator::ASAP(ASAPOp::SummaryAgg {
        child,
        family,
        input,
        reduction: Reduction::Reduce(by),
        grouping,
        filter,
    }) = states.operator()
    else {
        return Err(CoverageError::NotSummary);
    };
    if by.is_without() {
        return Err(CoverageError::NotSummary);
    }
    // Each state key column is the input column of the same name it groups by.
    let mut unmapped = None;
    let rows_pred = pred.0.map_columns(&mut |column| {
        let mapped = by.keys().get(column).copied().filter(|&input_column| {
            matches!((states.schema().fields.get(column), child.schema().fields.get(input_column)),
                (Some(at_state), Some(at_input)) if at_state.name == at_input.name)
        });
        mapped.unwrap_or_else(|| {
            unmapped = Some(column);
            column
        })
    });
    if let Some(key) = unmapped {
        return Err(CoverageError::NotAGroupKey { key });
    }
    let mut built = |input: &MemoGroupAsInput<'m>| InputOrTemporaryBuild::Input(*input);
    let regrouped_build = |error: SchemaDerivationError| CoverageError::RegroupedBuild {
        reason: error.to_string(),
    };
    let rows = InputOrTemporaryBuild::build(NonASAPOp::Filter {
        pred: Predicate(rows_pred.map_operator_refs(&mut built)),
        child: InputOrTemporaryBuild::Input(child),
    })
    .map_err(regrouped_build)?;
    let reduction = Reduction::Reduce(by);
    summary_agg_schema(rows.schema(), &family, &reduction).map_err(regrouped_build)?;
    Ok(Some(ASAPOp::SummaryAgg {
        child: rows,
        family,
        input,
        reduction,
        grouping,
        filter: filter.map(|pred| Predicate(pred.0.map_operator_refs(&mut built))),
    }))
}

/// The coverage of a merge of states covering `inputs`, whose builds read
/// rows with timestamp column `time_column`.
fn of_merge(
    inputs: &[SummaryCoverage],
    time_column: Option<&ColumnIdentity>,
) -> Result<SummaryCoverage, CoverageError> {
    let first = inputs.first().ok_or(CoverageError::EmptyMerge)?;
    if inputs
        .iter()
        .any(|input| input.definition != first.definition)
    {
        return Err(CoverageError::DefinitionMismatch);
    }
    // A family without a merge is checked as the strictest rule; the
    // executor rejects the merge itself.
    let required = match first.definition.unlifted_operators.first() {
        Some(Operator::ASAP(ASAPOp::SummaryAgg { family, .. })) => {
            FamilyCapability::of(family).merge_inputs
        }
        _ => None,
    }
    .unwrap_or(MergeInputRule::Disjoint);
    let separated = |a: &SelectionBox, b: &SelectionBox| match required {
        MergeInputRule::MayOverlap => true,
        // Consecutive windows of one series: disjoint on the timestamp column.
        MergeInputRule::DisjointInTime => time_column.is_some_and(|column| {
            matches!(
                (a.columns.get(column), b.columns.get(column)),
                (Some(a), Some(b)) if a.disjoint(b)
            )
        }),
        MergeInputRule::Disjoint => a.disjoint(b),
    };
    // Every pair of boxes of different inputs.
    if required != MergeInputRule::MayOverlap
        && inputs.iter().enumerate().any(|(i, input)| {
            inputs[..i].iter().any(|other| {
                input
                    .selection
                    .iter()
                    .any(|a| other.selection.iter().any(|b| !separated(a, b)))
            })
        })
    {
        return Err(CoverageError::InputsOverlap { required });
    }
    Ok(SummaryCoverage {
        definition: first.definition.clone(),
        selection: union(inputs.iter().flat_map(|c| c.selection.clone()).collect()),
    })
}

/// The coverage of a regrouping merge, keyed by `keys`, of states whose
/// merge covers `merged` (`first` is its first input): the same rows
/// (`selection`), computed by the input's build grouped by the columns
/// `keys` name. The input states of one output group are different groups
/// of one build, so their rows are disjoint. A counter family merges only
/// time-disjoint windows, never different groups at one time.
fn regrouped(
    merged: SummaryCoverage,
    first: &MemoGroupAsInput<'_>,
    keys: &[ColumnId],
) -> Result<SummaryCoverage, CoverageError> {
    let Some(Operator::ASAP(ASAPOp::SummaryAgg {
        family,
        input,
        reduction,
        grouping,
        filter,
        ..
    })) = merged.definition.unlifted_operators.first()
    else {
        return Err(CoverageError::NotSummary);
    };
    if !FamilyCapability::merges_groups(family)
        || *grouping != GroupingStrategy::PerSubpopulationInstance
    {
        return Err(CoverageError::FamilyCannotRegroup);
    }
    let state = first.schema();
    let rows = build_input_schema(first).ok_or(CoverageError::NotSummary)?;
    // The build's input column each key of the state names.
    let mapped = keys
        .iter()
        .map(|&key| {
            let column = match reduction {
                Reduction::Reduce(by) if !by.is_without() => by.keys().get(key).copied(),
                // A per-series state keeps its input's columns in place.
                Reduction::PerEntity => Some(key),
                Reduction::Reduce(_) => None,
            };
            column
                .filter(|&column| {
                    let (Some(at_state), Some(at_input)) =
                        (state.fields.get(key), rows.fields.get(column))
                    else {
                        return false;
                    };
                    at_state.is_plain()
                        && at_state.name == at_input.name
                        && at_state.table == at_input.table
                })
                .ok_or(CoverageError::NotAGroupKey { key })
        })
        .collect::<Result<Vec<_>, _>>()?;
    let reduction = Reduction::by(mapped);
    summary_agg_schema(&rows, family, &reduction).map_err(|error| {
        CoverageError::RegroupedBuild {
            reason: error.to_string(),
        }
    })?;
    // A direct build's definition, so its definition id is the same.
    let mut definition = merged.definition.clone();
    definition.unlifted_operators[0] = Operator::ASAP(ASAPOp::SummaryAgg {
        child: None,
        family: family.clone(),
        input: input.clone(),
        reduction,
        grouping: grouping.clone(),
        filter: filter.clone(),
    });
    Ok(SummaryCoverage {
        definition,
        selection: merged.selection,
    })
}

/// The schema of the rows the build of summary state `state` reads (its
/// `SummaryAgg`'s input; one build over a chain's window reads its range of
/// the chain's input, which has the input's schema). `None` when `state`
/// is no summary state.
fn build_input_schema<I: OperatorInput>(state: &I) -> Option<Schema> {
    if let Some(states) = selected_from(state) {
        return build_input_schema(&states);
    }
    match state.operator() {
        Operator::ASAP(
            ASAPOp::SummaryAgg { child, .. } | ASAPOp::SummaryTumblingWindowChain { child, .. },
        ) => Some(child.schema().clone()),
        Operator::ASAP(ASAPOp::SummaryMerge { children, .. }) => {
            build_input_schema(children.first()?)
        }
        _ => None,
    }
}

/// The timestamp column of the rows the build of summary state `state`
/// reads ([`build_input_schema`]), as a selection names it.
fn time_column_of_build_input<I: OperatorInput>(state: &I) -> Option<ColumnIdentity> {
    let rows = build_input_schema(state)?;
    column_identity(&rows, rows.time_index?)
}

/// The coverage of `build`, a `SummaryAgg` over memo groups (or over an
/// input a rule built over them).
fn of_summary_agg(build: &ASAPOp<GroupInput<'_>>) -> Result<SummaryCoverage, CoverageError> {
    let ASAPOp::SummaryAgg {
        child,
        family,
        input,
        reduction,
        grouping,
        filter,
    } = build
    else {
        unreachable!("called on a SummaryAgg");
    };
    of_aggregate(child, filter.as_ref(), |residual_filter| {
        Operator::ASAP(ASAPOp::SummaryAgg {
            child: None,
            family: family.clone(),
            input: input.clone(),
            reduction: reduction.clone(),
            grouping: grouping.clone(),
            filter: residual_filter,
        })
    })
}

/// The coverage of an aggregate over `child`: its input's operators walked
/// down through the liftable operators of the input groups' expressions.
/// `filter` is the aggregate's own row filter (a `SummaryAgg`'s), and
/// `unlifted_top` makes the aggregate's unlifted operator from that filter's residual.
fn of_aggregate(
    child: &GroupInput<'_>,
    filter: Option<&Predicate<GroupInput<'_>>>,
    unlifted_top: impl FnOnce(Option<Predicate<Option<MemoGroupId>>>) -> Operator<Option<MemoGroupId>>,
) -> Result<SummaryCoverage, CoverageError> {
    // The operators a predicate can be lifted through, top to bottom, and
    // the first input below them. A subquery's group stays named in the
    // residual of the operator that reads it.
    let mut chain: Vec<Operator<GroupInput<'_>>> = Vec::new();
    let mut base = child.clone();
    loop {
        let operator = base.operator();
        let passes = match &operator {
            Operator::NonASAP(NonASAPOp::Project { .. } | NonASAPOp::Filter { .. }) => true,
            Operator::NonASAP(NonASAPOp::TimeRange { kind, .. }) => *kind == TimeRangeKind::Range,
            Operator::NonASAP(NonASAPOp::TimeShift { .. }) => true,
            _ => false,
        };
        if !passes {
            break;
        }
        // The main input is an operator's first child.
        base = operator.children()[0].clone();
        chain.push(operator);
    }
    let InputOrTemporaryBuild::Input(input_group) = &base else {
        unreachable!("a rule builds only liftable operators over memo groups");
    };
    let top = child.schema();
    // Top-down: decide which conjuncts are lifted. `kept[d]` is the residual
    // of `chain[d]` when it is a `Filter`.
    let mut lifted = BTreeMap::new();
    let agg_filter = filter.and_then(|pred| residual(pred, top, &[], &mut lifted));
    let kept: Vec<Option<Predicate<Option<MemoGroupId>>>> = chain
        .iter()
        .enumerate()
        .map(|(depth, link)| match link {
            Operator::NonASAP(NonASAPOp::Filter { pred, .. }) => {
                residual(pred, top, &chain[..depth], &mut lifted)
            }
            _ => None,
        })
        .collect();
    // A `Scan`'s predicates (PromQL label matchers) are lifted too; when
    // one is, the `Scan` with the rest of its predicates is the last
    // unlifted operator, and there is no input group.
    let scan = match base.operator() {
        Operator::NonASAP(NonASAPOp::Scan {
            source,
            predicates,
            schema,
        }) => {
            let lifted_above = lifted.clone();
            let kept = predicates
                .iter()
                .filter_map(|pred| residual(pred, top, &chain, &mut lifted))
                .collect();
            (lifted != lifted_above).then_some(NonASAPOp::Scan {
                source,
                predicates: kept,
                schema,
            })
        }
        _ => None,
    };

    // The `TimeRange` and `TimeShift`s: one interval on the timestamp column
    // of the `TimeRange`'s input, when they make one and it combines with
    // what is lifted on that column already; else they stay unlifted.
    let time = time_constraint(&chain).and_then(|constraint| {
        let depth = chain
            .iter()
            .position(|link| matches!(link, Operator::NonASAP(NonASAPOp::TimeRange { .. })))?;
        let column = chain[depth].children()[0].schema().time_index?;
        let column = column_at_top(column, &chain[..=depth])?;
        column_identity(top, column)?;
        let combined = match lifted.get(&column) {
            Some(existing) => existing.intersect(&constraint)?,
            None => constraint,
        };
        Some((column, combined))
    });
    let lift_time = time.is_some();
    lifted.extend(time);

    let mut unlifted_operators = vec![unlifted_top(agg_filter)];
    for (depth, link) in chain.iter().enumerate() {
        match link {
            Operator::NonASAP(NonASAPOp::Filter { .. }) => {
                if let Some(rest) = kept[depth].clone() {
                    unlifted_operators.push(Operator::NonASAP(NonASAPOp::Filter {
                        pred: rest,
                        child: None,
                    }));
                }
            }
            Operator::NonASAP(NonASAPOp::TimeRange { .. } | NonASAPOp::TimeShift { .. })
                if lift_time => {}
            other => unlifted_operators.push(unlifted(other)),
        }
    }
    let input_group = match scan {
        Some(scan) => {
            unlifted_operators.push(Operator::NonASAP(scan));
            None
        }
        None => Some(input_group.group_id),
    };

    let selection = SelectionBox {
        columns: lifted
            .into_iter()
            .filter_map(|(column, constraint)| Some((column_identity(top, column)?, constraint)))
            .collect(),
    };
    Ok(SummaryCoverage {
        definition: SummaryStateDefinition {
            input_group,
            unlifted_operators,
        },
        selection: vec![selection],
    })
}

/// Lift what `pred`'s conjuncts can into `lifted` (keyed by the column of
/// `top`, the agg child's schema) and return the rest, each scalar subquery
/// naming the group it reads. `above`
/// are the operators between the predicate and the `SummaryAgg`, top to
/// bottom. A column [`column_identity`] cannot name stays.
fn residual(
    pred: &Predicate<GroupInput<'_>>,
    top: &Schema,
    above: &[Operator<GroupInput<'_>>],
    lifted: &mut BTreeMap<ColumnId, Constraint>,
) -> Option<Predicate<Option<MemoGroupId>>> {
    let mut rest: Vec<ScalarExpr<Option<MemoGroupId>>> = Vec::new();
    for conjunct in pred.0.conjuncts() {
        let lift = constraint_of(conjunct).and_then(|(column, constraint)| {
            let column = column_at_top(column, above)?;
            column_identity(top, column)?;
            let combined = match lifted.get(&column) {
                Some(existing) => existing.intersect(&constraint)?,
                None => constraint,
            };
            Some((column, combined))
        });
        match lift {
            Some((column, constraint)) => {
                lifted.insert(column, constraint);
            }
            None => rest.push(conjunct.map_operator_refs(&mut group_id)),
        }
    }
    match rest.len() {
        0 => None,
        1 => rest.pop().map(Predicate),
        _ => Some(Predicate(ScalarExpr::BoolAnd(rest))),
    }
}

/// The memo group `input` is; `None` for a rule's temporary build.
fn group_id(input: &GroupInput<'_>) -> Option<MemoGroupId> {
    match input {
        InputOrTemporaryBuild::Input(group) => Some(group.group_id),
        InputOrTemporaryBuild::TemporaryBuild(_) => None,
    }
}

/// `link`, a liftable operator or an exact `Aggregate` kept whole in a
/// definition: its main input `None`, every other input the group it is.
fn unlifted(link: &Operator<GroupInput<'_>>) -> Operator<Option<MemoGroupId>> {
    let mut operator = link.map_children(group_id);
    match &mut operator {
        Operator::NonASAP(
            NonASAPOp::Filter { child, .. }
            | NonASAPOp::Project { child, .. }
            | NonASAPOp::TimeRange { child, .. }
            | NonASAPOp::TimeShift { child, .. }
            | NonASAPOp::Aggregate { child, .. },
        ) => *child = None,
        _ => unreachable!("only liftable operators and aggregates are unlifted"),
    }
    operator
}

/// Map a column of the input of `above.last()` up to the agg child's output:
/// a `Project` passes it only as a direct column item.
fn column_at_top<C>(mut column: ColumnId, above: &[Operator<C>]) -> Option<ColumnId> {
    for link in above.iter().rev() {
        if let Operator::NonASAP(NonASAPOp::Project { cols, .. }) = link {
            column = cols
                .iter()
                .position(|item| matches!(item.expr, ScalarExpr::Column(c) if c == column))?;
        }
    }
    Some(column)
}

/// The identity of `schema`'s column `column`; `None` when its
/// `(table, name)` is not unique in `schema`, so a selection cannot name it.
fn column_identity(schema: &Schema, column: ColumnId) -> Option<ColumnIdentity> {
    let field = schema.fields.get(column)?;
    let namesakes = schema
        .fields
        .iter()
        .filter(|f| f.name == field.name && f.table == field.table)
        .count();
    (namesakes == 1).then(|| ColumnIdentity {
        table: field.table.clone(),
        name: field.name.clone(),
    })
}

/// The interval on the timestamp column that `links` (top down, the
/// `TimeRange` and `TimeShift`s between an aggregate and its input) select:
/// `[anchor − o − W, anchor − o)` for the one range `TimeRange` of width W
/// and the shifts' offsets summing to o. The anchor is the evaluation time
/// t (`EvaluationTimePlusMs`), also for `@ end()`, or T for `@ T`
/// (`FixedValue(Timestamp)`). `None` when they are not one interval: no or
/// several range `TimeRange`s, an instant one, or several `@`s.
fn time_constraint<C>(links: &[Operator<C>]) -> Option<Constraint> {
    let (mut width, mut offset_ms, mut at) = (None, 0i64, None::<AtModifier>);
    for link in links {
        match link {
            Operator::NonASAP(NonASAPOp::TimeRange { range, kind, .. }) => {
                if *kind != TimeRangeKind::Range || width.replace(*range).is_some() {
                    return None;
                }
            }
            Operator::NonASAP(NonASAPOp::TimeShift { shift, .. }) => {
                offset_ms = offset_ms.checked_add(shift.offset_ms)?;
                if let Some(anchor) = shift.at {
                    if at.replace(anchor).is_some() {
                        return None;
                    }
                }
            }
            _ => {}
        }
    }
    let width_ms = i64::try_from(width?.as_millis()).ok()?;
    let fixed_anchor = match at {
        Some(AtModifier::Timestamp(ms)) => Some(ms),
        Some(AtModifier::End) | None => None,
    };
    let before_anchor = |ms: i64| -> Option<IntervalConstraintLimit> {
        Some(match fixed_anchor {
            Some(anchor) => {
                IntervalConstraintLimit::FixedValue(ScalarValue::Timestamp(anchor.checked_sub(ms)?))
            }
            None => IntervalConstraintLimit::EvaluationTimePlusMs(ms.checked_neg()?),
        })
    };
    Some(Constraint::Interval {
        lower: Bound::Included(before_anchor(offset_ms.checked_add(width_ms)?)?),
        upper: Bound::Excluded(before_anchor(offset_ms)?),
    })
}

/// The coverage of `selection`, a `TimeRange` or `TimeShift` over a chain
/// (through more of them): the chain's coverage with the interval they
/// select on the timestamp column of the chain's input, in every box.
fn of_window_selection(
    selection: &Operator<MemoGroupAsInput<'_>>,
) -> Result<SummaryCoverage, CoverageError> {
    let mut links = vec![selection.clone()];
    let mut below = *selection.children()[0];
    loop {
        match below.operator() {
            link @ Operator::NonASAP(
                NonASAPOp::TimeRange { child, .. } | NonASAPOp::TimeShift { child, .. },
            ) => {
                below = child;
                links.push(link);
            }
            Operator::ASAP(ASAPOp::SummaryTumblingWindowChain { .. }) => break,
            _ => return Err(CoverageError::NotSummary),
        }
    }
    let chain = below
        .aggregate_coverage()
        .ok_or(CoverageError::NotSummary)?;
    let time_column = time_column_of_build_input(&below).ok_or(CoverageError::NotSummary)?;
    let constraint = time_constraint(&links).ok_or(CoverageError::NotSummary)?;
    let selection = chain
        .selection
        .iter()
        .map(|selection_box| {
            let mut columns = selection_box.columns.clone();
            let combined = match columns.get(&time_column) {
                Some(existing) => existing.intersect(&constraint),
                None => Some(constraint.clone()),
            };
            columns.insert(
                time_column.clone(),
                combined.ok_or(CoverageError::NotSummary)?,
            );
            Ok(SelectionBox { columns })
        })
        .collect::<Result<_, _>>()?;
    Ok(SummaryCoverage {
        definition: chain.definition.clone(),
        selection,
    })
}

/// `column = v`, `!=`, `<`, `<=`, `>`, `>=`, `[NOT] IN (...)` and `OR` of
/// equalities on one column, with non-null literals.
fn constraint_of<C>(expr: &ScalarExpr<C>) -> Option<(ColumnId, Constraint)> {
    let literal = |e: &ScalarExpr<C>| match e {
        ScalarExpr::Literal(v) if *v != ScalarValue::Null => Some(v.clone()),
        _ => None,
    };
    match expr {
        ScalarExpr::Compare {
            left, op, right, ..
        } => {
            let (column, value, op) = match (left.as_ref(), right.as_ref()) {
                (ScalarExpr::Column(c), r) => (*c, literal(r)?, op.clone()),
                (l, ScalarExpr::Column(c)) => (*c, literal(l)?, flip(op)?),
                _ => return None,
            };
            let constraint = match op {
                CompareOpKind::Eq => Constraint::In(vec![value]),
                CompareOpKind::Ne => Constraint::NotIn(vec![value]),
                CompareOpKind::Lt => interval(Bound::Unbounded, Bound::Excluded(value)),
                CompareOpKind::Le => interval(Bound::Unbounded, Bound::Included(value)),
                CompareOpKind::Gt => interval(Bound::Excluded(value), Bound::Unbounded),
                CompareOpKind::Ge => interval(Bound::Included(value), Bound::Unbounded),
                _ => return None,
            };
            Some((column, constraint))
        }
        ScalarExpr::InList {
            expr,
            list,
            negated,
        } => {
            let ScalarExpr::Column(column) = expr.as_ref() else {
                return None;
            };
            let values = list.iter().map(literal).collect::<Option<Vec<_>>>()?;
            let values = dedup(values);
            Some((
                *column,
                if *negated {
                    Constraint::NotIn(values)
                } else {
                    Constraint::In(values)
                },
            ))
        }
        ScalarExpr::BoolOr(disjuncts) => {
            let mut column = None;
            let mut values = Vec::new();
            for disjunct in disjuncts {
                let (c, Constraint::In(v)) = constraint_of(disjunct)? else {
                    return None;
                };
                if column.replace(c).is_some_and(|prev| prev != c) {
                    return None;
                }
                values.extend(v);
            }
            Some((column?, Constraint::In(dedup(values))))
        }
        _ => None,
    }
}

fn flip(op: &CompareOpKind) -> Option<CompareOpKind> {
    Some(match op {
        CompareOpKind::Eq => CompareOpKind::Eq,
        CompareOpKind::Ne => CompareOpKind::Ne,
        CompareOpKind::Lt => CompareOpKind::Gt,
        CompareOpKind::Le => CompareOpKind::Ge,
        CompareOpKind::Gt => CompareOpKind::Lt,
        CompareOpKind::Ge => CompareOpKind::Le,
        _ => return None,
    })
}

/// An interval between two fixed values.
fn interval(lower: Bound<ScalarValue>, upper: Bound<ScalarValue>) -> Constraint {
    Constraint::Interval {
        lower: lower.map(IntervalConstraintLimit::FixedValue),
        upper: upper.map(IntervalConstraintLimit::FixedValue),
    }
}

fn dedup(values: Vec<ScalarValue>) -> Vec<ScalarValue> {
    let mut out: Vec<ScalarValue> = Vec::new();
    for v in values {
        if !out.contains(&v) {
            out.push(v);
        }
    }
    out
}

/// Whether `v` equals one of `values`; `None` when some pair cannot be
/// compared (different types, NaN), since `1` and `1.0` may select the same
/// rows.
fn among(v: &ScalarValue, values: &[ScalarValue]) -> Option<bool> {
    for w in values {
        if compare(v, w)? == Ordering::Equal {
            return Some(true);
        }
    }
    Some(false)
}

/// The values of `a` that are (`keep = true`) or are not in `b`.
fn filter_values(a: &[ScalarValue], b: &[ScalarValue], keep: bool) -> Option<Vec<ScalarValue>> {
    let mut out = Vec::new();
    for v in a {
        if among(v, b)? == keep {
            out.push(v.clone());
        }
    }
    Some(out)
}

/// Order of two literals of the same type; `None` across types or for NaN.
fn compare(a: &ScalarValue, b: &ScalarValue) -> Option<Ordering> {
    match (a, b) {
        (ScalarValue::Int64(a), ScalarValue::Int64(b)) => Some(a.cmp(b)),
        (ScalarValue::Float64(a), ScalarValue::Float64(b)) => a.partial_cmp(b),
        (ScalarValue::Utf8(a), ScalarValue::Utf8(b)) => Some(a.cmp(b)),
        (ScalarValue::Boolean(a), ScalarValue::Boolean(b)) => Some(a.cmp(b)),
        (ScalarValue::Timestamp(a), ScalarValue::Timestamp(b)) => Some(a.cmp(b)),
        _ => None,
    }
}

/// Order of two interval limits; `None` across variants or when the fixed
/// values do not compare.
fn compare_limits(a: &IntervalConstraintLimit, b: &IntervalConstraintLimit) -> Option<Ordering> {
    use IntervalConstraintLimit::*;
    match (a, b) {
        (FixedValue(a), FixedValue(b)) => compare(a, b),
        (EvaluationTimePlusMs(a), EvaluationTimePlusMs(b)) => Some(a.cmp(b)),
        _ => None,
    }
}

/// Whether value `v` lies inside the interval; `None` when unsure.
fn value_inside(
    v: &ScalarValue,
    bounds: (
        &Bound<IntervalConstraintLimit>,
        &Bound<IntervalConstraintLimit>,
    ),
) -> Option<bool> {
    inside(
        &IntervalConstraintLimit::FixedValue(v.clone()),
        bounds,
        compare_limits,
    )
}

/// Whether `v` lies inside the interval; `None` when it cannot be compared.
fn inside<T>(
    v: &T,
    (lower, upper): (&Bound<T>, &Bound<T>),
    cmp: impl Fn(&T, &T) -> Option<Ordering>,
) -> Option<bool> {
    let above = match lower {
        Bound::Unbounded => true,
        Bound::Included(l) => cmp(v, l)? != Ordering::Less,
        Bound::Excluded(l) => cmp(v, l)? == Ordering::Greater,
    };
    let below = match upper {
        Bound::Unbounded => true,
        Bound::Included(u) => cmp(v, u)? != Ordering::Greater,
        Bound::Excluded(u) => cmp(v, u)? == Ordering::Less,
    };
    Some(above && below)
}

/// Whether no value is both at most `upper` and at least `lower`.
fn ends_before<T>(
    upper: &Bound<T>,
    lower: &Bound<T>,
    cmp: impl Fn(&T, &T) -> Option<Ordering>,
) -> bool {
    match (upper, lower) {
        (Bound::Included(u), Bound::Included(l)) => cmp(u, l) == Some(Ordering::Less),
        (Bound::Included(u) | Bound::Excluded(u), Bound::Included(l) | Bound::Excluded(l)) => {
            matches!(cmp(u, l), Some(Ordering::Less | Ordering::Equal))
        }
        _ => false,
    }
}

fn intervals_disjoint<T>(
    (al, au): (&Bound<T>, &Bound<T>),
    (bl, bu): (&Bound<T>, &Bound<T>),
    cmp: impl Fn(&T, &T) -> Option<Ordering> + Copy,
) -> bool {
    ends_before(au, bl, cmp) || ends_before(bu, al, cmp)
}

/// The tighter of two lower (`want = Greater`) or upper (`Less`) bounds.
fn tighter(
    a: &Bound<IntervalConstraintLimit>,
    b: &Bound<IntervalConstraintLimit>,
    want: Ordering,
) -> Option<Bound<IntervalConstraintLimit>> {
    let value = |bound: &Bound<IntervalConstraintLimit>| match bound {
        Bound::Included(v) | Bound::Excluded(v) => Some(v.clone()),
        Bound::Unbounded => None,
    };
    Some(match (value(a), value(b)) {
        (None, _) => b.clone(),
        (_, None) => a.clone(),
        (Some(x), Some(y)) => match compare_limits(&x, &y)? {
            Ordering::Equal if matches!(a, Bound::Excluded(_)) => a.clone(),
            Ordering::Equal => b.clone(),
            order if order == want => a.clone(),
            _ => b.clone(),
        },
    })
}

impl Constraint {
    /// The conjunction of two constraints on one column, when it is again a
    /// single constraint.
    fn intersect(&self, other: &Self) -> Option<Self> {
        use Constraint::*;
        Some(match (self, other) {
            (In(a), In(b)) => In(filter_values(a, b, true)?),
            (In(a), NotIn(b)) | (NotIn(b), In(a)) => In(filter_values(a, b, false)?),
            (NotIn(a), NotIn(b)) => NotIn(dedup(a.iter().chain(b).cloned().collect())),
            (In(a), Interval { lower, upper }) | (Interval { lower, upper }, In(a)) => {
                let mut kept = Vec::new();
                for v in a {
                    if value_inside(v, (lower, upper))? {
                        kept.push(v.clone());
                    }
                }
                In(kept)
            }
            (
                Interval {
                    lower: al,
                    upper: au,
                },
                Interval {
                    lower: bl,
                    upper: bu,
                },
            ) => Interval {
                lower: tighter(al, bl, Ordering::Greater)?,
                upper: tighter(au, bu, Ordering::Less)?,
            },
            (NotIn(_), Interval { .. }) | (Interval { .. }, NotIn(_)) => return None,
        })
    }

    /// Whether no value satisfies both; `false` when unsure.
    fn disjoint(&self, other: &Self) -> bool {
        use Constraint::*;
        match (self, other) {
            (In(a), In(b)) => a.iter().all(|v| among(v, b) == Some(false)),
            (In(a), NotIn(b)) | (NotIn(b), In(a)) => a.iter().all(|v| among(v, b) == Some(true)),
            (In(a), Interval { lower, upper }) | (Interval { lower, upper }, In(a)) => a
                .iter()
                .all(|v| value_inside(v, (lower, upper)) == Some(false)),
            (
                Interval {
                    lower: al,
                    upper: au,
                },
                Interval {
                    lower: bl,
                    upper: bu,
                },
            ) => intervals_disjoint((al, au), (bl, bu), compare_limits),
            (NotIn(_), _) | (_, NotIn(_)) => false,
        }
    }
}

#[cfg(any(test, feature = "test-support"))]
impl SelectionBox {
    pub fn new(columns: BTreeMap<ColumnIdentity, Constraint>) -> Self {
        Self { columns }
    }

    /// The per-column constraints.
    pub fn columns(&self) -> &BTreeMap<ColumnIdentity, Constraint> {
        &self.columns
    }
}

impl SelectionBox {
    /// Whether no row lies in both boxes: some column is restricted by both
    /// and the restrictions are disjoint.
    fn disjoint(&self, other: &Self) -> bool {
        self.columns
            .iter()
            .any(|(column, a)| other.columns.get(column).is_some_and(|b| a.disjoint(b)))
    }
}

/// Join boxes that differ only in one column whose union is again one
/// constraint: touching value ranges, or the values of one `In` column.
/// Gaps stay separate boxes.
fn union(mut boxes: Vec<SelectionBox>) -> Vec<SelectionBox> {
    let mut i = 0;
    while i < boxes.len() {
        let joined = (i + 1..boxes.len()).find_map(|j| join(&boxes[i], &boxes[j]).map(|b| (j, b)));
        match joined {
            Some((j, joined)) => {
                boxes.remove(j);
                boxes[i] = joined;
                i = 0;
            }
            None => i += 1,
        }
    }
    boxes
}

fn join(a: &SelectionBox, b: &SelectionBox) -> Option<SelectionBox> {
    if a == b {
        return Some(a.clone());
    }
    if a.columns.len() != b.columns.len() {
        return None;
    }
    let mut differing = a
        .columns
        .iter()
        .filter(|(column, constraint)| b.columns.get(*column) != Some(*constraint));
    let (column, constraint) = differing.next()?;
    if differing.next().is_some() {
        return None;
    }
    let joined = match (constraint, b.columns.get(column)?) {
        (Constraint::In(values), Constraint::In(more)) => {
            Constraint::In(dedup(values.iter().chain(more).cloned().collect()))
        }
        (
            Constraint::Interval {
                lower: al,
                upper: au,
            },
            Constraint::Interval {
                lower: bl,
                upper: bu,
            },
        ) => {
            let (lower, upper) = touching((al, au), (bl, bu), compare_limits)?;
            Constraint::Interval { lower, upper }
        }
        _ => return None,
    };
    let mut columns = a.columns.clone();
    columns.insert(column.clone(), joined);
    Some(SelectionBox { columns })
}

/// The union of two intervals when one ends exactly where the other starts,
/// with the shared end point in exactly one of them.
fn touching<T: Clone>(
    (al, au): (&Bound<T>, &Bound<T>),
    (bl, bu): (&Bound<T>, &Bound<T>),
    cmp: impl Fn(&T, &T) -> Option<Ordering>,
) -> Option<(Bound<T>, Bound<T>)> {
    let meets = |upper: &Bound<T>, lower: &Bound<T>| match (upper, lower) {
        (Bound::Included(u), Bound::Excluded(l)) | (Bound::Excluded(u), Bound::Included(l)) => {
            cmp(u, l) == Some(Ordering::Equal)
        }
        _ => false,
    };
    if meets(au, bl) {
        Some((al.clone(), bu.clone()))
    } else if meets(bu, al) {
        Some((bl.clone(), au.clone()))
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ir::scalar::ExprSemantics;
    use IntervalConstraintLimit::{EvaluationTimePlusMs, FixedValue};

    fn relative(lower_ms: i64, upper_ms: i64) -> Constraint {
        Constraint::Interval {
            lower: Bound::Included(EvaluationTimePlusMs(lower_ms)),
            upper: Bound::Excluded(EvaluationTimePlusMs(upper_ms)),
        }
    }

    /// SQL `price > 10` lifts to `(FixedValue(10), ∞)`.
    #[test]
    fn a_value_comparison_is_an_interval_between_fixed_values() {
        let price_gt_10: ScalarExpr<()> = ScalarExpr::Compare {
            left: Box::new(ScalarExpr::Column(0)),
            op: CompareOpKind::Gt,
            right: Box::new(ScalarExpr::Literal(ScalarValue::Float64(10.0))),
            semantics: ExprSemantics::Sql,
        };
        assert_eq!(
            constraint_of(&price_gt_10),
            Some((
                0,
                Constraint::Interval {
                    lower: Bound::Excluded(FixedValue(ScalarValue::Float64(10.0))),
                    upper: Bound::Unbounded,
                }
            ))
        );
    }

    /// `m[5m]` and `m[5m] offset 5m` are disjoint and touch; neither
    /// compares with an `@ T` interval.
    #[test]
    fn limits_compare_only_within_one_variant() {
        let last_5m = relative(-300_000, 0);
        let offset_5m = relative(-600_000, -300_000);
        assert!(last_5m.disjoint(&offset_5m));
        let at = Constraint::Interval {
            lower: Bound::Included(FixedValue(ScalarValue::Timestamp(1_699_999_700_000))),
            upper: Bound::Excluded(FixedValue(ScalarValue::Timestamp(1_700_000_000_000))),
        };
        assert!(!last_5m.disjoint(&at));
        assert_eq!(last_5m.intersect(&at), None);
        let column = ColumnIdentity::new(None, "ts");
        let one = |constraint: Constraint| SelectionBox {
            columns: BTreeMap::from([(column.clone(), constraint)]),
        };
        assert_eq!(
            union(vec![one(last_5m), one(offset_5m)]),
            vec![one(relative(-600_000, 0))]
        );
    }

    #[test]
    fn an_interval_round_trips_through_json() {
        let constraint = relative(-3_900_000, -3_600_000);
        let json = serde_json::to_value(&constraint).unwrap();
        assert_eq!(
            json,
            serde_json::json!({"interval": {
                "lower": {"included": {"evaluation_time_plus_ms": -3_900_000}},
                "upper": {"excluded": {"evaluation_time_plus_ms": -3_600_000}},
            }})
        );
        assert_eq!(
            serde_json::from_value::<Constraint>(json).unwrap(),
            constraint
        );
    }
}

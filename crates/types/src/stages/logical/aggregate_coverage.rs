//! Aggregate coverage, a memo-group property: what an aggregate (a summary
//! state or an exact `Aggregate`) covers ([`AggregateCoverage`]):
//! `definition` (what it computes, its inputs named by memo group) and
//! `selection` (which output rows of that computation it took, in the
//! selection algebra of [`crate::ir::properties::selection`]). Design:
//! `docs/design_docs/proposals/asap-primitive-schema.md` §4.2, after
//! Goldstein & Larson's view matching.
//!
//! The memo derives it once per aggregate group ([`aggregate_coverage`])
//! over the group's input groups and stores it in
//! `GroupProperties::aggregate_coverage`. Walking
//! down from a `SummaryAgg` or `Aggregate` through its inputs' operators, a
//! predicate conjunct moves into `selection` when it can be lifted to the
//! top (through `Filter`, a range `TimeRange`, a `TimeShift` and
//! direct-column `Project` items) and it is a value set or an interval on
//! one column. The `TimeRange` and `TimeShift`s become one interval on the
//! timestamp column ([`IntervalConstraintLimit`]). Everything else stays in
//! `definition` as a residual, so two states are merged only when they
//! compute the same thing over disjoint rows.

use std::collections::BTreeMap;
use std::ops::Bound;

use thiserror::Error;

use super::group::MemoGroupAsInput;
use crate::ir::operator::arguments::{AtModifier, Reduction};
use crate::ir::operator::asap::{selected_from, summary_agg_layout, ASAPOp};
use crate::ir::operator::input::{InputOrTemporaryBuild, OperatorInput};
use crate::ir::operator::node::{Operator, OperatorResultKind};
use crate::ir::operator::non_asap::{NonASAPOp, TimeRangeKind};
use crate::ir::properties::selection::{
    constraint_of, union, ColumnIdentity, Constraint, IntervalConstraintLimit, SelectionBox,
};
use crate::ir::scalar::expr_ir::ScalarValue;
use crate::ir::scalar::{Predicate, ScalarExpr};
use crate::ir::schema::{ColumnId, Schema};
use crate::ir::SchemaDerivationError;
use crate::stages::MemoGroupId;
use crate::summary_type::{MergeInputRule, SummaryTypeCapability};

/// What an aggregate computes, with every lifted selection removed, its
/// inputs named by memo group.
#[derive(Debug, Clone, PartialEq)]
pub struct AggregateDefinition {
    /// The input the unlifted operators read: the first input under the
    /// `SummaryAgg` or exact `Aggregate` that is not a `Filter`, a range
    /// `TimeRange`, a `TimeShift` without `@ start()`/`@ end()` or a
    /// direct-column `Project`. `None` when that input is a `Scan` with a
    /// liftable predicate (a PromQL label matcher): it is lifted too, and the
    /// `Scan` with the rest of its predicates is the last unlifted operator,
    /// so states over one source with different matchers may merge.
    pub input: Option<MemoGroupId>,
    /// The operators not lifted into the selection, listed top down from the
    /// `SummaryAgg` or `Aggregate`, each with only its residual arguments. `None` marks an
    /// operator's main input (the next unlifted operator, or `input`);
    /// `Some(g)` is another input it reads, such as the input a scalar
    /// subquery in a Filter reads. Two states may merge only if `input`
    /// and these operators are equal.
    pub unlifted_operators: Vec<Operator<Option<MemoGroupId>>>,
}

/// What an aggregate covers, its inputs named by memo group.
#[derive(Debug, Clone, PartialEq)]
pub struct AggregateCoverage {
    pub(crate) definition: AggregateDefinition,
    /// Union of boxes over the output rows of the unlifted operators' input.
    pub(crate) selection: Vec<SelectionBox>,
}

#[cfg(any(test, feature = "test-support"))]
impl AggregateCoverage {
    pub fn definition(&self) -> &AggregateDefinition {
        &self.definition
    }

    /// Union of boxes over the output rows of the unlifted operators' input.
    pub fn selection(&self) -> &[SelectionBox] {
        &self.selection
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
    NoCoverage,
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
    /// A regrouping `SummaryMerge` of states whose summary type does not
    /// merge different groups: counter states (`Rate`, `Increase`) merge
    /// consecutive windows of one series, a summary type without a merge
    /// merges nothing, and a Hydra's groups share one state. Bad input.
    #[error("the inputs' summary type cannot merge different groups")]
    SummaryTypeCannotRegroup,
    /// The build a regrouping `SummaryMerge` stands for does not derive:
    /// bad input.
    #[error("the regrouped build does not derive: {reason}")]
    RegroupedBuild { reason: String },
}

/// An input of a coverage rule: an input group, or a build the rule
/// constructs only while it derives.
type Built<'m> = InputOrTemporaryBuild<MemoGroupAsInput<'m>>;

/// How a definition names `input`: its memo group; `None` for a build a
/// rule constructs only while it derives, which has none.
fn group_of(input: &Built<'_>) -> Option<MemoGroupId> {
    match input {
        InputOrTemporaryBuild::Input(input) => Some(input.group_id),
        InputOrTemporaryBuild::TemporaryBuild(_) => None,
    }
}

/// What input group `input` covers: its stored
/// `GroupProperties::aggregate_coverage`.
fn stored_coverage<'m>(input: &MemoGroupAsInput<'m>) -> Option<&'m AggregateCoverage> {
    input.group_properties().aggregate_coverage.as_ref()
}

/// Coverage of what a memo group computes with `operator` over its input
/// groups: a
/// `SummaryAgg`, an exact `Aggregate`, a
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
pub(super) fn aggregate_coverage<'m>(
    operator: &Operator<MemoGroupAsInput<'m>>,
) -> Result<AggregateCoverage, CoverageError> {
    let as_built = |op: &ASAPOp<MemoGroupAsInput<'m>>| -> ASAPOp<Built<'m>> {
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
            None => Err(CoverageError::NoCoverage),
        },
        Operator::ASAP(
            op @ (ASAPOp::SummaryAgg { .. } | ASAPOp::SummaryTumblingWindowChain { .. }),
        ) => of_summary_agg(&as_built(op)),
        Operator::ASAP(ASAPOp::SummaryMerge {
            children, group_by, ..
        }) => {
            let inputs = children
                .iter()
                .map(|child| {
                    stored_coverage(child)
                        .cloned()
                        .ok_or(CoverageError::NoCoverage)
                })
                .collect::<Result<Vec<_>, _>>()?;
            let merged = of_merge(&inputs, time_column_of_build_input(&children[0]).as_ref())?;
            match group_by {
                None => Ok(merged),
                Some(keys) => regrouped(merged, &children[0], keys),
            }
        }
        Operator::ASAP(_) => Err(CoverageError::NoCoverage),
    }
}

/// For a `Filter` that keeps whole states of a grouped `SummaryAgg` by
/// their key columns, the build that computes exactly the kept states: the
/// same build over its input rows filtered by the same predicate on the
/// input columns those keys name. `None` for any other operator.
fn selected_build<'m>(
    operator: &Operator<MemoGroupAsInput<'m>>,
) -> Result<Option<ASAPOp<Built<'m>>>, CoverageError> {
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
        summary_type: family,
        input,
        reduction: Reduction::Reduce(by),
        filter,
    }) = states.operator()
    else {
        return Err(CoverageError::NoCoverage);
    };
    if by.is_without() {
        return Err(CoverageError::NoCoverage);
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
    summary_agg_layout(rows.schema(), &family, &reduction).map_err(regrouped_build)?;
    Ok(Some(ASAPOp::SummaryAgg {
        child: rows,
        summary_type: family,
        input,
        reduction,
        filter: filter.map(|pred| Predicate(pred.0.map_operator_refs(&mut built))),
    }))
}

/// The coverage of a merge of states covering `inputs`, whose builds read
/// rows with timestamp column `time_column`.
fn of_merge(
    inputs: &[AggregateCoverage],
    time_column: Option<&ColumnIdentity>,
) -> Result<AggregateCoverage, CoverageError> {
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
        Some(Operator::ASAP(ASAPOp::SummaryAgg {
            summary_type: family,
            ..
        })) => SummaryTypeCapability::of(family).merge_inputs,
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
    Ok(AggregateCoverage {
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
    merged: AggregateCoverage,
    first: &MemoGroupAsInput<'_>,
    keys: &[ColumnId],
) -> Result<AggregateCoverage, CoverageError> {
    let Some(Operator::ASAP(ASAPOp::SummaryAgg {
        summary_type: family,
        input,
        reduction,
        filter,
        ..
    })) = merged.definition.unlifted_operators.first()
    else {
        return Err(CoverageError::NoCoverage);
    };
    if !SummaryTypeCapability::merges_groups(family) {
        return Err(CoverageError::SummaryTypeCannotRegroup);
    }
    let state = first.schema();
    let rows = build_input_schema(first).ok_or(CoverageError::NoCoverage)?;
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
    summary_agg_layout(&rows, family, &reduction).map_err(|error| {
        CoverageError::RegroupedBuild {
            reason: error.to_string(),
        }
    })?;
    // A direct build's definition, so it is `==` to one.
    let mut definition = merged.definition.clone();
    definition.unlifted_operators[0] = Operator::ASAP(ASAPOp::SummaryAgg {
        child: None,
        summary_type: family.clone(),
        input: input.clone(),
        reduction,
        filter: filter.clone(),
    });
    Ok(AggregateCoverage {
        definition,
        selection: merged.selection,
    })
}

/// The schema of the rows the build of summary state `state` reads (its
/// `SummaryAgg`'s input; one build over a chain's window reads its range of
/// the chain's input, which has the input's schema). `None` when `state`
/// is no summary state.
fn build_input_schema(state: &MemoGroupAsInput<'_>) -> Option<Schema> {
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
fn time_column_of_build_input(state: &MemoGroupAsInput<'_>) -> Option<ColumnIdentity> {
    let rows = build_input_schema(state)?;
    column_identity(&rows, rows.time_index?)
}

/// The coverage of `build`, a `SummaryAgg` or `SummaryTumblingWindowChain`
/// over inputs (or over an input a rule built over them): a chain covers
/// what its `SummaryAgg` over the same input would.
fn of_summary_agg(build: &ASAPOp<Built<'_>>) -> Result<AggregateCoverage, CoverageError> {
    let (ASAPOp::SummaryAgg {
        child,
        summary_type: family,
        input,
        reduction,
        filter,
    }
    | ASAPOp::SummaryTumblingWindowChain {
        child,
        summary_type: family,
        input,
        reduction,
        filter,
        ..
    }) = build
    else {
        unreachable!("called on a SummaryAgg or a chain");
    };
    of_aggregate(child, filter.as_ref(), |residual_filter| {
        Operator::ASAP(ASAPOp::SummaryAgg {
            child: None,
            summary_type: family.clone(),
            input: input.clone(),
            reduction: reduction.clone(),
            filter: residual_filter,
        })
    })
}

/// The coverage of an aggregate over `child`: its input's operators walked
/// down through the liftable operators of its inputs' operators.
/// `filter` is the aggregate's own row filter (a `SummaryAgg`'s), and
/// `unlifted_top` makes the aggregate's unlifted operator from that filter's residual.
fn of_aggregate<'m>(
    child: &Built<'m>,
    filter: Option<&Predicate<Built<'m>>>,
    unlifted_top: impl FnOnce(Option<Predicate<Option<MemoGroupId>>>) -> Operator<Option<MemoGroupId>>,
) -> Result<AggregateCoverage, CoverageError> {
    // The operators a predicate can be lifted through, top to bottom, and
    // the first input below them. A subquery's input stays named in the
    // residual of the operator that reads it.
    let mut chain: Vec<Operator<Built<'m>>> = Vec::new();
    let mut base = child.clone();
    loop {
        let operator = base.operator();
        let passes = match &operator {
            Operator::NonASAP(NonASAPOp::Project { .. } | NonASAPOp::Filter { .. }) => true,
            Operator::NonASAP(NonASAPOp::TimeRange { kind, .. }) => *kind == TimeRangeKind::Range,
            // `@ end()` / `@ start()` pin their subtree to one end of the
            // request: not lifted, so their group is the input group.
            Operator::NonASAP(NonASAPOp::TimeShift { shift, .. }) => {
                !matches!(shift.at, Some(AtModifier::Start | AtModifier::End))
            }
            _ => false,
        };
        if !passes {
            break;
        }
        // The main input is an operator's first child.
        base = operator.children()[0].clone();
        chain.push(operator);
    }
    let InputOrTemporaryBuild::Input(base_input) = &base else {
        unreachable!("a rule builds only liftable operators over real inputs");
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
    // unlifted operator, and there is no input.
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
        Some((column, combined_with(lifted.get(&column), constraint)?))
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
    let input = match scan {
        Some(scan) => {
            unlifted_operators.push(Operator::NonASAP(scan));
            None
        }
        None => Some(base_input.group_id),
    };

    let selection = SelectionBox {
        columns: lifted
            .into_iter()
            .filter_map(|(column, constraint)| Some((column_identity(top, column)?, constraint)))
            .collect(),
    };
    Ok(AggregateCoverage {
        definition: AggregateDefinition {
            input,
            unlifted_operators,
        },
        selection: vec![selection],
    })
}

/// Lift what `pred`'s conjuncts can into `lifted` (keyed by the column of
/// `top`, the agg child's schema) and return the rest, each scalar subquery
/// naming the input it reads. `above`
/// are the operators between the predicate and the `SummaryAgg`, top to
/// bottom. A column [`column_identity`] cannot name stays.
fn residual(
    pred: &Predicate<Built<'_>>,
    top: &Schema,
    above: &[Operator<Built<'_>>],
    lifted: &mut BTreeMap<ColumnId, Constraint>,
) -> Option<Predicate<Option<MemoGroupId>>> {
    let mut rest: Vec<ScalarExpr<Option<MemoGroupId>>> = Vec::new();
    for conjunct in pred.0.conjuncts() {
        let lift = constraint_of(conjunct).and_then(|(column, constraint)| {
            let column = column_at_top(column, above)?;
            column_identity(top, column)?;
            Some((column, combined_with(lifted.get(&column), constraint)?))
        });
        match lift {
            Some((column, constraint)) => {
                lifted.insert(column, constraint);
            }
            None => rest.push(conjunct.map_operator_refs(&mut group_of)),
        }
    }
    match rest.len() {
        0 => None,
        1 => rest.pop().map(Predicate),
        _ => Some(Predicate(ScalarExpr::BoolAnd(rest))),
    }
}

/// `constraint` combined with what is lifted on its column already
/// (`existing`); `None` when the two do not combine.
fn combined_with(existing: Option<&Constraint>, constraint: Constraint) -> Option<Constraint> {
    match existing {
        Some(existing) => existing.intersect(&constraint),
        None => Some(constraint),
    }
}

/// `link`, a liftable operator or an exact `Aggregate` kept whole in a
/// definition: its main input `None`, every other input its reference.
fn unlifted(link: &Operator<Built<'_>>) -> Operator<Option<MemoGroupId>> {
    let mut operator = link.map_children(group_of);
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
/// `(anchor − o − W, anchor − o]` for the one range `TimeRange` of width W
/// and the shifts' offsets summing to o, left-open and right-closed like a
/// PromQL range. The anchor is the evaluation time t
/// (`EvaluationTimePlusMs`), or T for `@ T` (`FixedValue(Timestamp)`).
/// `None` when they are not one interval: no or several range `TimeRange`s,
/// an instant one, several `@`s, or an `@ end()` or `@ start()`, which pins
/// its subtree to one end of the request and is never lifted (the front
/// ends put it above the subtree it anchors).
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
        Some(AtModifier::Start | AtModifier::End) => return None,
        None => None,
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
        lower: Bound::Excluded(before_anchor(offset_ms.checked_add(width_ms)?)?),
        upper: Bound::Included(before_anchor(offset_ms)?),
    })
}

/// The coverage of `selection`, a `TimeRange` or `TimeShift` over a chain
/// (through more of them): the chain's coverage with the interval they
/// select on the timestamp column of the chain's input, in every box.
fn of_window_selection(
    selection: &Operator<MemoGroupAsInput<'_>>,
) -> Result<AggregateCoverage, CoverageError> {
    let mut links = vec![selection.clone()];
    let mut below = *selection.children()[0];
    while let Some(child) = selected_from(&below) {
        links.push(below.operator());
        below = child;
    }
    let only_time_selections = links.iter().all(|link| {
        matches!(
            link,
            Operator::NonASAP(NonASAPOp::TimeRange { .. } | NonASAPOp::TimeShift { .. })
        )
    });
    if !only_time_selections
        || !matches!(
            below.operator(),
            Operator::ASAP(ASAPOp::SummaryTumblingWindowChain { .. })
        )
    {
        return Err(CoverageError::NoCoverage);
    }
    let chain = stored_coverage(&below).ok_or(CoverageError::NoCoverage)?;
    let time_column = time_column_of_build_input(&below).ok_or(CoverageError::NoCoverage)?;
    let constraint = time_constraint(&links).ok_or(CoverageError::NoCoverage)?;
    let selection = chain
        .selection
        .iter()
        .map(|selection_box| {
            let mut columns = selection_box.columns.clone();
            let combined = combined_with(columns.get(&time_column), constraint.clone())
                .ok_or(CoverageError::NoCoverage)?;
            columns.insert(time_column.clone(), combined);
            Ok(SelectionBox { columns })
        })
        .collect::<Result<_, _>>()?;
    Ok(AggregateCoverage {
        definition: chain.definition.clone(),
        selection,
    })
}

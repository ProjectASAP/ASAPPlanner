//! The operator that computes one measure by one way, and the groups it
//! reads, built pending ([`Pending`]): exact accumulators, summaries and
//! their readouts, `avg` from its `sum` and `count`, a heap's ranked rows,
//! and a multi-measure aggregate's measures joined on their group keys.

use asap_types::ir::operator::arguments::{BinaryOpKind, JoinKind, Reduction};
use asap_types::ir::operator::AggIntent;
use asap_types::ir::scalar::expr_ir::ArithmeticOpKind;
use asap_types::ir::scalar::{resolve_column_ref, ColumnRef, CompareOpKind, ScalarValue};
use asap_types::ir::schema::{DataType, FieldDataType, Schema};
use asap_types::ir::{
    ASAPOp, BinaryOperator, ExprSemantics, NonASAPOp, Operator, OperatorResultKind, Predicate,
    ProjectItem, ScalarExpr,
};
use asap_types::stages::{Memo, MemoGroupId};
use asap_types::summary_type::{ExactKind, SummaryInputExpr};

use asap_accuracy::sizing::sketch_statistic;

use super::pending::{add_candidate, commit_pending, Pending};
use super::SingleMeasure;
use crate::computation_targets::HowToComputeAggregateMeasure;
use crate::summary_realization::{
    measure_input, summary_update, with_raw_data_samples_requirement,
};
use crate::{schema, PerComputationCandidateError};

/// The operator that computes `measure` by `way` (not `Exact`) for
/// `result_group`: its value after its group keys, its column names not
/// yet the group's ([`add_output_candidate`] renames them). The groups it
/// reads are pushed onto `pending`; nothing is added to `memo`.
pub(super) fn measure_output(
    memo: &Memo,
    pending: &mut Pending,
    measure: &SingleMeasure<'_>,
    way: &HowToComputeAggregateMeasure,
    result_group: MemoGroupId,
) -> Result<Operator<MemoGroupId>, PerComputationCandidateError> {
    Ok(match way {
        HowToComputeAggregateMeasure::Exact => {
            unreachable!(
                "plain exact execution is the group's own expression, not a measure output"
            )
        }
        HowToComputeAggregateMeasure::ExactAccumulator(kinds) if kinds.len() == 1 => {
            let read = measure_input(
                measure.intent,
                memo,
                measure.unfiltered_input,
                measure.filter,
            )?;
            let build = accumulator_build(memo, kinds[0], measure.intent, measure, read)?;
            Operator::ASAP(ASAPOp::FinalizeExactAccumulator {
                child: pending.push(memo, build)?,
            })
        }
        HowToComputeAggregateMeasure::Summary(algorithm) => {
            let query = sketch_statistic(measure.intent).ok_or(
                PerComputationCandidateError::Unsupported("sketch evaluation"),
            )?;
            let summary_type = FieldDataType::Sketch(*algorithm);
            let read = measure_input(
                measure.intent,
                memo,
                measure.unfiltered_input,
                measure.filter,
            )?;
            let update = with_raw_data_samples_requirement(
                summary_update(
                    measure.intent,
                    &summary_type,
                    measure.reduction,
                    schema(memo, measure.unfiltered_input),
                    read,
                )?,
                measure.raw_data_samples_of,
            );
            let state = pending.push(
                memo,
                Operator::ASAP(ASAPOp::SummaryAgg {
                    child: measure.input,
                    input: update,
                    summary_type,
                    reduction: measure.reduction.clone(),
                    filter: measure.filter.cloned(),
                }),
            )?;
            let estimate = Operator::ASAP(ASAPOp::SummaryEstimate {
                summary_input: state,
                query,
            });
            match measure.intent {
                AggIntent::TopK { .. } => {
                    let estimate = pending.push(memo, estimate)?;
                    ranked_as_topk(
                        measure.reduction,
                        schema(memo, result_group),
                        pending.schema(memo, estimate),
                        estimate,
                    )?
                }
                _ => estimate,
            }
        }
        HowToComputeAggregateMeasure::ExactAccumulator(kinds) => {
            let (AggIntent::Avg { col }, [ExactKind::Sum, ExactKind::Count]) =
                (measure.intent, kinds.as_slice())
            else {
                return Err(PerComputationCandidateError::Unsupported(
                    "several exact states other than avg's sum then count",
                ));
            };
            // The count counts the averaged column, as `avg` does: it is
            // non-null (a nullable input prunes this candidate), so it counts
            // the rows the sum reads.
            let sum_intent = AggIntent::Sum { col: *col };
            let count_intent = AggIntent::Count;
            let read = measure_input(&sum_intent, memo, measure.unfiltered_input, measure.filter)?;
            let sum = accumulator_build(memo, ExactKind::Sum, &sum_intent, measure, read.clone())?;
            let count = accumulator_build(memo, ExactKind::Count, &count_intent, measure, read)?;
            let sum = pending.push(memo, sum)?;
            let count = pending.push(memo, count)?;
            let sum = pending.push(
                memo,
                Operator::ASAP(ASAPOp::FinalizeExactAccumulator { child: sum }),
            )?;
            let count = pending.push(
                memo,
                Operator::ASAP(ASAPOp::FinalizeExactAccumulator { child: count }),
            )?;
            if memo.groups()[measure.unfiltered_input.0 as usize]
                .group_properties
                .result_kind
                == OperatorResultKind::Relation
            {
                return sql_avg(
                    memo,
                    pending,
                    measure.reduction,
                    measure.filter.is_some(),
                    sum,
                    count,
                );
            }
            // `avg` is Float64 whatever the summed type, as `Avg`'s output is,
            // and a PromQL binary operator reads Float64 sample values. A
            // grouped sum or count ends with its value, cast when not Float64
            // (a count is Int64). A per-series sum keeps the input's labels,
            // so its value is the sample-value column; a per-series count is
            // Float64.
            let (sum, count) = match measure.reduction {
                Reduction::Reduce(by) => (
                    last_as_float64(memo, pending, by.len(), sum)?,
                    last_as_float64(memo, pending, by.len(), count)?,
                ),
                Reduction::PerEntity => {
                    let sum_schema = pending.schema(memo, sum);
                    let value = resolve_column_ref(&ColumnRef::SampleValue, sum_schema)
                        .ok()
                        .and_then(|column| sum_schema.fields.get(column));
                    if value.map(|field| &field.dtype)
                        != Some(&FieldDataType::Plain(DataType::Float64))
                    {
                        return Err(PerComputationCandidateError::Unsupported(
                            "non-Float64 per-series avg",
                        ));
                    }
                    (sum, count)
                }
            };
            Operator::NonASAP(NonASAPOp::BinaryOp {
                operator: BinaryOperator {
                    checked_relative_division: false,
                    // PromQL's `avg` and `avg_over_time` switch to an
                    // incremental mean when the sum overflows, so the
                    // quotient stands in only when operands and result are
                    // finite; otherwise the original query runs.
                    checked_finite_division: true,
                    kind: BinaryOpKind::Arithmetic(ArithmeticOpKind::Div),
                    vector_match: None,
                },
                return_bool: false,
                lhs: sum,
                rhs: count,
            })
        }
    })
}

/// The accumulator build of `kind` for `intent` over `measure`'s input,
/// filtered and grouped as `measure`.
pub(super) fn accumulator_build(
    memo: &Memo,
    kind: ExactKind,
    intent: &AggIntent,
    measure: &SingleMeasure<'_>,
    read: SummaryInputExpr,
) -> Result<Operator<MemoGroupId>, PerComputationCandidateError> {
    let summary_type = FieldDataType::exact(kind);
    Ok(Operator::ASAP(ASAPOp::SummaryAgg {
        input: summary_update(
            intent,
            &summary_type,
            measure.reduction,
            schema(memo, measure.unfiltered_input),
            read,
        )?,
        child: measure.input,
        summary_type,
        reduction: measure.reduction.clone(),
        filter: measure.filter.cloned(),
    }))
}

/// Adds `output` (over memo groups and `pending`) as a candidate of
/// `result_group`, then the `above` filters over it, and returns what each
/// id names once `pending` is added ([`commit_pending`]). When its column
/// names differ from the group's, `output` becomes its own group, read
/// through a `Project` that names its columns as the group does
/// ([`renaming_project`]). With `above` predicates the output (named as the
/// group) is a group, read by one `Filter` group per predicate but the last,
/// whose `Filter` is the candidate. Every check runs on the pending
/// expressions before any is added.
pub(super) fn add_output_candidate(
    memo: &mut Memo,
    result_group: MemoGroupId,
    mut pending: Pending,
    output: Operator<MemoGroupId>,
    above: &[Predicate<MemoGroupId>],
) -> Result<impl Fn(MemoGroupId) -> MemoGroupId, PerComputationCandidateError> {
    let to = schema(memo, result_group).clone();
    let output_group = pending.push(memo, output)?;
    let from = pending.schema(memo, output_group).clone();
    let candidate = match above.split_last() {
        None if from == to => pending.pop(),
        None => renaming_project(&from, &to, output_group)?,
        Some((last, filters)) => {
            let mut named = output_group;
            if from != to {
                let renamed = renaming_project(&from, &to, output_group)?;
                named = pending.push(memo, renamed)?;
            }
            for pred in filters {
                named = pending.push(
                    memo,
                    Operator::NonASAP(NonASAPOp::Filter {
                        pred: pred.clone(),
                        child: named,
                    }),
                )?;
            }
            Operator::NonASAP(NonASAPOp::Filter {
                pred: last.clone(),
                child: named,
            })
        }
    };
    let group_of = commit_pending(memo, pending, &[])?;
    let candidate = candidate.map_children(|&input| group_of(input));
    add_candidate(memo, result_group, candidate, None, Vec::new())?;
    Ok(group_of)
}

/// A `Project` over `output` (schema `from`) that names its columns as a
/// group of schema `to` does. `output` must have the group's number of
/// columns, and the group's schema must be closed (list every column its
/// rows carry).
fn renaming_project(
    from: &Schema,
    to: &Schema,
    output: MemoGroupId,
) -> Result<Operator<MemoGroupId>, PerComputationCandidateError> {
    // An open schema's rows carry columns it does not list (a PromQL
    // series whose identity is not realized as a column): a `Project` lists
    // its columns, so it would drop them, and its rows are not the group's.
    if !to.closed {
        return Err(PerComputationCandidateError::Unsupported(
            "a rename drops the columns an open schema does not list",
        ));
    }
    if from.fields.len() != to.fields.len() {
        return Err(PerComputationCandidateError::Unsupported(
            "substituted output with other columns than the aggregate",
        ));
    }
    let cols = to
        .fields
        .iter()
        .enumerate()
        .map(|(column, field)| ProjectItem {
            alias: Some(field.name.clone()),
            expr: ScalarExpr::Column(column),
        })
        .collect();
    Ok(Operator::NonASAP(NonASAPOp::Project {
        cols,
        child: output,
    }))
}

/// A heap's top-k `estimate` (schema `estimated`: `[partitions.., items..,
/// topk]`, one row per ranked item) in the columns of a result group of
/// schema `to`, as a `Project`: its `[keys.., topk_k]`, with `topk_k` the
/// estimated Float64 weight, then the item columns. This is the exact
/// top-k's physical shape (its ranked rows' value, then the columns naming
/// each ranked item: PromQL labels, SQL group keys), so consumers of
/// `[keys.., topk_k]` find those columns where they were.
pub(super) fn ranked_as_topk(
    reduction: &Reduction,
    to: &Schema,
    estimated: &Schema,
    estimate: MemoGroupId,
) -> Result<Operator<MemoGroupId>, PerComputationCandidateError> {
    let ranked_index = reduction.group_keys().map_or(0, |keys| keys.len());
    let fields = &estimated.fields;
    let estimated_value = fields.len() - 1;
    let cols = to
        .fields
        .iter()
        .enumerate()
        .map(|(index, field)| {
            let source = if index == ranked_index {
                estimated_value
            } else {
                fields[..estimated_value]
                    .iter()
                    .position(|input| input.name == field.name)
                    .ok_or(PerComputationCandidateError::Unsupported(
                        "top-k estimate without a ranked-row identity column",
                    ))?
            };
            // The estimated weight is Float64; an exact top-k of a SQL
            // count ranks Int64 values, so its column is cast to them.
            let expr = match (&fields[source].dtype, &field.dtype) {
                (FieldDataType::Plain(from), FieldDataType::Plain(to)) if from != to => {
                    ScalarExpr::Cast {
                        expr: Box::new(ScalarExpr::Column(source)),
                        to: to.clone(),
                        try_cast: false,
                    }
                }
                _ => ScalarExpr::Column(source),
            };
            Ok(ProjectItem {
                alias: Some(field.name.clone()),
                expr,
            })
        })
        .collect::<Result<Vec<_>, PerComputationCandidateError>>()?;
    Ok(Operator::NonASAP(NonASAPOp::Project {
        cols,
        child: estimate,
    }))
}

/// `group` (a memo group or pending), a grouped aggregate's `keys` group
/// keys and then its value, with the value cast to Float64 unless it
/// already is one (a pending `Project` over it).
fn last_as_float64(
    memo: &Memo,
    pending: &mut Pending,
    keys: usize,
    group: MemoGroupId,
) -> Result<MemoGroupId, PerComputationCandidateError> {
    if pending
        .schema(memo, group)
        .fields
        .last()
        .map(|field| &field.dtype)
        == Some(&FieldDataType::Plain(DataType::Float64))
    {
        return Ok(group);
    }
    let mut cols = key_items(keys);
    cols.push(ProjectItem {
        alias: None,
        expr: ScalarExpr::Cast {
            expr: Box::new(ScalarExpr::Column(keys)),
            to: DataType::Float64,
            try_cast: false,
        },
    });
    pending.push(
        memo,
        Operator::NonASAP(NonASAPOp::Project { cols, child: group }),
    )
}

/// SQL `avg` from its finalized `sum` and `count` groups (memo groups or
/// pending): joined on the group keys (NULL keys match), then
/// `sum / count` as Float64. A group with no row passing the measure
/// filter, or an empty global input, counts 0 and its `avg` is NULL.
fn sql_avg(
    memo: &Memo,
    pending: &mut Pending,
    reduction: &Reduction,
    filtered: bool,
    sum: MemoGroupId,
    count: MemoGroupId,
) -> Result<Operator<MemoGroupId>, PerComputationCandidateError> {
    let Some(by) = reduction.group_keys().filter(|by| !by.is_without()) else {
        return Err(PerComputationCandidateError::Unsupported(
            "SQL avg without `by` keys",
        ));
    };
    let keys = by.len();
    let float = |column| {
        Box::new(ScalarExpr::Cast {
            expr: Box::new(ScalarExpr::Column(column)),
            to: DataType::Float64,
            try_cast: false,
        })
    };
    let count_column = keys + 1 + keys;
    let mut avg = ScalarExpr::Arithmetic {
        op: ArithmeticOpKind::Div,
        left: float(keys),
        right: float(count_column),
        semantics: ExprSemantics::Sql,
    };
    if filtered || keys == 0 {
        avg = ScalarExpr::Case {
            operand: None,
            branches: vec![(
                ScalarExpr::Compare {
                    left: Box::new(ScalarExpr::Column(count_column)),
                    op: CompareOpKind::Eq,
                    right: Box::new(ScalarExpr::Literal(ScalarValue::Int64(0))),
                    semantics: ExprSemantics::Sql,
                },
                ScalarExpr::Literal(ScalarValue::Null),
            )],
            else_expr: Some(Box::new(avg)),
        };
    }
    let mut cols = key_items(keys);
    cols.push(ProjectItem {
        alias: None,
        expr: avg,
    });
    let join = join_on_keys(memo, pending, keys, sum, count);
    let joined = pending.push(memo, join)?;
    Ok(Operator::NonASAP(NonASAPOp::Project {
        cols,
        child: joined,
    }))
}

/// `left` joined with `right` (memo groups or pending) on their leading
/// `keys` columns; NULL keys match each other, as grouping treats them.
fn join_on_keys(
    memo: &Memo,
    pending: &Pending,
    keys: usize,
    left: MemoGroupId,
    right: MemoGroupId,
) -> Operator<MemoGroupId> {
    let width = pending.schema(memo, left).fields.len();
    let right_schema = pending.schema(memo, right);
    let column = |id| Box::new(ScalarExpr::Column(id));
    let matches = (0..keys)
        .map(|key| {
            let equal = ScalarExpr::Compare {
                left: column(key),
                op: CompareOpKind::Eq,
                right: column(width + key),
                semantics: ExprSemantics::Sql,
            };
            if right_schema.fields[key].nullable {
                ScalarExpr::BoolOr(vec![
                    equal,
                    ScalarExpr::BoolAnd(vec![
                        ScalarExpr::IsNull(column(key)),
                        ScalarExpr::IsNull(column(width + key)),
                    ]),
                ])
            } else {
                equal
            }
        })
        .collect::<Vec<_>>();
    let (kind, pred) = match matches.len() {
        0 => (
            JoinKind::Cross,
            ScalarExpr::Literal(ScalarValue::Boolean(true)),
        ),
        _ => (JoinKind::Inner, ScalarExpr::BoolAnd(matches)),
    };
    Operator::NonASAP(NonASAPOp::Join {
        kind,
        pred: Predicate(pred),
        left,
        right,
    })
}

/// Joins single-measure `parts` (each a group, memo or pending: group
/// keys, then one value) on their group keys, pushing the joins onto
/// `pending`, and projects `result_group`'s output columns. Every part has
/// the same groups, so an inner join keeps them all; NULL keys match each
/// other, as grouping treats them.
pub(super) fn join_measures(
    memo: &Memo,
    pending: &mut Pending,
    reduction: &Reduction,
    result_group: MemoGroupId,
    parts: &[MemoGroupId],
) -> Result<Operator<MemoGroupId>, PerComputationCandidateError> {
    let Some(by) = reduction.group_keys().filter(|by| !by.is_without()) else {
        return Err(PerComputationCandidateError::Unsupported(
            "multi-measure aggregate without `by` keys",
        ));
    };
    let keys = by.len();
    let width = keys + 1;
    let (&first, rest) = parts.split_first().expect("an aggregate has measures");
    let mut joined = first;
    for &part in rest {
        let join = join_on_keys(memo, pending, keys, joined, part);
        joined = pending.push(memo, join)?;
    }
    let to = schema(memo, result_group);
    let mut cols = key_items(keys);
    cols.extend((0..to.fields.len() - keys).map(|measure| ProjectItem {
        alias: None,
        expr: ScalarExpr::Column(measure * width + keys),
    }));
    for (item, field) in cols.iter_mut().zip(&to.fields) {
        item.alias = Some(field.name.clone());
    }
    Ok(Operator::NonASAP(NonASAPOp::Project {
        cols,
        child: joined,
    }))
}

/// The first `keys` columns, unrenamed.
fn key_items(keys: usize) -> Vec<ProjectItem<MemoGroupId>> {
    (0..keys)
        .map(|key| ProjectItem {
            alias: None,
            expr: ScalarExpr::Column(key),
        })
        .collect()
}

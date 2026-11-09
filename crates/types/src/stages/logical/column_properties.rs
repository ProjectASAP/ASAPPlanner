//! Value sign: the sign of each output column of a memo group (concepts
//! doc, section 3, "Value sign"), derived once from the input groups' signs
//! and, for a `Scan`, the signs the data workload declares for its source's
//! columns. Error contracts that hold only for values of one sign read it.

use std::collections::BTreeMap;

use super::{MemoGroupAsInput, ValueSign};
use crate::ir::operator::arguments::{
    BinaryOpKind, JoinKind, PromQLVectorSetOpKind, Reduction, SourceColumn,
};
use crate::ir::operator::asap::{summary_agg_schema, summary_build};
use crate::ir::operator::input::OperatorInput;
use crate::ir::operator::AggIntent;
use crate::ir::scalar::expr_ir::ScalarValue;
use crate::ir::scalar::{resolve_column_ref, ColumnRef, ProjectItem, ScalarExpr};
use crate::ir::schema::{FieldDataType, Schema};
use crate::ir::{ASAPOp, NonASAPOp, Operator};
use crate::summary_type::{ExactKind, SketchStatistic, SummaryInputExpr, SummaryUpdate};
use crate::workload::bounds::source_name;

use ValueSign::{NonNegative, NonPositive, Unknown};

type Input<'m> = MemoGroupAsInput<'m>;

/// The sign of each column of `output`, the output schema of `operator`,
/// from its input groups' `column_sign` and, for a `Scan`, `declared`.
pub(crate) fn output_column_sign(
    operator: &Operator<Input<'_>>,
    output: &Schema,
    declared: &BTreeMap<SourceColumn, ValueSign>,
) -> Vec<ValueSign> {
    let signs = match operator {
        Operator::NonASAP(op) => non_asap_signs(op, output, declared),
        Operator::ASAP(op) => asap_signs(op, output),
    };
    debug_assert_eq!(
        signs.len(),
        output.fields.len(),
        "one sign per output column of {}",
        operator.kind_name()
    );
    signs
}

fn signs_of(input: &Input<'_>) -> Vec<ValueSign> {
    input.group_properties().column_sign.clone()
}

fn non_asap_signs(
    op: &NonASAPOp<Input<'_>>,
    output: &Schema,
    declared: &BTreeMap<SourceColumn, ValueSign>,
) -> Vec<ValueSign> {
    use NonASAPOp::*;
    let unknown = || vec![Unknown; output.fields.len()];
    match op {
        Scan { source, schema, .. } => schema
            .fields
            .iter()
            .map(|field| {
                let column = SourceColumn {
                    source: source_name(source).to_owned(),
                    column: field.name.clone(),
                };
                declared.get(&column).copied().unwrap_or(Unknown)
            })
            .collect(),
        Values { .. } | PromqlVectorFromScalar(_) => unknown(),
        Filter { child, .. }
        | Sort { child, .. }
        | Limit { child, .. }
        | Dedup { child, .. }
        | TimeRange { child, .. }
        | TimeShift { child, .. }
        | PromqlSubquery { child, .. }
        | PromqlSeriesSample { child, .. }
        | PromqlInfoEnrich { child, .. } => signs_of(child),
        // The relabeled column is written; every other column is copied.
        PromqlRelabel { dst, child, .. } => {
            let mut signs = signs_of(child);
            match child.schema().column_id(dst) {
                Some(written) => signs[written] = Unknown,
                None => signs.push(Unknown),
            }
            signs
        }
        Project { cols, child, .. } => project_signs(cols, &signs_of(child)),
        Aggregate {
            reduction,
            measures,
            child,
            ..
        } => {
            if let [AggIntent::TopK { .. }] = measures.as_slice() {
                // The ranked rows, projected: each column copied.
                return match crate::ir::schema::topk::ranked_row_projection(child, reduction, "") {
                    Ok(cols) => project_signs(&cols, &signs_of(child)),
                    Err(_) => unknown(),
                };
            }
            aggregate_signs(child, reduction, measures, output)
        }
        Join {
            kind, left, right, ..
        } => {
            let mut signs = signs_of(left);
            if !matches!(kind, JoinKind::Semi | JoinKind::Anti) {
                signs.extend(signs_of(right));
            }
            signs
        }
        SetOp { left, right, .. } => agreed(&[signs_of(left), signs_of(right)]),
        Concat { children } => agreed(&children.iter().map(signs_of).collect::<Vec<_>>()),
        SQLWindowFunc { child, .. } => {
            let mut signs = signs_of(child);
            signs.push(Unknown);
            signs
        }
        // The left side's columns, its value per the operator, then the
        // labels the match adds (unknown).
        BinaryOp {
            operator, lhs, rhs, ..
        } => {
            let mut signs = signs_of(lhs);
            if let Ok(value) = resolve_column_ref(&ColumnRef::SampleValue, lhs.schema()) {
                let rhs_value = resolve_column_ref(&ColumnRef::SampleValue, rhs.schema())
                    .ok()
                    .map_or(Unknown, |column| signs_of(rhs)[column]);
                signs[value] = match &operator.kind {
                    // A filter keeps the left side's values.
                    BinaryOpKind::Compare(_)
                    | BinaryOpKind::Set(
                        PromQLVectorSetOpKind::And | PromQLVectorSetOpKind::Unless,
                    ) => signs[value],
                    BinaryOpKind::Set(PromQLVectorSetOpKind::Or) => agree(signs[value], rhs_value),
                    BinaryOpKind::CompareBool(_) => NonNegative,
                    BinaryOpKind::Arithmetic(_) => Unknown,
                };
            }
            signs.resize(output.fields.len(), Unknown);
            signs
        }
    }
}

fn asap_signs(op: &ASAPOp<Input<'_>>, output: &Schema) -> Vec<ValueSign> {
    use ASAPOp::*;
    match op {
        SummaryAgg {
            child,
            input,
            reduction,
            ..
        } => state_signs(child, input, reduction, output),
        // The build's layout without its own time column, after the
        // computed `window_end` (column 0).
        SummaryTumblingWindowChain {
            child,
            summary_type,
            input,
            reduction,
            ..
        } => match summary_agg_schema(child.schema(), summary_type, reduction) {
            Ok(build) => {
                let mut signs = state_signs(child, input, reduction, &build);
                if let Some(time) = build.time_index {
                    signs.remove(time);
                }
                signs.insert(0, Unknown);
                signs
            }
            Err(_) => vec![Unknown; output.fields.len()],
        },
        SummaryMerge { children, group_by } => {
            // The windows a `TimeRange` over a chain selects merge into the
            // chain's build over that range.
            if let (None, [child]) = (group_by, children.as_slice()) {
                if let Some(SummaryTumblingWindowChain {
                    child,
                    input,
                    reduction,
                    ..
                }) = summary_build(child)
                {
                    return state_signs(&child, &input, &reduction, output);
                }
            }
            let merged = agreed(&children.iter().map(signs_of).collect::<Vec<_>>());
            match group_by {
                None => merged,
                Some(keys) => {
                    let state = children[0]
                        .schema()
                        .fields
                        .iter()
                        .position(|field| !field.is_plain());
                    keys.iter()
                        .chain(state.as_ref())
                        .map(|&column| merged.get(column).copied().unwrap_or(Unknown))
                        .collect()
                }
            }
        }
        SummaryEstimate {
            summary_input,
            query,
        } => readout_signs(summary_input, output, |state| match query {
            SketchStatistic::Cardinality
            | SketchStatistic::FrequencyL2
            | SketchStatistic::FrequencyEntropy => NonNegative,
            SketchStatistic::Quantile { .. }
            | SketchStatistic::PointCount { .. }
            | SketchStatistic::TopK { .. } => state,
        }),
        FinalizeExactAccumulator { child } => {
            let counts = child.schema().fields.iter().any(|field| {
                matches!(
                    field.dtype,
                    FieldDataType::ExactAggregate(ExactKind::Count, _)
                )
            });
            readout_signs(
                child,
                output,
                |state| if counts { NonNegative } else { state },
            )
        }
        MaintainPopulation { child, .. } => signs_of(child),
        EvaluatePopulation { child, .. } if output == child.schema() => signs_of(child),
        EvaluatePopulation { .. }
        | SummarySubtract { .. }
        | SummaryDelete { .. }
        | SummaryJoin { .. }
        | Extension { .. } => vec![Unknown; output.fields.len()],
    }
}

/// A column item keeps its input column's sign; an expression item's sign
/// is [`expression_sign`]'s.
fn project_signs<C>(cols: &[ProjectItem<C>], input: &[ValueSign]) -> Vec<ValueSign> {
    cols.iter()
        .map(|item| expression_sign(&item.expr, input))
        .collect()
}

/// `abs` is non-negative, a negation flips its operand's sign, a numeric
/// cast keeps it and a numeric literal has its own; any other expression is
/// `Unknown`.
fn expression_sign<C>(expr: &ScalarExpr<C>, input: &[ValueSign]) -> ValueSign {
    match expr {
        ScalarExpr::Column(column) => input.get(*column).copied().unwrap_or(Unknown),
        ScalarExpr::Literal(ScalarValue::Float64(value)) => sign_of_number(*value),
        ScalarExpr::Literal(ScalarValue::Int64(value)) => sign_of_number(*value as f64),
        ScalarExpr::Negative { expr, .. } => flipped(expression_sign(expr, input)),
        ScalarExpr::Cast { expr, .. } => expression_sign(expr, input),
        // PromQL's `abs(v)` lowers to `promql_abs`, SQL's to `abs`.
        ScalarExpr::FunctionCall { name, .. }
            if name.eq_ignore_ascii_case("abs") || name == "promql_abs" =>
        {
            NonNegative
        }
        _ => Unknown,
    }
}

/// An aggregate's, in `aggregate_output_schema`'s layout: a per-entity
/// reduction keeps its input's columns in place, its value replaced by the
/// measure's; a `by` reduction lists its keys, then its measures; a
/// `without` reduction the input's other label columns, then its measures.
fn aggregate_signs(
    child: &Input<'_>,
    reduction: &Reduction,
    measures: &[AggIntent],
    output: &Schema,
) -> Vec<ValueSign> {
    let input = signs_of(child);
    let schema = child.schema();
    let sample_value = resolve_column_ref(&ColumnRef::SampleValue, schema).ok();
    let value_of = |measure: &AggIntent| measure.input_cols().first().copied().or(sample_value);
    let keys: Vec<ValueSign> = match reduction {
        Reduction::PerEntity => {
            let mut signs = input.clone();
            if let Some(measure) = measures.first() {
                if let Some(value) = value_of(measure).filter(|&value| value < signs.len()) {
                    signs[value] = measure_sign(measure, input[value]);
                }
            }
            return signs;
        }
        Reduction::Reduce(by) if by.is_without() => (0..schema.fields.len())
            .filter(|&column| {
                schema.time_index != Some(column)
                    && sample_value != Some(column)
                    && !by.keys().contains(&column)
            })
            .map(|column| input[column])
            .collect(),
        Reduction::Reduce(by) => by
            .keys()
            .iter()
            .map(|&key| input.get(key).copied().unwrap_or(Unknown))
            .collect(),
    };
    let measured: Vec<ValueSign> = measures
        .iter()
        .flat_map(|measure| {
            let sign = measure_sign(
                measure,
                value_of(measure)
                    .and_then(|value| input.get(value).copied())
                    .unwrap_or(Unknown),
            );
            // `count_values` and `absent` add label columns before their value.
            let labels = match measure {
                AggIntent::CountValues { label } => {
                    usize::from(!output.fields[..keys.len()].iter().any(|f| f.name == *label))
                }
                AggIntent::Absent { labels } => labels.len(),
                _ => 0,
            };
            std::iter::repeat_n(Unknown, labels).chain([sign])
        })
        .collect();
    keys.into_iter().chain(measured).collect()
}

/// The sign of `measure`'s value over an input value of sign `input`.
fn measure_sign(measure: &AggIntent, input: ValueSign) -> ValueSign {
    use AggIntent::*;
    match measure {
        Count
        | Cardinality { .. }
        | CountValues { .. }
        | Changes
        | Resets
        | Group
        | Absent { .. }
        | AbsentOverTime
        | PresentOverTime
        | StdDev { .. }
        | Variance { .. }
        | FrequencyL2 { .. }
        | FrequencyEntropy { .. }
        | Math(crate::ir::operator::MathFunc::Abs) => NonNegative,
        Sum { .. }
        | Avg { .. }
        | Min { .. }
        | Max { .. }
        | Quantile { .. }
        | LastOverTime
        | FirstOverTime => input,
        // A counter's rate and increase are non-negative; over a value of
        // another sign a decrease reads as a reset, so no sign is known.
        Rate | IRate | Increase if input == NonNegative => NonNegative,
        _ => Unknown,
    }
}

/// A summary state's, in `state_schema`'s layout (an aggregate of one
/// measure): its key columns keep their input's sign and its state column
/// records the sign of the values it is updated with.
fn state_signs(
    child: &Input<'_>,
    update: &SummaryUpdate,
    reduction: &Reduction,
    state_schema: &Schema,
) -> Vec<ValueSign> {
    let mut signs = aggregate_signs(
        child,
        reduction,
        &[AggIntent::Sum { col: None }],
        state_schema,
    );
    let updated_with = match &update.weight {
        SummaryInputExpr::Column(column) => resolve_column_ref(column, child.schema())
            .ok()
            .and_then(|column| signs_of(child).get(column).copied())
            .unwrap_or(Unknown),
        SummaryInputExpr::Constant(value) => sign_of_number(*value),
        SummaryInputExpr::Tuple(_) | SummaryInputExpr::EntityIdentity(_) => Unknown,
    };
    if let Some(state) = state_schema.fields.iter().position(|f| !f.is_plain()) {
        signs[state] = updated_with;
    }
    signs
}

/// A readout of summary state: the state's plain columns keep their sign
/// in place, and the value read out has `value(the state's recorded sign)`.
/// `Unknown` everywhere when the output does not keep the state's layout (a
/// top-k's rows).
fn readout_signs(
    state: &Input<'_>,
    output: &Schema,
    value: impl Fn(ValueSign) -> ValueSign,
) -> Vec<ValueSign> {
    let input = state.schema();
    if input.fields.len() != output.fields.len() {
        return vec![Unknown; output.fields.len()];
    }
    signs_of(state)
        .into_iter()
        .zip(&input.fields)
        .map(|(sign, field)| if field.is_plain() { sign } else { value(sign) })
        .collect()
}

/// Per column, the inputs' sign if they all agree, else `Unknown`.
fn agreed(inputs: &[Vec<ValueSign>]) -> Vec<ValueSign> {
    let Some((first, rest)) = inputs.split_first() else {
        return Vec::new();
    };
    first
        .iter()
        .enumerate()
        .map(|(column, &sign)| {
            rest.iter().fold(sign, |agreed, other| {
                agree(agreed, other.get(column).copied().unwrap_or(Unknown))
            })
        })
        .collect()
}

fn agree(left: ValueSign, right: ValueSign) -> ValueSign {
    if left == right {
        left
    } else {
        Unknown
    }
}

fn flipped(sign: ValueSign) -> ValueSign {
    match sign {
        NonNegative => NonPositive,
        NonPositive => NonNegative,
        Unknown => Unknown,
    }
}

fn sign_of_number(value: f64) -> ValueSign {
    if value >= 0.0 {
        NonNegative
    } else if value < 0.0 {
        NonPositive
    } else {
        Unknown
    }
}

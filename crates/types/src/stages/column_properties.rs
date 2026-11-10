//! Column lineage and value sign: two per-column group properties
//! (concepts doc, section 3, "Column lineage" and "Value sign"), derived
//! together, once per group, from the input groups' and, for a `Scan`, from
//! its source's columns and the signs the data workload declares for them.
//! One per-operator column layout says which input column each output
//! column copies (lineage and sign kept) and which it computes (lineage
//! `Computed`, the sign by the operator's rule).

use std::collections::BTreeMap;

use super::{ColumnLineage, MemoGroupAsInput, ValueSign};
use crate::ir::operator::asap::{summary_agg_schema, summary_build};
use crate::ir::operator::input::OperatorInput;
use crate::ir::operator::operator_properties::{
    BinaryOpKind, GroupSide, JoinKind, PromQLVectorSetOpKind, Reduction, SourceColumn,
};
use crate::ir::operator::AggIntent;
use crate::ir::scalar::expr_ir::ScalarValue;
use crate::ir::scalar::{resolve_column_ref, ColumnRef, ProjectItem, ScalarExpr};
use crate::ir::schema::sketch::{ExactKind, SketchStatistic, SummaryInputExpr, SummaryUpdate};
use crate::ir::schema::topk::{ranked_row_projection, topk_output_name};
use crate::ir::schema::{FieldDataType, Schema};
use crate::ir::{ASAPOp, NonASAPOp, Operator};
use crate::workload::bounds::source_name;
use crate::workload::SourceFacts;

use ValueSign::{NonNegative, NonPositive, Unknown};

/// One output column's lineage and sign.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ColumnProperties {
    pub(crate) lineage: ColumnLineage,
    pub(crate) sign: ValueSign,
}

impl ColumnProperties {
    /// A column the operator computes, with values of `sign`.
    fn computed(sign: ValueSign) -> Self {
        Self {
            lineage: ColumnLineage::Computed,
            sign,
        }
    }

    /// What two inputs' columns at one position agree on: each property
    /// where they are equal, else `Computed` / `Unknown`.
    fn agreed_with(self, other: &Self) -> Self {
        Self {
            lineage: if self.lineage == other.lineage {
                self.lineage
            } else {
                ColumnLineage::Computed
            },
            sign: agree(self.sign, other.sign),
        }
    }
}

/// The columns of a memo group, read from its `group_properties`.
pub(crate) fn columns_of_group(group: &MemoGroupAsInput<'_>) -> Vec<ColumnProperties> {
    let properties = group.group_properties();
    properties
        .column_lineage
        .iter()
        .zip(&properties.column_sign)
        .map(|(lineage, &sign)| ColumnProperties {
            lineage: lineage.clone(),
            sign,
        })
        .collect()
}

/// The lineage and sign of each column of `output`, the output schema of
/// `operator`, from its inputs' columns (`columns_of`) and, for a `Scan`,
/// the signs `source_facts` declares for its source's columns.
pub(crate) fn output_column_properties<I: OperatorInput>(
    operator: &Operator<I>,
    output: &Schema,
    source_facts: &BTreeMap<String, SourceFacts>,
    columns_of: &dyn Fn(&I) -> Vec<ColumnProperties>,
) -> Vec<ColumnProperties> {
    let columns = match operator {
        Operator::NonASAP(op) => non_asap_columns(op, output, source_facts, columns_of),
        Operator::ASAP(op) => asap_columns(op, output, columns_of),
    };
    debug_assert_eq!(
        columns.len(),
        output.fields.len(),
        "one lineage and sign per output column of {}",
        operator.kind_name()
    );
    columns
}

fn non_asap_columns<I: OperatorInput>(
    op: &NonASAPOp<I>,
    output: &Schema,
    source_facts: &BTreeMap<String, SourceFacts>,
    columns_of: &dyn Fn(&I) -> Vec<ColumnProperties>,
) -> Vec<ColumnProperties> {
    use NonASAPOp::*;
    let unknown = || vec![ColumnProperties::computed(Unknown); output.fields.len()];
    match op {
        Scan { source, schema, .. } => schema
            .fields
            .iter()
            .map(|field| {
                let column = SourceColumn {
                    source: source_name(source).to_owned(),
                    column: field.name.clone(),
                };
                ColumnProperties {
                    sign: source_facts
                        .get(&column.source)
                        .and_then(|facts| facts.column_signs.get(&column.column))
                        .copied()
                        .unwrap_or(Unknown),
                    lineage: ColumnLineage::SourceColumn(column),
                }
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
        | PromqlInfoEnrich { child, .. } => columns_of(child),
        // The relabeled column is written; every other column is copied.
        PromqlRelabel { dst, child, .. } => {
            let mut columns = columns_of(child);
            let written = ColumnProperties::computed(Unknown);
            match child.schema().column_id(dst) {
                Some(column) => columns[column] = written,
                None => columns.push(written),
            }
            columns
        }
        Project { cols, child } => project_columns(cols, &columns_of(child)),
        Aggregate {
            reduction,
            measures,
            output_names,
            child,
            ..
        } => {
            if let [AggIntent::TopK { k }] = measures.as_slice() {
                // The ranked rows, projected: each column copied.
                let name = topk_output_name(*k, output_names);
                return match ranked_row_projection(child, reduction, &name) {
                    Ok(cols) => project_columns(&cols, &columns_of(child)),
                    Err(_) => unknown(),
                };
            }
            aggregate_columns(child, reduction, measures, output, columns_of)
        }
        Join {
            kind, left, right, ..
        } => {
            let mut columns = columns_of(left);
            if !matches!(kind, JoinKind::Semi | JoinKind::Anti) {
                columns.extend(columns_of(right));
            }
            columns
        }
        SetOp { left, right, .. } => agreed(&[columns_of(left), columns_of(right)]),
        Concat { children } => agreed(&children.iter().map(columns_of).collect::<Vec<_>>()),
        SQLWindowFunc { child, .. } => {
            let mut columns = columns_of(child);
            columns.push(ColumnProperties::computed(Unknown));
            columns
        }
        // The left side's columns, its value computed per the operator, then
        // the right side's labels the match adds. A label that may come from
        // either side (`or`, `group_right`) keeps what both sides agree on.
        BinaryOp {
            operator, lhs, rhs, ..
        } => {
            let left = columns_of(lhs);
            let right = columns_of(rhs);
            let mut columns = left.clone();
            let right_named = |name: &str| {
                rhs.schema()
                    .column_id(name)
                    .map_or(ColumnProperties::computed(Unknown), |column| {
                        right[column].clone()
                    })
            };
            let right_rows = matches!(operator.kind, BinaryOpKind::Set(PromQLVectorSetOpKind::Or))
                || operator
                    .vector_match
                    .as_ref()
                    .and_then(|m| m.grouping.as_ref())
                    .is_some_and(|grouping| grouping.side == GroupSide::Right);
            if right_rows {
                for (column, field) in lhs.schema().fields.iter().enumerate() {
                    columns[column] = columns[column]
                        .clone()
                        .agreed_with(&right_named(&field.name));
                }
            }
            if let Ok(value) = resolve_column_ref(&ColumnRef::SampleValue, lhs.schema()) {
                let left_sign = left[value].sign;
                let right_sign = resolve_column_ref(&ColumnRef::SampleValue, rhs.schema())
                    .map_or(Unknown, |column| right[column].sign);
                columns[value] = ColumnProperties::computed(match &operator.kind {
                    // A filter keeps the left side's values.
                    BinaryOpKind::Compare(_)
                    | BinaryOpKind::Set(
                        PromQLVectorSetOpKind::And | PromQLVectorSetOpKind::Unless,
                    ) => left_sign,
                    BinaryOpKind::Set(PromQLVectorSetOpKind::Or) => agree(left_sign, right_sign),
                    BinaryOpKind::CompareBool(_) => NonNegative,
                    BinaryOpKind::Arithmetic(_) => Unknown,
                });
            }
            let added = output.fields.get(columns.len()..).unwrap_or_default();
            columns.extend(added.iter().map(|field| right_named(&field.name)));
            columns
        }
    }
}

fn asap_columns<I: OperatorInput>(
    op: &ASAPOp<I>,
    output: &Schema,
    columns_of: &dyn Fn(&I) -> Vec<ColumnProperties>,
) -> Vec<ColumnProperties> {
    use ASAPOp::*;
    let unknown = || vec![ColumnProperties::computed(Unknown); output.fields.len()];
    match op {
        SummaryAgg {
            child,
            input,
            reduction,
            ..
        } => state_columns(child, input, reduction, output, columns_of),
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
                let mut columns = state_columns(child, input, reduction, &build, columns_of);
                if let Some(time) = build.time_index {
                    columns.remove(time);
                }
                columns.insert(0, ColumnProperties::computed(Unknown));
                columns
            }
            Err(_) => unknown(),
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
                    return state_columns(&child, &input, &reduction, output, columns_of);
                }
            }
            let merged = agreed(&children.iter().map(columns_of).collect::<Vec<_>>());
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
                        .map(|&column| {
                            merged
                                .get(column)
                                .cloned()
                                .unwrap_or(ColumnProperties::computed(Unknown))
                        })
                        .collect()
                }
            }
        }
        SummaryEstimate {
            summary_input,
            query,
        } => readout_columns(summary_input, output, columns_of, |state| match query {
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
            readout_columns(child, output, columns_of, |state| {
                if counts {
                    NonNegative
                } else {
                    state
                }
            })
        }
        MaintainPopulation { child, .. } => columns_of(child),
        EvaluatePopulation { child, .. } if output == child.schema() => columns_of(child),
        EvaluatePopulation { .. }
        | SummarySubtract { .. }
        | SummaryDelete { .. }
        | SummaryJoin { .. }
        | Extension { .. } => unknown(),
    }
}

/// A column item, renamed or not, copies its input column; an expression
/// item is computed, with [`expression_sign`]'s sign.
fn project_columns<C>(
    cols: &[ProjectItem<C>],
    input: &[ColumnProperties],
) -> Vec<ColumnProperties> {
    let signs: Vec<ValueSign> = input.iter().map(|column| column.sign).collect();
    cols.iter()
        .map(|item| match item.expr {
            ScalarExpr::Column(column) => input
                .get(column)
                .cloned()
                .unwrap_or(ColumnProperties::computed(Unknown)),
            _ => ColumnProperties::computed(expression_sign(&item.expr, &signs)),
        })
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
/// Keys are copied; measures are computed.
fn aggregate_columns<I: OperatorInput>(
    child: &I,
    reduction: &Reduction,
    measures: &[AggIntent],
    output: &Schema,
    columns_of: &dyn Fn(&I) -> Vec<ColumnProperties>,
) -> Vec<ColumnProperties> {
    let input = columns_of(child);
    let schema = child.schema();
    let sample_value = resolve_column_ref(&ColumnRef::SampleValue, schema).ok();
    let value_of = |measure: &AggIntent| measure.input_cols().first().copied().or(sample_value);
    let input_sign = |column: Option<usize>| {
        column
            .and_then(|column| input.get(column))
            .map_or(Unknown, |column| column.sign)
    };
    let keys: Vec<ColumnProperties> = match reduction {
        Reduction::PerEntity => {
            let mut columns = input.clone();
            if let Some(measure) = measures.first() {
                if let Some(value) = value_of(measure).filter(|&value| value < columns.len()) {
                    columns[value] =
                        ColumnProperties::computed(measure_sign(measure, input[value].sign));
                }
            }
            return columns;
        }
        Reduction::Reduce(by) if by.is_without() => (0..schema.fields.len())
            .filter(|&column| {
                schema.time_index != Some(column)
                    && sample_value != Some(column)
                    && !by.keys().contains(&column)
            })
            .map(|column| input[column].clone())
            .collect(),
        Reduction::Reduce(by) => by
            .keys()
            .iter()
            .map(|&key| {
                input
                    .get(key)
                    .cloned()
                    .unwrap_or(ColumnProperties::computed(Unknown))
            })
            .collect(),
    };
    let measured: Vec<ColumnProperties> = measures
        .iter()
        .flat_map(|measure| {
            let sign = measure_sign(measure, input_sign(value_of(measure)));
            // `count_values` and `absent` add label columns before their value.
            let labels = match measure {
                AggIntent::CountValues { label } => {
                    usize::from(!output.fields[..keys.len()].iter().any(|f| f.name == *label))
                }
                AggIntent::Absent { labels } => labels.len(),
                _ => 0,
            };
            std::iter::repeat_n(ColumnProperties::computed(Unknown), labels)
                .chain([ColumnProperties::computed(sign)])
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
/// measure): its key columns copy their input's, and its state column is
/// computed and records the sign of the values it is updated with.
fn state_columns<I: OperatorInput>(
    child: &I,
    update: &SummaryUpdate,
    reduction: &Reduction,
    state_schema: &Schema,
    columns_of: &dyn Fn(&I) -> Vec<ColumnProperties>,
) -> Vec<ColumnProperties> {
    let mut columns = aggregate_columns(
        child,
        reduction,
        &[AggIntent::Sum { col: None }],
        state_schema,
        columns_of,
    );
    let updated_with = match &update.weight {
        SummaryInputExpr::Column(column) => resolve_column_ref(column, child.schema())
            .ok()
            .and_then(|column| columns_of(child).get(column).map(|column| column.sign))
            .unwrap_or(Unknown),
        SummaryInputExpr::Constant(value) => sign_of_number(*value),
        SummaryInputExpr::Tuple(_) | SummaryInputExpr::EntityIdentity(_) => Unknown,
    };
    if let Some(state) = state_schema.fields.iter().position(|f| !f.is_plain()) {
        columns[state] = ColumnProperties::computed(updated_with);
    }
    columns
}

/// A readout of summary state: the state's plain columns are copied in
/// place, and the value read out is computed, with `value(the state's
/// recorded sign)`. All computed with an unknown sign when the output does
/// not keep the state's layout (a top-k's rows).
fn readout_columns<I: OperatorInput>(
    state: &I,
    output: &Schema,
    columns_of: &dyn Fn(&I) -> Vec<ColumnProperties>,
    value: impl Fn(ValueSign) -> ValueSign,
) -> Vec<ColumnProperties> {
    let input = state.schema();
    if input.fields.len() != output.fields.len() {
        return vec![ColumnProperties::computed(Unknown); output.fields.len()];
    }
    columns_of(state)
        .into_iter()
        .zip(&input.fields)
        .map(|(column, field)| {
            if field.is_plain() {
                column
            } else {
                ColumnProperties::computed(value(column.sign))
            }
        })
        .collect()
}

/// Per column, what all inputs agree on ([`ColumnProperties::agreed_with`]).
fn agreed(inputs: &[Vec<ColumnProperties>]) -> Vec<ColumnProperties> {
    let Some((first, rest)) = inputs.split_first() else {
        return Vec::new();
    };
    first
        .iter()
        .enumerate()
        .map(|(column, properties)| {
            rest.iter().fold(properties.clone(), |agreed, other| {
                agreed.agreed_with(
                    &other
                        .get(column)
                        .cloned()
                        .unwrap_or(ColumnProperties::computed(Unknown)),
                )
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

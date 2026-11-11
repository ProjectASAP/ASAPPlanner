//! Column lineage and value sign: two per-column group properties
//! (concepts doc, section 3, "Column lineage" and "Value sign"), derived
//! together, once per group, from the input groups' and, for a `Scan`, from
//! its source's columns and the signs the data workload declares for them.
//! One per-operator column layout says which input column each output
//! column copies (lineage and sign kept) and which it computes (lineage
//! `Computed` by this group, the sign by the operator's rule). Error
//! contracts that hold only for values of one sign read the sign;
//! aggregate coverage names a selection's columns by the lineage.

use std::collections::BTreeMap;

use super::group::{MemoGroupAsInput, MemoGroupId};
use crate::ir::operator::arguments::{
    BinaryOpKind, GroupSide, JoinKind, PromQLVectorSetOpKind, Reduction, Source, SourceColumn,
};
use crate::ir::operator::asap::{summary_agg_layout, summary_build};
use crate::ir::operator::input::OperatorInput;
use crate::ir::operator::AggIntent;
use crate::ir::scalar::expr_ir::{ArithmeticOpKind, ScalarValue};
use crate::ir::scalar::{resolve_column_ref, ColumnRef, ProjectItem, ScalarExpr};
use crate::ir::schema::aggregate_schema::{aggregate_output, OutputColumnSource};
use crate::ir::schema::topk::{ranked_row_projection, topk_output_name};
use crate::ir::schema::{ColumnId, FieldDataType, Schema};
use crate::ir::{ASAPOp, NonASAPOp, Operator};
use crate::summary_type::{
    ExactKind, SketchStatistic, SummaryInputExpr, SummaryTypeCapability, SummaryUpdate,
};
use crate::workload::bounds::source_name;
use crate::workload::{series_facts, PrometheusMetricType, SourceFacts};

/// The sign of one column's values, a group property derived from the
/// input groups like lineage (concepts doc, section 3, "Value sign"); a
/// `Scan`'s are declared per column in the data workload's source facts.
/// Error contracts that hold only for values of one sign read it.
///
/// A sign is the IEEE 754 sign bit, so `-0.0` is `NonPositive` and `+0.0`
/// `NonNegative`; with it the rules are exact: a product or quotient has
/// the XOR of its operands' sign bits, a sum of two values with one sign
/// bit keeps it, a negation flips it and `abs` clears it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ValueSign {
    /// Every non-NaN value has its sign bit clear (`+0.0`, positive,
    /// `+inf`).
    NonNegative,
    /// Every non-NaN value has its sign bit set (`-0.0`, negative, `-inf`).
    NonPositive,
    /// Neither is known.
    Unknown,
}

/// Where an output column's values come from, a group property derived
/// from the input groups (concepts doc, section 3, "Column lineage"). It
/// names a column across groups where a schema field's name cannot: two
/// join sides' columns of one name have different lineage, a self-join's
/// two scans of one source have different scan ordinals, and a renamed
/// column keeps its lineage. A column passed through unchanged keeps its
/// input column's lineage, so two columns have equal lineage only when
/// they hold the same values: the same column of the same scan, or the
/// same computation. Aggregate coverage names a selection's column by its
/// source column.
#[derive(Debug, Clone, PartialEq)]
pub enum ColumnLineage {
    /// Copied unchanged from a column of one scan of a source.
    SourceColumn(SourceColumn),
    /// Column `column` of memo group `group`, where it was computed: a
    /// measure, an expression, a summary state or a readout, or a column
    /// the inputs of a union-like operator (`SetOp`, `Concat`,
    /// `SummaryMerge`) disagree on.
    Computed {
        group: MemoGroupId,
        column: ColumnId,
    },
}

use ValueSign::{NonNegative, NonPositive, Unknown};

type Input<'m> = MemoGroupAsInput<'m>;

/// One output column's lineage and sign.
#[derive(Clone)]
struct ColumnProperties {
    /// `None` for a column the group being derived computes: its lineage
    /// is `Computed` by that group at the column's final position.
    lineage: Option<ColumnLineage>,
    sign: ValueSign,
}

/// A column the operator computes, with no sign known.
impl Default for ColumnProperties {
    fn default() -> Self {
        Self::computed(Unknown)
    }
}

/// `columns` columns the operator computes, with no sign known.
fn all_computed(columns: usize) -> Vec<ColumnProperties> {
    vec![ColumnProperties::default(); columns]
}

impl ColumnProperties {
    /// A column the operator computes, with values of `sign`.
    fn computed(sign: ValueSign) -> Self {
        Self {
            lineage: None,
            sign,
        }
    }

    /// What two inputs' columns at one position agree on: each property
    /// where they are equal, else computed by this group / `Unknown`.
    fn agreed_with(self, other: &Self) -> Self {
        Self {
            lineage: self
                .lineage
                .filter(|lineage| other.lineage.as_ref() == Some(lineage)),
            sign: agree(self.sign, other.sign),
        }
    }
}

/// The lineage and sign of each column of `output`, the output schema of
/// `operator` computed by memo group `group`, from its input groups'
/// `column_lineage` and `column_signs` and, for a `Scan`, its source and
/// `source_facts`.
pub(super) fn column_lineage_and_signs(
    group: MemoGroupId,
    operator: &Operator<Input<'_>>,
    output: &Schema,
    source_facts: &BTreeMap<String, SourceFacts>,
) -> (Vec<ColumnLineage>, Vec<ValueSign>) {
    let columns = match operator {
        Operator::NonASAP(op) => non_asap_columns(op, output, source_facts),
        Operator::ASAP(op) => asap_columns(op, output),
    };
    debug_assert_eq!(
        columns.len(),
        output.fields.len(),
        "one lineage and sign per output column of {}",
        operator.kind_name()
    );
    columns
        .into_iter()
        .enumerate()
        .map(|(index, properties)| {
            let lineage = properties.lineage.unwrap_or(ColumnLineage::Computed {
                group,
                column: index,
            });
            (lineage, properties.sign)
        })
        .unzip()
}

/// The columns of a memo group, read from its `group_properties`.
fn columns_of(input: &Input<'_>) -> Vec<ColumnProperties> {
    let properties = input.group_properties();
    properties
        .column_lineage
        .iter()
        .zip(&properties.column_signs)
        .map(|(lineage, &sign)| ColumnProperties {
            lineage: Some(lineage.clone()),
            sign,
        })
        .collect()
}

/// `right`, the right side's columns of a `Join`, which concatenates them
/// after `left`'s: a source column of a source `left` also
/// scans moves to the scans after `left`'s (its scan ordinal shifted by
/// `left`'s largest for that source, plus one), so a self-join's two sides
/// name different scans.
fn after_left_scans(
    left: &[ColumnProperties],
    mut right: Vec<ColumnProperties>,
) -> Vec<ColumnProperties> {
    let mut first_free_scan: BTreeMap<&Source, u32> = BTreeMap::new();
    for column in left {
        if let Some(ColumnLineage::SourceColumn(source_column)) = &column.lineage {
            let free = first_free_scan.entry(&source_column.source).or_default();
            *free = (*free).max(source_column.scan_ordinal + 1);
        }
    }
    for column in &mut right {
        if let Some(ColumnLineage::SourceColumn(source_column)) = &mut column.lineage {
            if let Some(shift) = first_free_scan.get(&source_column.source) {
                source_column.scan_ordinal += shift;
            }
        }
    }
    right
}

fn non_asap_columns(
    op: &NonASAPOp<Input<'_>>,
    output: &Schema,
    source_facts: &BTreeMap<String, SourceFacts>,
) -> Vec<ColumnProperties> {
    use NonASAPOp::*;
    match op {
        // Each column copied from its source, with the sign its source's
        // facts declare (a histogram's or summary's series reads its
        // family's); a counter's `value` is non-negative without a
        // declaration.
        Scan { source, schema, .. } => {
            let facts = series_facts(source_facts, source_name(source));
            schema
                .fields
                .iter()
                .map(|field| ColumnProperties {
                    sign: match facts {
                        Some((facts, series_type)) => facts
                            .column_signs
                            .get(&field.name)
                            .copied()
                            .or((field.name == "value"
                                && series_type == Some(PrometheusMetricType::Counter))
                            .then_some(NonNegative))
                            .unwrap_or(Unknown),
                        None => Unknown,
                    },
                    lineage: Some(ColumnLineage::SourceColumn(SourceColumn {
                        source: source.clone(),
                        scan_ordinal: 0,
                        column: field.name.clone(),
                    })),
                })
                .collect()
        }
        Values { .. } => all_computed(output.fields.len()),
        // The scalar's value, every other column `Unknown`.
        PromqlVectorFromScalar(expr) => {
            let mut columns = all_computed(output.fields.len());
            if let Ok(value) = resolve_column_ref(&ColumnRef::SampleValue, output) {
                columns[value] = ColumnProperties::computed(expression_sign(expr, &[]));
            }
            columns
        }
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
            let written = ColumnProperties::default();
            match child.schema().column_id(dst) {
                Some(column) => columns[column] = written,
                None => columns.push(written),
            }
            columns
        }
        Project { cols, child, .. } => project_columns(cols, child),
        Aggregate {
            reduction,
            measures,
            output_names,
            child,
            ..
        } => {
            if let [AggIntent::TopK { k, .. }] = measures.as_slice() {
                // The ranked rows, projected: each column copied.
                let name = topk_output_name(*k, output_names);
                return match ranked_row_projection(child, reduction, &name) {
                    Ok(cols) => project_columns(&cols, child),
                    Err(_) => all_computed(output.fields.len()),
                };
            }
            aggregate_columns(child, reduction, measures, output)
        }
        Join {
            kind, left, right, ..
        } => {
            let mut columns = columns_of(left);
            if !matches!(kind, JoinKind::Semi | JoinKind::Anti) {
                let right = after_left_scans(&columns, columns_of(right));
                columns.extend(right);
            }
            columns
        }
        SetOp { left, right, .. } => agreed(&[columns_of(left), columns_of(right)]),
        Concat { children } => agreed(&children.iter().map(columns_of).collect::<Vec<_>>()),
        SQLWindowFunc { child, .. } => {
            let mut columns = columns_of(child);
            columns.push(ColumnProperties::default());
            columns
        }
        // The output side's columns (the left's; the right's, the "many"
        // side, for `group_right`), its value computed per the operator,
        // then the right side's labels the match adds. A matched label has
        // equal values on both sides, so it takes the output side's lineage
        // and sign; `or`, a union, keeps what both sides agree on (a label
        // one side lacks is computed). A label
        // `group_left(...)` names comes from the right ("one") side and one
        // `group_right(...)` names from the left, overwriting a column of
        // that name. The output does not concatenate both sides' columns,
        // so no scan ordinal shifts.
        BinaryOp {
            operator, lhs, rhs, ..
        } => {
            let left = columns_of(lhs);
            let right = columns_of(rhs);
            let named = |input: &Input<'_>, columns: &[ColumnProperties], name: &str| {
                input
                    .schema()
                    .column_id(name)
                    .and_then(|column| columns.get(column).cloned())
                    .unwrap_or_default()
            };
            let grouping = operator
                .vector_match
                .as_ref()
                .and_then(|m| m.grouping.as_ref());
            let output_side_is_right =
                grouping.is_some_and(|grouping| grouping.side == GroupSide::Right);
            let is_or = matches!(operator.kind, BinaryOpKind::Set(PromQLVectorSetOpKind::Or));
            let mut columns: Vec<ColumnProperties> = lhs
                .schema()
                .fields
                .iter()
                .enumerate()
                .map(|(column, field)| {
                    let from_right = || named(rhs, &right, &field.name);
                    if output_side_is_right {
                        from_right()
                    } else if is_or {
                        left[column].clone().agreed_with(&from_right())
                    } else {
                        left[column].clone()
                    }
                })
                .collect();
            if let Ok(value) = resolve_column_ref(&ColumnRef::SampleValue, lhs.schema()) {
                let left_sign = left[value].sign;
                let right_sign = resolve_column_ref(&ColumnRef::SampleValue, rhs.schema())
                    .ok()
                    .and_then(|column| right.get(column))
                    .map_or(Unknown, |column| column.sign);
                columns[value] = ColumnProperties::computed(match &operator.kind {
                    // A filter keeps the left side's values.
                    BinaryOpKind::Compare(_)
                    | BinaryOpKind::Set(
                        PromQLVectorSetOpKind::And | PromQLVectorSetOpKind::Unless,
                    ) => left_sign,
                    BinaryOpKind::Set(PromQLVectorSetOpKind::Or) => agree(left_sign, right_sign),
                    BinaryOpKind::CompareBool(_) => NonNegative,
                    BinaryOpKind::Arithmetic(op) => arithmetic_sign(op, left_sign, right_sign),
                });
            }
            // Under `or` a label only the right side has is null on the left
            // side's rows: computed, like a label only the left side has.
            let added = output.fields.get(columns.len()..).unwrap_or_default();
            columns.extend(added.iter().map(|field| {
                if is_or {
                    ColumnProperties::default()
                } else {
                    named(rhs, &right, &field.name)
                }
            }));
            columns.truncate(output.fields.len());
            if let Some(grouping) = grouping {
                let (one_side, one_side_columns) = match grouping.side {
                    GroupSide::Left => (rhs, &right),
                    GroupSide::Right => (lhs, &left),
                };
                for label in &grouping.labels {
                    if let Some(column) = output.column_id(label) {
                        columns[column] = named(one_side, one_side_columns, label);
                    }
                }
            }
            columns
        }
    }
}

fn asap_columns(op: &ASAPOp<Input<'_>>, output: &Schema) -> Vec<ColumnProperties> {
    use ASAPOp::*;
    match op {
        SummaryAgg {
            child,
            summary_type,
            input,
            reduction,
            ..
        } => state_columns(child, summary_type, input, reduction)
            .map_or_else(|| all_computed(output.fields.len()), |(_, columns)| columns),
        // The build's layout without its own time column, after the
        // computed `window_end` (column 0).
        SummaryTumblingWindowChain {
            child,
            summary_type,
            input,
            reduction,
            ..
        } => match state_columns(child, summary_type, input, reduction) {
            Some((build, mut columns)) => {
                if let Some(time) = build.time_index {
                    columns.remove(time);
                }
                columns.insert(0, ColumnProperties::default());
                columns
            }
            None => all_computed(output.fields.len()),
        },
        SummaryMerge { children, group_by } => {
            // The windows a `TimeRange` over a chain selects merge into the
            // chain's build over that range.
            if let (None, [child]) = (group_by, children.as_slice()) {
                if let Some(SummaryTumblingWindowChain {
                    child,
                    summary_type,
                    input,
                    reduction,
                    ..
                }) = summary_build(child)
                {
                    return state_columns(&child, &summary_type, &input, &reduction)
                        .map_or_else(|| all_computed(output.fields.len()), |(_, columns)| columns);
                }
            }
            let merged = agreed(&children.iter().map(columns_of).collect::<Vec<_>>());
            match group_by {
                None => merged,
                // The keys copied; the state, merged across the inputs'
                // groups, computed with the sign they agree on.
                Some(keys) => {
                    let column_at = |column: usize| merged.get(column).cloned().unwrap_or_default();
                    let state = children[0]
                        .schema()
                        .fields
                        .iter()
                        .position(|field| !field.is_plain())
                        .map(|state| ColumnProperties::computed(column_at(state).sign));
                    keys.iter()
                        .map(|&key| column_at(key))
                        .chain(state)
                        .collect()
                }
            }
        }
        SummaryEstimate {
            summary_input,
            query,
        } => readout_columns(summary_input, output, |state, summary_type| match query {
            SketchStatistic::Cardinality
            | SketchStatistic::FrequencyL2
            | SketchStatistic::FrequencyEntropy => NonNegative,
            SketchStatistic::Quantile { q } => quantile_sign(*q, state),
            // A count read from a state whose counts may flip the weights'
            // sign has none.
            SketchStatistic::PointCount { .. } | SketchStatistic::TopK { .. }
                if SummaryTypeCapability::point_counts_may_flip_sign(summary_type) =>
            {
                Unknown
            }
            SketchStatistic::PointCount { .. } | SketchStatistic::TopK { .. } => state,
        }),
        FinalizeExactAccumulator { child } => {
            let counts = child.schema().fields.iter().any(|field| {
                matches!(
                    field.dtype,
                    FieldDataType::ExactAggregate(ExactKind::Count, _)
                )
            });
            readout_columns(
                child,
                output,
                |state, _| if counts { NonNegative } else { state },
            )
        }
        MaintainPopulation { child, .. } => columns_of(child),
        EvaluatePopulation { child, .. } if output == child.schema() => columns_of(child),
        EvaluatePopulation { .. }
        | SummarySubtract { .. }
        | SummaryDelete { .. }
        | SummaryJoin { .. }
        | Extension { .. } => all_computed(output.fields.len()),
    }
}

/// The columns of `cols`, a `Project` over `child`: lineage by
/// [`project_lineage`], sign by [`expression_sign`].
fn project_columns<C>(cols: &[ProjectItem<C>], child: &Input<'_>) -> Vec<ColumnProperties> {
    let input = child.group_properties();
    let input_lineage: Vec<Option<ColumnLineage>> =
        input.column_lineage.iter().cloned().map(Some).collect();
    project_lineage(cols, &input_lineage)
        .into_iter()
        .zip(cols)
        .map(|(lineage, item)| ColumnProperties {
            lineage,
            sign: expression_sign(&item.expr, &input.column_signs),
        })
        .collect()
}

/// The lineage of the output columns of `cols`, a `Project` over input
/// columns of lineage `input` (`None`: computed by the group being
/// derived): a column item, renamed or not, copies its input column's; an
/// expression item is `None`, computed by the `Project`'s group.
pub(super) fn project_lineage<C>(
    cols: &[ProjectItem<C>],
    input: &[Option<ColumnLineage>],
) -> Vec<Option<ColumnLineage>> {
    cols.iter()
        .map(|item| match item.expr {
            ScalarExpr::Column(column) => input.get(column).cloned().flatten(),
            _ => None,
        })
        .collect()
}

/// `abs` is non-negative, a negation flips its operand's sign, a numeric
/// cast keeps it, a numeric literal has its own, and arithmetic follows
/// [`arithmetic_sign`]; any other expression is `Unknown`.
fn expression_sign<C>(expr: &ScalarExpr<C>, input: &[ValueSign]) -> ValueSign {
    match expr {
        ScalarExpr::Column(column) => input.get(*column).copied().unwrap_or(Unknown),
        ScalarExpr::Literal(ScalarValue::Float64(value)) => sign_of_number(*value),
        ScalarExpr::Literal(ScalarValue::Int64(value)) => sign_of_number(*value as f64),
        ScalarExpr::Negative { expr, .. } => flipped(expression_sign(expr, input)),
        ScalarExpr::Cast { expr, .. } => expression_sign(expr, input),
        ScalarExpr::Arithmetic {
            op, left, right, ..
        } => arithmetic_sign(
            op,
            expression_sign(left, input),
            expression_sign(right, input),
        ),
        // PromQL's `abs(v)` lowers to `promql_abs`, SQL's to `abs`.
        ScalarExpr::FunctionCall { name, .. }
            if name.eq_ignore_ascii_case("abs") || name == "promql_abs" =>
        {
            NonNegative
        }
        _ => Unknown,
    }
}

/// An aggregate's, by where each output column comes from
/// ([`aggregate_output`]): a kept input column is copied, a measure's
/// value is computed with [`measure_sign`] of its input column's sign, and
/// a label a measure adds is computed with no sign.
fn aggregate_columns(
    child: &Input<'_>,
    reduction: &Reduction,
    measures: &[AggIntent],
    output: &Schema,
) -> Vec<ColumnProperties> {
    let Ok((_, sources)) = aggregate_output(child.schema(), reduction, measures, &[]) else {
        return all_computed(output.fields.len());
    };
    columns_from_sources(child, &sources, |index, input| {
        measure_sign(&measures[index], input)
    })
}

/// Each output column from `sources`: an input column is copied, measure
/// `index` is computed with `measure(index, the sign of its input
/// column)`, and an added label is computed with no sign.
fn columns_from_sources(
    child: &Input<'_>,
    sources: &[OutputColumnSource],
    measure: impl Fn(usize, ValueSign) -> ValueSign,
) -> Vec<ColumnProperties> {
    let input = columns_of(child);
    let column_at = |column: Option<usize>| {
        column
            .and_then(|column| input.get(column).cloned())
            .unwrap_or_default()
    };
    sources
        .iter()
        .map(|source| match source {
            OutputColumnSource::InputColumn(column) => column_at(Some(*column)),
            OutputColumnSource::Measure {
                index,
                input_column,
            } => ColumnProperties::computed(measure(*index, column_at(*input_column).sign)),
            OutputColumnSource::AddedLabel => ColumnProperties::default(),
        })
        .collect()
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
        Sum { .. } | Avg { .. } | Min { .. } | Max { .. } | LastOverTime | FirstOverTime => input,
        Quantile { q, .. } => quantile_sign(*q, input),
        // A counter's rate and increase are non-negative; over a value of
        // another sign a decrease reads as a reset, so no sign is known.
        Rate | IRate | Increase if input == NonNegative => NonNegative,
        _ => Unknown,
    }
}

/// A summary state's schema and columns, in its layout
/// ([`summary_agg_layout`]): its key columns copy their input's and its
/// state column is computed and records the sign of the values it is
/// updated with; `None` when the layout does not derive.
fn state_columns(
    child: &Input<'_>,
    summary_type: &FieldDataType,
    update: &SummaryUpdate,
    reduction: &Reduction,
) -> Option<(Schema, Vec<ColumnProperties>)> {
    let (schema, sources) = summary_agg_layout(child.schema(), summary_type, reduction).ok()?;
    let updated_with = match &update.weight {
        SummaryInputExpr::Column(column) => resolve_column_ref(column, child.schema())
            .ok()
            .and_then(|column| child.group_properties().column_signs.get(column).copied())
            .unwrap_or(Unknown),
        SummaryInputExpr::Constant(value) => sign_of_number(*value),
        SummaryInputExpr::Tuple(_) | SummaryInputExpr::EntityIdentity(_) => Unknown,
    };
    let columns = columns_from_sources(child, &sources, |_, _| updated_with);
    Some((schema, columns))
}

/// A readout of summary state: the state's plain columns are copied in
/// place, and the value read out is computed, with `value(the state's
/// recorded sign, the state column's type)`. All computed with no sign
/// when the output does not keep the state's layout (a top-k's rows).
fn readout_columns(
    state: &Input<'_>,
    output: &Schema,
    value: impl Fn(ValueSign, &FieldDataType) -> ValueSign,
) -> Vec<ColumnProperties> {
    let input = state.schema();
    if input.fields.len() != output.fields.len() {
        return all_computed(output.fields.len());
    }
    columns_of(state)
        .into_iter()
        .zip(&input.fields)
        .map(|(column, field)| {
            if field.is_plain() {
                column
            } else {
                ColumnProperties::computed(value(column.sign, &field.dtype))
            }
        })
        .collect()
}

/// The sign of `left op right`: a sum of two values of one sign has that
/// sign; a product or quotient of two values of one sign is non-negative,
/// of opposite signs non-positive; any other operator, or an operand of
/// unknown sign, gives `Unknown`. PromQL's vector arithmetic and scalar
/// expressions both read it.
fn arithmetic_sign(op: &ArithmeticOpKind, left: ValueSign, right: ValueSign) -> ValueSign {
    match (op, left, right) {
        (ArithmeticOpKind::Add, NonNegative, NonNegative) => NonNegative,
        (ArithmeticOpKind::Add, NonPositive, NonPositive) => NonPositive,
        (ArithmeticOpKind::Mul | ArithmeticOpKind::Div, NonNegative, NonNegative)
        | (ArithmeticOpKind::Mul | ArithmeticOpKind::Div, NonPositive, NonPositive) => NonNegative,
        (ArithmeticOpKind::Mul | ArithmeticOpKind::Div, NonNegative, NonPositive)
        | (ArithmeticOpKind::Mul | ArithmeticOpKind::Div, NonPositive, NonNegative) => NonPositive,
        _ => Unknown,
    }
}

/// A quantile's sign: the input's for 0 ≤ q ≤ 1; PromQL gives ±Inf
/// outside it, so `Unknown`.
fn quantile_sign(q: f64, input: ValueSign) -> ValueSign {
    if (0.0..=1.0).contains(&q) {
        input
    } else {
        Unknown
    }
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
                agreed.agreed_with(&other.get(column).cloned().unwrap_or_default())
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

/// The sign of a number: its sign bit; `Unknown` for NaN.
fn sign_of_number(value: f64) -> ValueSign {
    if value.is_nan() {
        Unknown
    } else if value.is_sign_negative() {
        NonPositive
    } else {
        NonNegative
    }
}

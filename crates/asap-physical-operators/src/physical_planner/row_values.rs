//! Query-time PromQL value computation over logical row schemas.
use super::*;
use planner_types::post_asap::{maintained_population::PopulationReadout, BinaryOperator};
use planner_types::pre_asap::{
    BinaryOpKind, DataType, Predicate, ScalarValue, VectorMatch, VectorMatchKind,
};
use std::rc::Rc;

/// A PromQL number literal has no row schema; its consumer folds it in.
pub(super) fn scalar_literal(expression: &QueryExpr) -> Option<f64> {
    match expression {
        QueryExpr::PromqlScalarBridge(child) => scalar_literal(child),
        QueryExpr::Literal(ScalarValue::Float64(value)) => Some(*value),
        _ => None,
    }
}

/// Rows without a time column, label map, or series identity carry only
/// their group labels, so those labels are the complete PromQL identity.
fn grouped_value(input: &Schema) -> Result<(usize, Vec<usize>), Error> {
    if input.time_index.is_some()
        || input
            .fields
            .iter()
            .any(|field| field.name == promql_rows::SERIES_IDENTITY_COLUMN)
    {
        return Err(invalid(
            "row binary requires grouped rows or rows with a series identity",
        ));
    }
    let mut value = None;
    let mut labels = Vec::new();
    for (i, field) in input.fields.iter().enumerate() {
        match &field.dtype {
            SummaryFamilyType::Plain(DataType::Float64) if value.is_none() => value = Some(i),
            SummaryFamilyType::Plain(DataType::Utf8) => labels.push(i),
            _ => {
                return Err(invalid(
                    "row binary requires Utf8 labels and one Float64 value",
                ))
            }
        }
    }
    Ok((
        value.ok_or_else(|| invalid("row binary requires a Float64 value"))?,
        labels,
    ))
}

fn arithmetic(operator: &BinaryOperator) -> Result<(), Error> {
    if !matches!(operator.kind, BinaryOpKind::Arithmetic(_)) {
        return Err(invalid(
            "row comparison requires filter or bool semantics, which Binary does not carry",
        ));
    }
    Ok(())
}

/// Rows whose labels are the encoded PromQL series identity, such as
/// per-series readouts of stored state.
pub(super) fn per_series(input: &Schema) -> bool {
    input
        .fields
        .iter()
        .any(|field| field.name == promql_rows::SERIES_IDENTITY_COLUMN)
}

/// Apply `vector op scalar` (or `scalar op vector`) to each row's value.
pub(super) fn scalar_binary(
    input: &Schema,
    operator: &BinaryOperator,
    literal: f64,
    literal_left: bool,
) -> Result<Operator, Error> {
    arithmetic(operator)?;
    let (value, _) = grouped_value(input)?;
    literal_projection(input, value, operator, literal, literal_left)
}

/// `scalar_binary` over per-series rows: PromQL arithmetic also drops the
/// metric name from the series identity. Returns a chain.
pub(super) fn series_scalar_binary(
    input: &Schema,
    operator: &BinaryOperator,
    literal: f64,
    literal_left: bool,
) -> Result<Vec<Operator>, Error> {
    arithmetic(operator)?;
    let relabel = Operator::series_labels(input.clone(), VectorMatchKind::Ignoring, vec![])?;
    let value = input
        .fields
        .iter()
        .position(|field| {
            field.dtype == SummaryFamilyType::Plain(DataType::Float64) && !field.nullable
        })
        .ok_or_else(|| invalid("per-series rows require a Float64 value"))?;
    let project = literal_projection(input, value, operator, literal, literal_left)?;
    Ok(vec![relabel, project])
}

/// PromQL one-to-one arithmetic where either side carries a series identity.
/// Returns the left and right relabelings to the matching labels, and the
/// binary over their outputs.
pub(super) fn series_vector_binary(
    left: &Schema,
    right: &Schema,
    operator: &BinaryOperator,
) -> Result<[Operator; 3], Error> {
    arithmetic(operator)?;
    let (kind, labels) = match &operator.vector_match {
        None => (VectorMatchKind::Ignoring, vec![]),
        Some(VectorMatch {
            kind,
            labels,
            grouping: None,
        }) => (kind.clone(), labels.clone()),
        Some(_) => return Err(invalid("group_left/group_right matching is unsupported")),
    };
    let left_labels = Operator::series_labels(left.clone(), kind.clone(), labels.clone())?;
    let right_labels = Operator::series_labels(right.clone(), kind, labels)?;
    let binary = Operator::series_binary(
        left_labels.schema(),
        right_labels.schema(),
        BinaryOperator {
            vector_match: None,
            ..operator.clone()
        },
    )?;
    Ok([left_labels, right_labels, binary])
}

fn literal_projection(
    input: &Schema,
    value: usize,
    operator: &BinaryOperator,
    literal: f64,
    literal_left: bool,
) -> Result<Operator, Error> {
    let literal = Expression::Literal {
        value: crate::values::Value::Float64(literal),
        dtype: DataType::Float64,
    };
    let columns = input
        .fields
        .iter()
        .enumerate()
        .map(|(i, field)| {
            let expression = if i != value {
                Expression::Column(i)
            } else if literal_left {
                binary(operator, literal.clone(), Expression::Column(i))
            } else {
                binary(operator, Expression::Column(i), literal.clone())
            };
            (field.name.clone(), expression)
        })
        .collect();
    Operator::project(input.clone(), columns)
}

/// One-to-one PromQL matching of grouped rows on equal label sets. Returns
/// the inner equi-join and the projection that applies the operator.
pub(super) fn grouped_binary(
    left: &Schema,
    right: &Schema,
    operator: &BinaryOperator,
) -> Result<(Operator, Operator), Error> {
    arithmetic(operator)?;
    let (left_value, left_labels) = grouped_value(left)?;
    let (right_value, right_labels) = grouped_value(right)?;
    if left_labels.len() != right_labels.len() {
        return Err(invalid("row binary inputs have different label sets"));
    }
    let width = left.fields.len();
    let keys = left_labels
        .iter()
        .map(|&l| {
            let name = &left.fields[l].name;
            let r = right_labels
                .iter()
                .copied()
                .find(|&r| &right.fields[r].name == name)
                .ok_or_else(|| invalid("row binary inputs have different label sets"))?;
            let (a, b) = (
                Rc::new(QueryExpr::Column(l)),
                Rc::new(QueryExpr::Column(width + r)),
            );
            let equal = QueryExpr::Compare {
                left: a.clone(),
                op: CompareOpKind::Eq,
                right: b.clone(),
            };
            // A nullable label compares like PromQL's empty label: absent on both sides matches.
            Ok(if left.fields[l].nullable || right.fields[r].nullable {
                QueryExpr::BoolOr(vec![
                    equal,
                    QueryExpr::BoolAnd(vec![QueryExpr::IsNull(a), QueryExpr::IsNull(b)]),
                ])
            } else {
                equal
            })
        })
        .collect::<Result<Vec<_>, Error>>()?;
    let predicate = Predicate(Rc::new(QueryExpr::BoolAnd(keys)));
    let mut joined = left.fields.clone();
    joined.extend(right.fields.iter().cloned());
    let join = Operator::relational_join(
        left.clone(),
        right.clone(),
        planner_types::pre_asap::JoinKind::Inner,
        &predicate,
        Arc::new(planner_types::post_asap::SummarySchema {
            fields: joined,
            time_index: None,
        }),
    )?;
    let columns = left
        .fields
        .iter()
        .enumerate()
        .map(|(i, field)| {
            let expression = if i == left_value {
                binary(
                    operator,
                    Expression::Column(i),
                    Expression::Column(width + right_value),
                )
            } else {
                Expression::Column(i)
            };
            (field.name.clone(), expression)
        })
        .collect();
    let project = Operator::project(join.schema(), columns)?;
    Ok((join, project))
}

fn binary(operator: &BinaryOperator, left: Expression, right: Expression) -> Expression {
    Expression::Binary {
        operator: operator.clone(),
        left: Box::new(left),
        right: Box::new(right),
    }
}

/// Aggregate readouts of a maintained current-series population, as a chain.
pub(super) fn population_aggregate(
    input: &Schema,
    grouping: &[String],
    readout: &PopulationReadout,
) -> Result<Vec<Operator>, Error> {
    let groups = grouping
        .iter()
        .map(|name| named_column(input, &ColumnRef::Named(name.clone())))
        .collect::<Result<Vec<_>, _>>()?;
    let value = named_column(input, &ColumnRef::SampleValue)?;
    let reduction = match readout {
        PopulationReadout::Sum => Reduction::Sum(value),
        PopulationReadout::Count => Reduction::Count,
        PopulationReadout::Average => Reduction::Avg(value),
        PopulationReadout::Quantile { q } => Reduction::Quantile {
            column: value,
            q: *q,
        },
        PopulationReadout::TopK { .. } => {
            return Err(invalid(
                "TopK population readout ranks; it does not aggregate",
            ))
        }
    };
    if !groups.is_empty() {
        return Ok(vec![Operator::aggregate(
            input.clone(),
            groups,
            vec![("value".into(), reduction)],
        )?]);
    }
    // A global aggregate over no members is an empty PromQL vector, not one row.
    let aggregate = Operator::aggregate(
        input.clone(),
        vec![],
        vec![
            ("value".into(), reduction),
            ("members".into(), Reduction::Count),
        ],
    )?;
    let zero = Expression::Literal {
        value: crate::values::Value::Int64(0),
        dtype: DataType::Int64,
    };
    let filter = Operator::filter(
        aggregate.schema(),
        Expression::Less(Box::new(zero), Box::new(Expression::Column(1))),
    )?;
    let project = Operator::project(
        filter.schema(),
        vec![("value".into(), Expression::Column(0))],
    )?;
    Ok(vec![aggregate, filter, project])
}

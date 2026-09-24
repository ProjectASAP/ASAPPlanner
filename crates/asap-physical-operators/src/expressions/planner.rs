//! Planner scalar expressions evaluated over native typed rows.
use crate::{
    values::{Schema, Value},
    Error,
};
use planner_types::pre_asap::{ArithmeticOpKind, CompareOpKind, DataType, QueryExpr, ScalarValue};
use std::{cmp::Ordering, sync::Arc};

pub(super) fn evaluate(
    expr: &QueryExpr,
    row: &[Value],
    schema: &planner_types::pre_asap::Schema,
) -> Result<Value, Error> {
    match expr {
        QueryExpr::Column(index) => row.get(*index).cloned().ok_or(Error::Invalid(format!(
            "column {index} outside row width {}",
            row.len()
        ))),
        QueryExpr::Literal(value) => Ok(match value {
            ScalarValue::Interval {
                months,
                days,
                nanos,
            } => Value::Interval {
                months: *months,
                days: *days,
                nanos: *nanos,
            },
            ScalarValue::Int64(value) => Value::Int64(*value),
            ScalarValue::Float64(value) => Value::Float64(*value),
            ScalarValue::Utf8(value) => Value::Utf8(value.clone().into()),
            ScalarValue::Boolean(value) => Value::Bool(*value),
            ScalarValue::Null => Value::Null,
        }),
        QueryExpr::Compare { left, op, right } => {
            let left = evaluate(left, row, schema)?;
            let right = evaluate(right, row, schema)?;
            compare(op, left, right)
        }
        QueryExpr::Arithmetic { op, left, right } => arithmetic(
            op,
            evaluate(left, row, schema)?,
            evaluate(right, row, schema)?,
        ),
        QueryExpr::BoolAnd(parts) | QueryExpr::BoolOr(parts) => {
            let and = matches!(expr, QueryExpr::BoolAnd(_));
            let mut null = false;
            for part in parts {
                match evaluate(part, row, schema)? {
                    Value::Bool(value) if value != and => return Ok(Value::Bool(value)),
                    Value::Bool(_) => {}
                    Value::Null => null = true,
                    _ => return Err(Error::Invalid("boolean predicate required".into())),
                }
            }
            Ok(if null { Value::Null } else { Value::Bool(and) })
        }
        QueryExpr::Not(value) => match evaluate(value, row, schema)? {
            Value::Bool(value) => Ok(Value::Bool(!value)),
            Value::Null => Ok(Value::Null),
            _ => Err(Error::Invalid("boolean predicate required".into())),
        },
        QueryExpr::IsNull(value) => Ok(Value::Bool(matches!(
            evaluate(value, row, schema)?,
            Value::Null
        ))),
        QueryExpr::IsNotNull(value) => Ok(Value::Bool(!matches!(
            evaluate(value, row, schema)?,
            Value::Null
        ))),
        QueryExpr::FunctionCall { name, args } => {
            use planner_types::pre_asap::scalar_signature::MapScalarFunction;
            if name.eq_ignore_ascii_case("asap_struct_field") {
                expr.scalar_type(schema)
                    .map_err(|error| Error::Invalid(error.to_string()))?;
                let DataType::Struct { fields } = args[0]
                    .scalar_type(schema)
                    .map_err(|error| Error::Invalid(error.to_string()))?
                    .0
                else {
                    unreachable!()
                };
                let offset = match &args[1] {
                    QueryExpr::Literal(ScalarValue::Int64(index)) => {
                        usize::try_from(index - 1).ok()
                    }
                    QueryExpr::Literal(ScalarValue::Utf8(name)) => {
                        fields.iter().position(|field| &field.name == name)
                    }
                    _ => None,
                }
                .ok_or_else(|| Error::Invalid("struct field selector".into()))?;
                let Value::Struct(values) = evaluate(&args[0], row, schema)? else {
                    return Err(Error::Invalid("struct field input".into()));
                };
                return values
                    .get(offset)
                    .cloned()
                    .ok_or_else(|| Error::Invalid("struct field value".into()));
            }
            if name.eq_ignore_ascii_case("asap_element_access") {
                let (output_type, _) = expr
                    .scalar_type(schema)
                    .map_err(|error| Error::Invalid(error.to_string()))?;
                if let DataType::List { element } = args[0]
                    .scalar_type(schema)
                    .map_err(|error| Error::Invalid(error.to_string()))?
                    .0
                {
                    let Value::List(values) = evaluate(&args[0], row, schema)? else {
                        return Err(Error::Invalid("array access input".into()));
                    };
                    let index = match evaluate(&args[1], row, schema)? {
                        Value::Null => return Ok(Value::Null),
                        Value::Int64(index) => index,
                        _ => return Err(Error::Invalid("array access index".into())),
                    };
                    let offset = if index > 0 {
                        usize::try_from(index - 1).ok()
                    } else if index < 0 {
                        usize::try_from(index.unsigned_abs())
                            .ok()
                            .and_then(|distance| values.len().checked_sub(distance))
                    } else {
                        None
                    };
                    return match offset.and_then(|offset| values.get(offset)) {
                        Some(value) => Ok(value.clone()),
                        None => default_collection_element(&output_type, element.nullable),
                    };
                }
            }
            let function = (if name.eq_ignore_ascii_case("asap_element_access") {
                Some(MapScalarFunction::Access)
            } else {
                MapScalarFunction::from_name(name)
            })
            .ok_or_else(|| Error::Invalid(format!("scalar function {name}")))?;
            expr.scalar_type(schema)
                .map_err(|error| Error::Invalid(error.to_string()))?;
            let values = args
                .iter()
                .map(|arg| evaluate(arg, row, schema))
                .collect::<Result<Vec<_>, _>>()?;
            match function {
                MapScalarFunction::Construct => {
                    let mut values = values.into_iter();
                    let mut entries = Vec::new();
                    while let Some(key) = values.next() {
                        if !matches!(key, Value::Int64(_) | Value::Utf8(_) | Value::Bool(_)) {
                            return Err(Error::Invalid("map key value type".into()));
                        }
                        entries.push((
                            key,
                            values
                                .next()
                                .ok_or_else(|| Error::Invalid("odd map argument count".into()))?,
                        ));
                    }
                    Ok(Value::Map(entries.into()))
                }
                MapScalarFunction::Concat => {
                    let mut entries = Vec::new();
                    for value in values {
                        let Value::Map(next) = value else {
                            return Err(Error::Invalid("map concat argument".into()));
                        };
                        entries.extend(next.iter().cloned());
                    }
                    Ok(Value::Map(entries.into()))
                }
                MapScalarFunction::Access => {
                    let [Value::Map(entries), key] = values.as_slice() else {
                        return Err(Error::Invalid("map access arguments".into()));
                    };
                    if matches!(key, Value::Null) {
                        return Ok(Value::Null);
                    }
                    if !matches!(key, Value::Int64(_) | Value::Utf8(_) | Value::Bool(_)) {
                        return Err(Error::Invalid("map lookup key type".into()));
                    }
                    if let Some((_, value)) = entries
                        .iter()
                        .find(|(candidate, _)| cell_cmp(candidate, key) == Some(Ordering::Equal))
                    {
                        return Ok(value.clone());
                    }
                    let (
                        DataType::Map {
                            value,
                            value_nullable,
                            ..
                        },
                        _,
                    ) = args[0]
                        .scalar_type(schema)
                        .map_err(|error| Error::Invalid(error.to_string()))?
                    else {
                        unreachable!()
                    };
                    default_collection_element(&value, value_nullable)
                }
            }
        }
        other => Err(Error::Invalid(format!("scalar expression {other:?}"))),
    }
}

fn default_collection_element(dtype: &DataType, nullable: bool) -> Result<Value, Error> {
    if nullable {
        return Ok(Value::Null);
    }
    Ok(match dtype {
        DataType::Interval | DataType::Date => {
            return Err(Error::Invalid("temporal value transport".into()))
        }
        DataType::Null => Value::Null,
        DataType::Int64 => Value::Int64(0),
        DataType::Float64 => Value::Float64(0.0),
        DataType::Utf8 => Value::Utf8("".into()),
        DataType::Bool => Value::Bool(false),
        DataType::Map { .. } => Value::Map(Arc::from([])),
        DataType::List { .. } => Value::List(Arc::from([])),
        DataType::Struct { fields } => Value::Struct(
            fields
                .iter()
                .map(|field| default_collection_element(&field.dtype, field.nullable))
                .collect::<Result<Vec<_>, _>>()?
                .into(),
        ),
        _ => {
            return Err(Error::Invalid(
                "collection missing-element default type".into(),
            ))
        }
    })
}

fn compare(op: &CompareOpKind, left: Value, right: Value) -> Result<Value, Error> {
    if matches!(left, Value::Null) || matches!(right, Value::Null) {
        return Ok(Value::Null);
    }
    // NaN is unordered, not a type mismatch. Match the native scalar path.
    if matches!(&left, Value::Float64(v) if v.is_nan())
        || matches!(&right, Value::Float64(v) if v.is_nan())
    {
        return match op {
            CompareOpKind::Ne => Ok(Value::Bool(true)),
            CompareOpKind::Eq
            | CompareOpKind::Lt
            | CompareOpKind::Le
            | CompareOpKind::Gt
            | CompareOpKind::Ge => Ok(Value::Bool(false)),
            _ => Err(Error::Invalid(format!("comparison {op:?}"))),
        };
    }
    let ordering = cell_cmp(&left, &right)
        .ok_or_else(|| Error::Invalid("comparison of incompatible values".into()))?;
    let value = match op {
        CompareOpKind::Eq => ordering == Ordering::Equal,
        CompareOpKind::Ne => ordering != Ordering::Equal,
        CompareOpKind::Lt => ordering == Ordering::Less,
        CompareOpKind::Le => ordering != Ordering::Greater,
        CompareOpKind::Gt => ordering == Ordering::Greater,
        CompareOpKind::Ge => ordering != Ordering::Less,
        _ => return Err(Error::Invalid(format!("comparison {op:?}"))),
    };
    Ok(Value::Bool(value))
}

fn arithmetic(op: &ArithmeticOpKind, left: Value, right: Value) -> Result<Value, Error> {
    let (left, right) = match (left, right) {
        (Value::Int64(a), Value::Float64(b)) => (Value::Float64(a as f64), Value::Float64(b)),
        (Value::Float64(a), Value::Int64(b)) => (Value::Float64(a), Value::Float64(b as f64)),
        pair => pair,
    };
    super::numeric(op, left, right)
}

fn integer_float_cmp(integer: i64, float: f64) -> Option<Ordering> {
    if float.is_nan() {
        return None;
    }
    // These bounds are powers of two, exactly representable as Float64.
    if float >= 9_223_372_036_854_775_808.0 {
        return Some(Ordering::Less);
    }
    if float < -9_223_372_036_854_775_808.0 {
        return Some(Ordering::Greater);
    }
    let integral = float as i64;
    match integer.cmp(&integral) {
        Ordering::Equal => 0.0_f64.partial_cmp(&float.fract()),
        other => Some(other),
    }
}

fn cell_cmp(left: &Value, right: &Value) -> Option<Ordering> {
    match (left, right) {
        (Value::Int64(left), Value::Int64(right)) => Some(left.cmp(right)),
        (Value::Float64(left), Value::Float64(right)) => left.partial_cmp(right),
        (Value::Int64(left), Value::Float64(right)) => integer_float_cmp(*left, *right),
        (Value::Float64(left), Value::Int64(right)) => {
            integer_float_cmp(*right, *left).map(Ordering::reverse)
        }
        (Value::Utf8(left), Value::Utf8(right)) => Some(left.cmp(right)),
        (Value::Bool(left), Value::Bool(right)) => Some(left.cmp(right)),
        (Value::Timestamp(left), Value::Timestamp(right)) => Some(left.cmp(right)),
        (Value::Map(left), Value::Map(right)) => {
            for ((left_key, left_value), (right_key, right_value)) in left.iter().zip(right.iter())
            {
                let order = cell_cmp(left_key, right_key)?;
                if order != Ordering::Equal {
                    return Some(order);
                }
                let order = match (left_value, right_value) {
                    (Value::Null, Value::Null) => Ordering::Equal,
                    (Value::Null, _) => Ordering::Greater,
                    (_, Value::Null) => Ordering::Less,
                    _ => cell_cmp(left_value, right_value)?,
                };
                if order != Ordering::Equal {
                    return Some(order);
                }
            }
            Some(left.len().cmp(&right.len()))
        }
        _ => None,
    }
}

#[derive(Clone, Debug)]
pub struct CompiledExpression {
    expression: QueryExpr,
    schema: planner_types::pre_asap::Schema,
    output: (DataType, bool),
}
impl CompiledExpression {
    pub fn compile(expression: &QueryExpr, input: &Schema) -> Result<Self, Error> {
        let schema = input
            .fields
            .iter()
            .map(|field| {
                let planner_types::post_asap::SummaryFamilyType::Plain(dtype) = &field.dtype else {
                    return Err(Error::Invalid(
                        "scalar expression cannot consume opaque summary state".into(),
                    ));
                };
                Ok(planner_types::pre_asap::Column::new(
                    field.name.clone(),
                    dtype.clone(),
                    field.nullable,
                ))
            })
            .collect::<Result<Vec<_>, Error>>()?;
        let schema = planner_types::pre_asap::Schema::new(schema);
        validate(expression, &schema)?;
        let output = expression
            .scalar_type(&schema)
            .map_err(|e| Error::Invalid(e.to_string()))?;
        Ok(Self {
            expression: expression.clone(),
            schema,
            output,
        })
    }
    pub(crate) fn dtype(&self) -> (DataType, bool) {
        self.output.clone()
    }
    pub(crate) fn validate_input(&self, input: &Schema) -> Result<(), Error> {
        if input.fields.len() != self.schema.columns.len()
            || input
                .fields
                .iter()
                .zip(&self.schema.columns)
                .any(|(field, column)| {
                    field.dtype
                        != planner_types::post_asap::SummaryFamilyType::Plain(column.dtype.clone())
                        || field.nullable != column.nullable
                })
        {
            return Err(Error::Invalid(
                "expression input differs from its bound schema".into(),
            ));
        }
        Ok(())
    }
    /// Evaluate a row under the same typed schema used when binding the expression.
    pub fn evaluate(&self, row: &[Value]) -> Result<Value, Error> {
        if row.len() != self.schema.columns.len()
            || row
                .iter()
                .zip(&self.schema.columns)
                .any(|(value, column)| !value.matches(&column.dtype, column.nullable))
        {
            return Err(Error::Invalid(
                "expression input differs from its bound schema".into(),
            ));
        }
        evaluate(&self.expression, row, &self.schema)
    }
}
fn validate(expr: &QueryExpr, schema: &planner_types::pre_asap::Schema) -> Result<(), Error> {
    let invalid = || Error::Invalid(format!("unsupported scalar expression: {expr:?}"));
    expr.scalar_type(schema)
        .map_err(|e| Error::Invalid(e.to_string()))?;
    match expr {
        QueryExpr::Column(_) | QueryExpr::Literal(_) => Ok(()),
        QueryExpr::Arithmetic { left, right, .. } => {
            for value in [left, right] {
                validate(value, schema)?;
                if !matches!(
                    value
                        .scalar_type(schema)
                        .map_err(|e| Error::Invalid(e.to_string()))?
                        .0,
                    DataType::Int64 | DataType::Float64 | DataType::Null
                ) {
                    return Err(invalid());
                }
            }
            Ok(())
        }
        QueryExpr::Compare { left, right, op } => {
            if !matches!(
                op,
                CompareOpKind::Eq
                    | CompareOpKind::Ne
                    | CompareOpKind::Lt
                    | CompareOpKind::Le
                    | CompareOpKind::Gt
                    | CompareOpKind::Ge
            ) {
                return Err(invalid());
            }
            validate(left, schema)?;
            validate(right, schema)?;
            let (a, _) = left
                .scalar_type(schema)
                .map_err(|e| Error::Invalid(e.to_string()))?;
            let (b, _) = right
                .scalar_type(schema)
                .map_err(|e| Error::Invalid(e.to_string()))?;
            fn comparable(dtype: &DataType) -> bool {
                match dtype {
                    DataType::Null
                    | DataType::Int64
                    | DataType::Float64
                    | DataType::Utf8
                    | DataType::Bool
                    | DataType::Timestamp => true,
                    DataType::Map { key, value, .. } => comparable(key) && comparable(value),
                    _ => false,
                }
            }
            let numeric = |dtype: &DataType| matches!(dtype, DataType::Int64 | DataType::Float64);
            if !comparable(&a)
                || !comparable(&b)
                || (a != b
                    && !matches!(a, DataType::Null)
                    && !matches!(b, DataType::Null)
                    && !(numeric(&a) && numeric(&b)))
            {
                return Err(invalid());
            }
            Ok(())
        }
        QueryExpr::FunctionCall { name, args } => {
            if name != "asap_struct_field"
                && name != "asap_element_access"
                && planner_types::pre_asap::scalar_signature::MapScalarFunction::from_name(name)
                    .is_none()
            {
                return Err(invalid());
            }
            for arg in args {
                validate(arg, schema)?;
            }
            Ok(())
        }
        QueryExpr::BoolAnd(parts) | QueryExpr::BoolOr(parts) => {
            for part in parts {
                validate(part, schema)?;
                if !matches!(
                    part.scalar_type(schema)
                        .map_err(|e| Error::Invalid(e.to_string()))?
                        .0,
                    DataType::Bool | DataType::Null
                ) {
                    return Err(invalid());
                }
            }
            Ok(())
        }
        QueryExpr::Not(value) => {
            validate(value, schema)?;
            if !matches!(
                value
                    .scalar_type(schema)
                    .map_err(|e| Error::Invalid(e.to_string()))?
                    .0,
                DataType::Bool | DataType::Null
            ) {
                return Err(invalid());
            }
            Ok(())
        }
        QueryExpr::IsNull(value) | QueryExpr::IsNotNull(value) => validate(value, schema),
        _ => Err(invalid()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn mixed_comparison_preserves_integer_precision_and_boundaries() {
        assert_eq!(
            integer_float_cmp(9_007_199_254_740_993, 9_007_199_254_740_992.0),
            Some(Ordering::Greater)
        );
        assert_eq!(
            integer_float_cmp(i64::MAX, 9_223_372_036_854_775_808.0),
            Some(Ordering::Less)
        );
        assert_eq!(
            integer_float_cmp(i64::MIN, -9_223_372_036_854_775_808.0),
            Some(Ordering::Equal)
        );
        assert_eq!(integer_float_cmp(-1, -1.5), Some(Ordering::Greater));
        assert_eq!(integer_float_cmp(1, 1.5), Some(Ordering::Less));
        assert_eq!(integer_float_cmp(0, f64::INFINITY), Some(Ordering::Less));
        assert_eq!(
            integer_float_cmp(0, f64::NEG_INFINITY),
            Some(Ordering::Greater)
        );
        assert_eq!(integer_float_cmp(0, f64::NAN), None);
    }
}

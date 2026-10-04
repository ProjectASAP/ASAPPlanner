//! Planner scalar expressions evaluated over native typed rows.
use crate::{
    values::{SchemaRef, Value},
    Error,
};
use planner_types::ir::scalar::{ArithmeticOpKind, CompareOpKind, ScalarValue};
use planner_types::ir::schema::DataType;

use planner_types::ir::ScalarExpr;
use std::{cmp::Ordering, sync::Arc};

pub(super) fn evaluate(
    expr: &ScalarExpr,
    row: &[Value],
    schema: &planner_types::ir::schema::Schema,
) -> Result<Value, Error> {
    match expr {
        ScalarExpr::Column(index) => row.get(*index).cloned().ok_or(Error::Invalid(format!(
            "column {index} outside row width {}",
            row.len()
        ))),
        ScalarExpr::Literal(value) => Ok(match value {
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
        ScalarExpr::Cast { expr, to, .. } => {
            let value = evaluate(expr, row, schema)?;
            match (value, to) {
                (Value::Null, _) => Ok(Value::Null),
                (Value::Int64(value), DataType::Float64) => Ok(Value::Float64(value as f64)),
                (value, _)
                    if expr
                        .scalar_type(schema)
                        .map_err(|e| Error::Invalid(e.to_string()))?
                        .0
                        == *to =>
                {
                    Ok(value)
                }
                _ => Err(Error::Invalid("unsupported cast".into())),
            }
        }
        ScalarExpr::Negative { expr, .. } => match evaluate(expr, row, schema)? {
            Value::Float64(v) => Ok(Value::Float64(-v)),
            Value::Int64(v) => v
                .checked_neg()
                .map(Value::Int64)
                .ok_or_else(|| Error::Invalid("integer negation overflow".into())),
            Value::Null => Ok(Value::Null),
            _ => Err(Error::Invalid("invalid negation input".into())),
        },
        ScalarExpr::Compare {
            left, op, right, ..
        } => {
            let left = evaluate(left, row, schema)?;
            let right = evaluate(right, row, schema)?;
            compare(op, left, right)
        }
        ScalarExpr::Arithmetic {
            op, left, right, ..
        } => arithmetic(
            op,
            evaluate(left, row, schema)?,
            evaluate(right, row, schema)?,
        ),
        ScalarExpr::Case {
            operand: None,
            branches,
            else_expr,
        } => {
            for (condition, value) in branches {
                if matches!(evaluate(condition, row, schema)?, Value::Bool(true)) {
                    return evaluate(value, row, schema);
                }
            }
            else_expr
                .as_ref()
                .map_or(Ok(Value::Null), |e| evaluate(e, row, schema))
        }
        ScalarExpr::BoolAnd(parts) | ScalarExpr::BoolOr(parts) => {
            let and = matches!(expr, ScalarExpr::BoolAnd(_));
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
        ScalarExpr::Not(value) => match evaluate(value, row, schema)? {
            Value::Bool(value) => Ok(Value::Bool(!value)),
            Value::Null => Ok(Value::Null),
            _ => Err(Error::Invalid("boolean predicate required".into())),
        },
        ScalarExpr::IsNull(value) => Ok(Value::Bool(matches!(
            evaluate(value, row, schema)?,
            Value::Null
        ))),
        ScalarExpr::IsNotNull(value) => Ok(Value::Bool(!matches!(
            evaluate(value, row, schema)?,
            Value::Null
        ))),
        ScalarExpr::FunctionCall { name, args } => {
            use planner_types::ir::scalar::scalar_type_rules::MapScalarFunction;
            if planner_types::ir::scalar::scalar_type_rules::promql_function_arity(name).is_some() {
                let values = args
                    .iter()
                    .map(|arg| match evaluate(arg, row, schema)? {
                        Value::Float64(v) => Ok(v),
                        _ => Err(Error::Invalid("PromQL function requires floats".into())),
                    })
                    .collect::<Result<Vec<_>, _>>()?;
                return Ok(Value::Float64(promql_function(name, &values)?));
            }
            if name.eq_ignore_ascii_case("sqrt") {
                return match evaluate(&args[0], row, schema)? {
                    Value::Null => Ok(Value::Null),
                    Value::Float64(value) => Ok(Value::Float64(value.sqrt())),
                    Value::Int64(value) => Ok(Value::Float64((value as f64).sqrt())),
                    _ => Err(Error::Invalid(
                        "SQL sqrt requires a numeric argument".into(),
                    )),
                };
            }
            if name == "promql_drop_metric_name" {
                let Value::Utf8(encoded) = evaluate(&args[0], row, schema)? else {
                    return Err(Error::Invalid("series identity must be Utf8".into()));
                };
                let mut labels: std::collections::BTreeMap<String, String> =
                    serde_json::from_str(&encoded).map_err(|e| Error::Invalid(e.to_string()))?;
                labels.remove("__name__");
                return Ok(Value::Utf8(
                    serde_json::to_string(&labels)
                        .map_err(|e| Error::Invalid(e.to_string()))?
                        .into(),
                ));
            }
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
                    ScalarExpr::Literal(ScalarValue::Int64(index)) => {
                        usize::try_from(index - 1).ok()
                    }
                    ScalarExpr::Literal(ScalarValue::Utf8(name)) => {
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

fn promql_function(name: &str, args: &[f64]) -> Result<f64, Error> {
    let x = args[0];
    Ok(match &name[7..] {
        "abs" => x.abs(),
        "ceil" => x.ceil(),
        "floor" => x.floor(),
        "exp" => x.exp(),
        "ln" => x.ln(),
        "log2" => x.log2(),
        "log10" => x.log10(),
        "sqrt" => x.sqrt(),
        "sgn" => {
            if x.is_nan() {
                f64::NAN
            } else if x == 0.0 {
                0.0
            } else {
                x.signum()
            }
        }
        "sin" => x.sin(),
        "cos" => x.cos(),
        "tan" => x.tan(),
        "asin" => x.asin(),
        "acos" => x.acos(),
        "atan" => x.atan(),
        "sinh" => x.sinh(),
        "cosh" => x.cosh(),
        "tanh" => x.tanh(),
        "asinh" => x.asinh(),
        "acosh" => x.acosh(),
        "atanh" => x.atanh(),
        "deg" => x.to_degrees(),
        "rad" => x.to_radians(),
        "round" => {
            let inverse = 1.0 / args[1];
            (x * inverse + 0.5).floor() / inverse
        }
        "clamp_min" => {
            if x.is_nan() || args[1].is_nan() {
                f64::NAN
            } else {
                x.max(args[1])
            }
        }
        "clamp_max" => {
            if x.is_nan() || args[1].is_nan() {
                f64::NAN
            } else {
                x.min(args[1])
            }
        }
        "clamp" => {
            if args.iter().any(|x| x.is_nan()) {
                f64::NAN
            } else {
                x.max(args[1]).min(args[2])
            }
        }
        part => {
            use chrono::{Datelike, Timelike};
            if !x.is_finite() || x < i64::MIN as f64 || x >= i64::MAX as f64 {
                return Ok(f64::NAN);
            }
            let Some(date) = chrono::DateTime::from_timestamp(x as i64, 0) else {
                return Ok(f64::NAN);
            };
            match part {
                "minute" => date.minute() as f64,
                "hour" => date.hour() as f64,
                "day_of_week" => date.weekday().num_days_from_sunday() as f64,
                "day_of_month" => date.day() as f64,
                "day_of_year" => date.ordinal() as f64,
                "month" => date.month() as f64,
                "year" => date.year() as f64,
                "days_in_month" => {
                    let year = date.year();
                    let leap = year % 4 == 0 && (year % 100 != 0 || year % 400 == 0);
                    match date.month() {
                        2 => {
                            if leap {
                                29.0
                            } else {
                                28.0
                            }
                        }
                        4 | 6 | 9 | 11 => 30.0,
                        _ => 31.0,
                    }
                }
                _ => return Err(Error::Invalid("unregistered PromQL function".into())),
            }
        }
    })
}

#[derive(serde::Serialize, serde::Deserialize, Clone, Debug)]
pub struct CompiledExpression {
    expression: ScalarExpr,
    schema: planner_types::ir::schema::Schema,
    output: (DataType, bool),
}
impl CompiledExpression {
    pub(crate) fn expression(&self) -> &ScalarExpr {
        &self.expression
    }

    pub fn compile(expression: &ScalarExpr, input: &SchemaRef) -> Result<Self, Error> {
        if !input.is_all_plain() {
            return Err(Error::Invalid(
                "scalar expression cannot consume summary state".into(),
            ));
        }
        let schema = input.as_ref().clone();
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
    pub(crate) fn validate_input(&self, input: &SchemaRef) -> Result<(), Error> {
        let checked = Self::compile(&self.expression, input)?;
        if checked.output != self.output {
            return Err(Error::Invalid(
                "persisted expression type differs from its semantics".into(),
            ));
        }
        if input.fields.len() != self.schema.fields.len()
            || input
                .fields
                .iter()
                .zip(&self.schema.fields)
                .any(|(field, column)| {
                    field.dtype != column.dtype.clone() || field.nullable != column.nullable
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
        if row.len() != self.schema.fields.len()
            || row.iter().zip(&self.schema.fields).any(|(value, column)| {
                !column
                    .plain_dtype()
                    .is_some_and(|dtype| value.matches(dtype, column.nullable))
            })
        {
            return Err(Error::Invalid(
                "expression input differs from its bound schema".into(),
            ));
        }
        evaluate(&self.expression, row, &self.schema)
    }
}
fn validate(expr: &ScalarExpr, schema: &planner_types::ir::schema::Schema) -> Result<(), Error> {
    let invalid = || Error::Invalid(format!("unsupported scalar expression: {expr:?}"));
    expr.scalar_type(schema)
        .map_err(|e| Error::Invalid(e.to_string()))?;
    match expr {
        ScalarExpr::Column(_) | ScalarExpr::Literal(_) => Ok(()),
        ScalarExpr::Cast { expr, to, .. } => {
            let source = expr
                .scalar_type(schema)
                .map_err(|e| Error::Invalid(e.to_string()))?
                .0;
            if source != *to
                && source != DataType::Null
                && !(source == DataType::Int64 && *to == DataType::Float64)
            {
                return Err(invalid());
            }
            validate(expr, schema)
        }
        ScalarExpr::Negative { expr, .. } => validate(expr, schema),
        ScalarExpr::Arithmetic { left, right, .. } => {
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
        ScalarExpr::Compare {
            left, right, op, ..
        } => {
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
        ScalarExpr::FunctionCall { name, args } => {
            if name.eq_ignore_ascii_case("sqrt") {
                if args.len() != 1
                    || !matches!(
                        args[0]
                            .scalar_type(schema)
                            .map_err(|e| Error::Invalid(e.to_string()))?
                            .0,
                        DataType::Int64 | DataType::Float64 | DataType::Null
                    )
                {
                    return Err(invalid());
                }
            } else if name != "promql_drop_metric_name"
                && planner_types::ir::scalar::scalar_type_rules::promql_function_arity(name)
                    .is_none()
                && name != "asap_struct_field"
                && name != "asap_element_access"
                && planner_types::ir::scalar::scalar_type_rules::MapScalarFunction::from_name(name)
                    .is_none()
            {
                return Err(invalid());
            }
            for arg in args {
                validate(arg, schema)?;
            }
            Ok(())
        }
        ScalarExpr::Case {
            operand: None,
            branches,
            else_expr,
        } => {
            for (condition, value) in branches {
                validate(condition, schema)?;
                if condition
                    .scalar_type(schema)
                    .map_err(|e| Error::Invalid(e.to_string()))?
                    .0
                    != DataType::Bool
                {
                    return Err(invalid());
                }
                validate(value, schema)?;
            }
            if let Some(value) = else_expr {
                validate(value, schema)?;
            }
            Ok(())
        }
        ScalarExpr::BoolAnd(parts) | ScalarExpr::BoolOr(parts) => {
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
        ScalarExpr::Not(value) => {
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
        ScalarExpr::IsNull(value) | ScalarExpr::IsNotNull(value) => validate(value, schema),
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

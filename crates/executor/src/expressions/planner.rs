//! Planner scalar expressions evaluated over native typed rows.
use crate::{
    values::{SchemaRef, Value},
    Error,
};
use planner_types::ir::scalar::{ArithmeticOpKind, CompareOpKind, ScalarValue};
use planner_types::ir::schema::DataType;

use planner_types::ir::ScalarExpr as PlannedExpr;

/// A scalar expression the executor evaluates: a Planner expression with
/// no operator reference, so it is `Send + Sync`.
pub(crate) type ScalarExpr = PlannedExpr<()>;

/// Planner typing for a reference-free expression: the Planner types a
/// planned expression, which this one is with its (absent) references.
pub(super) trait LocalTyping {
    fn scalar_type(
        &self,
        schema: &planner_types::ir::schema::Schema,
    ) -> Result<(DataType, bool), planner_types::ir::SchemaDerivationError>;
}

impl LocalTyping for ScalarExpr {
    fn scalar_type(
        &self,
        schema: &planner_types::ir::schema::Schema,
    ) -> Result<(DataType, bool), planner_types::ir::SchemaDerivationError> {
        planned(self).scalar_type(schema)
    }
}

/// `expression` as a planned expression; it has no reference to map.
fn planned(expression: &ScalarExpr) -> PlannedExpr {
    expression.map_operator_refs(&mut |_| -> std::rc::Rc<planner_types::ir::OperatorNode> {
        unreachable!("compiled expressions hold no operator reference")
    })
}
use std::{cmp::Ordering, sync::Arc};

/// `expr`'s value over `row`. `evaluation_time_ms` is SQL `now()`: the
/// query's evaluation time, `None` outside a query run. `regexes` holds the
/// compiled pattern of each `=~` / `!~` in `expr` ([`label_regexes_of`]).
pub(super) fn evaluate(
    expr: &ScalarExpr,
    row: &[Value],
    schema: &planner_types::ir::schema::Schema,
    evaluation_time_ms: Option<i64>,
    regexes: &LabelRegexes,
) -> Result<Value, Error> {
    match expr {
        ScalarExpr::CurrentTimestamp => evaluation_time_ms.map(Value::Timestamp).ok_or_else(|| {
            Error::Invalid("now() has a value only in a query run, at its evaluation time".into())
        }),
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
            let value = evaluate(expr, row, schema, evaluation_time_ms, regexes)?;
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
        ScalarExpr::Negative { expr, .. } => {
            match evaluate(expr, row, schema, evaluation_time_ms, regexes)? {
                Value::Float64(v) => Ok(Value::Float64(-v)),
                Value::Int64(v) => v
                    .checked_neg()
                    .map(Value::Int64)
                    .ok_or_else(|| Error::Invalid("integer negation overflow".into())),
                Value::Null => Ok(Value::Null),
                _ => Err(Error::Invalid("invalid negation input".into())),
            }
        }
        ScalarExpr::Compare {
            left,
            op: op @ (CompareOpKind::Regex | CompareOpKind::NotRegex),
            right,
            ..
        } => {
            let ScalarExpr::Literal(ScalarValue::Utf8(pattern)) = &**right else {
                return Err(Error::Invalid(format!("{op:?} needs a literal pattern")));
            };
            let regex = regexes
                .get(pattern.as_str())
                .ok_or_else(|| Error::Invalid(format!("label regex {pattern:?} not compiled")))?;
            match evaluate(left, row, schema, evaluation_time_ms, regexes)? {
                Value::Null => Ok(Value::Null),
                Value::Utf8(text) => Ok(Value::Bool(
                    regex.is_match(text.as_ref()) == matches!(op, CompareOpKind::Regex),
                )),
                _ => Err(Error::Invalid(format!(
                    "comparison {op:?} of non-text values"
                ))),
            }
        }
        ScalarExpr::Compare {
            left, op, right, ..
        } => {
            let left = evaluate(left, row, schema, evaluation_time_ms, regexes)?;
            let right = evaluate(right, row, schema, evaluation_time_ms, regexes)?;
            compare(op, left, right)
        }
        ScalarExpr::Arithmetic {
            op, left, right, ..
        } => arithmetic(
            op,
            evaluate(left, row, schema, evaluation_time_ms, regexes)?,
            evaluate(right, row, schema, evaluation_time_ms, regexes)?,
        ),
        ScalarExpr::Case {
            operand: None,
            branches,
            else_expr,
        } => {
            for (condition, value) in branches {
                if matches!(
                    evaluate(condition, row, schema, evaluation_time_ms, regexes)?,
                    Value::Bool(true)
                ) {
                    return evaluate(value, row, schema, evaluation_time_ms, regexes);
                }
            }
            else_expr.as_ref().map_or(Ok(Value::Null), |e| {
                evaluate(e, row, schema, evaluation_time_ms, regexes)
            })
        }
        ScalarExpr::BoolAnd(parts) | ScalarExpr::BoolOr(parts) => {
            let and = matches!(expr, ScalarExpr::BoolAnd(_));
            let mut null = false;
            for part in parts {
                match evaluate(part, row, schema, evaluation_time_ms, regexes)? {
                    Value::Bool(value) if value != and => return Ok(Value::Bool(value)),
                    Value::Bool(_) => {}
                    Value::Null => null = true,
                    _ => return Err(Error::Invalid("boolean predicate required".into())),
                }
            }
            Ok(if null { Value::Null } else { Value::Bool(and) })
        }
        ScalarExpr::Not(value) => {
            match evaluate(value, row, schema, evaluation_time_ms, regexes)? {
                Value::Bool(value) => Ok(Value::Bool(!value)),
                Value::Null => Ok(Value::Null),
                _ => Err(Error::Invalid("boolean predicate required".into())),
            }
        }
        ScalarExpr::IsNull(value) => Ok(Value::Bool(matches!(
            evaluate(value, row, schema, evaluation_time_ms, regexes)?,
            Value::Null
        ))),
        ScalarExpr::IsNotNull(value) => Ok(Value::Bool(!matches!(
            evaluate(value, row, schema, evaluation_time_ms, regexes)?,
            Value::Null
        ))),
        ScalarExpr::FunctionCall { name, args } => {
            use planner_types::ir::scalar::scalar_type_rules::MapScalarFunction;
            if planner_types::ir::scalar::scalar_type_rules::promql_function_arity(name).is_some() {
                let values = args
                    .iter()
                    .map(
                        |arg| match evaluate(arg, row, schema, evaluation_time_ms, regexes)? {
                            Value::Float64(v) => Ok(v),
                            _ => Err(Error::Invalid("PromQL function requires floats".into())),
                        },
                    )
                    .collect::<Result<Vec<_>, _>>()?;
                return Ok(Value::Float64(promql_function(name, &values)?));
            }
            if name == "promql_drop_metric_name" {
                let Value::Utf8(encoded) =
                    evaluate(&args[0], row, schema, evaluation_time_ms, regexes)?
                else {
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
                let Value::Struct(values) =
                    evaluate(&args[0], row, schema, evaluation_time_ms, regexes)?
                else {
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
                    let Value::List(values) =
                        evaluate(&args[0], row, schema, evaluation_time_ms, regexes)?
                    else {
                        return Err(Error::Invalid("array access input".into()));
                    };
                    let index = match evaluate(&args[1], row, schema, evaluation_time_ms, regexes)?
                    {
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
                .map(|arg| evaluate(arg, row, schema, evaluation_time_ms, regexes))
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

/// The compiled pattern of each PromQL `=~` / `!~` in an expression, by
/// pattern text.
pub(super) type LabelRegexes = std::collections::HashMap<String, regex::Regex>;

/// Compiles every `=~` / `!~` pattern in `expr` ([`super::promql_anchored_regex`]).
fn label_regexes_of(expr: &ScalarExpr) -> Result<LabelRegexes, Error> {
    let mut regexes = LabelRegexes::new();
    let mut pending = vec![expr];
    while let Some(expr) = pending.pop() {
        if let ScalarExpr::Compare {
            op: CompareOpKind::Regex | CompareOpKind::NotRegex,
            right,
            ..
        } = expr
        {
            if let ScalarExpr::Literal(ScalarValue::Utf8(pattern)) = &**right {
                if !regexes.contains_key(pattern.as_str()) {
                    let regex = super::promql_anchored_regex(pattern).map_err(|e| {
                        Error::Invalid(format!("invalid label regex {pattern:?}: {e}"))
                    })?;
                    regexes.insert(pattern.to_string(), regex);
                }
            }
        }
        pending.extend(expr.children());
    }
    Ok(regexes)
}

fn arithmetic(op: &ArithmeticOpKind, left: Value, right: Value) -> Result<Value, Error> {
    match (op, &left, &right) {
        (ArithmeticOpKind::Add | ArithmeticOpKind::Sub, Value::Timestamp(at_ms), interval)
            if matches!(interval, Value::Interval { .. }) =>
        {
            return shifted_timestamp(*at_ms, interval, *op == ArithmeticOpKind::Sub)
        }
        (ArithmeticOpKind::Add, interval @ Value::Interval { .. }, Value::Timestamp(at_ms)) => {
            return shifted_timestamp(*at_ms, interval, false)
        }
        _ => {}
    }
    let (left, right) = match (left, right) {
        (Value::Int64(a), Value::Float64(b)) => (Value::Float64(a as f64), Value::Float64(b)),
        (Value::Float64(a), Value::Int64(b)) => (Value::Float64(a), Value::Float64(b as f64)),
        pair => pair,
    };
    super::numeric(op, left, right)
}

/// The timestamp `at_ms` (milliseconds since the epoch) plus `interval`, or
/// minus it when `subtract`: its months by the calendar (UTC), then its
/// days and nanoseconds as fixed durations. Bad input when the interval has
/// a fraction of a millisecond or the result leaves the timestamp range.
fn shifted_timestamp(at_ms: i64, interval: &Value, subtract: bool) -> Result<Value, Error> {
    let Value::Interval {
        months,
        days,
        nanos,
    } = *interval
    else {
        unreachable!("an interval operand")
    };
    let out_of_range = || Error::Invalid("timestamp arithmetic leaves the timestamp range".into());
    if nanos % 1_000_000 != 0 {
        return Err(Error::Invalid(
            "an interval with a fraction of a millisecond has no millisecond timestamp".into(),
        ));
    }
    let sign: i64 = if subtract { -1 } else { 1 };
    let mut at = chrono::DateTime::from_timestamp_millis(at_ms).ok_or_else(out_of_range)?;
    let months = i64::from(months) * sign;
    let calendar_months = chrono::Months::new(months.unsigned_abs() as u32);
    at = if months >= 0 {
        at.checked_add_months(calendar_months)
    } else {
        at.checked_sub_months(calendar_months)
    }
    .ok_or_else(out_of_range)?;
    let fixed_ms = i64::from(days)
        .checked_mul(86_400_000)
        .and_then(|days_ms| days_ms.checked_add(nanos / 1_000_000))
        .and_then(|ms| ms.checked_mul(sign))
        .ok_or_else(out_of_range)?;
    at.timestamp_millis()
        .checked_add(fixed_ms)
        .map(Value::Timestamp)
        .ok_or_else(out_of_range)
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
    /// SQL `now()` in this run: the query's evaluation time
    /// ([`Self::at_evaluation_time`]); `None` in a compiled plan and
    /// outside a query run.
    #[serde(skip)]
    evaluation_time_ms: Option<i64>,
    /// Its `=~` / `!~` patterns, compiled when the expression is, or on
    /// first use after deserialization; clones share them.
    #[serde(skip)]
    label_regexes: Arc<std::sync::OnceLock<LabelRegexes>>,
}
impl CompiledExpression {
    pub(crate) fn expression(&self) -> &ScalarExpr {
        &self.expression
    }

    pub(crate) fn evaluation_time_ms(&self) -> Option<i64> {
        self.evaluation_time_ms
    }

    /// Whether this expression reads SQL `now()`.
    pub(crate) fn reads_current_timestamp(&self) -> bool {
        fn reads(expr: &ScalarExpr) -> bool {
            matches!(expr, ScalarExpr::CurrentTimestamp) || expr.children().into_iter().any(reads)
        }
        reads(&self.expression)
    }

    /// This expression with `now()` the evaluation time of `context`'s
    /// query run (no value outside one).
    pub fn at_evaluation_time(&self, context: &crate::runtime::RunContext) -> Self {
        Self {
            evaluation_time_ms: context.evaluation_time_ms(),
            ..self.clone()
        }
    }

    /// `expression` compiled over `input`. Bad input when it reads another
    /// operator (a subquery or `scalar(v)`), summary state, or types the
    /// executor does not evaluate.
    pub fn compile(expression: &PlannedExpr, input: &SchemaRef) -> Result<Self, Error> {
        if !expression.operator_refs().is_empty() {
            return Err(Error::Invalid(
                "a scalar expression that reads another operator has no native evaluation".into(),
            ));
        }
        let expression: ScalarExpr = expression.map_operator_refs(&mut |_| ());
        Self::compile_local(&expression, input)
    }

    fn compile_local(expression: &ScalarExpr, input: &SchemaRef) -> Result<Self, Error> {
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
            label_regexes: Arc::new(label_regexes_of(expression)?.into()),
            expression: expression.clone(),
            schema,
            output,
            evaluation_time_ms: None,
        })
    }
    pub(crate) fn dtype(&self) -> (DataType, bool) {
        self.output.clone()
    }
    pub(crate) fn validate_input(&self, input: &SchemaRef) -> Result<(), Error> {
        let checked = Self::compile_local(&self.expression, input)?;
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
        evaluate(
            &self.expression,
            row,
            &self.schema,
            self.evaluation_time_ms,
            self.label_regexes()?,
        )
    }

    fn label_regexes(&self) -> Result<&LabelRegexes, Error> {
        if let Some(regexes) = self.label_regexes.get() {
            return Ok(regexes);
        }
        let regexes = label_regexes_of(&self.expression)?;
        Ok(self.label_regexes.get_or_init(|| regexes))
    }
}
fn validate(expr: &ScalarExpr, schema: &planner_types::ir::schema::Schema) -> Result<(), Error> {
    let invalid = || Error::Invalid(format!("unsupported scalar expression: {expr:?}"));
    expr.scalar_type(schema)
        .map_err(|e| Error::Invalid(e.to_string()))?;
    match expr {
        ScalarExpr::Column(_) | ScalarExpr::Literal(_) | ScalarExpr::CurrentTimestamp => Ok(()),
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
        ScalarExpr::Arithmetic {
            left, right, op, ..
        } => {
            let operand_type = |value: &ScalarExpr| {
                value
                    .scalar_type(schema)
                    .map(|(dtype, _)| dtype)
                    .map_err(|e| Error::Invalid(e.to_string()))
            };
            // A timestamp shifted by an interval.
            if matches!(
                (op, operand_type(left)?, operand_type(right)?),
                (
                    ArithmeticOpKind::Add | ArithmeticOpKind::Sub,
                    DataType::Timestamp,
                    DataType::Interval
                ) | (
                    ArithmeticOpKind::Add,
                    DataType::Interval,
                    DataType::Timestamp
                )
            ) {
                validate(left, schema)?;
                return validate(right, schema);
            }
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
        // PromQL's `=~` / `!~`: text against a literal pattern, compiled
        // here so a bad pattern fails before any row.
        ScalarExpr::Compare {
            left,
            right,
            op: CompareOpKind::Regex | CompareOpKind::NotRegex,
            ..
        } => {
            validate(left, schema)?;
            let text = left
                .scalar_type(schema)
                .map_err(|e| Error::Invalid(e.to_string()))?
                .0;
            match &**right {
                ScalarExpr::Literal(ScalarValue::Utf8(pattern)) if text == DataType::Utf8 => {
                    super::promql_anchored_regex(pattern)
                        .map(|_| ())
                        .map_err(|e| {
                            Error::Invalid(format!("invalid label regex {pattern:?}: {e}"))
                        })
                }
                _ => Err(invalid()),
            }
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
            if name != "promql_drop_metric_name"
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

    // A label regex must be a valid literal pattern over text; both fail at
    // compile time, the bad pattern named.
    #[test]
    fn label_regex_rejects_bad_patterns_at_compile_time() {
        let schema = std::sync::Arc::new(planner_types::ir::schema::Schema::new(vec![
            planner_types::ir::schema::Field::plain("job", DataType::Utf8, false),
            planner_types::ir::schema::Field::plain("v", DataType::Float64, false),
        ]));
        let matcher = |column: usize, right: PlannedExpr| {
            CompiledExpression::compile(
                &PlannedExpr::Compare {
                    left: Box::new(PlannedExpr::Column(column)),
                    op: CompareOpKind::Regex,
                    right: Box::new(right),
                    semantics: planner_types::ir::scalar::ExprSemantics::Promql,
                },
                &schema,
            )
        };
        let text = |value: &str| PlannedExpr::Literal(ScalarValue::Utf8(value.into()));
        assert!(matcher(0, text("api|db")).is_ok());
        let error = matcher(0, text("a(")).unwrap_err().to_string();
        assert!(error.contains("\"a(\""), "{error}");
        // Non-text input fails the Planner's typing; a column pattern is not a literal.
        assert!(matcher(1, text("1")).is_err());
        assert!(matcher(0, PlannedExpr::Column(0)).is_err());
    }

    // The compiled patterns are not serialized: a deserialized expression
    // compiles them on first use and matches as the original does.
    #[test]
    fn a_deserialized_label_regex_matches_as_compiled() {
        let schema = std::sync::Arc::new(planner_types::ir::schema::Schema::new(vec![
            planner_types::ir::schema::Field::plain("job", DataType::Utf8, false),
        ]));
        let compiled = CompiledExpression::compile(
            &PlannedExpr::Compare {
                left: Box::new(PlannedExpr::Column(0)),
                op: CompareOpKind::NotRegex,
                right: Box::new(PlannedExpr::Literal(ScalarValue::Utf8("api|db".into()))),
                semantics: planner_types::ir::scalar::ExprSemantics::Promql,
            },
            &schema,
        )
        .unwrap();
        let restored: CompiledExpression =
            serde_json::from_str(&serde_json::to_string(&compiled).unwrap()).unwrap();
        for (job, kept) in [("api", false), ("db", false), ("web", true), ("api2", true)] {
            let row = [Value::Utf8(job.into())];
            for expression in [&compiled, &restored] {
                assert!(
                    matches!(expression.evaluate(&row), Ok(Value::Bool(b)) if b == kept),
                    "{job}"
                );
            }
        }
    }
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

    fn interval(months: i32, days: i32, nanos: i64) -> Value {
        Value::Interval {
            months,
            days,
            nanos,
        }
    }

    fn at(text: &str) -> i64 {
        chrono::DateTime::parse_from_rfc3339(text)
            .unwrap()
            .timestamp_millis()
    }

    // An interval's months move by the calendar, then its days and
    // nanoseconds as fixed durations; a fraction of a millisecond has no
    // millisecond timestamp.
    #[test]
    fn a_timestamp_shifts_by_an_interval() {
        let shifted = |from: &str, by: Value, subtract: bool| match shifted_timestamp(
            at(from),
            &by,
            subtract,
        ) {
            Ok(Value::Timestamp(ms)) => ms,
            other => panic!("{other:?}"),
        };
        assert_eq!(
            shifted("2026-01-31T10:00:00Z", interval(1, 0, 0), false),
            at("2026-02-28T10:00:00Z")
        );
        assert_eq!(
            shifted(
                "2026-03-01T00:00:00Z",
                interval(0, 1, 5 * 60 * 1_000_000_000),
                true
            ),
            at("2026-02-27T23:55:00Z")
        );
        assert!(shifted_timestamp(0, &interval(0, 0, 1), false).is_err());
        assert!(shifted_timestamp(i64::MAX, &interval(0, 1, 0), false).is_err());
    }

    // `now()` is the evaluation time the expression is bound to, and has no
    // value without one.
    #[test]
    fn now_reads_the_evaluation_time() {
        let schema = planner_types::ir::schema::Schema::new(vec![]);
        let now_minus_a_second = ScalarExpr::Arithmetic {
            op: ArithmeticOpKind::Sub,
            left: Box::new(ScalarExpr::CurrentTimestamp),
            right: Box::new(ScalarExpr::Literal(ScalarValue::Interval {
                months: 0,
                days: 0,
                nanos: 1_000_000_000,
            })),
            semantics: planner_types::ir::scalar::ExprSemantics::Sql,
        };
        validate(&now_minus_a_second, &schema).unwrap();
        assert!(matches!(
            evaluate(
                &now_minus_a_second,
                &[],
                &schema,
                Some(5_000),
                &Default::default()
            ),
            Ok(Value::Timestamp(4_000))
        ));
        assert!(evaluate(&now_minus_a_second, &[], &schema, None, &Default::default()).is_err());
    }
}

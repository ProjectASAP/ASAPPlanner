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
        // PromQL `time()`: the evaluation time in seconds.
        ScalarExpr::EvalTimestamp => evaluation_time_ms
            .map(|at_ms| Value::Float64(at_ms as f64 / 1000.0))
            .ok_or_else(|| {
                Error::Invalid(
                    "time() has a value only in a query run, at its evaluation time".into(),
                )
            }),
        ScalarExpr::Cast { expr, to, try_cast } => {
            let value = evaluate(expr, row, schema, evaluation_time_ms, regexes)?;
            match cast(value, to) {
                Ok(value) => Ok(value),
                Err(_) if *try_cast => Ok(Value::Null),
                Err(error) => Err(error),
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
            left,
            op:
                op @ (CompareOpKind::Like
                | CompareOpKind::NotLike
                | CompareOpKind::ILike
                | CompareOpKind::NotILike),
            right,
            ..
        } => {
            let text = evaluate(left, row, schema, evaluation_time_ms, regexes)?;
            let pattern = evaluate(right, row, schema, evaluation_time_ms, regexes)?;
            let (Value::Utf8(text), Value::Utf8(pattern)) = (text, pattern) else {
                return Ok(Value::Null);
            };
            let case_insensitive = matches!(op, CompareOpKind::ILike | CompareOpKind::NotILike);
            let negated = matches!(op, CompareOpKind::NotLike | CompareOpKind::NotILike);
            let matched = match regexes.get(&like_key(&pattern, case_insensitive)) {
                Some(regex) => regex.is_match(&text),
                None => like_regex(&pattern, case_insensitive)?.is_match(&text),
            };
            Ok(Value::Bool(matched != negated))
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
        // `CASE x WHEN v THEN …` is `CASE WHEN x = v THEN …`, `x` evaluated
        // once: a NULL `x` or `v` matches no branch.
        ScalarExpr::Case {
            operand: Some(operand),
            branches,
            else_expr,
        } => {
            let operand = evaluate(operand, row, schema, evaluation_time_ms, regexes)?;
            for (value, result) in branches {
                let value = evaluate(value, row, schema, evaluation_time_ms, regexes)?;
                if matches!(
                    compare(&CompareOpKind::Eq, operand.clone(), value)?,
                    Value::Bool(true)
                ) {
                    return evaluate(result, row, schema, evaluation_time_ms, regexes);
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
        // SQL's `x IN (v1, …)` is `x = v1 OR …`: true on a match, else NULL
        // when x or an item is NULL, else false; `NOT IN` negates it.
        ScalarExpr::InList {
            expr,
            list,
            negated,
        } => {
            let value = evaluate(expr, row, schema, evaluation_time_ms, regexes)?;
            let mut null = false;
            for item in list {
                let item = evaluate(item, row, schema, evaluation_time_ms, regexes)?;
                match compare(&CompareOpKind::Eq, value.clone(), item)? {
                    Value::Bool(true) => return Ok(Value::Bool(!negated)),
                    Value::Bool(false) => {}
                    Value::Null => null = true,
                    _ => return Err(Error::Invalid("boolean predicate required".into())),
                }
            }
            Ok(if null {
                Value::Null
            } else {
                Value::Bool(*negated)
            })
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

/// Whether `CAST(value AS to)` from `from` has a native evaluation:
/// numbers to each other, an integer or a boolean to text, text to a
/// number, and integers to booleans and back.
fn castable(from: &DataType, to: &DataType) -> bool {
    use DataType::*;
    from == to
        || *from == Null
        || matches!(
            (from, to),
            (Int64, Float64 | Utf8 | Bool)
                | (Float64, Int64)
                | (Bool, Utf8 | Int64)
                | (Utf8, Int64 | Float64)
        )
}

/// `value` cast to `to` as DataFusion's `CAST` does, for the pairs
/// [`castable`] accepts: a float truncates toward zero to an integer, an
/// integer is true unless zero, text parses as a decimal number. Bad input
/// when a value has no image (NaN or out of range to an integer, text that
/// is no number); `TRY_CAST` gives NULL instead.
fn cast(value: Value, to: &DataType) -> Result<Value, Error> {
    let fail = |value: &Value| Error::Invalid(format!("cannot cast {value:?} to {to:?}"));
    Ok(match (value, to) {
        (value, _) if value.matches(to, true) => value,
        (Value::Int64(v), DataType::Float64) => Value::Float64(v as f64),
        // Every finite float in [-2^63, 2^63) truncates into Int64.
        (Value::Float64(v), DataType::Int64)
            if v.is_finite()
                && (-9_223_372_036_854_775_808.0..9_223_372_036_854_775_808.0).contains(&v) =>
        {
            Value::Int64(v.trunc() as i64)
        }
        (Value::Int64(v), DataType::Utf8) => Value::Utf8(v.to_string().into()),
        (Value::Bool(v), DataType::Utf8) => Value::Utf8(v.to_string().into()),
        (Value::Int64(v), DataType::Bool) => Value::Bool(v != 0),
        (Value::Bool(v), DataType::Int64) => Value::Int64(v.into()),
        (Value::Utf8(text), DataType::Int64) => match text.parse() {
            Ok(v) => Value::Int64(v),
            Err(_) => return Err(fail(&Value::Utf8(text))),
        },
        (Value::Utf8(text), DataType::Float64) => match text.parse() {
            Ok(v) => Value::Float64(v),
            Err(_) => return Err(fail(&Value::Utf8(text))),
        },
        (value, _) => return Err(fail(&value)),
    })
}

/// The key of SQL `LIKE` pattern `pattern` among an expression's compiled
/// patterns, apart from the PromQL label patterns.
fn like_key(pattern: &str, case_insensitive: bool) -> String {
    format!(
        "\0{}like\0{pattern}",
        if case_insensitive { "i" } else { "" }
    )
}

/// SQL `LIKE` pattern `pattern` as an anchored regex, as Arrow's `like`
/// kernel reads it: `%` is any text, `_` any one character, `\` makes the
/// next character literal (a trailing `\` is itself); `ILIKE` ignores case.
fn like_regex(pattern: &str, case_insensitive: bool) -> Result<regex::Regex, Error> {
    let mut regex = String::from(if case_insensitive { "(?is)^" } else { "(?s)^" });
    let mut chars = pattern.chars();
    while let Some(c) = chars.next() {
        match c {
            '%' => regex.push_str(".*"),
            '_' => regex.push('.'),
            '\\' => regex.push_str(&regex::escape(&chars.next().unwrap_or('\\').to_string())),
            c => regex.push_str(&regex::escape(&c.to_string())),
        }
    }
    regex.push('$');
    regex::Regex::new(&regex).map_err(|e| Error::Invalid(format!("LIKE pattern {pattern:?}: {e}")))
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
/// pattern text, and of each literal SQL `LIKE` pattern, by [`like_key`].
pub(super) type LabelRegexes = std::collections::HashMap<String, regex::Regex>;

/// Compiles every `=~` / `!~` pattern in `expr` ([`super::promql_anchored_regex`]).
fn label_regexes_of(expr: &ScalarExpr) -> Result<LabelRegexes, Error> {
    let mut regexes = LabelRegexes::new();
    let mut pending = vec![expr];
    while let Some(expr) = pending.pop() {
        if let ScalarExpr::Compare {
            op:
                op @ (CompareOpKind::Like
                | CompareOpKind::NotLike
                | CompareOpKind::ILike
                | CompareOpKind::NotILike),
            right,
            ..
        } = expr
        {
            if let ScalarExpr::Literal(ScalarValue::Utf8(pattern)) = &**right {
                let case_insensitive = matches!(op, CompareOpKind::ILike | CompareOpKind::NotILike);
                if let std::collections::hash_map::Entry::Vacant(entry) =
                    regexes.entry(like_key(pattern, case_insensitive))
                {
                    entry.insert(like_regex(pattern, case_insensitive)?);
                }
            }
        }
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

    /// Whether this expression reads SQL `now()` or PromQL `time()`.
    pub(crate) fn reads_current_timestamp(&self) -> bool {
        fn reads(expr: &ScalarExpr) -> bool {
            matches!(
                expr,
                ScalarExpr::CurrentTimestamp | ScalarExpr::EvalTimestamp
            ) || expr.children().into_iter().any(reads)
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

    /// `expression` compiled over `input` whose other columns may hold
    /// summary state: a filter that keeps whole states by their keys. The
    /// expression itself reads only plain columns.
    pub fn compile_over_states(expression: &PlannedExpr, input: &SchemaRef) -> Result<Self, Error> {
        if !expression.operator_refs().is_empty() {
            return Err(Error::Invalid(
                "a state filter must read only plain columns".into(),
            ));
        }
        Self::bind_over_states(&expression.map_operator_refs(&mut |_| ()), input)
    }

    fn bind_over_states(expression: &ScalarExpr, input: &SchemaRef) -> Result<Self, Error> {
        if expression.columns_referenced().iter().any(|&column| {
            input
                .fields
                .get(column)
                .is_none_or(|field| !field.is_plain())
        }) {
            return Err(Error::Invalid(
                "a state filter must read only plain columns".into(),
            ));
        }
        Self::bind(expression, input)
    }

    fn compile_local(expression: &ScalarExpr, input: &SchemaRef) -> Result<Self, Error> {
        if !input.is_all_plain() {
            return Err(Error::Invalid(
                "scalar expression cannot consume summary state".into(),
            ));
        }
        Self::bind(expression, input)
    }

    fn bind(expression: &ScalarExpr, input: &SchemaRef) -> Result<Self, Error> {
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
        let checked = if input.is_all_plain() {
            Self::compile_local(&self.expression, input)?
        } else {
            // A state filter's expression, compiled by `compile_over_states`.
            Self::bind_over_states(&self.expression, input)?
        };
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
                match column.plain_dtype() {
                    Some(dtype) => !value.matches(dtype, column.nullable),
                    // A state column a state filter carries but does not read;
                    // batch evaluation passes NULL for columns it does not read.
                    None => !matches!(value, Value::Summary { .. } | Value::Null),
                }
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
        ScalarExpr::Column(_)
        | ScalarExpr::Literal(_)
        | ScalarExpr::CurrentTimestamp
        | ScalarExpr::EvalTimestamp => Ok(()),
        ScalarExpr::Cast { expr, to, .. } => {
            let source = expr
                .scalar_type(schema)
                .map_err(|e| Error::Invalid(e.to_string()))?
                .0;
            if !castable(&source, to) {
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
        // SQL `LIKE` / `ILIKE`: text against a text pattern; a literal
        // pattern compiles here, so a bad one fails before any row.
        ScalarExpr::Compare {
            left,
            right,
            op:
                op @ (CompareOpKind::Like
                | CompareOpKind::NotLike
                | CompareOpKind::ILike
                | CompareOpKind::NotILike),
            ..
        } => {
            for side in [left, right] {
                validate(side, schema)?;
                if !matches!(
                    side.scalar_type(schema)
                        .map_err(|e| Error::Invalid(e.to_string()))?
                        .0,
                    DataType::Utf8 | DataType::Null
                ) {
                    return Err(invalid());
                }
            }
            if let ScalarExpr::Literal(ScalarValue::Utf8(pattern)) = &**right {
                like_regex(
                    pattern,
                    matches!(op, CompareOpKind::ILike | CompareOpKind::NotILike),
                )?;
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
        // Each branch compares its value to the operand.
        ScalarExpr::Case {
            operand: Some(operand),
            branches,
            else_expr,
        } => {
            for (value, result) in branches {
                validate(
                    &ScalarExpr::Compare {
                        left: operand.clone(),
                        op: CompareOpKind::Eq,
                        right: Box::new(value.clone()),
                        semantics: planner_types::ir::scalar::ExprSemantics::Sql,
                    },
                    schema,
                )?;
                validate(result, schema)?;
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
        // Valid when its operand is (an empty list still evaluates it) and
        // each `expr = item` it evaluates is.
        ScalarExpr::InList { expr, list, .. } => {
            validate(expr, schema)?;
            list.iter().try_for_each(|item| {
                validate(
                    &ScalarExpr::Compare {
                        left: expr.clone(),
                        op: CompareOpKind::Eq,
                        right: Box::new(item.clone()),
                        semantics: planner_types::ir::scalar::ExprSemantics::Sql,
                    },
                    schema,
                )
            })
        }
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

    // `IN` and `NOT IN` follow SQL: a match is true; otherwise a NULL
    // operand or item gives NULL, else false.
    #[test]
    fn in_list_follows_sql_three_valued_logic() {
        let schema = Arc::new(planner_types::ir::schema::Schema::new(vec![
            planner_types::ir::schema::Field::plain("city", DataType::Utf8, true),
        ]));
        let text = |value: &str| PlannedExpr::Literal(ScalarValue::Utf8(value.into()));
        let in_list = |list: Vec<PlannedExpr>, negated: bool| {
            CompiledExpression::compile(
                &PlannedExpr::InList {
                    expr: Box::new(PlannedExpr::Column(0)),
                    list,
                    negated,
                },
                &schema,
            )
            .unwrap()
        };
        let nyc_sf = in_list(vec![text("NYC"), text("SF")], false);
        let not_nyc_sf = in_list(vec![text("NYC"), text("SF")], true);
        let with_null = in_list(
            vec![text("NYC"), PlannedExpr::Literal(ScalarValue::Null)],
            false,
        );
        let utf8 = |value: &str| vec![Value::Utf8(value.into())];
        let result = |expression: &CompiledExpression, row: Vec<Value>| match expression
            .evaluate(&row)
            .unwrap()
        {
            Value::Bool(value) => Some(value),
            Value::Null => None,
            other => panic!("{other:?}"),
        };
        assert_eq!(result(&nyc_sf, utf8("SF")), Some(true));
        assert_eq!(result(&nyc_sf, utf8("LA")), Some(false));
        assert_eq!(result(&nyc_sf, vec![Value::Null]), None);
        assert_eq!(result(&not_nyc_sf, utf8("LA")), Some(true));
        assert_eq!(result(&not_nyc_sf, utf8("NYC")), Some(false));
        assert_eq!(result(&with_null, utf8("NYC")), Some(true));
        assert_eq!(result(&with_null, utf8("LA")), None);
        // An item of another type is not comparable.
        assert!(CompiledExpression::compile(
            &PlannedExpr::InList {
                expr: Box::new(PlannedExpr::Column(0)),
                list: vec![PlannedExpr::Literal(ScalarValue::Int64(1))],
                negated: false,
            },
            &schema,
        )
        .is_err());
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

    fn text(value: &str) -> ScalarExpr {
        ScalarExpr::Literal(ScalarValue::Utf8(value.into()))
    }

    /// `expr` over the row `row` of `schema`, compiled as the executor
    /// compiles it.
    fn value_of(
        expr: ScalarExpr,
        schema: &[(DataType, bool)],
        row: Vec<Value>,
    ) -> Result<Value, Error> {
        let schema = Arc::new(planner_types::ir::schema::Schema::new(
            schema
                .iter()
                .enumerate()
                .map(|(i, (dtype, nullable))| {
                    planner_types::ir::schema::Field::plain(
                        format!("c{i}"),
                        dtype.clone(),
                        *nullable,
                    )
                })
                .collect(),
        ));
        CompiledExpression::bind(&expr, &schema)?.evaluate(&row)
    }

    // `LIKE`: `%` any text, `_` one character, `\` a literal next
    // character, the whole text matched; `ILIKE` ignores case; NULL in,
    // NULL out.
    #[test]
    fn like_follows_sql() {
        let like = |value: Option<&str>, op: CompareOpKind, pattern: &str| match value_of(
            ScalarExpr::Compare {
                left: Box::new(ScalarExpr::Column(0)),
                op,
                right: Box::new(text(pattern)),
                semantics: planner_types::ir::scalar::ExprSemantics::Sql,
            },
            &[(DataType::Utf8, true)],
            vec![value.map_or(Value::Null, |v| Value::Utf8(v.into()))],
        )
        .unwrap()
        {
            Value::Bool(b) => Some(b),
            Value::Null => None,
            other => panic!("{other:?}"),
        };
        use CompareOpKind::*;
        assert_eq!(like(Some("abc"), Like, "a%"), Some(true));
        assert_eq!(like(Some("abc"), Like, "b%"), Some(false));
        assert_eq!(like(Some("abc"), Like, "a_c"), Some(true));
        assert_eq!(like(Some("abbc"), Like, "a_c"), Some(false));
        assert_eq!(like(Some("a_c"), Like, "a\\_c"), Some(true));
        assert_eq!(like(Some("abc"), Like, "a\\_c"), Some(false));
        assert_eq!(like(Some("a.c"), Like, "a.c"), Some(true));
        assert_eq!(like(Some("abc"), Like, "a.c"), Some(false));
        assert_eq!(like(Some("a\nb"), Like, "a%b"), Some(true));
        assert_eq!(like(Some("ABC"), Like, "a%"), Some(false));
        assert_eq!(like(Some("ABC"), ILike, "a%"), Some(true));
        assert_eq!(like(Some("ABC"), NotILike, "a%"), Some(false));
        assert_eq!(like(Some("abc"), NotLike, "x%"), Some(true));
        assert_eq!(like(None, Like, "%"), None);
    }

    // `CASE x WHEN v THEN …` matches the first branch with `x = v`; a NULL
    // operand matches none.
    #[test]
    fn case_with_an_operand_compares_it() {
        let case = |x: Value| {
            value_of(
                ScalarExpr::Case {
                    operand: Some(Box::new(ScalarExpr::Column(0))),
                    branches: vec![
                        (ScalarExpr::Literal(ScalarValue::Int64(1)), text("one")),
                        (ScalarExpr::Literal(ScalarValue::Int64(2)), text("two")),
                    ],
                    else_expr: Some(Box::new(text("other"))),
                },
                &[(DataType::Int64, true)],
                vec![x],
            )
            .unwrap()
        };
        assert!(matches!(case(Value::Int64(2)), Value::Utf8(t) if &*t == "two"));
        assert!(matches!(case(Value::Int64(3)), Value::Utf8(t) if &*t == "other"));
        assert!(matches!(case(Value::Null), Value::Utf8(t) if &*t == "other"));
    }

    // Casts follow DataFusion: a float truncates toward zero to an integer;
    // NaN, out-of-range and unparsable text fail, or are NULL under
    // `TRY_CAST`.
    #[test]
    fn casts_follow_datafusion() {
        let cast = |value: Value, from: DataType, to: DataType, try_cast: bool| {
            value_of(
                ScalarExpr::Cast {
                    expr: Box::new(ScalarExpr::Column(0)),
                    to,
                    try_cast,
                },
                &[(from, true)],
                vec![value],
            )
        };
        let shown = |value: Result<Value, Error>| format!("{:?}", value.map_err(|_| ()));
        use DataType::*;
        assert_eq!(
            shown(cast(Value::Float64(-2.7), Float64, Int64, false)),
            shown(Ok(Value::Int64(-2)))
        );
        assert!(cast(Value::Float64(f64::NAN), Float64, Int64, false).is_err());
        assert!(cast(Value::Float64(1e19), Float64, Int64, false).is_err());
        assert_eq!(
            shown(cast(Value::Float64(1e19), Float64, Int64, true)),
            shown(Ok(Value::Null))
        );
        assert_eq!(
            shown(cast(Value::Int64(-5), Int64, Utf8, false)),
            shown(Ok(Value::Utf8("-5".into())))
        );
        assert_eq!(
            shown(cast(Value::Bool(true), Bool, Utf8, false)),
            shown(Ok(Value::Utf8("true".into())))
        );
        assert_eq!(
            shown(cast(Value::Utf8("42".into()), Utf8, Int64, false)),
            shown(Ok(Value::Int64(42)))
        );
        assert_eq!(
            shown(cast(Value::Utf8("2.5".into()), Utf8, Float64, false)),
            shown(Ok(Value::Float64(2.5)))
        );
        assert!(cast(Value::Utf8("x".into()), Utf8, Int64, false).is_err());
        assert_eq!(
            shown(cast(Value::Utf8("x".into()), Utf8, Int64, true)),
            shown(Ok(Value::Null))
        );
        assert_eq!(
            shown(cast(Value::Int64(0), Int64, Bool, false)),
            shown(Ok(Value::Bool(false)))
        );
        assert_eq!(
            shown(cast(Value::Bool(true), Bool, Int64, false)),
            shown(Ok(Value::Int64(1)))
        );
        // A cast without a native evaluation fails at compile time.
        assert!(cast(Value::Float64(1.5), Float64, Utf8, false).is_err());
    }

    // PromQL `time()` is the evaluation time in seconds, bound per query
    // run; it has no value outside one.
    #[test]
    fn time_reads_the_evaluation_time_in_seconds() {
        let schema = planner_types::ir::schema::Schema::new(vec![]);
        validate(&ScalarExpr::EvalTimestamp, &schema).unwrap();
        assert!(matches!(
            evaluate(&ScalarExpr::EvalTimestamp, &[], &schema, Some(1_500), &Default::default()),
            Ok(Value::Float64(v)) if v == 1.5
        ));
        assert!(evaluate(
            &ScalarExpr::EvalTimestamp,
            &[],
            &schema,
            None,
            &Default::default()
        )
        .is_err());
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

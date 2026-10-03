//! Scalar semantics and typed expression binding. Planner expressions enter through CompiledExpression.
use crate::{
    values::{plain, SchemaRef, Value},
    Error,
};
use planner_types::pre_asap::{ArithmeticOpKind, DataType};
pub mod arithmetic;
pub mod binary;
mod planner;
pub use planner::CompiledExpression;
#[derive(serde::Serialize, serde::Deserialize, Clone, Debug)]
pub enum Expression {
    Binary {
        operator: crate::expressions::binary::BinaryOperator,
        left: Box<Expression>,
        right: Box<Expression>,
    },
    Planner(Box<crate::expressions::CompiledExpression>),
    Column(usize),
    ExactFloat64(usize),
    FiniteFloat64(Box<Expression>),
    LabelSet {
        column: usize,
        labels: Vec<String>,
        without: bool,
    },
    /// One label of a label map; an absent label reads as empty, as in PromQL.
    Label {
        column: usize,
        name: String,
    },
    /// Canonical encoding of a label map less `excluding`, identical to
    /// `promql_rows::encode_series_identity`.
    LabelIdentity {
        column: usize,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        excluding: Vec<String>,
    },
    Literal {
        value: Value,
        dtype: DataType,
    },
    Negate(Box<Expression>),
    Arithmetic {
        op: ArithmeticOpKind,
        left: Box<Expression>,
        right: Box<Expression>,
    },
    Equal(Box<Expression>, Box<Expression>),
    Less(Box<Expression>, Box<Expression>),
    And(Box<Expression>, Box<Expression>),
    Or(Box<Expression>, Box<Expression>),
    Not(Box<Expression>),
    IsNull(Box<Expression>),
}
impl Expression {
    pub fn planner(expression: crate::expressions::CompiledExpression) -> Self {
        Self::Planner(Box::new(expression))
    }
    pub(crate) fn dtype(&self, input: &SchemaRef) -> Result<(DataType, bool), Error> {
        use Expression::*;
        match self {
            Binary {
                operator,
                left,
                right,
            } => {
                use crate::expressions::binary::BinaryOpKind;
                use planner_types::pre_asap::CompareOpKind;
                let (a, n) = left.dtype(input)?;
                let (b, m) = right.dtype(input)?;
                if a != DataType::Float64 || b != a || operator.vector_match.is_some() {
                    return Err(invalid(
                        "binary expression requires resolved Float64 operands",
                    ));
                }
                if (operator.checked_relative_division || operator.checked_finite_division)
                    && operator.kind != BinaryOpKind::Arithmetic(ArithmeticOpKind::Div)
                {
                    return Err(invalid("checked division contract on non-division"));
                }
                let dtype = match operator.kind {
                    BinaryOpKind::Arithmetic(_) => DataType::Float64,
                    BinaryOpKind::Compare(
                        CompareOpKind::Eq
                        | CompareOpKind::Ne
                        | CompareOpKind::Lt
                        | CompareOpKind::Le
                        | CompareOpKind::Gt
                        | CompareOpKind::Ge,
                    ) => DataType::Bool,
                    BinaryOpKind::CompareBool(
                        CompareOpKind::Eq
                        | CompareOpKind::Ne
                        | CompareOpKind::Lt
                        | CompareOpKind::Le
                        | CompareOpKind::Gt
                        | CompareOpKind::Ge,
                    ) => DataType::Float64,
                    _ => return Err(invalid("unsupported binary operation")),
                };
                Ok((dtype, n || m))
            }
            Planner(expression) => {
                expression.validate_input(input)?;
                Ok(expression.dtype())
            }
            FiniteFloat64(expression) => {
                if expression.dtype(input)? != (DataType::Float64, false) {
                    return Err(invalid("finite update requires non-null Float64"));
                }
                Ok((DataType::Float64, false))
            }
            ExactFloat64(column) => {
                let (dtype, nullable) = plain(input, *column)?;
                if nullable || !matches!(dtype, DataType::Int64 | DataType::Float64) {
                    return Err(invalid(
                        "exact Float64 conversion requires non-null numeric input",
                    ));
                }
                Ok((DataType::Float64, false))
            }
            LabelSet { column, labels, .. } => {
                let (dtype, nullable) = plain(input, *column)?;
                let expected = DataType::Map {
                    key: Box::new(DataType::Utf8),
                    value: Box::new(DataType::Utf8),
                    value_nullable: false,
                };
                if dtype != &expected
                    || nullable
                    || labels
                        .iter()
                        .collect::<std::collections::BTreeSet<_>>()
                        .len()
                        != labels.len()
                {
                    return Err(invalid(
                        "label projection requires a non-null Utf8 map and unique label names",
                    ));
                }
                Ok((expected, false))
            }
            Label { column, .. } | LabelIdentity { column, .. } => {
                if plain(input, *column)?
                    != (
                        &DataType::Map {
                            key: Box::new(DataType::Utf8),
                            value: Box::new(DataType::Utf8),
                            value_nullable: false,
                        },
                        false,
                    )
                {
                    return Err(invalid("label read requires a non-null Utf8 map"));
                }
                Ok((DataType::Utf8, false))
            }
            Column(i) => {
                let (t, n) = plain(input, *i)?;
                Ok((t.clone(), n))
            }
            Literal { value, dtype } => {
                if value.matches(dtype, true) {
                    Ok((dtype.clone(), matches!(value, Value::Null)))
                } else {
                    Err(invalid("literal type mismatch"))
                }
            }
            Negate(v) => {
                let (t, n) = v.dtype(input)?;
                if matches!(t, DataType::Int64 | DataType::Float64) {
                    Ok((t, n))
                } else {
                    Err(invalid("numeric negation required"))
                }
            }
            Arithmetic { op, left, right } => {
                let (a, n) = left.dtype(input)?;
                let (b, m) = right.dtype(input)?;
                if a == b
                    && matches!(a, DataType::Int64 | DataType::Float64)
                    && !(a == DataType::Int64 && *op == ArithmeticOpKind::Atan2)
                {
                    Ok((a, n || m))
                } else {
                    Err(invalid("arithmetic requires matching numeric types"))
                }
            }
            Equal(a, b) | Less(a, b) => {
                let (a, n) = a.dtype(input)?;
                let (b, m) = b.dtype(input)?;
                if a == b && ordered(&a) {
                    Ok((DataType::Bool, n || m))
                } else {
                    Err(invalid("comparison requires matching ordered types"))
                }
            }
            And(a, b) | Or(a, b) => {
                let (a, n) = a.dtype(input)?;
                let (b, m) = b.dtype(input)?;
                if a == DataType::Bool && b == DataType::Bool {
                    Ok((DataType::Bool, n || m))
                } else {
                    Err(invalid("boolean operands required"))
                }
            }
            Not(v) => {
                let (t, n) = v.dtype(input)?;
                if t == DataType::Bool {
                    Ok((t, n))
                } else {
                    Err(invalid("boolean operand required"))
                }
            }
            IsNull(v) => {
                v.dtype(input)?;
                Ok((DataType::Bool, false))
            }
        }
    }
    pub(crate) fn evaluate(&self, row: &[Value]) -> Result<Value, Error> {
        use Expression::*;
        Ok(match self {
            FiniteFloat64(expression) => match expression.evaluate(row)? {
                Value::Float64(value) if value.is_finite() => Value::Float64(value),
                _ => return Err(invalid("summary update must be finite")),
            },
            ExactFloat64(column) => match row[*column] {
                Value::Float64(value) => Value::Float64(value),
                Value::Int64(value) if value.unsigned_abs() <= (1u64 << 53) => {
                    Value::Float64(value as f64)
                }
                _ => {
                    return Err(invalid(
                        "numeric result cannot be represented exactly as Float64",
                    ))
                }
            },
            LabelSet {
                column,
                labels,
                without,
            } => {
                let Value::Map(entries) = &row[*column] else {
                    return Err(invalid("label projection requires a map"));
                };
                let mut selected = std::collections::BTreeMap::new();
                let mut seen = std::collections::BTreeSet::new();
                for (key, value) in entries.iter() {
                    let (Value::Utf8(key), Value::Utf8(value)) = (key, value) else {
                        return Err(invalid("label projection requires Utf8 entries"));
                    };
                    if !seen.insert(key.clone()) {
                        return Err(invalid("duplicate label name"));
                    }
                    let keep = if *without {
                        key.as_ref() != "__name__"
                            && !labels.iter().any(|label| label.as_str() == key.as_ref())
                    } else {
                        labels.iter().any(|label| label.as_str() == key.as_ref())
                    };
                    if keep && !value.is_empty() {
                        selected.insert(key.clone(), value.clone());
                    }
                }
                Value::Map(
                    selected
                        .into_iter()
                        .map(|(k, v)| (Value::Utf8(k), Value::Utf8(v)))
                        .collect::<Vec<_>>()
                        .into(),
                )
            }
            Binary {
                operator,
                left,
                right,
            } => {
                let (a, b) = (left.evaluate(row)?, right.evaluate(row)?);
                if matches!(a, Value::Null) || matches!(b, Value::Null) {
                    Value::Null
                } else {
                    let (Value::Float64(a), Value::Float64(b)) = (a, b) else {
                        return Err(invalid("binary value schema mismatch"));
                    };
                    arithmetic::evaluate_binary(operator, a, b)?
                }
            }
            Planner(expression) => expression.evaluate(row)?,
            Label { column, name } => {
                let Value::Map(entries) = &row[*column] else {
                    return Err(invalid("label read requires a map"));
                };
                let mut found = None;
                for (key, value) in entries.iter() {
                    let (Value::Utf8(key), Value::Utf8(value)) = (key, value) else {
                        return Err(invalid("label read requires Utf8 entries"));
                    };
                    if key.as_ref() == name.as_str() && found.replace(value.clone()).is_some() {
                        return Err(invalid("duplicate label name"));
                    }
                }
                Value::Utf8(found.unwrap_or_else(|| "".into()))
            }
            LabelIdentity { column, excluding } => {
                let Value::Map(entries) = &row[*column] else {
                    return Err(invalid("label identity requires a map"));
                };
                let mut labels = std::collections::BTreeMap::new();
                for (key, value) in entries.iter() {
                    let (Value::Utf8(key), Value::Utf8(value)) = (key, value) else {
                        return Err(invalid("label identity requires Utf8 entries"));
                    };
                    if excluding.iter().any(|label| label.as_str() == key.as_ref()) {
                        continue;
                    }
                    if labels.insert(key.to_string(), value.to_string()).is_some() {
                        return Err(invalid("duplicate label name"));
                    }
                }
                Value::Utf8(
                    crate::physical_planner::promql_rows::encode_series_identity(&labels)?.into(),
                )
            }
            Column(i) => row[*i].clone(),
            Literal { value, .. } => value.clone(),
            Negate(v) => match v.evaluate(row)? {
                Value::Int64(v) => Value::Int64(
                    v.checked_neg()
                        .ok_or_else(|| invalid("integer negation overflow"))?,
                ),
                Value::Float64(v) => Value::Float64(-v),
                Value::Null => Value::Null,
                _ => return Err(invalid("numeric negation required")),
            },
            Arithmetic { op, left, right } => {
                numeric(op, left.evaluate(row)?, right.evaluate(row)?)?
            }
            Equal(a, b) | Less(a, b) => {
                let (a, b) = (a.evaluate(row)?, b.evaluate(row)?);
                if matches!(a, Value::Null) || matches!(b, Value::Null) {
                    Value::Null
                } else if matches!((&a,&b),(Value::Float64(a),Value::Float64(b)) if a.is_nan() || b.is_nan())
                {
                    Value::Bool(false)
                } else {
                    let c = a.compare(&b)?;
                    Value::Bool(if matches!(self, Equal(..)) {
                        c.is_eq()
                    } else {
                        c.is_lt()
                    })
                }
            }
            And(a, b) | Or(a, b) => {
                let (a, b) = (a.evaluate(row)?, b.evaluate(row)?);
                match (a, b, matches!(self, And(..))) {
                    (Value::Bool(false), _, true) | (_, Value::Bool(false), true) => {
                        Value::Bool(false)
                    }
                    (Value::Bool(true), _, false) | (_, Value::Bool(true), false) => {
                        Value::Bool(true)
                    }
                    (Value::Null, _, _) | (_, Value::Null, _) => Value::Null,
                    (Value::Bool(a), Value::Bool(b), true) => Value::Bool(a && b),
                    (Value::Bool(a), Value::Bool(b), false) => Value::Bool(a || b),
                    _ => return Err(invalid("boolean operands required")),
                }
            }
            Not(v) => match v.evaluate(row)? {
                Value::Bool(v) => Value::Bool(!v),
                Value::Null => Value::Null,
                _ => return Err(invalid("boolean operand required")),
            },
            IsNull(v) => Value::Bool(matches!(v.evaluate(row)?, Value::Null)),
        })
    }
}
pub(crate) fn ordered(dtype: &DataType) -> bool {
    if let DataType::Map { key, value, .. } = dtype {
        return ordered(key) && ordered(value);
    }
    matches!(
        dtype,
        DataType::Null
            | DataType::Int64
            | DataType::Float64
            | DataType::Utf8
            | DataType::Bool
            | DataType::Timestamp
            | DataType::Date
    )
}
pub(crate) fn numeric(op: &ArithmeticOpKind, a: Value, b: Value) -> Result<Value, Error> {
    use ArithmeticOpKind::*;
    Ok(match (a, b) {
        (Value::Null, _) | (_, Value::Null) => Value::Null,
        (Value::Float64(a), Value::Float64(b)) => {
            Value::Float64(arithmetic::evaluate_float64_arithmetic(op, a, b))
        }
        (Value::Int64(a), Value::Int64(b)) => Value::Int64(
            match op {
                Add => a.checked_add(b),
                Sub => a.checked_sub(b),
                Mul => a.checked_mul(b),
                Div => a.checked_div(b),
                Mod => a.checked_rem(b),
                Pow => u32::try_from(b).ok().and_then(|b| a.checked_pow(b)),
                Atan2 => None,
            }
            .ok_or_else(|| invalid("invalid integer arithmetic or overflow"))?,
        ),
        _ => return Err(invalid("arithmetic type mismatch")),
    })
}

fn invalid(message: &str) -> Error {
    Error::Invalid(message.into())
}

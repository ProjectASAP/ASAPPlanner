//! Scalar semantics and typed expression binding. Planner expressions enter through CompiledExpression.
use crate::{
    values::{plain, Schema, Value},
    Error,
};
use planner_types::pre_asap::{ArithmeticOpKind, DataType};
pub mod arithmetic;
mod planner;
pub use planner::CompiledExpression;
#[derive(Clone, Debug)]
pub enum Expression {
    Binary {
        operator: planner_types::post_asap::BinaryOperator,
        left: Box<Expression>,
        right: Box<Expression>,
    },
    Planner(Box<crate::expressions::CompiledExpression>),
    Column(usize),
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
    pub(crate) fn dtype(&self, input: &Schema) -> Result<(DataType, bool), Error> {
        use Expression::*;
        match self {
            Binary {
                operator,
                left,
                right,
            } => {
                use planner_types::pre_asap::{BinaryOpKind, CompareOpKind};
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
                    _ => return Err(invalid("unsupported binary operation")),
                };
                Ok((dtype, n || m))
            }
            Planner(expression) => Ok(expression.dtype()),
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

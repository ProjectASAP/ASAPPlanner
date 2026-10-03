//! Float64 arithmetic shared by ASAP execution engines.
//! Preserve IEEE non-finite results; callers own their output policies.

pub fn evaluate_float64_arithmetic(
    operator: &planner_types::ir::scalar::ArithmeticOpKind,
    left: f64,
    right: f64,
) -> f64 {
    use planner_types::ir::scalar::ArithmeticOpKind::*;
    match operator {
        Add => left + right,
        Sub => left - right,
        Mul => left * right,
        Div => left / right,
        Mod => left % right,
        Pow => left.powf(right),
        Atan2 => left.atan2(right),
    }
}

/// Execute the Planner binary contract after a deployment has resolved matching rows.
pub fn evaluate_binary(
    operator: &crate::expressions::binary::BinaryOperator,
    left: f64,
    right: f64,
) -> Result<crate::values::Value, crate::Error> {
    use crate::expressions::binary::BinaryOpKind;
    use crate::{values::Value, Error};
    use planner_types::ir::scalar::ArithmeticOpKind;
    let invalid =
        || Error::Invalid("unsupported binary operation or invalid checked-division domain".into());
    if operator.vector_match.is_some() {
        return Err(invalid());
    }
    if operator.checked_relative_division || operator.checked_finite_division {
        if operator.kind != BinaryOpKind::Arithmetic(ArithmeticOpKind::Div)
            || !left.is_finite()
            || !right.is_finite()
            || right == 0.
        {
            return Err(invalid());
        }
        let value = left / right;
        if !value.is_finite() || (operator.checked_relative_division && !value.is_normal()) {
            return Err(invalid());
        }
        return Ok(Value::Float64(value));
    }
    Ok(match operator.kind {
        BinaryOpKind::Arithmetic(ref op) => {
            Value::Float64(evaluate_float64_arithmetic(op, left, right))
        }
        BinaryOpKind::Compare(ref op) => Value::Bool(compare(op, left, right).ok_or_else(invalid)?),
        BinaryOpKind::CompareBool(ref op) => {
            Value::Float64(if compare(op, left, right).ok_or_else(invalid)? {
                1.
            } else {
                0.
            })
        }
        _ => return Err(invalid()),
    })
}

/// IEEE comparison, as Go's: NaN is unequal to everything, itself included.
fn compare(op: &planner_types::ir::scalar::CompareOpKind, left: f64, right: f64) -> Option<bool> {
    use planner_types::ir::scalar::CompareOpKind;
    Some(match op {
        CompareOpKind::Eq => left == right,
        CompareOpKind::Ne => left != right,
        CompareOpKind::Lt => left < right,
        CompareOpKind::Le => left <= right,
        CompareOpKind::Gt => left > right,
        CompareOpKind::Ge => left >= right,
        _ => return None,
    })
}

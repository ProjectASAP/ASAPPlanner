//! Float64 arithmetic shared by ASAP execution engines.
//! Preserve IEEE non-finite results; callers own their output policies.

pub fn evaluate_float64_arithmetic(
    operator: &planner_types::pre_asap::ArithmeticOpKind,
    left: f64,
    right: f64,
) -> f64 {
    use planner_types::pre_asap::ArithmeticOpKind::*;
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

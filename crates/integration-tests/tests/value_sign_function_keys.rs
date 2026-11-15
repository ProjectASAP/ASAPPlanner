//! Every PromQL math or date function name parses to one typed key, the
//! `MathFunc` or `TimeFunc` that both the value-sign rules and the error
//! contracts match on (sign test plan, T2.1 and T2.2).
//!
//! The names come from the vendored MetricsQL parser's
//! `TransformFunction` list, which is independent of the planner's own name
//! tables: a function the planner lowers (`promql_function_arity` accepts
//! `promql_<name>`) but the parse functions do not know fails T2.1, and a
//! typed key no name produces fails T2.2.
use std::collections::BTreeMap;
use std::mem::discriminant;

use asap_types::ir::operator::{MathFunc, TimeFunc};
use asap_types::ir::scalar::expr_ir::ScalarValue;
use asap_types::ir::scalar::scalar_type_rules::{
    math_function_of_call, promql_function_arity, time_function_of_call,
};
use asap_types::ir::scalar::ScalarExpr;
use metricsql_parser::functions::TransformFunction;
use strum::IntoEnumIterator;

/// The literal bounds each call passes after its value argument, chosen so
/// that every parameter has a distinct value: `clamp(v, 1, 2)`,
/// `round(v, 0.5)`.
fn literals(name: &str, arity: usize) -> Vec<f64> {
    match (name, arity) {
        ("clamp", 3) => vec![1.0, 2.0],
        ("clamp_min", 2) => vec![3.0],
        ("clamp_max", 2) => vec![4.0],
        ("round", 2) => vec![0.5],
        (_, arity) => vec![7.0; arity - 1],
    }
}

/// `promql_<name>(value, literals…)`.
fn call_args(literals: &[f64]) -> Vec<ScalarExpr<u32>> {
    std::iter::once(ScalarExpr::Column(1))
        .chain(
            literals
                .iter()
                .map(|value| ScalarExpr::Literal(ScalarValue::Float64(*value))),
        )
        .collect()
}

/// The typed key of every PromQL math or date function, by hand from the
/// PromQL function reference, with the literals of [`literals`].
fn expected_keys() -> BTreeMap<&'static str, Key> {
    use MathFunc::*;
    let math = |f| Key::Math(f);
    let time = |f| Key::Time(f);
    BTreeMap::from([
        ("abs", math(Abs)),
        ("ceil", math(Ceil)),
        ("floor", math(Floor)),
        ("exp", math(Exp)),
        ("ln", math(Ln)),
        ("log2", math(Log2)),
        ("log10", math(Log10)),
        ("sqrt", math(Sqrt)),
        ("sgn", math(Sgn)),
        ("sin", math(Sin)),
        ("cos", math(Cos)),
        ("tan", math(Tan)),
        ("asin", math(Asin)),
        ("acos", math(Acos)),
        ("atan", math(Atan)),
        ("sinh", math(Sinh)),
        ("cosh", math(Cosh)),
        ("tanh", math(Tanh)),
        ("asinh", math(Asinh)),
        ("acosh", math(Acosh)),
        ("atanh", math(Atanh)),
        ("deg", math(Deg)),
        ("rad", math(Rad)),
        ("round", math(Round { to_nearest: 0.5 })),
        ("clamp", math(Clamp { min: 1.0, max: 2.0 })),
        ("clamp_min", math(ClampMin { min: 3.0 })),
        ("clamp_max", math(ClampMax { max: 4.0 })),
        ("minute", time(TimeFunc::Minute)),
        ("hour", time(TimeFunc::Hour)),
        ("day_of_week", time(TimeFunc::DayOfWeek)),
        ("day_of_month", time(TimeFunc::DayOfMonth)),
        ("day_of_year", time(TimeFunc::DayOfYear)),
        ("month", time(TimeFunc::Month)),
        ("year", time(TimeFunc::Year)),
        ("days_in_month", time(TimeFunc::DaysInMonth)),
    ])
}

#[derive(Debug, Clone, PartialEq)]
enum Key {
    Math(MathFunc),
    Time(TimeFunc),
}

/// The key `promql_<name>(value, literals…)` parses to, if any.
fn parsed_key(name: &str, arity: usize) -> Option<Key> {
    let full = format!("promql_{name}");
    let args = call_args(&literals(name, arity));
    math_function_of_call(&full, &args)
        .map(Key::Math)
        .or_else(|| time_function_of_call(&full).map(Key::Time))
}

/// The parser's function names the planner lowers, with their arity.
fn lowered_names() -> Vec<(&'static str, usize)> {
    TransformFunction::iter()
        .filter_map(|function| {
            let name = function.name();
            promql_function_arity(&format!("promql_{name}")).map(|arity| (name, arity))
        })
        .collect()
}

/// T2.1: every PromQL function the planner lowers parses to a typed key,
/// the exact one. The kept names are exactly the 35 math and date
/// functions (the plan's "31" counts only the one-argument ones).
///
/// Fails today: the parse functions are not implemented.
#[test]
fn every_promql_function_name_parses_to_a_typed_key() {
    let lowered = lowered_names();
    let expected = expected_keys();
    let mut kept: Vec<&str> = lowered.iter().map(|(name, _)| *name).collect();
    kept.sort_unstable();
    let mut listed: Vec<&str> = expected.keys().copied().collect();
    listed.sort_unstable();
    assert_eq!(
        kept, listed,
        "the PromQL functions promql_function_arity accepts"
    );
    let mut wrong = Vec::new();
    for (name, arity) in lowered {
        let got = parsed_key(name, arity);
        let want = expected.get(name).cloned();
        if got != want {
            wrong.push(format!("promql_{name}: parsed {got:?}, expected {want:?}"));
        }
    }
    assert!(wrong.is_empty(), "{}", wrong.join("\n"));
}

/// Every `MathFunc` variant, through an exhaustive match (no `_` arm).
fn all_math_funcs() -> Vec<MathFunc> {
    use MathFunc::*;
    let all = vec![
        Abs,
        Ceil,
        Floor,
        Exp,
        Ln,
        Log2,
        Log10,
        Sqrt,
        Sgn,
        Sin,
        Cos,
        Tan,
        Asin,
        Acos,
        Atan,
        Sinh,
        Cosh,
        Tanh,
        Asinh,
        Acosh,
        Atanh,
        Deg,
        Rad,
        Round { to_nearest: 1.0 },
        Clamp { min: 0.0, max: 1.0 },
        ClampMin { min: 0.0 },
        ClampMax { max: 1.0 },
    ];
    for function in &all {
        match function {
            Abs
            | Ceil
            | Floor
            | Exp
            | Ln
            | Log2
            | Log10
            | Sqrt
            | Sgn
            | Sin
            | Cos
            | Tan
            | Asin
            | Acos
            | Atan
            | Sinh
            | Cosh
            | Tanh
            | Asinh
            | Acosh
            | Atanh
            | Deg
            | Rad
            | Round { .. }
            | Clamp { .. }
            | ClampMin { .. }
            | ClampMax { .. } => {}
        }
    }
    all
}

/// Every `TimeFunc` variant, through an exhaustive match.
fn all_time_funcs() -> Vec<TimeFunc> {
    use TimeFunc::*;
    let all = vec![
        Timestamp,
        Minute,
        Hour,
        DayOfWeek,
        DayOfMonth,
        DayOfYear,
        Month,
        Year,
        DaysInMonth,
    ];
    for function in &all {
        match function {
            Timestamp | Minute | Hour | DayOfWeek | DayOfMonth | DayOfYear | Month | Year
            | DaysInMonth => {}
        }
    }
    all
}

/// T2.2: every typed key is produced by some PromQL name, except
/// `TimeFunc::Timestamp`, which reads a sample's time, not its value, and
/// lowers to `AggIntent::TimeFn`.
///
/// Fails today: the parse functions are not implemented.
#[test]
fn every_typed_key_is_reachable_from_a_name() {
    let produced: Vec<Key> = lowered_names()
        .into_iter()
        .filter_map(|(name, arity)| parsed_key(name, arity))
        .collect();
    let mut unreachable = Vec::new();
    for function in all_math_funcs() {
        let reached = produced
            .iter()
            .any(|key| matches!(key, Key::Math(f) if discriminant(f) == discriminant(&function)));
        if !reached {
            unreachable.push(format!("{function:?}"));
        }
    }
    for function in all_time_funcs() {
        if function == TimeFunc::Timestamp {
            continue;
        }
        if !produced.contains(&Key::Time(function)) {
            unreachable.push(format!("{function:?}"));
        }
    }
    assert!(
        unreachable.is_empty(),
        "keys no name produces: {unreachable:?}"
    );
}

/// A bound that is not a literal names no typed key:
/// `promql_clamp_min(value, value)` → `None`.
#[test]
fn a_non_literal_bound_has_no_typed_key() {
    let args: Vec<ScalarExpr<u32>> = vec![ScalarExpr::Column(1), ScalarExpr::Column(1)];
    assert_eq!(math_function_of_call("promql_clamp_min", &args), None);
}

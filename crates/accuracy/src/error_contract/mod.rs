//! Each operator's error contract: the linear bound $g_c$ on a consumer's
//! output error from its inputs' errors (concepts doc, section 4).

use asap_types::accuracy::AccuracyMetric;
use asap_types::ir::operator::arguments::{BinaryOpKind, PromQLVectorSetOpKind};
use asap_types::ir::operator::{AggIntent, Reduction};
use asap_types::ir::scalar::expr_ir::ArithmeticOpKind;
use asap_types::ir::scalar::{resolve_column_ref, ColumnRef, ScalarValue};
use asap_types::ir::schema::FieldDataType;
use asap_types::ir::{ASAPOp, BinaryOperator, NonASAPOp, Operator, ScalarExpr};
use asap_types::stages::{ColumnLineage, Memo, MemoGroupId, ValueSign};
use asap_types::summary_type::{ExactKind, SummaryInputExpr};

/// $g_c$'s linear bound for errors in `metric`, each input's and the
/// output's: $\beta_{c,i}$ and $b_{c,i}$ per input, read from the
/// operator, its arguments and, for a `Filter`, its input group's column
/// lineage in `memo`. `None` when the consumer has no error contract for
/// `metric`: an edge from an input whose error is in `metric` then fails
/// (the metric rule). No factor is guessed; every contract keeps the metric
/// (concepts doc, section 4, "Local propagation"):
/// - value-preserving (`Sort`, `TimeRange`, `TimeShift`, `Concat`, a
///   readout, a finalize, a `SummaryMerge`): $\beta = 1$ in any metric;
/// - PromQL `and`, `unless` and `or`, which copy values and choose series
///   by labels only, and `label_replace` / `label_join` of labels
///   ([`set_operator_error_factors`], [`reads_an_estimate`]): $\beta = 1$
///   on each input whose values are copied, 0 on `and`'s and `unless`'s
///   right input, in any metric;
/// - a `Project` whose items reading an estimate ([`reads_an_estimate`])
///   each keep its error ([`keeps_error`]): $\beta = 1$, in any metric for
///   a copied column, in absolute error for `abs`, `clamp`, `clamp_min`,
///   `clamp_max` (also as the PromQL frontend's `promql_` functions) and a
///   negation, in relative error for `abs` and a negation
///   ([`negation_or_abs`]) and a product with or quotient by a nonzero
///   literal (`q * 2`, `q / 1000`);
/// - a `Filter` whose predicate reads only columns copied from a source
///   (lineage `SourceColumn`, labels and keys): $\beta = 1$ in any metric;
/// - an `Aggregate` (or exact accumulator build) of counts ($\beta = 0$),
///   min and max (absolute or relative, $\beta = 1$), avg (absolute,
///   $\beta = 1$) and sum and avg of values of one sign (relative,
///   $\beta = 1$, [`values_of_one_sign`]), without measure filters or
///   `HAVING`, whose group-by keys are copied from a source as a
///   `Filter`'s predicate columns are (a count by an estimate has no
///   bound);
/// - `+` and `-`: absolute, $\beta = 1$; `/`: relative,
///   $\beta = 1/(1-b)$ on both, the denominator capped at $b$; `*`:
///   relative, $(1+b, 1)$, the right operand capped at $b$
///   ($(1+x)(1+y)-1 \le (1+b)\,x + y$ for $y \le b$), with $b$ =
///   [`RELATIVE_OPERAND_VALIDITY_CAP`].
///
/// Every other operator (a filter comparing a computed value, a ranking,
/// a `sum` of values of unknown sign or in absolute error, a comparison,
/// another function) has none.
pub(crate) fn error_contract(
    memo: &Memo,
    consumer: &Operator<MemoGroupId>,
    metric: AccuracyMetric,
) -> Option<ErrorContract> {
    let inputs = consumer.children().len();
    let uniform = |factor: f64| ErrorContract {
        error_factor_per_input: vec![factor; inputs],
        linear_bound_holds_up_to_per_input: vec![None; inputs],
    };
    let absolute = matches!(metric, AccuracyMetric::Absolute { .. });
    let extremum_metric = absolute || metric == AccuracyMetric::Relative;
    match consumer {
        Operator::NonASAP(op) => match op {
            NonASAPOp::Scan { .. }
            | NonASAPOp::Values { .. }
            | NonASAPOp::Sort { .. }
            | NonASAPOp::TimeRange { .. }
            | NonASAPOp::TimeShift { .. }
            | NonASAPOp::Concat { .. } => Some(uniform(1.0)),
            NonASAPOp::Project { cols, child } => cols
                .iter()
                .filter(|item| reads_an_estimate(memo, *child, &item.expr))
                .all(|item| keeps_error(&item.expr, metric))
                .then(|| uniform(1.0)),
            // `label_replace(v, "x", "$1", "job", "(.*)")` writes label `x`
            // from label `job` and copies every value: a value's error is
            // unchanged in any metric, while the label it writes reads no
            // estimate.
            NonASAPOp::PromqlRelabel { value, child, .. } => {
                (!reads_an_estimate(memo, *child, value)).then(|| uniform(1.0))
            }
            NonASAPOp::BinaryOp {
                operator:
                    BinaryOperator {
                        kind: BinaryOpKind::Set(kind),
                        ..
                    },
                ..
            } => Some(ErrorContract {
                error_factor_per_input: set_operator_error_factors(kind).to_vec(),
                linear_bound_holds_up_to_per_input: vec![None; 2],
            }),
            NonASAPOp::Filter { pred, child } => (pred.0.operator_refs().is_empty()
                && copied_from_a_source(memo, *child, pred.0.columns_referenced()))
            .then(|| uniform(1.0)),
            NonASAPOp::Aggregate {
                measures,
                filters,
                having: None,
                reduction,
                child,
                ..
            } if filters.iter().all(Option::is_none)
                && !measures.is_empty()
                && keys_copied_from_a_source(memo, *child, reduction) =>
            {
                let child_schema = &memo.groups()[child.0 as usize].group_properties.schema;
                let mut factor: f64 = 0.0;
                for measure in measures {
                    match measure {
                        AggIntent::Count => {}
                        AggIntent::Min { .. } | AggIntent::Max { .. } if extremum_metric => {
                            factor = factor.max(1.0)
                        }
                        AggIntent::Avg { .. } if absolute => factor = factor.max(1.0),
                        AggIntent::Sum { col } | AggIntent::Avg { col }
                            if metric == AccuracyMetric::Relative
                                && col
                                    .or_else(|| {
                                        resolve_column_ref(&ColumnRef::SampleValue, child_schema)
                                            .ok()
                                    })
                                    .is_some_and(|column| {
                                        values_of_one_sign(memo, *child, column)
                                    }) =>
                        {
                            factor = factor.max(1.0)
                        }
                        _ => return None,
                    }
                }
                Some(uniform(factor))
            }
            NonASAPOp::BinaryOp {
                operator:
                    BinaryOperator {
                        kind: BinaryOpKind::Arithmetic(kind),
                        ..
                    },
                return_bool: false,
                ..
            } => {
                let b = RELATIVE_OPERAND_VALIDITY_CAP;
                match (kind, metric) {
                    (ArithmeticOpKind::Add | ArithmeticOpKind::Sub, _) if absolute => {
                        Some(uniform(1.0))
                    }
                    (ArithmeticOpKind::Div, AccuracyMetric::Relative) => Some(ErrorContract {
                        error_factor_per_input: vec![1.0 / (1.0 - b); 2],
                        linear_bound_holds_up_to_per_input: vec![None, Some(b)],
                    }),
                    (ArithmeticOpKind::Mul, AccuracyMetric::Relative) => Some(ErrorContract {
                        error_factor_per_input: vec![1.0 + b, 1.0],
                        linear_bound_holds_up_to_per_input: vec![None, Some(b)],
                    }),
                    _ => None,
                }
            }
            _ => None,
        },
        Operator::ASAP(op) => match op {
            ASAPOp::SummaryEstimate { .. }
            | ASAPOp::EvaluatePopulation { .. }
            | ASAPOp::FinalizeExactAccumulator { .. }
            | ASAPOp::SummaryMerge { .. } => Some(uniform(1.0)),
            ASAPOp::SummaryAgg {
                summary_type: FieldDataType::ExactAggregate(kind, _),
                input,
                filter: None,
                reduction,
                child,
            }
            | ASAPOp::SummaryTumblingWindowChain {
                summary_type: FieldDataType::ExactAggregate(kind, _),
                input,
                filter: None,
                reduction,
                child,
                ..
            } if keys_copied_from_a_source(memo, *child, reduction) => match kind {
                ExactKind::Count => Some(uniform(0.0)),
                ExactKind::Min | ExactKind::Max if extremum_metric => Some(uniform(1.0)),
                ExactKind::Sum if metric == AccuracyMetric::Relative => {
                    let SummaryInputExpr::Column(column) = &input.weight else {
                        return None;
                    };
                    let schema = &memo.groups()[child.0 as usize].group_properties.schema;
                    let column = resolve_column_ref(column, schema).ok()?;
                    values_of_one_sign(memo, *child, column).then(|| uniform(1.0))
                }
                _ => None,
            },
            _ => None,
        },
    }
}

/// Whether each of `columns` of `group`'s output is copied from a source
/// (lineage `SourceColumn`: a label or key), not computed: an estimate's
/// error can change which rows a predicate or a grouping on it selects.
fn copied_from_a_source(
    memo: &Memo,
    group: MemoGroupId,
    columns: impl IntoIterator<Item = usize>,
) -> bool {
    let lineage = &memo.groups()[group.0 as usize]
        .group_properties
        .column_lineage;
    columns
        .into_iter()
        .all(|column| matches!(lineage.get(column), Some(ColumnLineage::SourceColumn(_))))
}

/// Whether the values of column `column` of `group`'s output all have one
/// sign (`column_signs` `NonNegative` or
/// `NonPositive`, derived through the operators below `group`): then a
/// relative error ε on each value is at most ε on their sum and mean,
/// $|\sum_i (\hat{x}_i - x_i)| \le \varepsilon \sum_i |x_i| =
/// \varepsilon |\sum_i x_i|$. Values of unknown sign may cancel.
fn values_of_one_sign(memo: &Memo, group: MemoGroupId, column: usize) -> bool {
    matches!(
        memo.groups()[group.0 as usize]
            .group_properties
            .column_signs
            .get(column),
        Some(ValueSign::NonNegative | ValueSign::NonPositive)
    )
}

/// Whether every key `reduction` groups `child`'s rows by is copied from a
/// source ([`copied_from_a_source`]): `by` keys as listed; a `without`
/// grouping keys by every column of `child` but the time, the sample value
/// and the excluded ones; a per-entity reduction keeps each row's group.
fn keys_copied_from_a_source(memo: &Memo, child: MemoGroupId, reduction: &Reduction) -> bool {
    let Reduction::Reduce(by) = reduction else {
        return true;
    };
    if !by.is_without() {
        return copied_from_a_source(memo, child, by.keys().iter().copied());
    }
    let schema = &memo.groups()[child.0 as usize].group_properties.schema;
    let value = resolve_column_ref(&ColumnRef::SampleValue, schema).ok();
    copied_from_a_source(
        memo,
        child,
        (0..schema.fields.len()).filter(|&column| {
            schema.time_index != Some(column)
                && value != Some(column)
                && !by.keys().contains(&column)
        }),
    )
}

/// Whether `expr`, an item of a `Project` over `child`, reads a value that
/// may carry an estimate's error: a column of `child` that is neither
/// copied from a source (lineage `SourceColumn`: a label, a key, the series
/// identity) nor its time, or another operator's result. A PromQL function
/// of the value replaces only the value; the time and labels it copies
/// (and `promql_drop_metric_name` of the series identity) read no estimate.
fn reads_an_estimate(memo: &Memo, child: MemoGroupId, expr: &ScalarExpr<MemoGroupId>) -> bool {
    let properties = &memo.groups()[child.0 as usize].group_properties;
    !expr.operator_refs().is_empty()
        || expr.columns_referenced().into_iter().any(|column| {
            properties.schema.time_index != Some(column)
                && !matches!(
                    properties.column_lineage.get(column),
                    Some(ColumnLineage::SourceColumn(_))
                )
        })
}

/// Whether a `Project` item `expr` keeps the error in `metric` of the
/// column it reads, with $\beta = 1$: a copy of a column or a literal, or
/// the series identity without its metric name (`promql_drop_metric_name`,
/// which changes a label, not a value), in any metric; a 1-Lipschitz
/// function of a column ([`one_lipschitz`]) in absolute error; a column's
/// negation or `abs` ([`negation_or_abs`]), or a column times or divided
/// by a nonzero literal, in relative error.
fn keeps_error<C>(expr: &ScalarExpr<C>, metric: AccuracyMetric) -> bool {
    match expr {
        ScalarExpr::Column(_) | ScalarExpr::Literal(_) => true,
        ScalarExpr::FunctionCall { name, args } if name == "promql_drop_metric_name" => {
            matches!(args[..], [ScalarExpr::Column(_)])
        }
        _ if matches!(metric, AccuracyMetric::Absolute { .. }) => one_lipschitz(expr),
        _ if metric == AccuracyMetric::Relative => {
            negation_or_abs(expr) || scaled_by_a_constant(expr)
        }
        _ => false,
    }
}

/// Whether `expr` changes a column's value by at most its error: `abs`,
/// `clamp`, `clamp_min` or `clamp_max` (or the PromQL frontend's
/// `promql_abs`, …) of a column with literal bounds, or a column's
/// negation (1-Lipschitz).
fn one_lipschitz<C>(expr: &ScalarExpr<C>) -> bool {
    negation_or_abs(expr) || column_function_of_literals(expr, &["clamp", "clamp_min", "clamp_max"])
}

/// Whether `expr` is a column's negation or `abs` (`promql_abs`). Each
/// moves the value by at most the column's error and keeps its magnitude,
/// so it keeps the error both absolutely and as a share of the value:
/// $|(-\hat{x}) - (-x)| = |\hat{x} - x|$ and
/// $||\hat{x}| - |x|| \le |\hat{x} - x|$ (the reverse triangle
/// inequality), while $|-x| = ||x|| = |x|$. With x = 10 and x̂ = 10.5 (5%
/// off), −x̂ = −10.5 is 5% off −10; with x = −10 and x̂ = −10.5, |x̂| =
/// 10.5 is 5% off 10.
fn negation_or_abs<C>(expr: &ScalarExpr<C>) -> bool {
    match expr {
        ScalarExpr::Negative { expr, .. } => matches!(**expr, ScalarExpr::Column(_)),
        _ => column_function_of_literals(expr, &["abs"]),
    }
}

/// Whether `expr` calls one of `functions` (or the PromQL frontend's
/// `promql_` function of that name), any case, on a column and then
/// literals only: `promql_clamp(value, 0, 100)`.
fn column_function_of_literals<C>(expr: &ScalarExpr<C>, functions: &[&str]) -> bool {
    let ScalarExpr::FunctionCall { name, args } = expr else {
        return false;
    };
    let name = name.to_ascii_lowercase();
    functions.contains(&name.strip_prefix("promql_").unwrap_or(&name))
        && matches!(args.first(), Some(ScalarExpr::Column(_)))
        && args[1..]
            .iter()
            .all(|arg| matches!(arg, ScalarExpr::Literal(_)))
}

/// $\beta$ of a PromQL set operator's left and right inputs. Each output
/// series is an input series, value copied, and which ones are kept
/// depends only on which labels the other side has: `lhs and rhs` and
/// `lhs unless rhs` keep left series, so the output's error is the left's
/// and the right's values are never read, $(1, 0)$; `lhs or rhs` copies
/// each value from one of them, $(1, 1)$. With q̂ = 10.5 for q = 10 on the
/// left, `q unless r` outputs 10.5 or nothing, whatever `r`'s values.
fn set_operator_error_factors(kind: &PromQLVectorSetOpKind) -> [f64; 2] {
    match kind {
        PromQLVectorSetOpKind::And | PromQLVectorSetOpKind::Unless => [1.0, 0.0],
        PromQLVectorSetOpKind::Or => [1.0, 1.0],
    }
}

/// Whether `expr` is a column times a nonzero literal (either side) or
/// divided by one: $c\hat{x}$ is off from $cx$ by the same share as
/// $\hat{x}$ from $x$ (a unit conversion, `q * 1000`).
fn scaled_by_a_constant<C>(expr: &ScalarExpr<C>) -> bool {
    let nonzero = |expr: &ScalarExpr<C>| match expr {
        ScalarExpr::Literal(ScalarValue::Float64(x)) => x.is_finite() && *x != 0.0,
        ScalarExpr::Literal(ScalarValue::Int64(x)) => *x != 0,
        _ => false,
    };
    let column = |expr: &ScalarExpr<C>| matches!(expr, ScalarExpr::Column(_));
    match expr {
        ScalarExpr::Arithmetic {
            op: ArithmeticOpKind::Mul,
            left,
            right,
            ..
        } => (column(left) && nonzero(right)) || (nonzero(left) && column(right)),
        ScalarExpr::Arithmetic {
            op: ArithmeticOpKind::Div,
            left,
            right,
            ..
        } => column(left) && nonzero(right),
        _ => false,
    }
}

/// $b$ of a ratio's denominator and a product's right operand: the
/// relative error under which [`error_contract`]'s linear bound holds,
/// $\varepsilon_{X/Y} \le (\varepsilon_X + \varepsilon_Y) / (1 - b)$
/// (concepts doc, section 4). 0.1 is the weakest error of every
/// algorithm's list, so it never excludes a listed θ.
const RELATIVE_OPERAND_VALIDITY_CAP: f64 = 0.1;

/// The error contract of one consumer in one metric: its output error is
/// at most $\sum_i \beta_{c,i}\varepsilon_{c,i}$ while each input's error
/// stays within $b_{c,i}$ (concepts doc, section 4).
#[derive(Debug, Clone)]
#[cfg_attr(test, derive(PartialEq))]
pub struct ErrorContract {
    /// $\beta_{c,i}$ per input: how much of that input's error, in the
    /// contract's metric, reaches the output.
    pub error_factor_per_input: Vec<f64>,
    /// $b_{c,i}$ per input: the largest error of that input up to which
    /// the linear bound holds; `None` when it holds for any error (a ratio
    /// bounds only its denominator's).
    pub linear_bound_holds_up_to_per_input: Vec<Option<f64>>,
}

#[cfg(test)]
mod tests;

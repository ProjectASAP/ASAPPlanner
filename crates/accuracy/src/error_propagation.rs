//! Each operator's local error propagation: the linear bound $g_c$ on a
//! consumer's output error from its inputs' errors (concepts doc, section 4).

use asap_types::accuracy::AccuracyMetric;
use asap_types::ir::operator::arguments::BinaryOpKind;
use asap_types::ir::operator::{AggIntent, Reduction};
use asap_types::ir::scalar::expr_ir::ArithmeticOpKind;
use asap_types::ir::scalar::{resolve_column_ref, ColumnRef, ScalarValue};
use asap_types::ir::schema::FieldDataType;
use asap_types::ir::{ASAPOp, BinaryOperator, NonASAPOp, Operator, ScalarExpr};
use asap_types::stages::{ColumnLineage, Memo, MemoGroupId};
use asap_types::summary_type::ExactKind;

/// $g_c$'s linear bound for errors in `metric`, each input's and the
/// output's: $\beta_{c,i}$ and $b_{c,i}$ per input, read from the
/// operator, its arguments and, for a `Filter`, its input group's column
/// lineage in `memo`. `None` when the consumer has no error contract for
/// `metric`: an input whose error is in `metric` then gives no edge (the
/// metric rule). No factor is guessed; every contract keeps the metric
/// (concepts doc, section 4, "Local propagation"):
/// - value-preserving (`Sort`, `TimeRange`, `TimeShift`, `Concat`, a
///   readout, a finalize, a `SummaryMerge`): $\beta = 1$ in any metric;
/// - a `Project` whose items reading an estimate ([`reads_an_estimate`])
///   each keep its error ([`keeps_error`]): $\beta = 1$, in any metric for
///   a copied column, in absolute error for `abs`, `clamp`, `clamp_min`,
///   `clamp_max` (also as the PromQL frontend's `promql_` functions) and a
///   negation, in relative error for a product with or quotient by a
///   nonzero literal (`q * 2`, `q / 1000`);
/// - a `Filter` whose predicate reads only columns copied from a source
///   (lineage `SourceColumn`, labels and keys): $\beta = 1$ in any metric;
/// - an `Aggregate` (or exact accumulator build) of counts ($\beta = 0$),
///   min and max (absolute or relative, $\beta = 1$) and avg (absolute,
///   $\beta = 1$), without measure filters or `HAVING`, whose group-by
///   keys are copied from a source as a `Filter`'s predicate columns are
///   (a count by an estimate has no bound);
/// - `+` and `-`: absolute, $\beta = 1$; `/`: relative,
///   $\beta = 1/(1-b)$ on both, the denominator capped at $b$; `*`:
///   relative, $(1+b, 1)$, the right operand capped at $b$
///   ($(1+x)(1+y)-1 \le (1+b)\,x + y$ for $y \le b$), with $b$ =
///   [`RELATIVE_OPERAND_VALIDITY_CAP`].
///
/// Every other operator (a filter comparing a computed value, a ranking,
/// a `sum`, a comparison, another function) has none.
pub fn error_propagation(
    memo: &Memo,
    consumer: &Operator<MemoGroupId>,
    metric: AccuracyMetric,
) -> Option<ErrorPropagation> {
    let inputs = consumer.children().len();
    let uniform = |factor: f64| ErrorPropagation {
        factor_per_input: vec![factor; inputs],
        validity_cap_per_input: vec![None; inputs],
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
                let mut factor: f64 = 0.0;
                for measure in measures {
                    match measure {
                        AggIntent::Count => {}
                        AggIntent::Min { .. } | AggIntent::Max { .. } if extremum_metric => {
                            factor = factor.max(1.0)
                        }
                        AggIntent::Avg { .. } if absolute => factor = factor.max(1.0),
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
                    (ArithmeticOpKind::Div, AccuracyMetric::Relative) => Some(ErrorPropagation {
                        factor_per_input: vec![1.0 / (1.0 - b); 2],
                        validity_cap_per_input: vec![None, Some(b)],
                    }),
                    (ArithmeticOpKind::Mul, AccuracyMetric::Relative) => Some(ErrorPropagation {
                        factor_per_input: vec![1.0 + b, 1.0],
                        validity_cap_per_input: vec![None, Some(b)],
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
                filter: None,
                reduction,
                child,
                ..
            }
            | ASAPOp::SummaryTumblingWindowChain {
                summary_type: FieldDataType::ExactAggregate(kind, _),
                filter: None,
                reduction,
                child,
                ..
            } if keys_copied_from_a_source(memo, *child, reduction) => match kind {
                ExactKind::Count => Some(uniform(0.0)),
                ExactKind::Min | ExactKind::Max if extremum_metric => Some(uniform(1.0)),
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
/// function of a column ([`one_lipschitz`]) in absolute error; a column
/// times or divided by a nonzero literal in relative error.
fn keeps_error<C>(expr: &ScalarExpr<C>, metric: AccuracyMetric) -> bool {
    match expr {
        ScalarExpr::Column(_) | ScalarExpr::Literal(_) => true,
        ScalarExpr::FunctionCall { name, args } if name == "promql_drop_metric_name" => {
            matches!(args[..], [ScalarExpr::Column(_)])
        }
        _ if matches!(metric, AccuracyMetric::Absolute { .. }) => one_lipschitz(expr),
        _ if metric == AccuracyMetric::Relative => scaled_by_a_constant(expr),
        _ => false,
    }
}

/// Whether `expr` changes a column's value by at most its error: `abs`,
/// `clamp`, `clamp_min` or `clamp_max` (or the PromQL frontend's
/// `promql_abs`, …) of a column with literal bounds, or a column's
/// negation (1-Lipschitz).
fn one_lipschitz<C>(expr: &ScalarExpr<C>) -> bool {
    match expr {
        ScalarExpr::Negative { expr, .. } => matches!(**expr, ScalarExpr::Column(_)),
        ScalarExpr::FunctionCall { name, args } => {
            let name = name.to_ascii_lowercase();
            matches!(
                name.strip_prefix("promql_").unwrap_or(&name),
                "abs" | "clamp" | "clamp_min" | "clamp_max"
            ) && matches!(args.first(), Some(ScalarExpr::Column(_)))
                && args[1..]
                    .iter()
                    .all(|arg| matches!(arg, ScalarExpr::Literal(_)))
        }
        _ => false,
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
/// relative error under which [`error_propagation`]'s linear bound holds,
/// $\varepsilon_{X/Y} \le (\varepsilon_X + \varepsilon_Y) / (1 - b)$
/// (concepts doc, section 4). 0.1 is the weakest error of every
/// algorithm's list, so it never excludes a listed θ.
const RELATIVE_OPERAND_VALIDITY_CAP: f64 = 0.1;

/// The local error propagation of one consumer: its output error is at
/// most $\sum_i \beta_{c,i}\varepsilon_{c,i}$ while each input's error stays
/// within its validity cap (concepts doc, section 4; approved 2026-11-05).
#[derive(Debug)]
#[cfg_attr(test, derive(PartialEq))]
pub struct ErrorPropagation {
    /// $\beta_{c,i}$ per input, for that input's metric.
    pub factor_per_input: Vec<f64>,
    /// $b_{c,i}$ per input: the cap under which the linear bound holds;
    /// `None` when the bound holds everywhere (e.g. a ratio caps only its
    /// denominator).
    pub validity_cap_per_input: Vec<Option<f64>>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use asap_types::ir::operator::arguments::Reduction;
    use asap_types::ir::scalar::CompareOpKind;
    use asap_types::ir::schema::{DataType, PROMQL_SERIES_IDENTITY};
    use asap_types::ir::{ExprSemantics, Predicate, ProjectItem};
    use asap_types::summary_type::{SketchAlgorithm, SketchStatistic, SummaryUpdate};
    use asap_types::test_support::memo_fixtures::{aggregate, readout, scan};
    use asap_types::workload::DataWorkload;

    /// `column <op> literal`, PromQL semantics.
    fn compare(column: usize, op: CompareOpKind, literal: ScalarValue) -> ScalarExpr<MemoGroupId> {
        ScalarExpr::Compare {
            left: Box::new(ScalarExpr::Column(column)),
            op,
            right: Box::new(ScalarExpr::Literal(literal)),
            semantics: ExprSemantics::Promql,
        }
    }

    /// Constant error contracts. A `Filter` whose
    /// predicate reads only columns copied from a source keeps or drops
    /// whole rows: β = 1 in every metric. Over a readout of a KLL state of
    /// `latency` by `job`, `job = "api"` reads such a column; `value > 0.5`
    /// compares the computed estimate and has no contract. A `Project` of
    /// `abs`, `clamp` or a negation of the value changes it by at most its
    /// error: β = 1 for absolute errors only; one scaling it by a nonzero
    /// constant keeps its relative error: β = 1 for relative errors. Only
    /// the items reading the estimate are judged, so the series identity
    /// `promql_drop_metric_name` computes does not count.
    #[test]
    fn key_filters_and_value_preserving_projects_have_unit_contracts() {
        let mut memo = Memo::new(&DataWorkload::default());
        let g1 = memo
            .find_or_add_group(scan("latency", &[("job", DataType::Utf8)], vec![]))
            .unwrap();
        let state = memo
            .find_or_add_group(Operator::ASAP(ASAPOp::SummaryAgg {
                child: g1,
                summary_type: FieldDataType::Sketch(SketchAlgorithm::Kll),
                input: SummaryUpdate::column(asap_types::ir::scalar::ColumnRef::SampleValue),
                reduction: Reduction::by(vec![2]),
                filter: None,
            }))
            .unwrap();
        let readout = memo
            .find_or_add_group(readout(SketchStatistic::Quantile { q: 0.9 }, state))
            .unwrap();
        let lineage = &memo.groups()[readout.0 as usize]
            .group_properties
            .column_lineage;
        let column_where = |copied: bool| {
            lineage
                .iter()
                .position(|l| matches!(l, ColumnLineage::SourceColumn(_)) == copied)
                .expect("the readout has a label and an estimate")
        };
        let (job, estimate) = (column_where(true), column_where(false));
        let filter = |pred| {
            Operator::NonASAP(NonASAPOp::Filter {
                pred: Predicate(pred),
                child: readout,
            })
        };
        let by_job = filter(compare(
            job,
            CompareOpKind::Eq,
            ScalarValue::Utf8("api".into()),
        ));
        let by_value = filter(compare(
            estimate,
            CompareOpKind::Gt,
            ScalarValue::Float64(0.5),
        ));
        for metric in AccuracyMetric::ALL {
            assert_eq!(
                error_propagation(&memo, &by_job, metric)
                    .map(|propagation| propagation.factor_per_input),
                Some(vec![1.0]),
                "key-only filter, {metric:?}"
            );
            assert_eq!(
                error_propagation(&memo, &by_value, metric),
                None,
                "a filter comparing the estimate, {metric:?}"
            );
        }
        // A `Project` over the readout, as the PromQL frontend lowers a
        // function of the value: the time and every label copied, the series
        // identity through `promql_drop_metric_name`, the estimate replaced.
        let value = || Box::new(ScalarExpr::Column(estimate));
        let literal = |x: f64| ScalarExpr::Literal(ScalarValue::Float64(x));
        let project_value = |expr: ScalarExpr<MemoGroupId>| {
            let fields = &memo.groups()[readout.0 as usize]
                .group_properties
                .schema
                .fields;
            Operator::NonASAP(NonASAPOp::Project {
                cols: fields
                    .iter()
                    .enumerate()
                    .map(|(column, field)| ProjectItem {
                        alias: Some(field.name.clone()),
                        expr: if column == estimate {
                            expr.clone()
                        } else if field.name == PROMQL_SERIES_IDENTITY {
                            ScalarExpr::FunctionCall {
                                name: "promql_drop_metric_name".into(),
                                args: vec![ScalarExpr::Column(column)],
                            }
                        } else {
                            ScalarExpr::Column(column)
                        },
                    })
                    .collect(),
                child: readout,
            })
        };
        let factor = |project: &Operator<MemoGroupId>, metric| {
            error_propagation(&memo, project, metric)
                .map(|propagation| propagation.factor_per_input)
        };
        let one_lipschitz = [
            ScalarExpr::FunctionCall {
                name: "promql_abs".into(),
                args: vec![*value()],
            },
            ScalarExpr::FunctionCall {
                name: "promql_clamp".into(),
                args: vec![*value(), literal(0.0), literal(100.0)],
            },
            ScalarExpr::FunctionCall {
                name: "promql_clamp_max".into(),
                args: vec![*value(), literal(5.0)],
            },
            ScalarExpr::Negative {
                expr: value(),
                semantics: ExprSemantics::Promql,
            },
        ];
        for expr in one_lipschitz {
            let project = project_value(expr.clone());
            assert_eq!(
                factor(&project, AccuracyMetric::ABSOLUTE),
                Some(vec![1.0]),
                "{expr:?}"
            );
            assert_eq!(factor(&project, AccuracyMetric::Relative), None, "{expr:?}");
        }
        // Scaling by a nonzero constant keeps a relative error: `q / 2`,
        // `q * 2`, `2 * q`; scaling by 0 has no contract.
        let scaled = |op, left, right| ScalarExpr::Arithmetic {
            op,
            left: Box::new(left),
            right: Box::new(right),
            semantics: ExprSemantics::Promql,
        };
        for expr in [
            scaled(ArithmeticOpKind::Div, *value(), literal(2.0)),
            scaled(ArithmeticOpKind::Mul, *value(), literal(2.0)),
            scaled(ArithmeticOpKind::Mul, literal(2.0), *value()),
        ] {
            let project = project_value(expr.clone());
            assert_eq!(
                factor(&project, AccuracyMetric::Relative),
                Some(vec![1.0]),
                "{expr:?}"
            );
            assert_eq!(factor(&project, AccuracyMetric::Rank), None, "{expr:?}");
        }
        let times_zero = project_value(scaled(ArithmeticOpKind::Mul, *value(), literal(0.0)));
        assert_eq!(factor(&times_zero, AccuracyMetric::Relative), None);
        // Copying the value (and dropping the metric name from the series
        // identity) keeps it in every metric.
        let copied = project_value(*value());
        for metric in AccuracyMetric::ALL {
            assert_eq!(factor(&copied, metric), Some(vec![1.0]), "{metric:?}");
        }
    }

    /// A count (and min, max, avg) grouped by a column that is itself an
    /// estimate has no contract: an estimate's error moves rows between
    /// groups, so a count by it has no bound, as for a filter comparing it.
    /// Over a readout of a KLL state of `latency` by `job`, `count by
    /// (estimate)` and an exact count accumulator by it have none, while
    /// `count by (job)` reads no value (factor 0).
    #[test]
    fn a_reduction_keyed_by_an_estimate_has_no_contract() {
        let mut memo = Memo::new(&DataWorkload::default());
        let g1 = memo
            .find_or_add_group(scan("latency", &[("job", DataType::Utf8)], vec![]))
            .unwrap();
        let state = memo
            .find_or_add_group(Operator::ASAP(ASAPOp::SummaryAgg {
                child: g1,
                summary_type: FieldDataType::Sketch(SketchAlgorithm::Kll),
                input: SummaryUpdate::column(asap_types::ir::scalar::ColumnRef::SampleValue),
                reduction: Reduction::by(vec![2]),
                filter: None,
            }))
            .unwrap();
        let readout = memo
            .find_or_add_group(readout(SketchStatistic::Quantile { q: 0.99 }, state))
            .unwrap();
        let lineage = &memo.groups()[readout.0 as usize]
            .group_properties
            .column_lineage;
        let column_where = |copied: bool| {
            lineage
                .iter()
                .position(|l| matches!(l, ColumnLineage::SourceColumn(_)) == copied)
                .expect("the readout has a label and an estimate")
        };
        let (job, estimate) = (column_where(true), column_where(false));
        let exact_count_by = |key: usize| {
            Operator::ASAP(ASAPOp::SummaryAgg {
                child: readout,
                summary_type: FieldDataType::exact(ExactKind::Count),
                input: SummaryUpdate::column(asap_types::ir::scalar::ColumnRef::SampleValue),
                reduction: Reduction::by(vec![key]),
                filter: None,
            })
        };
        for metric in AccuracyMetric::ALL {
            for measure in [
                AggIntent::Count,
                AggIntent::Min { col: None },
                AggIntent::Max { col: None },
                AggIntent::Avg { col: None },
            ] {
                assert_eq!(
                    error_propagation(
                        &memo,
                        &aggregate(Reduction::by(vec![estimate]), measure.clone(), readout),
                        metric
                    ),
                    None,
                    "{measure:?} by the estimate, {metric:?}"
                );
            }
            assert_eq!(
                error_propagation(&memo, &exact_count_by(estimate), metric),
                None,
                "an exact count by the estimate, {metric:?}"
            );
            for count_by_job in [
                aggregate(Reduction::by(vec![job]), AggIntent::Count, readout),
                exact_count_by(job),
            ] {
                assert_eq!(
                    error_propagation(&memo, &count_by_job, metric)
                        .map(|propagation| propagation.factor_per_input),
                    Some(vec![0.0]),
                    "a count by job, {metric:?}"
                );
            }
        }
    }

    /// A ratio carries relative error with factor 1/(1 − 0.1) on both
    /// inputs and caps its denominator at 0.1; a count reads no value
    /// (factor 0); a sum has no contract.
    #[test]
    fn error_contracts_of_a_ratio_a_count_and_a_sum() {
        let mut memo = Memo::new(&DataWorkload::default());
        let g1 = memo
            .find_or_add_group(scan("latency", &[("job", DataType::Utf8)], vec![]))
            .unwrap();
        let divide = Operator::NonASAP(NonASAPOp::BinaryOp {
            operator: BinaryOperator {
                checked_relative_division: false,
                checked_finite_division: false,
                kind: BinaryOpKind::Arithmetic(ArithmeticOpKind::Div),
                vector_match: None,
            },
            return_bool: false,
            lhs: g1,
            rhs: g1,
        });
        assert_eq!(
            error_propagation(&memo, &divide, AccuracyMetric::Relative),
            Some(ErrorPropagation {
                factor_per_input: vec![1.0 / 0.9; 2],
                validity_cap_per_input: vec![None, Some(0.1)],
            })
        );
        for metric in [AccuracyMetric::ABSOLUTE, AccuracyMetric::Rank] {
            assert_eq!(error_propagation(&memo, &divide, metric), None);
        }
        let by_job = || Reduction::by(vec![2]);
        assert_eq!(
            error_propagation(
                &memo,
                &aggregate(by_job(), AggIntent::Count, g1),
                AccuracyMetric::Rank
            )
            .map(|propagation| propagation.factor_per_input),
            Some(vec![0.0])
        );
        assert_eq!(
            error_propagation(
                &memo,
                &aggregate(by_job(), AggIntent::Sum { col: Some(1) }, g1),
                AccuracyMetric::ABSOLUTE
            ),
            None
        );
    }
}

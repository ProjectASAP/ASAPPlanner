//! Each operator's local error propagation: the linear bound $g_c$ on a
//! consumer's output error from its inputs' errors (concepts doc, section 4).

use asap_types::accuracy::AccuracyMetric;
use asap_types::ir::operator::arguments::BinaryOpKind;
use asap_types::ir::operator::AggIntent;
use asap_types::ir::scalar::expr_ir::ArithmeticOpKind;
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
/// - value-preserving (a `Project` of columns and literals, `Sort`,
///   `TimeRange`, `TimeShift`, `Concat`, a readout, a finalize, a
///   `SummaryMerge`): $\beta = 1$ in any metric;
/// - a `Filter` whose predicate reads only columns copied from a source
///   (lineage `SourceColumn`, labels and keys): $\beta = 1$ in any metric;
/// - a `Project` whose computed items are `abs`, `clamp`, `clamp_min`,
///   `clamp_max` or a negation of a column: absolute, $\beta = 1$;
/// - an `Aggregate` (or exact accumulator build) of counts ($\beta = 0$),
///   min and max (absolute or relative, $\beta = 1$) and avg (absolute,
///   $\beta = 1$), without measure filters or `HAVING`;
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
            NonASAPOp::Project { cols, .. } => {
                let preserves = |expr: &ScalarExpr<MemoGroupId>| {
                    matches!(expr, ScalarExpr::Column(_) | ScalarExpr::Literal(_))
                };
                // A computed item that changes a value by at most its
                // error keeps an absolute bound only.
                cols.iter()
                    .all(|item| preserves(&item.expr) || (absolute && one_lipschitz(&item.expr)))
                    .then(|| uniform(1.0))
            }
            NonASAPOp::Filter { pred, child } => {
                let lineage = &memo.groups()[child.0 as usize]
                    .group_properties
                    .column_lineage;
                (pred.0.operator_refs().is_empty()
                    && pred.0.columns_referenced().iter().all(|&column| {
                        matches!(lineage.get(column), Some(ColumnLineage::SourceColumn(_)))
                    }))
                .then(|| uniform(1.0))
            }
            NonASAPOp::Aggregate {
                measures,
                filters,
                having: None,
                ..
            } if filters.iter().all(Option::is_none) && !measures.is_empty() => {
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
                ..
            }
            | ASAPOp::SummaryTumblingWindowChain {
                summary_type: FieldDataType::ExactAggregate(kind, _),
                filter: None,
                ..
            } => match kind {
                ExactKind::Count => Some(uniform(0.0)),
                ExactKind::Min | ExactKind::Max if extremum_metric => Some(uniform(1.0)),
                _ => None,
            },
            _ => None,
        },
    }
}

/// Whether `expr` changes a column's value by at most its error: `abs`,
/// `clamp`, `clamp_min` or `clamp_max` of a column with literal bounds, or
/// a column's negation (1-Lipschitz).
fn one_lipschitz<C>(expr: &ScalarExpr<C>) -> bool {
    match expr {
        ScalarExpr::Negative { expr, .. } => matches!(**expr, ScalarExpr::Column(_)),
        ScalarExpr::FunctionCall { name, args } => {
            matches!(
                name.to_ascii_lowercase().as_str(),
                "abs" | "clamp" | "clamp_min" | "clamp_max"
            ) && matches!(args.first(), Some(ScalarExpr::Column(_)))
                && args[1..]
                    .iter()
                    .all(|arg| matches!(arg, ScalarExpr::Literal(_)))
        }
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
#[derive(Debug, Clone, PartialEq)]
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
    use asap_types::ir::scalar::{CompareOpKind, ScalarValue};
    use asap_types::ir::schema::DataType;
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

    /// Decided 2026-11-07 (E1): constant error contracts. A `Filter` whose
    /// predicate reads only columns copied from a source keeps or drops
    /// whole rows: β = 1 in every metric. Over a readout of a KLL state of
    /// `latency` by `job`, `job = "api"` reads such a column; `value > 0.5`
    /// compares the computed estimate and has no contract. A `Project` of
    /// `abs`, `clamp` or a negation of the value changes it by at most its
    /// error: β = 1 for absolute errors only.
    #[test]
    fn e1_key_filters_and_value_preserving_projects_have_unit_contracts() {
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
        let value = || Box::new(ScalarExpr::Column(1));
        let literal = |x: f64| ScalarExpr::Literal(ScalarValue::Float64(x));
        let computed = [
            ScalarExpr::FunctionCall {
                name: "abs".into(),
                args: vec![*value()],
            },
            ScalarExpr::FunctionCall {
                name: "clamp".into(),
                args: vec![*value(), literal(0.0), literal(100.0)],
            },
            ScalarExpr::Negative {
                expr: value(),
                semantics: ExprSemantics::Promql,
            },
        ];
        for expr in computed {
            let project = Operator::NonASAP(NonASAPOp::Project {
                cols: vec![
                    ProjectItem {
                        alias: None,
                        expr: ScalarExpr::Column(0),
                    },
                    ProjectItem {
                        alias: Some("value".into()),
                        expr: expr.clone(),
                    },
                    ProjectItem {
                        alias: None,
                        expr: ScalarExpr::Column(2),
                    },
                ],
                child: g1,
            });
            assert_eq!(
                error_propagation(&memo, &project, AccuracyMetric::ABSOLUTE)
                    .map(|propagation| propagation.factor_per_input),
                Some(vec![1.0]),
                "{expr:?}"
            );
            assert_eq!(
                error_propagation(&memo, &project, AccuracyMetric::Relative),
                None,
                "{expr:?}"
            );
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

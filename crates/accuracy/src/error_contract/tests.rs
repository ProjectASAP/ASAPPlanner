use super::*;
use asap_types::ir::operator::arguments::{PromQLVectorSetOpKind, Reduction};
use asap_types::ir::scalar::CompareOpKind;
use asap_types::ir::schema::{DataType, PROMQL_SERIES_IDENTITY};
use asap_types::ir::{ExprSemantics, Predicate, ProjectItem};
use asap_types::stages::ValueSign;
use asap_types::summary_type::{SketchAlgorithm, SketchStatistic, SummaryUpdate};
use asap_types::test_support::memo_fixtures::{aggregate, arithmetic, compare, readout, scan};
use asap_types::workload::{DataWorkload, SourceFacts};
use std::collections::BTreeMap;

/// Constant error contracts. A `Filter` whose
/// predicate reads only columns copied from a source keeps or drops
/// whole rows: β = 1 in every metric. Over a readout of a KLL state of
/// `latency` by `job`, `job = "api"` reads such a column; `value > 0.5`
/// compares the computed estimate and has no contract. A `Project` of
/// `abs`, `clamp` or a negation of the value changes it by at most its
/// error: β = 1 for absolute errors. `abs` and a negation also keep its
/// magnitude, so they keep its relative error too (β = 1): q = 10,
/// q̂ = 10.5 gives −q̂ = −10.5, 5% off −10. A clamp does not: it is not
/// judged in relative error. One scaling it by a nonzero constant keeps
/// its relative error: β = 1 for relative errors. Only
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
        ExprSemantics::Promql,
    ));
    let by_value = filter(compare(
        estimate,
        CompareOpKind::Gt,
        ScalarValue::Float64(0.5),
        ExprSemantics::Promql,
    ));
    for metric in AccuracyMetric::ALL {
        assert_eq!(
            error_contract(&memo, &by_job, metric).map(|contract| contract.error_factor_per_input),
            Some(vec![1.0]),
            "key-only filter, {metric:?}"
        );
        assert_eq!(
            error_contract(&memo, &by_value, metric),
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
        error_contract(&memo, project, metric).map(|contract| contract.error_factor_per_input)
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
        let keeps_magnitude = match &expr {
            ScalarExpr::Negative { .. } => true,
            ScalarExpr::FunctionCall { name, .. } => name == "promql_abs",
            _ => false,
        };
        assert_eq!(
            factor(&project, AccuracyMetric::Relative),
            keeps_magnitude.then(|| vec![1.0]),
            "{expr:?}"
        );
        assert_eq!(factor(&project, AccuracyMetric::Rank), None, "{expr:?}");
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
                error_contract(
                    &memo,
                    &aggregate(Reduction::by(vec![estimate]), measure.clone(), readout),
                    metric
                ),
                None,
                "{measure:?} by the estimate, {metric:?}"
            );
        }
        assert_eq!(
            error_contract(&memo, &exact_count_by(estimate), metric),
            None,
            "an exact count by the estimate, {metric:?}"
        );
        for count_by_job in [
            aggregate(Reduction::by(vec![job]), AggIntent::Count, readout),
            exact_count_by(job),
        ] {
            assert_eq!(
                error_contract(&memo, &count_by_job, metric)
                    .map(|contract| contract.error_factor_per_input),
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
    let divide = arithmetic(ArithmeticOpKind::Div, g1, g1);
    assert_eq!(
        error_contract(&memo, &divide, AccuracyMetric::Relative),
        Some(ErrorContract {
            error_factor_per_input: vec![1.0 / 0.9; 2],
            linear_bound_holds_up_to_per_input: vec![None, Some(0.1)],
        })
    );
    for metric in [AccuracyMetric::ABSOLUTE, AccuracyMetric::Rank] {
        assert_eq!(error_contract(&memo, &divide, metric), None);
    }
    let by_job = || Reduction::by(vec![2]);
    assert_eq!(
        error_contract(
            &memo,
            &aggregate(by_job(), AggIntent::Count, g1),
            AccuracyMetric::Rank
        )
        .map(|contract| contract.error_factor_per_input),
        Some(vec![0.0])
    );
    assert_eq!(
        error_contract(
            &memo,
            &aggregate(by_job(), AggIntent::Sum { col: Some(1) }, g1),
            AccuracyMetric::ABSOLUTE
        ),
        None
    );
}

/// Over values of one sign, a relative error ε on each input is at most
/// ε on their sum and on their mean: $|\sum_i (\hat{x}_i - x_i)| \le
/// \varepsilon \sum_i |x_i| = \varepsilon |\sum_i x_i|$. A `sum` and an
/// `avg` by `job` over a DDSketch 0.99-quantile readout of `latency`,
/// and an exact sum accumulator over it, carry relative error with
/// factor 1 when `latency` is declared non-negative or non-positive (the
/// quantile keeps its sign); undeclared, its values may have both signs
/// and they have none. A `sum` has no absolute contract either way (its
/// errors add up), an `avg` has one with factor 1.
#[test]
fn sums_and_averages_carry_relative_error_over_values_of_one_sign() {
    for sign in [
        ValueSign::NonNegative,
        ValueSign::NonPositive,
        ValueSign::Unknown,
    ] {
        let data = DataWorkload {
            sources: BTreeMap::from([(
                "latency".to_string(),
                SourceFacts {
                    column_signs: BTreeMap::from([("value".to_string(), sign)]),
                    ..SourceFacts::default()
                },
            )]),
            ..DataWorkload::default()
        };
        let mut memo = Memo::new(&data);
        let g1 = memo
            .find_or_add_group(scan("latency", &[("job", DataType::Utf8)], vec![]))
            .unwrap();
        let state = memo
            .find_or_add_group(Operator::ASAP(ASAPOp::SummaryAgg {
                child: g1,
                summary_type: FieldDataType::Sketch(SketchAlgorithm::DDSketch),
                input: SummaryUpdate::column(ColumnRef::SampleValue),
                reduction: Reduction::by(vec![2]),
                filter: None,
            }))
            .unwrap();
        let readout = memo
            .find_or_add_group(readout(SketchStatistic::Quantile { q: 0.99 }, state))
            .unwrap();
        let job = memo.groups()[readout.0 as usize]
            .group_properties
            .column_lineage
            .iter()
            .position(|l| matches!(l, ColumnLineage::SourceColumn(_)))
            .expect("the readout keeps job");
        let by_job = || Reduction::by(vec![job]);
        let sum = aggregate(by_job(), AggIntent::Sum { col: None }, readout);
        let avg = aggregate(by_job(), AggIntent::Avg { col: None }, readout);
        let exact_sum = Operator::ASAP(ASAPOp::SummaryAgg {
            child: readout,
            summary_type: FieldDataType::exact(ExactKind::Sum),
            input: SummaryUpdate::column(ColumnRef::SampleValue),
            reduction: by_job(),
            filter: None,
        });
        let factors = |consumer: &Operator<MemoGroupId>, metric| {
            error_contract(&memo, consumer, metric).map(|contract| contract.error_factor_per_input)
        };
        let relative = (sign != ValueSign::Unknown).then(|| vec![1.0]);
        for consumer in [&sum, &avg, &exact_sum] {
            assert_eq!(
                factors(consumer, AccuracyMetric::Relative),
                relative,
                "{sign:?}: {consumer:?}"
            );
            assert_eq!(factors(consumer, AccuracyMetric::Rank), None, "{sign:?}");
        }
        assert_eq!(factors(&sum, AccuracyMetric::ABSOLUTE), None, "{sign:?}");
        assert_eq!(
            factors(&exact_sum, AccuracyMetric::ABSOLUTE),
            None,
            "{sign:?}"
        );
        assert_eq!(
            factors(&avg, AccuracyMetric::ABSOLUTE),
            Some(vec![1.0]),
            "{sign:?}"
        );
    }
}

/// PromQL operators that copy values and only choose which series to
/// keep, or rewrite a label, keep their input's error in every metric:
/// - `lhs and rhs` / `lhs unless rhs` keep or drop each left series by
///   whether a right series has its labels, and copy the kept value: the
///   output's error is the left's (β = 1), and the right's values are not
///   read (β = 0). With q̂ = 10.5 for q = 10 on the left, the output is
///   10.5 or absent, whatever the right's values;
/// - `lhs or rhs` copies each value from one side: β = 1 on both;
/// - `label_replace(v, "x", "$1", "job", "(.*)")` writes label `x` from
///   label `job` and copies the value: β = 1. A rewrite reading the
///   estimate column itself has no contract.
#[test]
fn set_operators_and_relabels_copy_their_inputs_error() {
    let mut memo = Memo::new(&DataWorkload::default());
    let g1 = memo
        .find_or_add_group(scan("latency", &[("job", DataType::Utf8)], vec![]))
        .unwrap();
    let state = memo
        .find_or_add_group(Operator::ASAP(ASAPOp::SummaryAgg {
            child: g1,
            summary_type: FieldDataType::Sketch(SketchAlgorithm::DDSketch),
            input: SummaryUpdate::column(ColumnRef::SampleValue),
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
    let set_op = |kind| {
        Operator::NonASAP(NonASAPOp::BinaryOp {
            operator: BinaryOperator {
                checked_relative_division: false,
                checked_finite_division: false,
                kind: BinaryOpKind::Set(kind),
                vector_match: None,
            },
            return_bool: false,
            lhs: readout,
            rhs: readout,
        })
    };
    let relabel = |source_column| {
        Operator::NonASAP(NonASAPOp::PromqlRelabel {
            dst: "x".into(),
            value: ScalarExpr::FunctionCall {
                name: "label_replace".into(),
                args: vec![
                    ScalarExpr::Column(source_column),
                    ScalarExpr::Literal(ScalarValue::Utf8("(.*)".into())),
                    ScalarExpr::Literal(ScalarValue::Utf8("$1".into())),
                ],
            },
            child: readout,
        })
    };
    let factors = |consumer: &Operator<MemoGroupId>, metric| {
        error_contract(&memo, consumer, metric).map(|contract| contract.error_factor_per_input)
    };
    for metric in AccuracyMetric::ALL {
        for kind in [PromQLVectorSetOpKind::And, PromQLVectorSetOpKind::Unless] {
            assert_eq!(
                factors(&set_op(kind.clone()), metric),
                Some(vec![1.0, 0.0]),
                "{kind:?}, {metric:?}"
            );
        }
        assert_eq!(
            factors(&set_op(PromQLVectorSetOpKind::Or), metric),
            Some(vec![1.0, 1.0]),
            "or, {metric:?}"
        );
        assert_eq!(
            factors(&relabel(job), metric),
            Some(vec![1.0]),
            "relabel from job, {metric:?}"
        );
        assert_eq!(
            factors(&relabel(estimate), metric),
            None,
            "relabel from the estimate, {metric:?}"
        );
    }
}

/// The consumers of `column_signs` in the error contract (sign test plan
/// T5.1 and T5.4): a relative contract of a `sum` or `avg` reads whether
/// its input's values have one sign, and a `Project` of a math function
/// keeps an error by the function's typed key, the one the sign
/// derivation also reads.
mod sign_consumers {
    use super::*;
    use asap_types::ir::operator::MathFunc;
    use asap_types::ir::scalar::scalar_type_rules::math_function_of_call;
    use asap_types::test_support::memo_fixtures::range;

    const FIVE_MINUTES_MS: i64 = 300_000;

    /// Sources `pos` (`value` declared non-negative), `neg` (declared
    /// non-positive), `mixed` (nothing declared) and `c_total` (a counter,
    /// its value non-negative without a declaration).
    fn signed_sources() -> DataWorkload {
        let declared = |sign| SourceFacts {
            column_signs: BTreeMap::from([("value".to_string(), sign)]),
            ..SourceFacts::default()
        };
        DataWorkload {
            sources: BTreeMap::from([
                ("pos".to_string(), declared(ValueSign::NonNegative)),
                ("neg".to_string(), declared(ValueSign::NonPositive)),
                ("mixed".to_string(), SourceFacts::default()),
                (
                    "c_total".to_string(),
                    SourceFacts {
                        metric_type: Some(asap_types::workload::PrometheusMetricType::Counter),
                        ..SourceFacts::default()
                    },
                ),
            ]),
            ..DataWorkload::default()
        }
    }

    /// The sample-value column of `group`'s output.
    fn value_column(memo: &Memo, group: MemoGroupId) -> usize {
        resolve_column_ref(
            &ColumnRef::SampleValue,
            &memo.groups()[group.0 as usize].group_properties.schema,
        )
        .expect("the group has a sample value")
    }

    /// `quantile_over_time(0.9, metric[5m])` per series, computed exactly.
    fn quantile_over_time(memo: &mut Memo, metric: &str) -> MemoGroupId {
        let input = memo
            .find_or_add_group(scan(metric, &[("job", DataType::Utf8)], vec![]))
            .unwrap();
        let window = memo
            .find_or_add_group(range(FIVE_MINUTES_MS, input))
            .unwrap();
        memo.find_or_add_group(aggregate(
            Reduction::PerEntity,
            AggIntent::Quantile { col: None, q: 0.9 },
            window,
        ))
        .unwrap()
    }

    /// `-child`: every column of `child` copied, its sample value negated.
    fn negated(memo: &mut Memo, child: MemoGroupId) -> MemoGroupId {
        let value = value_column(memo, child);
        let fields = memo.groups()[child.0 as usize]
            .group_properties
            .schema
            .fields
            .clone();
        let project = Operator::NonASAP(NonASAPOp::Project {
            cols: fields
                .iter()
                .enumerate()
                .map(|(column, field)| ProjectItem {
                    alias: Some(field.name.clone()),
                    expr: if column == value {
                        ScalarExpr::Negative {
                            expr: Box::new(ScalarExpr::Column(column)),
                            semantics: ExprSemantics::Promql,
                        }
                    } else {
                        ScalarExpr::Column(column)
                    },
                })
                .collect(),
            child,
        });
        memo.find_or_add_group(project).unwrap()
    }

    /// A relative error ε on each input of a `sum` or `avg` is at most ε
    /// on the result exactly when the inputs' exact values have one sign
    /// (`|Σ(x̂ - x)| ≤ ε Σ|x| = ε |Σx|`): β = 1 for a child whose value
    /// column is `NonNegative` or `NonPositive`, no contract for
    /// `Unknown`, whose values may cancel (1 + (-1) = 0, while 1.05 +
    /// (-0.95) = 0.1). In absolute error an `avg` keeps the error (β = 1)
    /// whatever the sign, a `sum` adds the errors up (no contract).
    ///
    /// The children's signs are declared (a `Scan`'s `SourceFacts`) or
    /// derived, each hand-derived under IEEE 754 with its sign bit:
    ///
    /// | Child | Sign | Why |
    /// |---|---|---|
    /// | `pos` | N | declared |
    /// | `neg` | P | declared |
    /// | `mixed` | U | undeclared |
    /// | `rate(c_total[5m])` | N | a counter's rate is ≥ +0 |
    /// | `-quantile_over_time(0.9, pos[5m])` | P | a quantile of N values is one of them or between two, `-x` flips the sign bit |
    /// | `q(pos) - q(neg)` | N | `x - y = x + (-y)`, N + N = N, and `+0 - (-0) = +0` |
    ///
    /// Every mismatch is listed, then the test fails once with all of
    /// them. Today the last row fails: `-` has no sign rule (B6).
    #[test]
    fn relative_sum_and_avg_contracts_need_one_signed_inputs() {
        use ValueSign::{NonNegative, NonPositive, Unknown};
        let mut memo = Memo::new(&signed_sources());
        let declared = |memo: &mut Memo, metric: &str| {
            memo.find_or_add_group(scan(metric, &[("job", DataType::Utf8)], vec![]))
                .unwrap()
        };
        let pos = declared(&mut memo, "pos");
        let neg = declared(&mut memo, "neg");
        let mixed = declared(&mut memo, "mixed");
        let rate_of_counter = {
            let input = declared(&mut memo, "c_total");
            let window = memo
                .find_or_add_group(range(FIVE_MINUTES_MS, input))
                .unwrap();
            memo.find_or_add_group(aggregate(Reduction::PerEntity, AggIntent::Rate, window))
                .unwrap()
        };
        let quantile_of_pos = quantile_over_time(&mut memo, "pos");
        let quantile_of_neg = quantile_over_time(&mut memo, "neg");
        let negated_quantile = negated(&mut memo, quantile_of_pos);
        let difference = memo
            .find_or_add_group(arithmetic(
                ArithmeticOpKind::Sub,
                quantile_of_pos,
                quantile_of_neg,
            ))
            .unwrap();
        let children = [
            ("pos", pos, NonNegative),
            ("neg", neg, NonPositive),
            ("mixed", mixed, Unknown),
            ("rate(c_total[5m])", rate_of_counter, NonNegative),
            ("-q(pos)", negated_quantile, NonPositive),
            ("q(pos) - q(neg)", difference, NonNegative),
        ];
        let mut mismatches = Vec::new();
        for (name, child, sign) in children {
            let derived = memo.groups()[child.0 as usize]
                .group_properties
                .column_signs[value_column(&memo, child)];
            if derived != sign {
                mismatches.push(format!(
                    "{name}: value sign {derived:?}, hand-derived {sign:?}"
                ));
            }
            // Over all rows (`sum(x)`), so no grouping key reads an
            // estimate.
            let total = || Reduction::by(vec![]);
            let sum = aggregate(total(), AggIntent::Sum { col: None }, child);
            let avg = aggregate(total(), AggIntent::Avg { col: None }, child);
            let exact_sum = Operator::ASAP(ASAPOp::SummaryAgg {
                child,
                summary_type: FieldDataType::exact(ExactKind::Sum),
                input: SummaryUpdate::column(ColumnRef::SampleValue),
                reduction: total(),
                filter: None,
            });
            let one_sign = matches!(sign, NonNegative | NonPositive);
            for (consumer_name, consumer, absolute) in [
                ("sum", &sum, None),
                ("avg", &avg, Some(vec![1.0])),
                ("exact sum", &exact_sum, None),
            ] {
                for (metric, expected) in [
                    (AccuracyMetric::Relative, one_sign.then(|| vec![1.0])),
                    (AccuracyMetric::ABSOLUTE, absolute.clone()),
                ] {
                    let factors = error_contract(&memo, consumer, metric)
                        .map(|contract| contract.error_factor_per_input);
                    if factors != expected {
                        mismatches.push(format!(
                            "{consumer_name}({name}), {metric:?}: {factors:?}, expected {expected:?}"
                        ));
                    }
                }
            }
        }
        assert!(mismatches.is_empty(), "{mismatches:#?}");
    }

    /// Every `MathFunc` variant, each with representative literal
    /// arguments. The `match` in [`promql_call`] lists every variant with
    /// no wildcard, so a new variant fails to compile until it is added
    /// there and here.
    fn all_math_funcs() -> Vec<MathFunc> {
        vec![
            MathFunc::Abs,
            MathFunc::Ceil,
            MathFunc::Floor,
            MathFunc::Exp,
            MathFunc::Ln,
            MathFunc::Log2,
            MathFunc::Log10,
            MathFunc::Sqrt,
            MathFunc::Sgn,
            MathFunc::Sin,
            MathFunc::Cos,
            MathFunc::Tan,
            MathFunc::Asin,
            MathFunc::Acos,
            MathFunc::Atan,
            MathFunc::Sinh,
            MathFunc::Cosh,
            MathFunc::Tanh,
            MathFunc::Asinh,
            MathFunc::Acosh,
            MathFunc::Atanh,
            MathFunc::Deg,
            MathFunc::Rad,
            MathFunc::Round { to_nearest: 1.0 },
            MathFunc::Clamp {
                min: 0.0,
                max: 100.0,
            },
            MathFunc::ClampMin { min: 0.0 },
            MathFunc::ClampMax { max: 5.0 },
        ]
    }

    /// The PromQL frontend's call of `f` on `value`: its name and its
    /// literal arguments after the value (`clamp_min(v, 0)` →
    /// `promql_clamp_min`, `[0]`), and whether it keeps an error with
    /// β = 1 in absolute and in relative error. A function moving a value
    /// by at most its error is 1-Lipschitz: `|f(x̂) - f(x)| ≤ |x̂ - x|`
    /// (abs, clamp, clamp_min, clamp_max). Only `abs` also keeps the
    /// magnitude, `||x̂| - |x|| ≤ |x̂ - x|` with `||x|| = |x|`, so a
    /// relative error ε on x is at most ε on |x|; `clamp_min(x, 0)` of
    /// x = -10 and x̂ = -9.5 is 0 for both, but of x = 0.1, x̂ = -0.1 it
    /// is 0 against 0.1: not relative.
    fn promql_call(f: &MathFunc) -> (&'static str, Vec<f64>, bool, bool) {
        match f {
            MathFunc::Abs => ("promql_abs", vec![], true, true),
            MathFunc::Ceil => ("promql_ceil", vec![], false, false),
            MathFunc::Floor => ("promql_floor", vec![], false, false),
            MathFunc::Exp => ("promql_exp", vec![], false, false),
            MathFunc::Ln => ("promql_ln", vec![], false, false),
            MathFunc::Log2 => ("promql_log2", vec![], false, false),
            MathFunc::Log10 => ("promql_log10", vec![], false, false),
            MathFunc::Sqrt => ("promql_sqrt", vec![], false, false),
            MathFunc::Sgn => ("promql_sgn", vec![], false, false),
            MathFunc::Sin => ("promql_sin", vec![], false, false),
            MathFunc::Cos => ("promql_cos", vec![], false, false),
            MathFunc::Tan => ("promql_tan", vec![], false, false),
            MathFunc::Asin => ("promql_asin", vec![], false, false),
            MathFunc::Acos => ("promql_acos", vec![], false, false),
            MathFunc::Atan => ("promql_atan", vec![], false, false),
            MathFunc::Sinh => ("promql_sinh", vec![], false, false),
            MathFunc::Cosh => ("promql_cosh", vec![], false, false),
            MathFunc::Tanh => ("promql_tanh", vec![], false, false),
            MathFunc::Asinh => ("promql_asinh", vec![], false, false),
            MathFunc::Acosh => ("promql_acosh", vec![], false, false),
            MathFunc::Atanh => ("promql_atanh", vec![], false, false),
            MathFunc::Deg => ("promql_deg", vec![], false, false),
            MathFunc::Rad => ("promql_rad", vec![], false, false),
            MathFunc::Round { to_nearest } => ("promql_round", vec![*to_nearest], false, false),
            MathFunc::Clamp { min, max } => ("promql_clamp", vec![*min, *max], true, false),
            MathFunc::ClampMin { min } => ("promql_clamp_min", vec![*min], true, false),
            MathFunc::ClampMax { max } => ("promql_clamp_max", vec![*max], true, false),
        }
    }

    /// The error contract and the sign derivation recognize a function by
    /// one typed key, [`math_function_of_call`], not by two string lists
    /// that can drift apart (B10). For every `MathFunc`, a `Project` over
    /// a KLL quantile readout replacing the estimate with
    /// `promql_<f>(value, literals…)`:
    /// - keeps the absolute error iff f ∈ {Abs, Clamp, ClampMin, ClampMax};
    /// - keeps the relative error iff f = Abs;
    /// - and `math_function_of_call` returns f for that call.
    ///
    /// SQL's `abs` and `ABS` are the same key as `promql_abs`; a clamp
    /// whose bound is a column, not a literal, has no key and no contract.
    #[test]
    fn error_contract_and_sign_read_one_function_key() {
        let mut memo = Memo::new(&DataWorkload::default());
        let input = memo
            .find_or_add_group(scan("latency", &[("job", DataType::Utf8)], vec![]))
            .unwrap();
        let state = memo
            .find_or_add_group(Operator::ASAP(ASAPOp::SummaryAgg {
                child: input,
                summary_type: FieldDataType::Sketch(SketchAlgorithm::Kll),
                input: SummaryUpdate::column(ColumnRef::SampleValue),
                reduction: Reduction::by(vec![2]),
                filter: None,
            }))
            .unwrap();
        let readout = memo
            .find_or_add_group(readout(SketchStatistic::Quantile { q: 0.9 }, state))
            .unwrap();
        let estimate = value_column(&memo, readout);
        let fields = memo.groups()[readout.0 as usize]
            .group_properties
            .schema
            .fields
            .clone();
        let project_value = |expr: ScalarExpr<MemoGroupId>| {
            Operator::NonASAP(NonASAPOp::Project {
                cols: fields
                    .iter()
                    .enumerate()
                    .map(|(column, field)| ProjectItem {
                        alias: Some(field.name.clone()),
                        expr: if column == estimate {
                            expr.clone()
                        } else {
                            ScalarExpr::Column(column)
                        },
                    })
                    .collect(),
                child: readout,
            })
        };
        let call = |name: &str, literals: &[f64]| ScalarExpr::FunctionCall {
            name: name.to_string(),
            args: std::iter::once(ScalarExpr::Column(estimate))
                .chain(
                    literals
                        .iter()
                        .map(|x| ScalarExpr::Literal(ScalarValue::Float64(*x))),
                )
                .collect(),
        };
        let keeps = |expr: &ScalarExpr<MemoGroupId>, metric| {
            error_contract(&memo, &project_value(expr.clone()), metric)
                .map(|contract| contract.error_factor_per_input)
                == Some(vec![1.0])
        };
        // The contracts, first: they hold today through the string list.
        let mut calls = Vec::new();
        for f in all_math_funcs() {
            let (name, literals, absolute, relative) = promql_call(&f);
            let expr = call(name, &literals);
            assert_eq!(
                keeps(&expr, AccuracyMetric::ABSOLUTE),
                absolute,
                "{name}: absolute"
            );
            assert_eq!(
                keeps(&expr, AccuracyMetric::Relative),
                relative,
                "{name}: relative"
            );
            calls.push((f, name, expr));
        }
        for name in ["abs", "ABS"] {
            let expr = call(name, &[]);
            assert!(keeps(&expr, AccuracyMetric::ABSOLUTE), "{name}");
            assert!(keeps(&expr, AccuracyMetric::Relative), "{name}");
        }
        let bound_by_a_column = ScalarExpr::FunctionCall {
            name: "promql_clamp_min".to_string(),
            args: vec![ScalarExpr::Column(estimate), ScalarExpr::Column(estimate)],
        };
        assert!(!keeps(&bound_by_a_column, AccuracyMetric::ABSOLUTE));
        // The key both read: one parse function for every spelling.
        for (f, name, expr) in &calls {
            let ScalarExpr::FunctionCall { args, .. } = expr else {
                unreachable!("built as a call")
            };
            assert_eq!(math_function_of_call(name, args), Some(f.clone()), "{name}");
        }
        for name in ["abs", "ABS"] {
            assert_eq!(
                math_function_of_call(name, &[ScalarExpr::<MemoGroupId>::Column(estimate)]),
                Some(MathFunc::Abs),
                "{name}"
            );
        }
        let ScalarExpr::FunctionCall { name, args } = &bound_by_a_column else {
            unreachable!("built as a call")
        };
        assert_eq!(math_function_of_call(name, args), None);
    }
}

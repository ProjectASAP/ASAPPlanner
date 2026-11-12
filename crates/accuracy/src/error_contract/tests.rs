use super::*;
use asap_types::ir::operator::arguments::Reduction;
use asap_types::ir::scalar::CompareOpKind;
use asap_types::ir::schema::{DataType, PROMQL_SERIES_IDENTITY};
use asap_types::ir::{ExprSemantics, Predicate, ProjectItem};
use asap_types::summary_type::{SketchAlgorithm, SketchStatistic, SummaryUpdate};
use asap_types::test_support::memo_fixtures::{aggregate, arithmetic, compare, readout, scan};
use asap_types::workload::DataWorkload;

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

use super::*;
use crate::NoMeasuredGuarantees;
use asap_types::accuracy::AccuracyTarget;
use asap_types::ir::operator::arguments::Reduction;
use asap_types::ir::operator::AggIntent;
use asap_types::ir::scalar::expr_ir::ArithmeticOpKind;
use asap_types::ir::scalar::{CompareOpKind, ScalarValue};
use asap_types::ir::{ExprSemantics, NonASAPOp, Operator, Predicate};
use asap_types::stages::{MemoQuery, RootPruneReason};
use asap_types::summary_type::{SketchAlgorithm, SketchStatistic};
use asap_types::test_support::memo_builders::{
    estimate, group, own_candidate, quantile_over_time, quantile_over_time_readouts, query,
    scan_group, QuantileReadout, QuantileReadouts,
};
use asap_types::test_support::memo_fixtures::{arithmetic, compare, range};
use asap_types::workload::DataWorkload;

fn error_metrics(memo: &Memo) -> ErrorMetricsOfMemo {
    ErrorMetricsOfMemo::of(memo, &memo.topological_order().expect("acyclic"))
}

/// The metrics `group`'s output may be off in.
fn allowed_at_output(memo: &Memo, group: MemoGroupId) -> Option<Vec<AccuracyMetric>> {
    error_metrics(memo).error_metrics_allowed_at_output[group.0 as usize].clone()
}

/// The metrics `consumer` allows on its inputs.
fn allowed_on_inputs(
    memo: &Memo,
    consumer: CandidateForMemoGroupId,
) -> Option<Vec<AccuracyMetric>> {
    error_metrics(memo).error_metrics_allowed_on_inputs(consumer)
}

/// $\alpha^{\log}$ of the edge from `producer` into input `input` of
/// `consumer`, with the metric rule of `memo`'s queries.
fn edge(
    memo: &Memo,
    producer: CandidateForMemoGroupId,
    consumer: CandidateForMemoGroupId,
    input: usize,
) -> Result<(), RootPruneReason> {
    error_metrics(memo).edge_meets_logical_requirements(
        memo,
        producer,
        consumer,
        input,
        &NoMeasuredGuarantees,
    )
}

/// The edge on which `readout` introduces its error into `consumer`'s
/// input `input`: from the readout candidate itself into `consumer`
/// when the quantile group reads it directly, else into the renaming
/// `Project` that reads it.
fn readout_edge(
    memo: &Memo,
    readout: &QuantileReadout,
    consumer: CandidateForMemoGroupId,
    input: usize,
) -> Result<(), RootPruneReason> {
    if readout.readout == readout.candidate_of_result {
        edge(memo, readout.readout, consumer, input)
    } else {
        edge(memo, readout.readout, readout.candidate_of_result, 0)
    }
}

/// `quantile_over_time(q, m[5m])` per series of `m` by `job` with KLL
/// and DDSketch readouts beside the exact quantile.
fn quantile_readouts(memo: &mut Memo, q: f64) -> QuantileReadouts {
    let input = scan_group(memo, "m", &["job"], vec![]);
    let window = group(memo, range(300_000, input));
    quantile_over_time_readouts(
        memo,
        window,
        q,
        &[SketchAlgorithm::Kll, SketchAlgorithm::DDSketch],
    )
}

/// A ratio reads no rank error: with a relative target, the KLL
/// readout's metric, rank, is accepted neither by the ratio nor at its
/// own output, so its edge into the division fails the metric rule.
#[test]
fn a_ratio_reads_no_rank_error() {
    let mut memo = Memo::new(&DataWorkload::default());
    let (_, state, p99) = quantile_over_time(&mut memo, SketchAlgorithm::Kll, 0.99, 300);
    let p50 = estimate(&mut memo, state, SketchStatistic::Quantile { q: 0.5 });
    let ratio = group(&mut memo, arithmetic(ArithmeticOpKind::Div, p99, p50));
    let target = AccuracyTarget::max_error(0.05).accuracy_metric(AccuracyMetric::Relative);
    query(&mut memo, ratio, Some(target));
    let relative = Some(vec![AccuracyMetric::Relative]);
    assert_eq!(allowed_at_output(&memo, p99), relative);
    assert_eq!(allowed_on_inputs(&memo, own_candidate(ratio)), relative);
    assert!(matches!(
        edge(&memo, own_candidate(p99), own_candidate(ratio), 0),
        Err(RootPruneReason::MetricRule {
            producer_metric: AccuracyMetric::Rank,
            ..
        })
    ));
}

/// The metric rule cuts edges, not candidates.
/// `avg by (job)(q0.5 / q0.5 + q0.9)` at absolute error 0.05, each
/// quantile per series with KLL, DDSketch and exact candidates: the `+`
/// carries absolute error only, so the division's inputs accept no
/// metric and the edges from the DDSketch and KLL readouts into it are
/// cut; the exact quantile's edge into the division and the division's
/// edge into the `+` hold.
#[test]
fn the_metric_rule_cuts_the_readout_edges_and_keeps_the_exact_division() {
    let mut memo = Memo::new(&DataWorkload::default());
    let p50 = quantile_readouts(&mut memo, 0.5);
    let window = *memo.groups()[p50.result.0 as usize].expression.children()[0];
    let p90 = quantile_over_time_readouts(
        &mut memo,
        window,
        0.9,
        &[SketchAlgorithm::Kll, SketchAlgorithm::DDSketch],
    );
    let ratio = group(
        &mut memo,
        arithmetic(ArithmeticOpKind::Div, p50.result, p50.result),
    );
    let sum = group(
        &mut memo,
        arithmetic(ArithmeticOpKind::Add, ratio, p90.result),
    );
    let avg = group(
        &mut memo,
        Operator::NonASAP(NonASAPOp::Aggregate {
            child: sum,
            reduction: Reduction::by(vec![0]),
            measures: vec![AggIntent::Avg { col: None }],
            output_names: vec!["avg".into()],
            filters: vec![None],
            having: None,
        }),
    );
    let target = AccuracyTarget::max_error(0.05).accuracy_metric(AccuracyMetric::ABSOLUTE);
    query(&mut memo, avg, Some(target));
    let division = own_candidate(ratio);
    assert_eq!(allowed_on_inputs(&memo, division), Some(vec![]));
    let result = edge(&memo, division, own_candidate(sum), 0);
    assert!(result.is_ok(), "{result:?}");
    for input in 0..2 {
        let result = edge(&memo, own_candidate(p50.result), division, input);
        assert!(result.is_ok(), "{result:?}");
        for readout in &p50.by_algorithm {
            assert!(
                matches!(
                    readout_edge(&memo, readout, division, input),
                    Err(RootPruneReason::MetricRule { .. })
                ),
                "{:?}",
                readout.algorithm
            );
        }
    }
}

/// A `Filter` whose predicate reads only label columns keeps or drops
/// whole rows, so a value's error passes through it unchanged.
/// `quantile_over_time(0.99, latency[5m])` behind `job = "api"`,
/// relative error 0.05: the filter accepts relative error on its input
/// and the DDSketch readout's edge holds.
#[test]
fn a_key_only_filter_keeps_the_ddsketch_readout_admissible() {
    let mut memo = Memo::new(&DataWorkload::default());
    let input = scan_group(&mut memo, "latency", &["job"], vec![]);
    let window = group(&mut memo, range(300_000, input));
    let p99 = quantile_over_time_readouts(&mut memo, window, 0.99, &[SketchAlgorithm::DDSketch]);
    let job = memo.groups()[p99.result.0 as usize]
        .group_properties
        .schema
        .fields
        .iter()
        .position(|field| field.name == "job")
        .expect("the result keeps the job label");
    let filtered = group(
        &mut memo,
        Operator::NonASAP(NonASAPOp::Filter {
            pred: Predicate(compare(
                job,
                CompareOpKind::Eq,
                ScalarValue::Utf8("api".into()),
                ExprSemantics::Promql,
            )),
            child: p99.result,
        }),
    );
    let target = AccuracyTarget::max_error(0.05).accuracy_metric(AccuracyMetric::Relative);
    query(&mut memo, filtered, Some(target));
    assert_eq!(
        allowed_on_inputs(&memo, own_candidate(filtered)),
        Some(vec![AccuracyMetric::Relative])
    );
    let result = readout_edge(&memo, &p99.by_algorithm[0], own_candidate(filtered), 0);
    assert!(result.is_ok(), "{result:?}");
}

/// `avg` has an absolute contract only here; a relative bound on a
/// mean needs the values' signs. So
/// `avg(quantile_over_time(0.99, m[5m]))` with a relative target cuts
/// the edges from the sketch readouts into the `avg`: it reads the
/// exact quantile only.
#[test]
fn avg_of_readouts_under_a_relative_target_reads_exact_only() {
    let mut memo = Memo::new(&DataWorkload::default());
    let p99 = quantile_readouts(&mut memo, 0.99);
    let avg = group(
        &mut memo,
        Operator::NonASAP(NonASAPOp::Aggregate {
            child: p99.result,
            reduction: Reduction::by(vec![]),
            measures: vec![AggIntent::Avg { col: None }],
            output_names: vec!["avg".into()],
            filters: vec![None],
            having: None,
        }),
    );
    let target = AccuracyTarget::max_error(0.05).accuracy_metric(AccuracyMetric::Relative);
    query(&mut memo, avg, Some(target));
    for readout in &p99.by_algorithm {
        assert!(
            matches!(
                readout_edge(&memo, readout, own_candidate(avg), 0),
                Err(RootPruneReason::MetricRule { .. })
            ),
            "a sketch readout is admissible into avg: {:?}",
            readout.algorithm
        );
    }
    let result = edge(&memo, own_candidate(p99.result), own_candidate(avg), 0);
    assert!(result.is_ok(), "{result:?}");
}

/// A scalar root's operands are roots: `scalar(v)` returns the value
/// it reads, so `scalar(quantile_over_time(0.99, m[5m]))` at a relative
/// target allows relative error at the quantile group, and the DDSketch
/// readout's edge holds; `scalar(v) * 2` has no error contract, so its
/// operand allows no metric and every readout edge below it is cut.
#[test]
fn a_scalar_roots_operands_are_roots() {
    let target = AccuracyTarget::max_error(0.05).accuracy_metric(AccuracyMetric::Relative);
    fn scalar_of(result: MemoGroupId) -> ScalarExpr<MemoGroupId> {
        ScalarExpr::PromqlScalarFromVector(result)
    }
    fn doubled(result: MemoGroupId) -> ScalarExpr<MemoGroupId> {
        ScalarExpr::Arithmetic {
            op: ArithmeticOpKind::Mul,
            left: Box::new(scalar_of(result)),
            right: Box::new(ScalarExpr::Literal(ScalarValue::Float64(2.0))),
            semantics: ExprSemantics::Promql,
        }
    }
    let check = |root: fn(MemoGroupId) -> ScalarExpr<MemoGroupId>,
                 allowed: Vec<AccuracyMetric>,
                 ddsketch_holds: bool| {
        let mut memo = Memo::new(&DataWorkload::default());
        let p99 = quantile_readouts(&mut memo, 0.99);
        memo.add_query(MemoQuery {
            root: QueryRoot::Scalar(root(p99.result)),
            accuracy_target: Some(target.clone()),
            number_of_values_per_query_evaluation: Some(1),
        });
        assert_eq!(allowed_at_output(&memo, p99.result), Some(allowed));
        let ddsketch = p99
            .by_algorithm
            .iter()
            .find(|readout| readout.algorithm == SketchAlgorithm::DDSketch)
            .expect("a DDSketch readout");
        let edge = readout_edge(&memo, ddsketch, ddsketch.candidate_of_result, 0);
        assert_eq!(edge.is_ok(), ddsketch_holds, "{edge:?}");
    };
    check(scalar_of, vec![AccuracyMetric::Relative], true);
    check(doubled, vec![], false);
}

use super::super::default_metric::column_metric;
use super::*;
use crate::accuracy::default_metric_for_root;
use crate::ir::operator::{AggIntent, Reduction, Source};
use crate::ir::schema::{DataType, Field, Schema};
use crate::ir::{NonASAPOp, Operator, OperatorNode, QueryRoot, ScalarExpr};
use std::rc::Rc;

fn node(op: NonASAPOp) -> Rc<OperatorNode> {
    Rc::new(OperatorNode::new(Operator::NonASAP(op)).expect("fixture derives a schema"))
}

fn aggregate(measures: Vec<AggIntent>) -> QueryRoot {
    let scan = node(NonASAPOp::Scan {
        source: Source::TimeSeries { metric: "m".into() },
        predicates: vec![],
        schema: Schema::with_time_index(
            vec![
                Field::plain("ts", DataType::Timestamp, false),
                Field::plain("value", DataType::Float64, false),
            ],
            0,
            vec![],
        ),
    });
    QueryRoot::Operator(node(NonASAPOp::Aggregate {
        reduction: Reduction::by(vec![]),
        output_names: vec![String::new(); measures.len()],
        measures,
        filters: vec![],
        having: None,
        child: scan,
    }))
}

fn quantile() -> AggIntent {
    AggIntent::Quantile { col: None, q: 0.99 }
}

/// A target reads and writes as a plain struct: every field but
/// `max_error` may be omitted (no metric, no δ, the whole result), an unset
/// metric or δ writes nothing, and the old named forms (`"Exact"`,
/// `{"Epsilon": ε}`, `{"EpsilonDelta": …}`) and unknown fields are
/// rejected.
#[test]
fn accuracy_target_json_is_a_plain_struct() {
    use serde_json::json;
    let read = |value: serde_json::Value| serde_json::from_value::<AccuracyTarget>(value);
    assert_eq!(
        read(json!({"max_error": 0.01})).unwrap(),
        AccuracyTarget::max_error(0.01)
    );
    assert_eq!(
        read(json!({"max_error": 0.0, "failure_probability": 0.0})).unwrap(),
        AccuracyTarget::exact()
    );
    let rank = AccuracyTarget::max_error(0.01)
        .failure_probability(0.001)
        .accuracy_metric(AccuracyMetric::Rank);
    assert_eq!(
        read(json!({"accuracy_metric": "rank", "max_error": 0.01, "failure_probability": 0.001}))
            .unwrap(),
        rank
    );
    assert_eq!(
        serde_json::to_value(&rank).unwrap(),
        json!({"accuracy_metric": "rank", "max_error": 0.01, "failure_probability": 0.001,
                   "accuracy_scope": "whole_result"})
    );
    assert_eq!(
        serde_json::to_value(AccuracyTarget::max_error(0.01)).unwrap(),
        json!({"max_error": 0.01, "accuracy_scope": "whole_result"})
    );
    for old in [
        json!("Exact"),
        json!({"Epsilon": 0.01}),
        json!({"EpsilonDelta": {"epsilon": 0.01, "delta": 0.001}}),
        json!({"max_error": 0.1, "typo": true}),
    ] {
        assert!(read(old.clone()).is_err(), "{old}");
    }
}

/// Metrics have stable wire names.
#[test]
fn metric_wire_names() {
    for (metric, name) in [
        (AccuracyMetric::ABSOLUTE, "absolute"),
        (AccuracyMetric::ABSOLUTE_STREAM_L1, "absolute_stream_l1"),
        (AccuracyMetric::ABSOLUTE_STREAM_L2, "absolute_stream_l2"),
        (AccuracyMetric::Relative, "relative"),
        (AccuracyMetric::Rank, "rank"),
    ] {
        assert_eq!(serde_json::to_value(metric).unwrap(), name);
        assert_eq!(
            serde_json::from_value::<AccuracyMetric>(name.into()).unwrap(),
            metric
        );
    }
}

/// A per-entity count (`count_over_time`) keeps every label column and
/// replaces the sample value in place: the value column carries the
/// count's metric, and a label after it is absolute.
#[test]
fn a_per_entity_count_states_its_value_column_not_its_last_column() {
    let scan = node(NonASAPOp::Scan {
        source: Source::TimeSeries { metric: "m".into() },
        predicates: vec![],
        schema: Schema::with_time_index(
            vec![
                Field::plain("ts", DataType::Timestamp, false),
                Field::plain("value", DataType::Float64, false),
                Field::plain("job", DataType::Utf8, false),
            ],
            0,
            vec![],
        ),
    });
    let count = node(NonASAPOp::Aggregate {
        reduction: Reduction::PerEntity,
        measures: vec![AggIntent::Count],
        output_names: vec![String::new()],
        filters: vec![],
        having: None,
        child: scan,
    });
    assert_eq!(count.schema.fields[1].name, "value");
    assert_eq!(
        column_metric(&count, 1),
        Some(AccuracyMetric::ABSOLUTE_STREAM_L1)
    );
    assert_eq!(column_metric(&count, 2), Some(AccuracyMetric::ABSOLUTE));
}

/// A root's default metric follows its computation: counts and top-k
/// frequencies are L1-scaled absolute errors, distinct counts and L2
/// frequency moments relative ones, anything else (a sum or an entropy
/// among them) absolute value errors.
#[test]
fn root_metric_defaults_follow_the_query_intent() {
    let metric = |intent: AggIntent| default_metric_for_root(&aggregate(vec![intent])).unwrap();
    assert_eq!(metric(AggIntent::Count), AccuracyMetric::ABSOLUTE_STREAM_L1);
    assert_eq!(
        metric(AggIntent::Sum { col: None }),
        AccuracyMetric::ABSOLUTE
    );
    assert_eq!(
        metric(AggIntent::TopK { k: 5 }),
        AccuracyMetric::ABSOLUTE_STREAM_L1
    );
    assert_eq!(
        metric(AggIntent::Cardinality { cols: vec![] }),
        AccuracyMetric::Relative
    );
    assert_eq!(
        metric(AggIntent::FrequencyL2 { col: None }),
        AccuracyMetric::Relative
    );
    assert_eq!(
        metric(AggIntent::FrequencyEntropy { col: None }),
        AccuracyMetric::ABSOLUTE
    );
    assert_eq!(
        metric(AggIntent::Avg { col: None }),
        AccuracyMetric::ABSOLUTE
    );
    assert_eq!(
        default_metric_for_root(&QueryRoot::Scalar(ScalarExpr::literal_f64(1.0))),
        Some(AccuracyMetric::ABSOLUTE)
    );
}

/// SQL's empty-input NULL guard (a statistic cross-joined with an exact
/// row count, and `CASE WHEN count = 0 THEN NULL ELSE statistic END`)
/// keeps the statistic's metric: the root's value is the statistic's
/// column. A `CASE` whose value branches read columns of different
/// metrics, or any other expression, is absolute.
#[test]
fn root_metric_looks_through_a_null_guard_to_its_statistic() {
    use crate::ir::operator::arguments::JoinKind;
    use crate::ir::scalar::{CompareOpKind, ScalarValue};
    use crate::ir::{Predicate, ProjectItem};
    let QueryRoot::Operator(l2) = aggregate(vec![AggIntent::FrequencyL2 { col: None }]) else {
        unreachable!()
    };
    let QueryRoot::Operator(count) = aggregate(vec![AggIntent::Count]) else {
        unreachable!()
    };
    let joined = node(NonASAPOp::Join {
        kind: JoinKind::Cross,
        pred: Predicate(ScalarExpr::Literal(ScalarValue::Boolean(true))),
        left: l2,
        right: count,
    });
    let guard = |otherwise: ScalarExpr| {
        QueryRoot::Operator(node(NonASAPOp::Project {
            cols: vec![ProjectItem {
                alias: Some("v".into()),
                expr: ScalarExpr::Case {
                    operand: None,
                    branches: vec![(
                        ScalarExpr::Compare {
                            left: Box::new(ScalarExpr::Column(1)),
                            op: CompareOpKind::Eq,
                            right: Box::new(ScalarExpr::Literal(ScalarValue::Int64(0))),
                            semantics: crate::ir::ExprSemantics::Sql,
                        },
                        ScalarExpr::Cast {
                            expr: Box::new(ScalarExpr::Literal(ScalarValue::Null)),
                            to: DataType::Float64,
                            try_cast: false,
                        },
                    )],
                    else_expr: Some(Box::new(otherwise)),
                },
            }],
            child: joined.clone(),
        }))
    };
    let metric = |root: QueryRoot| default_metric_for_root(&root).unwrap();
    assert_eq!(
        metric(guard(ScalarExpr::Column(0))),
        AccuracyMetric::Relative
    );
    // The count's column, or arithmetic over the statistic.
    assert_eq!(
        metric(guard(ScalarExpr::Column(1))),
        AccuracyMetric::ABSOLUTE_STREAM_L1
    );
    assert_eq!(
        metric(guard(ScalarExpr::Negative {
            expr: Box::new(ScalarExpr::Column(0)),
            semantics: crate::ir::ExprSemantics::Sql,
        })),
        AccuracyMetric::ABSOLUTE
    );
}

/// A quantile root has no default metric: its error is as naturally a
/// rank as a relative one, and a target that sets none accepts either.
/// It is not an input error.
#[test]
fn a_quantile_root_has_no_default_metric() {
    assert_eq!(default_metric_for_root(&aggregate(vec![quantile()])), None);
}

/// A target on the groups holding at least a share of the records reads
/// and writes its share as a fraction; a share outside (0, 1] (say 5
/// meant as 5%, or 0, which would cover every group) is bad input.
#[test]
fn a_record_share_scope_reads_its_share_as_a_fraction() {
    use serde_json::json;
    let target = AccuracyTarget::max_error(0.05)
        .failure_probability(0.05)
        .accuracy_scope(AccuracyScope::GroupsWithRecordShareAtLeast(0.05));
    let wire = json!({"max_error": 0.05, "failure_probability": 0.05,
                          "accuracy_scope": {"groups_with_record_share_at_least": 0.05}});
    assert_eq!(serde_json::to_value(&target).unwrap(), wire);
    assert_eq!(
        serde_json::from_value::<AccuracyTarget>(wire).unwrap(),
        target
    );
    for share in [json!(0.0), json!(-0.1), json!(5.0), json!(1.5)] {
        let bad = json!({"max_error": 0.05,
                             "accuracy_scope": {"groups_with_record_share_at_least": share}});
        assert!(
            serde_json::from_value::<AccuracyTarget>(bad).is_err(),
            "{share}"
        );
    }
}

/// How many values one statement covers at most: one for a per-value
/// scope, the `G` values of the result for the whole result (unbound when
/// `G` is), and at most `⌊1 / share⌋` groups for a record share, since
/// more groups cannot each hold that share. So a 1% scope covers 100,
/// a 5% scope 20, fewer when the result has fewer values.
#[test]
fn a_scope_covers_at_most_one_over_its_share_groups() {
    use AccuracyScope::*;
    assert_eq!(PerValue.max_values_covered(None), Some(1));
    assert_eq!(WholeResult.max_values_covered(None), None);
    assert_eq!(WholeResult.max_values_covered(Some(625)), Some(625));
    assert_eq!(
        GroupsWithRecordShareAtLeast(0.05).max_values_covered(None),
        Some(20)
    );
    assert_eq!(
        GroupsWithRecordShareAtLeast(0.01).max_values_covered(None),
        Some(100)
    );
    assert_eq!(
        GroupsWithRecordShareAtLeast(0.03).max_values_covered(None),
        Some(33)
    );
    assert_eq!(
        GroupsWithRecordShareAtLeast(1.0).max_values_covered(None),
        Some(1)
    );
    for invalid in [0.0, -0.1, 1.5, f64::NAN] {
        assert_eq!(
            GroupsWithRecordShareAtLeast(invalid).max_values_covered(None),
            None,
            "{invalid}"
        );
    }
    assert_eq!(
        GroupsWithRecordShareAtLeast(0.01).max_values_covered(Some(4)),
        Some(4)
    );
}

/// Exact means zero error with certainty, whatever the metric; an
/// ε of zero without a δ is not exact.
#[test]
fn exact_targets() {
    assert!(AccuracyTarget::exact().is_exact());
    assert_eq!(AccuracyTarget::exact().accuracy_metric, None);
    assert!(AccuracyTarget::exact()
        .accuracy_metric(AccuracyMetric::Rank)
        .is_exact());
    assert!(!AccuracyTarget::max_error(0.0).is_exact());
    assert_eq!(
        AccuracyTarget::max_error(0.01)
            .failure_probability(0.001)
            .failure_probability,
        Some(0.001)
    );
    assert_eq!(
        AccuracyTarget::max_error(0.01).accuracy_scope,
        AccuracyScope::WholeResult
    );
}

fn counts(per_evaluation: Option<u64>, per_output_value: Option<u64>) -> ReadoutValueCounts {
    ReadoutValueCounts {
        per_evaluation,
        per_output_value,
    }
}

/// `q * on() group_left e`, `e` one sketched series matched onto 1000:
/// `R` = 1 is below `V · fan-in` = 1000, so every estimate read bounds the
/// whole result, at δ itself.
#[test]
fn a_whole_result_reading_fewer_estimates_than_its_values_bounds_every_estimate_read() {
    let whole = AccuracyTarget::max_error(0.01).failure_probability(0.01);
    let bound = whole.failure_probability_bound(Some(1_000), counts(Some(1), Some(1)));
    assert_eq!(
        bound,
        Some(FailureProbabilityBound::EveryEstimateRead { limit: 0.01 })
    );
    assert_eq!(
        whole.failure_probability_per_readout_value(Some(1_000), counts(Some(1), Some(1))),
        Some(0.01)
    );
}

/// `q or r` over 100 series each: 200 estimates read, 100 · 1 a covered
/// value depends on, so each covered value is checked at δ / 100.
#[test]
fn an_or_over_more_estimates_than_its_values_bounds_each_covered_value() {
    let whole = AccuracyTarget::max_error(0.01).failure_probability(0.01);
    assert_eq!(
        whole.failure_probability_bound(Some(100), counts(Some(200), Some(1))),
        Some(FailureProbabilityBound::EachCoveredValue {
            values_covered: 100,
            limit: 1e-4
        })
    );
    assert_eq!(
        whole.failure_probability_per_readout_value(Some(100), counts(Some(200), Some(1))),
        Some(1e-4)
    );
}

/// `max(q)` over 1000 series: every estimate read, 1e-5 per readout value.
#[test]
fn a_reduction_bounds_every_estimate_read() {
    let whole = AccuracyTarget::max_error(0.01).failure_probability(0.01);
    let read = counts(Some(1_000), Some(1_000));
    assert_eq!(
        whole.failure_probability_bound(Some(1), read),
        Some(FailureProbabilityBound::EveryEstimateRead { limit: 0.01 })
    );
    assert_eq!(
        whole.failure_probability_per_readout_value(Some(1), read),
        Some(1e-5)
    );
}

/// A per-value target covers one value: δ is not divided by $n_q$.
#[test]
fn a_per_value_target_covers_one_value() {
    let per_value = AccuracyTarget::max_error(0.01)
        .failure_probability(0.01)
        .accuracy_scope(AccuracyScope::PerValue);
    assert_eq!(
        per_value.failure_probability_bound(Some(8), counts(Some(8), Some(1))),
        Some(FailureProbabilityBound::EachCoveredValue {
            values_covered: 1,
            limit: 0.01
        })
    );
}

/// At most ⌊1 / 0.05⌋ = 20 groups hold 5% of the records: over 1000
/// estimates each covered group is checked at 0.01 / 20; over 10, every
/// estimate read at 0.01 (1e-3 per readout value).
#[test]
fn a_record_share_covers_at_most_one_over_its_share() {
    let share = AccuracyTarget::max_error(0.01)
        .failure_probability(0.01)
        .accuracy_scope(AccuracyScope::GroupsWithRecordShareAtLeast(0.05));
    assert_eq!(
        share.failure_probability_bound(Some(1_000), counts(Some(1_000), Some(1))),
        Some(FailureProbabilityBound::EachCoveredValue {
            values_covered: 20,
            limit: 5e-4
        })
    );
    assert_eq!(
        share.failure_probability_bound(Some(1_000), counts(Some(10), Some(1))),
        Some(FailureProbabilityBound::EveryEstimateRead { limit: 0.01 })
    );
    assert_eq!(
        share.failure_probability_per_readout_value(Some(1_000), counts(Some(10), Some(1))),
        Some(1e-3)
    );
}

/// An unknown count leaves the other bound; with both unknown there is
/// none, and no target δ gives none either.
#[test]
fn an_unbound_count_leaves_the_other_bound() {
    let whole = AccuracyTarget::max_error(0.01).failure_probability(0.01);
    assert_eq!(
        whole.failure_probability_bound(None, counts(Some(1_000), Some(1))),
        Some(FailureProbabilityBound::EveryEstimateRead { limit: 0.01 })
    );
    assert_eq!(
        whole.failure_probability_bound(Some(100), counts(None, Some(1))),
        Some(FailureProbabilityBound::EachCoveredValue {
            values_covered: 100,
            limit: 1e-4
        })
    );
    assert_eq!(
        whole.failure_probability_bound(None, counts(None, Some(1))),
        None
    );
    assert_eq!(
        AccuracyTarget::max_error(0.01)
            .failure_probability_bound(Some(1), counts(Some(1), Some(1))),
        None
    );
}

/// Pass 2's per-readout split times the estimates the named bound counts
/// (`R`, or `V · fan-in`) is δ: readouts at the split meet the bound by
/// construction.
#[test]
fn the_readout_split_meets_the_bound_it_names() {
    for delta in [1e-2, 1e-6] {
        for values in [Some(1), Some(7), Some(100), None] {
            for read in [Some(1), Some(7), Some(200), None] {
                for fan_in in [Some(1), Some(7), read] {
                    let target = AccuracyTarget::max_error(0.01).failure_probability(delta);
                    let readout_values = counts(read, fan_in);
                    let Some(bound) = target.failure_probability_bound(values, readout_values)
                    else {
                        continue;
                    };
                    let split = target
                        .failure_probability_per_readout_value(values, readout_values)
                        .unwrap();
                    let total = match bound {
                        FailureProbabilityBound::EveryEstimateRead { .. } => {
                            split * read.unwrap().max(1) as f64
                        }
                        FailureProbabilityBound::EachCoveredValue { values_covered, .. } => {
                            split * (values_covered * fan_in.unwrap().max(1)) as f64
                        }
                    };
                    assert!(
                        (total - delta).abs() <= 1e-12 * delta,
                        "{values:?} {readout_values:?}: {total}"
                    );
                }
            }
        }
    }
}

/// δ = 0.01 splits over min(R, V · fan-in) readout values; an unknown term
/// drops out of the `min`.
#[test]
fn failure_probability_splits_over_the_readout_values_covered() {
    let read = |per_evaluation, per_output_value| ReadoutValueCounts {
        per_evaluation,
        per_output_value,
    };
    let per_value = AccuracyTarget::max_error(0.05)
        .failure_probability(0.01)
        .accuracy_scope(AccuracyScope::PerValue);
    let whole = per_value.clone().accuracy_scope(AccuracyScope::WholeResult);
    // (target, n_q, readout values, divisor)
    let cases = [
        // 8 series, no reduction.
        (&per_value, Some(8), read(Some(8), Some(1)), 1),
        (&whole, Some(8), read(Some(8), Some(1)), 8),
        // A global `max` over 1000 series.
        (&per_value, Some(1), read(Some(1000), Some(1000)), 1000),
        (&whole, Some(1), read(Some(1000), Some(1000)), 1000),
        // A grouped sum whose groups are known to hold 100 series each.
        (&per_value, Some(10), read(Some(1000), Some(100)), 100),
        (&whole, Some(10), read(Some(1000), Some(100)), 1000),
        // Unknown terms.
        (&per_value, None, read(None, Some(1)), 1),
        (&whole, None, read(Some(100), Some(1)), 100),
        (&whole, Some(8), read(None, Some(1)), 8),
        (&per_value, Some(1), read(Some(1000), None), 1000),
    ];
    for (target, n_q, readout_values, divisor) in cases {
        let split = target.failure_probability_per_readout_value(n_q, readout_values);
        assert_eq!(
            split,
            Some(0.01 / divisor as f64),
            "{target:?} {n_q:?} {readout_values:?}"
        );
    }
    assert_eq!(
        whole.failure_probability_per_readout_value(None, read(None, Some(1))),
        None
    );
    assert_eq!(
        AccuracyTarget::max_error(0.05)
            .failure_probability_per_readout_value(None, read(None, None)),
        Some(1.0)
    );
}

/// `larger` and `plus` combine counts field by field; an unknown count
/// stays unknown.
#[test]
fn readout_value_counts_combine_count_by_count() {
    let counts = |per_evaluation, per_output_value| ReadoutValueCounts {
        per_evaluation,
        per_output_value,
    };
    let (a, b) = (counts(Some(1000), Some(1)), counts(Some(8), Some(8)));
    assert_eq!(a.larger(b), counts(Some(1000), Some(8)));
    assert_eq!(a.plus(b), counts(Some(1008), Some(9)));
    let unknown_fan_in = counts(Some(8), None);
    assert_eq!(a.larger(unknown_fan_in), counts(Some(1000), None));
    assert_eq!(a.plus(unknown_fan_in), counts(Some(1008), None));
    assert_eq!(
        counts(Some(u64::MAX), Some(1)).plus(a),
        counts(None, Some(2))
    );
}

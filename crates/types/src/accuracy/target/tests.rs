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
        measures,
        output_names: vec![],
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
        output_names: vec![],
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

//! `count_values` as the PromQL frontend lowers it, compiled and run by
//! the executor with no planning pass in between: its label replaces a
//! grouping label of that name. Also a `Sort` and a `Limit` partitioned
//! `without` over a table, which no frontend emits. The planned `topk`,
//! `without` and `count_values` workloads are in the planner's
//! `reduce_aggregates` tests. Answers are checked against Prometheus's.

use std::collections::BTreeMap;

use asap_executor::values::Value;
use asap_integration_tests::fixtures::lower_promql;
use asap_types::accuracy::AccuracyTarget;
use asap_types::ir::operator::Source;
use asap_types::ir::NonASAPOp;

use crate::physical_common::{execute_series, labels, series_labels, RawDataSample};

/// Evaluation time; every sample is 0.5 s before it, inside the instant
/// lookback of a one-second sample interval.
const EVALUATION_MS: i64 = 300_000;

/// One answer: its labels and value.
type Answer = (BTreeMap<String, String>, f64);

/// The gauge `x`: per `(job, instance)` its value.
const SERIES: [(&str, &str, f64); 5] = [
    ("a", "1", 1.0),
    ("a", "2", 2.0),
    ("a", "3", 4.0),
    ("b", "1", 2.0),
    ("b", "2", 8.0),
];

/// `query` lowered exactly by the frontend, with series identity, run over
/// [`SERIES`]: each answer's labels and value.
fn execute(query: &str) -> Vec<Answer> {
    let root = lower_promql(query, AccuracyTarget::exact()).unwrap();
    let root = asap_types::ir::schema::series_identity::with_promql_series_identity(&root).unwrap();
    let samples: Vec<RawDataSample> = SERIES
        .iter()
        .map(|(job, instance, value)| RawDataSample {
            labels: labels(&[("__name__", "x"), ("job", job), ("instance", instance)]),
            timestamp_ms: EVALUATION_MS - 500,
            value: *value,
        })
        .collect();
    let mut answers: Vec<Answer> = execute_series(&root, &samples, EVALUATION_MS)
        .iter()
        .flat_map(|batch| {
            batch.to_rows().into_iter().map(|row| {
                let value = row.iter().rev().find_map(|cell| match cell {
                    Value::Float64(v) => Some(*v),
                    Value::Int64(v) => Some(*v as f64),
                    _ => None,
                });
                (
                    series_labels(batch.schema(), &row),
                    value.expect("a sample value"),
                )
            })
        })
        .collect();
    answers.sort_by(|a, b| a.0.cmp(&b.0));
    answers
}

/// A `count_values` label named like a grouping label replaces it, as in
/// Prometheus: the groups are the values alone.
#[test]
fn count_values_label_replaces_a_grouping_label() {
    assert_eq!(
        execute(r#"count_values by (job) ("job", x)"#),
        vec![
            (labels(&[("job", "1")]), 1.0),
            (labels(&[("job", "2")]), 2.0),
            (labels(&[("job", "4")]), 1.0),
            (labels(&[("job", "8")]), 1.0),
        ]
    );
    assert_eq!(
        execute(r#"count_values by (job) ("v", x)"#),
        vec![
            (labels(&[("job", "a"), ("v", "1")]), 1.0),
            (labels(&[("job", "a"), ("v", "2")]), 1.0),
            (labels(&[("job", "a"), ("v", "4")]), 1.0),
            (labels(&[("job", "b"), ("v", "2")]), 1.0),
            (labels(&[("job", "b"), ("v", "8")]), 1.0),
        ]
    );
}

/// A `Sort` and a `Limit` partitioned `without (b)` over a table (no
/// frontend emits one; the IR allows it) partition by every other column
/// but the time and the value: the largest value per `a`.
#[test]
fn without_partitions_over_a_table_rank_per_kept_column() {
    use asap_types::ir::operator::GroupKeys;
    use asap_types::ir::scalar::SortKey;
    use asap_types::ir::schema::{DataType, Field, Schema};
    use asap_types::ir::{Operator, OperatorNode, ScalarExpr};
    let scan = OperatorNode::new_shared(Operator::NonASAP(NonASAPOp::Scan {
        source: Source::Table {
            table_ref: "t".into(),
        },
        predicates: vec![],
        schema: Schema::with_time_index(
            vec![
                Field::plain("ts", DataType::Timestamp, false),
                Field::plain("value", DataType::Float64, false),
                Field::plain("a", DataType::Utf8, false),
                Field::plain("b", DataType::Utf8, false),
            ],
            0,
            vec![],
        ),
    }))
    .unwrap();
    let sorted = OperatorNode::new_shared(Operator::NonASAP(NonASAPOp::Sort {
        keys: vec![SortKey {
            expr: ScalarExpr::Column(1),
            ascending: false,
            nulls_first: false,
        }],
        partition_by: GroupKeys::without(vec![3]),
        child: scan,
    }))
    .unwrap();
    let limited = OperatorNode::new_shared(Operator::NonASAP(NonASAPOp::Limit {
        n: Some(1),
        offset: 0,
        partition_by: GroupKeys::without(vec![3]),
        child: sorted,
    }))
    .unwrap();
    let row = |value: f64, a: &str, b: &str| {
        vec![
            Value::Timestamp(0),
            Value::Float64(value),
            Value::Utf8(a.into()),
            Value::Utf8(b.into()),
        ]
    };
    let rows = crate::physical_common::execute_raw_rows(
        &limited,
        vec![row(1., "x", "1"), row(5., "x", "2"), row(3., "y", "1")],
    );
    let mut ranked: Vec<_> = rows
        .iter()
        .map(|row| match (&row[1], &row[2], &row[3]) {
            (Value::Float64(value), Value::Utf8(a), Value::Utf8(b)) => {
                (a.to_string(), b.to_string(), *value)
            }
            other => panic!("{other:?}"),
        })
        .collect();
    ranked.sort_by(|x, y| x.0.cmp(&y.0));
    assert_eq!(
        ranked,
        [("x".into(), "2".into(), 5.), ("y".into(), "1".into(), 3.)]
    );
}

//! Cross-series aggregates as the PromQL frontend lowers them, compiled
//! and run by the executor with no planning pass in between: an exact
//! `topk` stays `Aggregate{Reduce, [TopK]}` (Stage 2 lowers it to a
//! `Sort` and `Limit` on main, the executor runs it either way), a
//! `without` grouping groups by the labels it keeps, and `count_values`
//! labels each value. Answers are checked against Prometheus's.

use std::collections::BTreeMap;
use std::sync::Arc;

use asap_executor::physical_planner::{compile, promql_rows};
use asap_executor::runtime::{Limits, RunContext, Scope};
use asap_executor::sources::InMemoryDataStore;
use asap_executor::values::{Batch, Value};
use asap_integration_tests::fixtures::lower_promql;
use asap_types::ir::operator::{AggIntent, Source};
use asap_types::ir::physical_export::{compile_physical_asap_dag, PhysicalASAPOperatorPayload};
use asap_types::ir::properties::AccuracyTarget;
use asap_types::ir::schema::PROMQL_SERIES_IDENTITY;
use asap_types::ir::{
    apply_materialization_timings, MaterializationAssignment, NodeToTimedNodeCache, NonASAPOp,
};
use futures::{executor::block_on, StreamExt};

/// Evaluation time; every sample is 0.5 s before it, inside the instant
/// lookback of a one-second sample interval.
const EVALUATION_MS: i64 = 300_000;

/// One answer: its labels and value.
type Answer = (BTreeMap<String, String>, f64);

fn labels(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
    pairs
        .iter()
        .map(|(name, value)| (name.to_string(), value.to_string()))
        .collect()
}

/// The gauge `x`: per `(job, instance)` its value.
const SERIES: [(&str, &str, f64); 5] = [
    ("a", "1", 1.0),
    ("a", "2", 2.0),
    ("a", "3", 4.0),
    ("b", "1", 2.0),
    ("b", "2", 8.0),
];

/// `query` lowered exactly by the frontend, with series identity, run over
/// [`SERIES`] bound at its `Scan` of `x`: each answer's labels (its series
/// identity's and each non-empty label column's) and value.
fn execute(query: &str) -> Vec<Answer> {
    let root = lower_promql(query, AccuracyTarget::exact()).unwrap();
    let root = promql_rows::with_series_identity(&root).unwrap();
    let timed = apply_materialization_timings(
        &root,
        &MaterializationAssignment::default(),
        &mut NodeToTimedNodeCache::new(),
    )
    .unwrap();
    let dag = compile_physical_asap_dag(&timed).unwrap();
    let scan = dag
        .nodes
        .iter()
        .find(|node| {
            matches!(
                node.payload,
                PhysicalASAPOperatorPayload::NonASAP(NonASAPOp::Scan { .. })
            )
        })
        .unwrap();
    let schema = Arc::new(scan.output_schema.clone());
    let rows = SERIES
        .iter()
        .map(|(job, instance, value)| {
            let labels = labels(&[("__name__", "x"), ("job", job), ("instance", instance)]);
            promql_rows::series_row(&schema, &labels, EVALUATION_MS - 500, *value).unwrap()
        })
        .collect();
    let batch = Batch::try_new(schema.clone(), rows).unwrap();
    let store = InMemoryDataStore::default()
        .with_raw_data(
            Source::TimeSeries { metric: "x".into() },
            schema,
            vec![batch],
        )
        .unwrap();
    let roots = [dag.roots[0] as u64];
    let bound = compile(&dag, &[], &roots)
        .unwrap_or_else(|error| panic!("{query}: {error}"))
        .instantiate(Arc::new(store))
        .unwrap();
    let context = RunContext::new(
        Scope::Query {
            evaluation_time_ms: EVALUATION_MS,
        },
        Limits::default(),
    )
    .unwrap();
    let batches: Vec<Batch> = block_on(async {
        let mut output = bound.execute(&roots, context).unwrap().remove(0);
        let mut batches = Vec::new();
        while let Some(batch) = output.next().await {
            batches.push((*batch.unwrap()).clone());
        }
        batches
    });
    let mut answers: Vec<Answer> = batches
        .iter()
        .flat_map(|batch| {
            batch.to_rows().into_iter().map(|row| {
                let mut labels = BTreeMap::new();
                let mut value = None;
                for (field, cell) in batch.schema().fields.iter().zip(&row) {
                    match cell {
                        Value::Utf8(text) if field.name == PROMQL_SERIES_IDENTITY => {
                            labels.extend(promql_rows::decode_series_identity(text).unwrap())
                        }
                        Value::Utf8(text) if !text.is_empty() => {
                            labels.insert(field.name.clone(), text.to_string());
                        }
                        Value::Float64(v) => value = Some(*v),
                        Value::Int64(v) => value = Some(*v as f64),
                        _ => {}
                    }
                }
                (labels, value.expect("a sample value"))
            })
        })
        .collect();
    answers.sort_by(|a, b| a.0.cmp(&b.0));
    answers
}

/// The frontend's exact `topk` is an `Aggregate` of `TopK`, which the
/// executor runs: the 2 largest job sums, with their jobs.
#[test]
fn exact_topk_of_a_grouped_sum_runs() {
    let root = lower_promql("topk(2, sum by (job) (x))", AccuracyTarget::exact()).unwrap();
    assert!(
        matches!(
            root.non_asap(),
            Some(NonASAPOp::Aggregate { measures, .. })
                if matches!(measures.as_slice(), [AggIntent::TopK { .. }])
        ),
        "{:?}",
        root.operator
    );
    assert_eq!(
        execute("topk(2, sum by (job) (x))"),
        vec![
            (labels(&[("job", "a")]), 7.0),
            (labels(&[("job", "b")]), 10.0)
        ]
    );
}

/// `topk by (job)` ranks series within each job and returns them as they
/// are, metric name included.
#[test]
fn exact_topk_per_partition_keeps_series_labels() {
    assert_eq!(
        execute("topk by (job) (1, x)"),
        vec![
            (
                labels(&[("__name__", "x"), ("instance", "2"), ("job", "b")]),
                8.0
            ),
            (
                labels(&[("__name__", "x"), ("instance", "3"), ("job", "a")]),
                4.0
            ),
        ]
    );
}

/// `without` groups by the labels it keeps, `__name__` dropped, through
/// the exact `Aggregate` (the planned workload test reaches it through an
/// exact accumulator).
#[test]
fn without_grouping_keeps_every_other_label() {
    assert_eq!(
        execute("max without (instance) (x)"),
        vec![
            (labels(&[("job", "a")]), 4.0),
            (labels(&[("job", "b")]), 8.0)
        ]
    );
    assert_eq!(
        execute("count without (job, instance) (x)"),
        vec![(labels(&[]), 5.0)]
    );
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

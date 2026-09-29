//! Compile, persist and rebind dynamic-label computation without deployment lowering.
use asap_physical_operators::{
    operators::Operator,
    physical_planner::{promql_values::*, CompiledPhysicalDag, Source},
    runtime::{Limits, RunContext, Scope},
    values::{Batch, Value},
};
use futures::{executor::block_on, StreamExt};
use planner_types::pre_asap::{AggIntent, ColumnRef, GroupKeys};
use std::collections::BTreeMap;

fn row(labels: &[(&str, &str)], value: f64) -> Vec<Value> {
    vec![
        Value::Map(
            labels
                .iter()
                .map(|(k, v)| (Value::Utf8((*k).into()), Value::Utf8((*v).into())))
                .collect::<Vec<_>>()
                .into(),
        ),
        Value::Float64(value),
    ]
}
fn run(graph: CompiledPhysicalDag, rows: Vec<Vec<Value>>) -> Vec<Vec<Value>> {
    let graph = CompiledPhysicalDag::decode(&graph.encode().unwrap()).unwrap();
    let input = Batch::try_new(vector_schema(), rows).unwrap();
    let source = Box::new(Operator::source(vector_schema(), vec![input]).unwrap()) as Source<'_>;
    let bound = graph.instantiate(BTreeMap::from([(0, source)])).unwrap();
    let context = RunContext::new(
        Scope::Query {
            evaluation_time_ms: 0,
            revision: 1,
        },
        Limits::default(),
    )
    .unwrap();
    block_on(async {
        let mut stream = bound.execute(graph.roots(), context).unwrap().remove(0);
        let mut rows = Vec::new();
        while let Some(batch) = stream.next().await {
            rows.extend(batch.unwrap().rows().iter().cloned());
        }
        rows
    })
}
fn equal_rows(actual: Vec<Vec<Value>>, expected: Vec<Vec<Value>>) {
    let mut actual = actual
        .into_iter()
        .map(|r| serde_json::to_string(&r).unwrap())
        .collect::<Vec<_>>();
    let mut expected = expected
        .into_iter()
        .map(|r| serde_json::to_string(&r).unwrap())
        .collect::<Vec<_>>();
    actual.sort();
    expected.sort();
    assert_eq!(actual, expected);
}

#[test]
fn grouping_preserves_unenumerated_labels_and_empty_label_semantics() {
    let rows = vec![
        row(&[("__name__", "m"), ("instance", "a"), ("job", "api")], 1.),
        row(&[("__name__", "m"), ("instance", "b"), ("job", "api")], 2.),
        row(&[("instance", "c"), ("job", "")], 4.),
        row(&[("instance", "d")], 8.),
    ];
    equal_rows(
        run(
            compile_aggregate(
                &AggIntent::Sum { col: None },
                &GroupKeys::without(vec![ColumnRef::Named("instance".into())]),
            )
            .unwrap(),
            rows.clone(),
        ),
        vec![row(&[("job", "api")], 3.), row(&[], 12.)],
    );
    equal_rows(
        run(
            compile_aggregate(
                &AggIntent::Count {
                    accuracy: planner_types::types::AccuracyTarget::Exact,
                },
                &GroupKeys::by(vec![ColumnRef::Named("job".into())]),
            )
            .unwrap(),
            rows,
        ),
        vec![row(&[("job", "api")], 2.), row(&[], 2.)],
    );
}

#[test]
fn ranking_and_grouped_limit_preserve_full_selected_series() {
    let grouping = GroupKeys::by(vec![ColumnRef::Named("job".into())]);
    let rows = vec![
        row(&[("instance", "a"), ("job", "api")], 1.),
        row(&[("instance", "b"), ("job", "api")], 3.),
        row(&[("instance", "c"), ("job", "worker")], 2.),
    ];
    let sorted = run(compile_sort(true, &grouping).unwrap(), rows);
    let selected = run(compile_limit(1, 0, &grouping).unwrap(), sorted);
    equal_rows(
        selected,
        vec![
            row(&[("instance", "b"), ("job", "api")], 3.),
            row(&[("instance", "c"), ("job", "worker")], 2.),
        ],
    );
    assert!(run(compile_limit(0, 0, &grouping).unwrap(), vec![row(&[], 1.)]).is_empty());
}

#[test]
fn empty_vector_aggregation_stays_empty() {
    assert!(run(
        compile_aggregate(&AggIntent::Sum { col: None }, &GroupKeys::default()).unwrap(),
        vec![]
    )
    .is_empty());
    let scalar = run(compile_vector_to_scalar().unwrap(), vec![]);
    assert!(matches!(scalar[0][0],Value::Float64(v) if v.is_nan()));
}

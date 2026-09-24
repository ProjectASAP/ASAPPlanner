//! Blocking operators enforce resources before returning their first batch.
use asap_physical_operators::dag::{
    operators::Operator,
    values::{Batch, Schema, Value},
    Error, Limits, PhysicalDag, PhysicalOperator, RunContext, Scope,
};
use futures::{executor::block_on, FutureExt, StreamExt};
use planner_types::{
    post_asap::{SummaryFamilyType, SummaryField, SummarySchema},
    pre_asap::{DataType, JoinKind, Predicate, QueryExpr, ScalarValue},
};
use std::sync::Arc;

fn schema(width: usize) -> Schema {
    Arc::new(SummarySchema {
        fields: (0..width)
            .map(|i| SummaryField {
                name: format!("v{i}"),
                dtype: SummaryFamilyType::Plain(DataType::Int64),
                nullable: false,
            })
            .collect(),
        time_index: None,
    })
}
fn context(max_bytes: usize) -> RunContext {
    RunContext::new(
        Scope::Query {
            evaluation_time_ms: 0,
            revision: 0,
        },
        Limits {
            max_bytes,
            ..Limits::default()
        },
    )
    .unwrap()
}
fn source(n: usize) -> PhysicalDag<'static, Batch, Schema> {
    let mut dag = PhysicalDag::default();
    dag.add(
        0,
        vec![],
        Operator::source(
            schema(1),
            vec![Batch::try_new(schema(1), vec![vec![Value::Int64(1)]; n]).unwrap()],
        )
        .unwrap(),
    )
    .unwrap();
    dag
}
fn cross_join() -> Operator {
    Operator::relational_join(
        schema(1),
        schema(1),
        JoinKind::Cross,
        &Predicate(std::rc::Rc::new(QueryExpr::Literal(ScalarValue::Boolean(
            true,
        )))),
        schema(2),
    )
    .unwrap()
}

// Even callers starting an operator directly cannot bypass its workspace budget.
#[test]
fn join_reserves_result_growth_before_returning_output() {
    let sources = source(64);
    let run = context(32 * 1024);
    let inputs = sources.execute(&[0, 0], run.clone()).unwrap();
    let join = cross_join();
    let mut output = join.start(inputs, run.clone()).unwrap();
    assert!(matches!(
        block_on(output.next()),
        Some(Err(Error::MemoryLimit))
    ));
    drop(output);
    assert_eq!(run.retained_bytes(), 0);
}

// A single large input batch must not monopolize the worker during a join.
#[test]
fn join_yields_during_computation_and_observes_cancellation() {
    let sources = source(64);
    let run = context(16 * 1024 * 1024);
    let inputs = sources.execute(&[0, 0], run.clone()).unwrap();
    let join = cross_join();
    let mut output = join.start(inputs, run.clone()).unwrap();
    assert!(
        output.next().now_or_never().is_none(),
        "join should yield before producing all 4096 rows"
    );
    run.cancel();
    assert!(matches!(
        block_on(output.next()),
        Some(Err(Error::Cancelled))
    ));
    drop(output);
    assert_eq!(run.retained_bytes(), 0);
}

// Sorting and grouping yield even for one large batch.
#[test]
fn blocking_reductions_yield_and_release_memory_on_cancellation() {
    use asap_physical_operators::{
        operators::{Reduction, SortKey},
        plan::PhysicalOperator,
    };
    let operators = vec![
        Operator::sort(
            schema(1),
            vec![SortKey {
                column: 0,
                descending: false,
                nulls_first: false,
            }],
            vec![],
        )
        .unwrap(),
        Operator::aggregate(schema(1), vec![], vec![("sum".into(), Reduction::Sum(0))]).unwrap(),
    ];
    for operator in operators {
        let sources = source(768);
        let run = context(16 * 1024 * 1024);
        let inputs = sources.execute(&[0], run.clone()).unwrap();
        let mut output = operator.start(inputs, run.clone()).unwrap();
        assert!(output.next().now_or_never().is_none());
        run.cancel();
        assert!(matches!(
            block_on(output.next()),
            Some(Err(Error::Cancelled))
        ));
        drop(output);
        assert_eq!(run.retained_bytes(), 0);
    }
}

// Merge-sort rounds preserve input order for tied keys across chunk boundaries.
#[test]
fn cooperative_sort_preserves_ties_across_chunks() {
    use asap_physical_operators::operators::SortKey;
    let batch = Batch::try_new(
        schema(2),
        (0..1025)
            .rev()
            .map(|i| vec![Value::Int64(i % 3), Value::Int64(i)])
            .collect(),
    )
    .unwrap();
    let mut dag = PhysicalDag::default();
    dag.add(0, vec![], Operator::source(schema(2), vec![batch]).unwrap())
        .unwrap();
    dag.add(
        1,
        vec![0],
        Operator::sort(
            schema(2),
            vec![SortKey {
                column: 0,
                descending: false,
                nulls_first: false,
            }],
            vec![],
        )
        .unwrap(),
    )
    .unwrap();
    let mut output = dag
        .execute(&[1], context(16 * 1024 * 1024))
        .unwrap()
        .remove(0);
    let batch = block_on(output.next()).unwrap().unwrap();
    let expected = (0..3)
        .flat_map(|key| (0..1025).rev().filter(move |i| i % 3 == key))
        .collect::<Vec<_>>();
    for (row, expected) in batch.rows().iter().zip(expected) {
        assert!(matches!(row[1], Value::Int64(i) if i == expected));
    }
    assert_eq!(batch.rows().len(), 1025);
}

// The integrated weighted-summary path obeys the same cooperative cancellation contract.
#[test]
fn weighted_summary_build_yields_within_a_batch() {
    use planner_types::post_asap::{SketchAlgorithm, SketchKind, SketchParams};
    let input = Arc::new(SummarySchema {
        fields: vec![
            SummaryField {
                name: "item".into(),
                dtype: SummaryFamilyType::Plain(DataType::Int64),
                nullable: false,
            },
            SummaryField {
                name: "weight".into(),
                dtype: SummaryFamilyType::Plain(DataType::Float64),
                nullable: false,
            },
        ],
        time_index: None,
    });
    let mut sources = PhysicalDag::default();
    let batch = Batch::try_new(
        input.clone(),
        (0..1500)
            .map(|i| vec![Value::Int64(i % 8), Value::Float64(0.25)])
            .collect(),
    )
    .unwrap();
    sources
        .add(
            0,
            vec![],
            Operator::source(input.clone(), vec![batch]).unwrap(),
        )
        .unwrap();
    let family = SummaryFamilyType::Sketch(
        SketchKind::new(
            SketchAlgorithm::CmsWithHeap,
            SketchParams::CmsWithHeap {
                width: 64,
                depth: 3,
                heap_size: 8,
            },
        ),
        Default::default(),
    );
    let operator = Operator::keyed_summary_build(input, family, 1, vec![0], vec![]).unwrap();
    let run = context(16 * 1024 * 1024);
    let inputs = sources.execute(&[0], run.clone()).unwrap();
    let mut output = operator.start(inputs, run.clone()).unwrap();
    assert!(output.next().now_or_never().is_none());
    run.cancel();
    assert!(matches!(
        block_on(output.next()),
        Some(Err(Error::Cancelled))
    ));
    drop(output);
    assert_eq!(run.retained_bytes(), 0);
}

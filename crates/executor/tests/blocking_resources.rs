//! Blocking operators enforce resources before returning their first batch.
use asap_executor::{
    operators::Operator,
    plan::{PhysicalDAG, PhysicalOperator},
    runtime::{Limits, RunContext, Scope},
    values::{Batch, SchemaRef, Value},
    Error,
};
use futures::{executor::block_on, FutureExt, StreamExt};
use planner_types::ir::operator::JoinKind;
use planner_types::ir::scalar::ScalarValue;
use planner_types::ir::schema::{DataType, Field, FieldDataType};
use planner_types::ir::Predicate;
use planner_types::ir::ScalarExpr;
use planner_types::stages::Lifetime;
use std::sync::Arc;

fn schema(width: usize) -> SchemaRef {
    Arc::new(planner_types::ir::schema::Schema {
        unique_keys: vec![],
        closed: false,
        fields: (0..width)
            .map(|i| Field {
                name: format!("v{i}"),
                dtype: FieldDataType::Plain(DataType::Int64),
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
        },
        Limits { max_bytes },
    )
    .unwrap()
}
fn source(n: usize) -> PhysicalDAG<'static, Batch, SchemaRef> {
    let mut dag = PhysicalDAG::default();
    dag.add(
        0,
        vec![],
        Operator::source(
            schema(1),
            vec![Batch::try_new(schema(1), vec![vec![Value::Int64(1)]; n]).unwrap()],
        )
        .unwrap(),
        Lifetime::None,
    )
    .unwrap();
    dag
}
fn cross_join() -> Operator {
    Operator::relational_join(
        schema(1),
        schema(1),
        JoinKind::Cross,
        &Predicate(ScalarExpr::Literal(ScalarValue::Boolean(true))),
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
    use asap_executor::{
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
            None,
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

// A sort that keeps the first `fetch` rows of each group holds those rows,
// not its input: 4 groups of 2 rows out of 262,144 rows run in a memory
// limit the full sort's gathered input exceeds.
#[test]
fn sort_with_fetch_holds_groups_times_fetch_rows() {
    use asap_executor::operators::SortKey;
    let input = schema(2);
    let run = |fetch: Option<u64>| {
        let batches = (0..64)
            .map(|batch| {
                let rows = (0..4096)
                    .map(|row| {
                        let i = batch * 4096 + row;
                        vec![Value::Int64(i % 4), Value::Int64(i)]
                    })
                    .collect();
                Batch::try_new(input.clone(), rows).unwrap()
            })
            .collect();
        let mut dag = PhysicalDAG::default();
        dag.add(0, vec![], Operator::source(input.clone(), batches).unwrap())
            .unwrap();
        let key = SortKey {
            column: 1,
            descending: true,
            nulls_first: false,
        };
        dag.add(
            1,
            vec![0],
            Operator::sort(input.clone(), vec![key], vec![0], fetch).unwrap(),
        )
        .unwrap();
        let context = context(1024 * 1024);
        let rows = block_on(async {
            let mut output = dag.execute(&[1], context.clone()).unwrap().remove(0);
            let mut rows = Vec::new();
            while let Some(batch) = output.next().await {
                rows.extend(batch?.to_rows());
            }
            Ok::<_, Error>(rows)
        });
        assert_eq!(context.retained_bytes(), 0);
        rows
    };
    assert!(matches!(run(None), Err(Error::MemoryLimit)));
    let top: Vec<i64> = run(Some(2))
        .unwrap()
        .iter()
        .map(|row| match row[1] {
            Value::Int64(i) => i,
            _ => unreachable!(),
        })
        .collect();
    let last = 64 * 4096 - 1;
    assert_eq!(
        top,
        [
            last - 3,
            last - 7,
            last - 2,
            last - 6,
            last - 1,
            last - 5,
            last,
            last - 4
        ]
    );
}

// Merge-sort rounds preserve input order for tied keys across chunk boundaries.
#[test]
fn cooperative_sort_preserves_ties_across_chunks() {
    use asap_executor::operators::SortKey;
    let batch = Batch::try_new(
        schema(2),
        (0..1025)
            .rev()
            .map(|i| vec![Value::Int64(i % 3), Value::Int64(i)])
            .collect(),
    )
    .unwrap();
    let mut dag = PhysicalDAG::default();
    dag.add(
        0,
        vec![],
        Operator::source(schema(2), vec![batch]).unwrap(),
        Lifetime::None,
    )
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
            None,
        )
        .unwrap(),
        Lifetime::None,
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
    for (row, expected) in batch.to_rows().iter().zip(expected) {
        assert!(matches!(row[1], Value::Int64(i) if i == expected));
    }
    assert_eq!(batch.to_rows().len(), 1025);
}

// The integrated weighted-summary path obeys the same cooperative cancellation contract.
#[test]
fn weighted_summary_build_yields_within_a_batch() {
    use planner_types::summary_type::{SketchAlgorithm, SketchParams};

    let input = Arc::new(planner_types::ir::schema::Schema {
        unique_keys: vec![],
        closed: false,
        fields: vec![
            Field {
                name: "item".into(),
                dtype: FieldDataType::Plain(DataType::Int64),
                nullable: false,
            },
            Field {
                name: "weight".into(),
                dtype: FieldDataType::Plain(DataType::Float64),
                nullable: false,
            },
        ],
        time_index: None,
    });
    let mut sources = PhysicalDAG::default();
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
            Lifetime::None,
        )
        .unwrap();
    let family = FieldDataType::Sketch(SketchAlgorithm::CmsWithHeap);
    let operator = Operator::per_group_keyed_summary_build(
        input,
        family,
        Some(SketchParams::CmsWithHeap {
            width: 64,
            depth: 3,
            heap_size: 8,
        }),
        vec![0],
        asap_executor::operators::RowWeight::Column(1),
        vec![],
    )
    .unwrap();
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

// Frequency and distinct dictionaries count against the budget and release
// their reservations on failure.
#[test]
fn frequency_dictionary_enforces_memory_budget() {
    use asap_executor::operators::Reduction;
    let input = schema(1);
    let mut sources = PhysicalDAG::default();
    sources
        .add(
            0,
            vec![],
            Operator::source(
                input.clone(),
                vec![Batch::try_new(
                    input.clone(),
                    (0..64).map(|i| vec![Value::Int64(i)]).collect(),
                )
                .unwrap()],
            )
            .unwrap(),
            Lifetime::None,
        )
        .unwrap();
    for (reduction, succeeds) in [
        (Reduction::Count, true),
        (Reduction::FrequencyL2(0), false),
        (Reduction::FrequencyEntropy(0), false),
        (Reduction::Cardinality(vec![0]), false),
    ] {
        // Count needs about 1.2 kB here (the input batch), the dictionaries
        // at least 13.6 kB.
        let run = context(8_000);
        let inputs = sources.execute(&[0], run.clone()).unwrap();
        let operator =
            Operator::aggregate(input.clone(), vec![], vec![("result".into(), reduction)]).unwrap();
        let mut output = operator.start(inputs, run.clone()).unwrap();
        let result = block_on(output.next()).unwrap();
        if succeeds {
            assert!(result.is_ok(), "{result:?}");
        } else {
            assert!(matches!(result, Err(Error::MemoryLimit)), "{result:?}");
        }
        drop(result);
        drop(output);
        assert_eq!(run.retained_bytes(), 0);
    }
}

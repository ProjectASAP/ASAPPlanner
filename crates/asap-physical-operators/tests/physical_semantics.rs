//! Contract tests inspired by DataFusion's limit, sort and join test matrices.
//! Expectations follow ASAP's IR (notably row-count and IEEE NaN equality).
//! Reference: apache/datafusion e2ca7f3, physical-plan/src/{limit.rs,sorts/sort.rs}.
use asap_physical_operators::{
    expressions::CompiledExpression,
    operators::{Expression, Operator, Reduction, SortKey},
    plan::PhysicalDag,
    runtime::{Limits, RunContext, Scope},
    values::{Batch, Schema, Value},
};
use futures::{executor::block_on, StreamExt};
use planner_types::{
    post_asap::{SummaryFamilyType, SummaryField, SummarySchema},
    pre_asap::{CompareOpKind, DataType, JoinKind, Predicate, QueryExpr},
};
use std::{rc::Rc, sync::Arc};

fn schema(fields: &[(&str, DataType, bool)]) -> Schema {
    Arc::new(SummarySchema {
        fields: fields
            .iter()
            .map(|(name, dtype, nullable)| SummaryField {
                name: (*name).into(),
                dtype: SummaryFamilyType::Plain(dtype.clone()),
                nullable: *nullable,
            })
            .collect(),
        time_index: None,
    })
}
fn context() -> RunContext {
    RunContext::new(
        Scope::Query {
            evaluation_time_ms: 0,
            revision: 1,
        },
        Limits {
            max_buffered_batches: 1,
            ..Limits::default()
        },
    )
    .unwrap()
}
fn collect(dag: &PhysicalDag<'_, Batch, Schema>, root: u64) -> Vec<Vec<Value>> {
    let run = context();
    let rows = block_on(async {
        let mut stream = dag.execute(&[root], run.clone()).unwrap().remove(0);
        let mut rows = vec![];
        while let Some(batch) = stream.next().await {
            rows.extend_from_slice(batch.unwrap().rows());
        }
        rows
    });
    assert_eq!(run.retained_bytes(), 0);
    rows
}
fn unary(input: Schema, batches: Vec<Vec<Vec<Value>>>, op: Operator) -> Vec<Vec<Value>> {
    let mut dag = PhysicalDag::default();
    let batches = batches
        .into_iter()
        .map(|rows| Batch::try_new(input.clone(), rows).unwrap())
        .collect();
    dag.add(0, vec![], Operator::source(input, batches).unwrap())
        .unwrap();
    dag.add(1, vec![0], op).unwrap();
    collect(&dag, 1)
}
fn keys(rows: &[Vec<Value>]) -> Vec<Vec<Vec<u8>>> {
    rows.iter()
        .map(|r| r.iter().map(|v| v.key().unwrap()).collect())
        .collect()
}
fn eq_predicate() -> Predicate {
    Predicate(Rc::new(QueryExpr::Compare {
        left: Rc::new(QueryExpr::Column(0)),
        op: CompareOpKind::Eq,
        right: Rc::new(QueryExpr::Column(1)),
    }))
}
fn join(left: Vec<Value>, right: Vec<Value>, kind: JoinKind, keyed: bool) -> Vec<Vec<Value>> {
    let input = schema(&[("key", DataType::Float64, true)]);
    let output = if matches!(kind, JoinKind::Semi | JoinKind::Anti) {
        input.clone()
    } else {
        schema(&[
            ("left", DataType::Float64, true),
            ("right", DataType::Float64, true),
        ])
    };
    let op = if keyed {
        Operator::semi_join(input.clone(), input.clone(), vec![(0, 0)]).unwrap()
    } else {
        Operator::relational_join(input.clone(), input.clone(), kind, &eq_predicate(), output)
            .unwrap()
    };
    let mut dag = PhysicalDag::default();
    for (id, values) in [(0, left), (1, right)] {
        let batches = values
            .into_iter()
            .map(|v| Batch::try_new(input.clone(), vec![vec![v]]).unwrap())
            .collect();
        dag.add(
            id,
            vec![],
            Operator::source(input.clone(), batches).unwrap(),
        )
        .unwrap();
    }
    dag.add(2, vec![0, 1], op).unwrap();
    collect(&dag, 2)
}

// OFFSET/FETCH must be invariant to empty batches and input batch boundaries.
#[test]
fn limit_offset_fetch_matrix() {
    let input = schema(&[("v", DataType::Int64, false)]);
    for chunk in [1, 2, 5, 12] {
        let values = (0..9).map(|n| vec![Value::Int64(n)]).collect::<Vec<_>>();
        let mut batches = vec![vec![]];
        for rows in values.chunks(chunk) {
            batches.push(rows.to_vec());
            batches.push(vec![]);
        }
        for offset in [0, 1, 8, 9, 10, u64::MAX] {
            for n in [0, 1, 3, 12, u64::MAX] {
                let rows = unary(
                    input.clone(),
                    batches.clone(),
                    Operator::limit(input.clone(), n, offset, vec![]).unwrap(),
                );
                let expected = values
                    .iter()
                    .skip(offset.min(9) as usize)
                    .take(n.min(9) as usize)
                    .cloned()
                    .collect::<Vec<_>>();
                assert_eq!(
                    keys(&rows),
                    keys(&expected),
                    "chunk={chunk}, offset={offset}, n={n}"
                );
            }
        }
    }
}

// Zero-column batches still have rows: LIMIT must not infer cardinality from columns.
#[test]
fn limit_preserves_zero_column_row_count() {
    let input = schema(&[]);
    let rows = unary(
        input.clone(),
        vec![vec![vec![]; 5], vec![vec![]; 5]],
        Operator::limit(input, 4, 3, vec![]).unwrap(),
    );
    assert_eq!(rows.len(), 4);
}

// NULL placement is independent of sort direction; ties retain original row order.
#[test]
fn sort_direction_null_placement_and_ties() {
    let input = schema(&[("v", DataType::Int64, true), ("id", DataType::Int64, false)]);
    let values = [Some(2), None, Some(1), Some(2), None];
    let rows = values
        .iter()
        .enumerate()
        .map(|(i, v)| {
            vec![
                v.map(Value::Int64).unwrap_or(Value::Null),
                Value::Int64(i as i64),
            ]
        })
        .collect::<Vec<_>>();
    for (descending, nulls_first, expected) in [
        (false, false, vec![2, 0, 3, 1, 4]),
        (false, true, vec![1, 4, 2, 0, 3]),
        (true, false, vec![0, 3, 2, 1, 4]),
        (true, true, vec![1, 4, 0, 3, 2]),
    ] {
        let op = Operator::sort(
            input.clone(),
            vec![SortKey {
                column: 0,
                descending,
                nulls_first,
            }],
            vec![],
        )
        .unwrap();
        let result = unary(
            input.clone(),
            vec![rows[..2].to_vec(), vec![], rows[2..].to_vec()],
            op,
        );
        let ids = result
            .iter()
            .map(|r| match r[1] {
                Value::Int64(n) => n,
                _ => unreachable!(),
            })
            .collect::<Vec<_>>();
        assert_eq!(ids, expected);
    }
}

// Outer joins preserve unmatched NULLs, while semi/anti joins preserve left multiplicity.
#[test]
fn joins_nulls_duplicates_and_empty_sides() {
    for (kind, expected_len) in [
        (JoinKind::Inner, 4),
        (JoinKind::Left, 6),
        (JoinKind::Right, 6),
        (JoinKind::Full, 8),
        (JoinKind::Semi, 2),
        (JoinKind::Anti, 2),
    ] {
        let left = vec![
            Value::Float64(1.),
            Value::Float64(1.),
            Value::Float64(2.),
            Value::Null,
        ];
        let right = vec![
            Value::Float64(1.),
            Value::Float64(1.),
            Value::Float64(3.),
            Value::Null,
        ];
        let result = join(left, right, kind.clone(), false);
        assert_eq!(result.len(), expected_len, "{kind:?}");
    }
    for (kind, expected_len) in [
        (JoinKind::Inner, 0),
        (JoinKind::Left, 1),
        (JoinKind::Right, 0),
        (JoinKind::Full, 1),
        (JoinKind::Semi, 0),
        (JoinKind::Anti, 1),
    ] {
        assert_eq!(
            join(vec![Value::Float64(7.)], vec![], kind.clone(), false).len(),
            expected_len,
            "{kind:?}"
        );
    }
    let result = join(vec![Value::Float64(7.)], vec![], JoinKind::Left, false);
    assert!(matches!(
        result[0].as_slice(),
        [Value::Float64(7.), Value::Null]
    ));
}

// Changing the semi-join algorithm must not turn IEEE NaN != NaN into a match.
#[test]
fn keyed_semijoin_obeys_ieee_equality_for_nan_and_zero() {
    let left = vec![
        Value::Float64(f64::NAN),
        Value::Float64(-0.),
        Value::Float64(0.),
        Value::Null,
    ];
    let right = vec![Value::Float64(f64::NAN), Value::Float64(0.), Value::Null];
    let keyed = join(left, right, JoinKind::Semi, true);
    let expected = vec![vec![Value::Float64(-0.)], vec![Value::Float64(0.)]];
    assert_eq!(keys(&keyed), keys(&expected));
}

// Group equality intentionally differs from predicate equality: NULL and NaNs group together.
#[test]
fn grouping_canonicalizes_null_nan_and_signed_zero() {
    let input = schema(&[("v", DataType::Float64, true)]);
    let op = Operator::aggregate(
        input.clone(),
        vec![0],
        vec![("count".into(), Reduction::Count)],
    )
    .unwrap();
    let values = vec![
        Value::Null,
        Value::Null,
        Value::Float64(0.),
        Value::Float64(-0.),
        Value::Float64(f64::NAN),
        Value::Float64(f64::from_bits(0x7ff8000000000001)),
    ];
    let result = unary(
        input,
        values.into_iter().map(|v| vec![vec![v]]).collect(),
        op,
    );
    assert_eq!(result.len(), 3);
    assert!(result.iter().all(|r| matches!(r[1], Value::Int64(2))));
}

// Global empty input yields one aggregate row; grouped empty input yields none.
#[test]
fn aggregate_empty_and_all_null_follow_asap_contract() {
    let input = schema(&[("v", DataType::Int64, true)]);
    for batches in [
        vec![],
        vec![vec![]],
        vec![vec![vec![Value::Null], vec![Value::Null]]],
    ] {
        let n = batches.iter().map(Vec::len).sum::<usize>();
        let op = Operator::aggregate(
            input.clone(),
            vec![],
            vec![
                ("count".into(), Reduction::Count),
                ("min".into(), Reduction::Min(0)),
                ("max".into(), Reduction::Max(0)),
            ],
        )
        .unwrap();
        let result = unary(input.clone(), batches, op);
        assert_eq!(result.len(), 1);
        assert!(matches!(result[0][0], Value::Int64(v) if v == n as i64));
        assert!(matches!(result[0][1], Value::Null));
        assert!(matches!(result[0][2], Value::Null));
    }
    let op = Operator::aggregate(
        input.clone(),
        vec![0],
        vec![("count".into(), Reduction::Count)],
    )
    .unwrap();
    assert!(unary(input, vec![], op).is_empty());
}

// A precompiled expression with a different input contract must fail during binding.
#[test]
fn projection_rejects_expression_bound_to_another_schema() {
    let original = schema(&[("a", DataType::Int64, false), ("b", DataType::Int64, false)]);
    let current = schema(&[("a", DataType::Int64, false)]);
    let expr = CompiledExpression::compile(&QueryExpr::Column(1), &original).unwrap();
    assert!(Operator::project(current, vec![("b".into(), Expression::planner(expr))]).is_err());
}

// A valid Planner MIN/MAX schema must bind even for a non-null input column.
#[test]
fn global_extrema_bind_with_planner_derived_schema() {
    use asap_physical_operators::physical_planner::compile_node;
    use planner_types::{
        post_asap::*,
        pre_asap::{AggIntent, Column, GroupKeys, Reduction as PlanReduction},
    };
    let input = schema(&[("v", DataType::Int64, false)]);
    for measure in [
        AggIntent::Min { col: Some(0) },
        AggIntent::Max { col: Some(0) },
    ] {
        let planner_input =
            planner_types::pre_asap::Schema::new(vec![Column::new("v", DataType::Int64, false)]);
        let derived = planner_types::pre_asap::query_expr::aggregate_output_schema(
            &planner_input,
            &PlanReduction::Reduce(GroupKeys::by(vec![])),
            std::slice::from_ref(&measure),
            &[],
        )
        .unwrap();
        let result = derived.columns[0].clone();
        let output = schema(&[(&result.name, result.dtype, result.nullable)]);
        let node = ExecutableDagNode {
            id: PostAsapNodeId(1),
            payload: ExecutableOperatorPayload::Value {
                operation: ValueOperation::Exact(ExactOperation::Aggregate {
                    reduction: PlanReduction::Reduce(GroupKeys::by(vec![])),
                    measures: vec![measure],
                    output_names: vec![result.name],
                    having: None,
                }),
            },
            output_state: ExecutionDataState::QUERY_ROWS,
            output_schema: (*output).clone(),
            guarantee: None,
        };
        let operator = compile_node(&node, std::slice::from_ref(&input))
            .expect("global extremum should bind to its Planner schema");
        assert!(operator.schema().fields[0].nullable);
        let empty = unary(input.clone(), vec![], operator.clone());
        assert!(matches!(empty[0][0], Value::Null));
        let nonempty = unary(input.clone(), vec![vec![vec![Value::Int64(7)]]], operator);
        assert!(matches!(nonempty[0][0], Value::Int64(7)));
    }
}

// NaN is a valid numeric input, not a schema error; all six comparisons obey IEEE rules.
#[test]
fn planner_comparisons_handle_nan_without_execution_errors() {
    let input = schema(&[
        ("a", DataType::Float64, false),
        ("b", DataType::Float64, false),
    ]);
    for op in [
        CompareOpKind::Eq,
        CompareOpKind::Ne,
        CompareOpKind::Lt,
        CompareOpKind::Le,
        CompareOpKind::Gt,
        CompareOpKind::Ge,
    ] {
        let expression = QueryExpr::Compare {
            left: Rc::new(QueryExpr::Column(0)),
            op: op.clone(),
            right: Rc::new(QueryExpr::Column(1)),
        };
        let compiled = CompiledExpression::compile(&expression, &input).unwrap();
        for row in [
            [Value::Float64(f64::NAN), Value::Float64(1.)],
            [Value::Float64(1.), Value::Float64(f64::NAN)],
            [Value::Float64(f64::NAN), Value::Float64(f64::NAN)],
        ] {
            let actual = compiled.evaluate(&row).unwrap();
            assert!(matches!(actual,Value::Bool(value) if value == (op == CompareOpKind::Ne)));
        }
    }
}

// A bounded LIMIT branch must unsubscribe so another branch can drain the producer.
#[test]
fn limit_branch_finishes_without_blocking_shared_sibling() {
    let input = schema(&[("v", DataType::Int64, false)]);
    let mut dag = PhysicalDag::default();
    let batches = (0..100)
        .map(|v| Batch::try_new(input.clone(), vec![vec![Value::Int64(v)]]).unwrap())
        .collect();
    dag.add(0, vec![], Operator::source(input.clone(), batches).unwrap())
        .unwrap();
    dag.add(
        1,
        vec![0],
        Operator::limit(input.clone(), 1, 0, vec![]).unwrap(),
    )
    .unwrap();
    dag.add(2, vec![0, 1], Operator::union(input, 2).unwrap())
        .unwrap();
    // Bound polls as well as rows so a backpressure regression cannot hang the suite.
    use futures::{task::noop_waker_ref, Stream};
    use std::{
        pin::Pin,
        task::{Context, Poll},
    };
    let run = context();
    let mut stream = dag.execute(&[2], run.clone()).unwrap().remove(0);
    let mut cx = Context::from_waker(noop_waker_ref());
    let mut count = 0;
    for _ in 0..2000 {
        match Pin::new(&mut stream).poll_next(&mut cx) {
            Poll::Ready(Some(batch)) => count += batch.unwrap().rows().len(),
            Poll::Ready(None) => {
                assert_eq!(count, 101);
                drop(stream);
                assert_eq!(run.retained_bytes(), 0);
                return;
            }
            Poll::Pending => {}
        }
    }
    panic!("shared LIMIT/Union failed to make progress");
}

// Mixed numeric comparisons must not round Int64 values through f64 before comparing.
#[test]
fn mixed_numeric_comparisons_preserve_large_integer_precision() {
    let input = schema(&[
        ("a", DataType::Int64, false),
        ("b", DataType::Float64, false),
    ]);
    let expr = QueryExpr::Compare {
        left: Rc::new(QueryExpr::Column(0)),
        op: CompareOpKind::Gt,
        right: Rc::new(QueryExpr::Column(1)),
    };
    let compiled = CompiledExpression::compile(&expr, &input).unwrap();
    for (a, b, expected) in [
        (9_007_199_254_740_993, 9_007_199_254_740_992.0, true),
        (i64::MAX, 9_223_372_036_854_775_808.0, false),
        (i64::MIN, f64::NEG_INFINITY, true),
    ] {
        assert!(
            matches!(compiled.evaluate(&[Value::Int64(a),Value::Float64(b)]).unwrap(), Value::Bool(v) if v == expected)
        );
    }
}

// Both expression paths must implement all nine combinations of three-valued booleans.
#[test]
fn boolean_truth_tables_agree_between_expression_paths() {
    let input = schema(&[("a", DataType::Bool, true), ("b", DataType::Bool, true)]);
    for and in [true, false] {
        for a in [None, Some(false), Some(true)] {
            for b in [None, Some(false), Some(true)] {
                let parts = vec![QueryExpr::Column(0), QueryExpr::Column(1)];
                let planner = if and {
                    QueryExpr::BoolAnd(parts)
                } else {
                    QueryExpr::BoolOr(parts)
                };
                let native = if and {
                    Expression::And(
                        Box::new(Expression::Column(0)),
                        Box::new(Expression::Column(1)),
                    )
                } else {
                    Expression::Or(
                        Box::new(Expression::Column(0)),
                        Box::new(Expression::Column(1)),
                    )
                };
                let expected = match (a, b, and) {
                    (Some(false), _, true) | (_, Some(false), true) => Some(false),
                    (Some(true), _, false) | (_, Some(true), false) => Some(true),
                    (None, _, _) | (_, None, _) => None,
                    (Some(a), Some(b), true) => Some(a && b),
                    (Some(a), Some(b), false) => Some(a || b),
                }
                .map(Value::Bool)
                .unwrap_or(Value::Null);
                let row = vec![
                    a.map(Value::Bool).unwrap_or(Value::Null),
                    b.map(Value::Bool).unwrap_or(Value::Null),
                ];
                let compiled = CompiledExpression::compile(&planner, &input).unwrap();
                assert_eq!(
                    compiled.evaluate(&row).unwrap().key().unwrap(),
                    expected.key().unwrap()
                );
                let op = Operator::project(input.clone(), vec![("result".into(), native)]).unwrap();
                let result = unary(input.clone(), vec![vec![row]], op);
                assert_eq!(result[0][0].key().unwrap(), expected.key().unwrap());
            }
        }
    }
}

// Partial/final execution must agree with one build for an uncompacted KLL population.
#[test]
fn kll_partial_merge_and_multiple_readouts_preserve_population() {
    use asap_physical_operators::Statistic;
    use planner_types::post_asap::{SketchAlgorithm, SketchKind, SketchParams};
    let input = schema(&[("v", DataType::Float64, false)]);
    let family = SummaryFamilyType::Sketch(
        SketchKind::new(SketchAlgorithm::Kll, SketchParams::Kll { k: 512 }),
        Default::default(),
    );
    let mut dag = PhysicalDag::default();
    for (id, range) in [(0, 0..64), (1, 64..128), (2, 0..128)] {
        let rows = range.map(|n| vec![Value::Float64(n as f64)]).collect();
        dag.add(
            id,
            vec![],
            Operator::source(
                input.clone(),
                vec![Batch::try_new(input.clone(), rows).unwrap()],
            )
            .unwrap(),
        )
        .unwrap();
        dag.add(
            id + 3,
            vec![id],
            Operator::summary_build(input.clone(), family.clone(), 0, None, vec![]).unwrap(),
        )
        .unwrap();
    }
    let state = Operator::summary_build(input, family, 0, None, vec![])
        .unwrap()
        .schema();
    dag.add(6, vec![3, 4], Operator::union(state.clone(), 2).unwrap())
        .unwrap();
    dag.add(
        7,
        vec![6],
        Operator::summary_merge(state.clone(), 0, vec![]).unwrap(),
    )
    .unwrap();
    let mut roots = vec![];
    for (i, q) in [0.0, 0.5, 1.0].into_iter().enumerate() {
        for (j, build) in [5, 7].into_iter().enumerate() {
            let id = 8 + (i * 2 + j) as u64;
            dag.add(
                id,
                vec![build],
                Operator::readout(
                    state.clone(),
                    0,
                    Statistic::Quantile,
                    std::collections::HashMap::from([("quantile".into(), q.to_string())]),
                )
                .unwrap(),
            )
            .unwrap();
            roots.push(id);
        }
    }
    for _ in 0..2 {
        let run = context();
        let outputs = block_on(futures::future::join_all(
            dag.execute(&roots, run.clone())
                .unwrap()
                .into_iter()
                .map(|s| s.collect::<Vec<_>>()),
        ));
        for (pair, expected) in outputs.chunks(2).zip([0., 64., 127.]) {
            let value = |batches: &[Result<
                asap_physical_operators::runtime::SharedValue<Batch>,
                asap_physical_operators::Error,
            >]| {
                assert_eq!(batches.len(), 1);
                match batches[0].as_ref().unwrap().rows()[0][0] {
                    Value::Float64(v) => v,
                    _ => panic!("quantile must be Float64"),
                }
            };
            assert_eq!(value(&pair[0]), value(&pair[1]));
            assert!((value(&pair[0]) - expected).abs() <= 1.);
        }
        drop(outputs);
        assert_eq!(run.retained_bytes(), 0);
    }
}

// Retained zero-column rows still own Vec headers and must consume the output budget.
#[test]
fn zero_column_output_obeys_memory_limit() {
    use asap_physical_operators::Error;
    let input = schema(&[]);
    let batch = Batch::try_new(input.clone(), vec![vec![]; 200]).unwrap();
    let mut dag = PhysicalDag::default();
    dag.add(0, vec![], Operator::source(input, vec![batch]).unwrap())
        .unwrap();
    let run = RunContext::new(
        Scope::Query {
            evaluation_time_ms: 0,
            revision: 0,
        },
        Limits {
            max_bytes: 1024,
            ..Limits::default()
        },
    )
    .unwrap();
    let mut stream = dag.execute(&[0], run.clone()).unwrap().remove(0);
    assert!(matches!(
        block_on(stream.next()),
        Some(Err(Error::MemoryLimit))
    ));
    drop(stream);
    assert_eq!(run.retained_bytes(), 0);
}

// Empty exact-state finalization must preserve ordinary global MIN/MAX null semantics.
#[test]
fn empty_exact_summary_extrema_agree_with_ordinary_aggregation() {
    use asap_physical_operators::Statistic;
    use planner_types::post_asap::{ExactKind, ExactParams};
    let input = schema(&[("v", DataType::Float64, false)]);
    for (kind, params, statistic) in [
        (ExactKind::Min, ExactParams::Min, Statistic::Min),
        (ExactKind::Max, ExactParams::Max, Statistic::Max),
    ] {
        let build = Operator::summary_build(
            input.clone(),
            SummaryFamilyType::ExactAggregate(kind, params),
            0,
            None,
            vec![],
        )
        .unwrap();
        let state = build.schema();
        let mut dag = PhysicalDag::default();
        dag.add(0, vec![], Operator::source(input.clone(), vec![]).unwrap())
            .unwrap();
        dag.add(1, vec![0], build).unwrap();
        dag.add(
            2,
            vec![1],
            Operator::readout(state, 0, statistic, Default::default()).unwrap(),
        )
        .unwrap();
        let rows = collect(&dag, 2);
        assert_eq!(rows.len(), 1);
        assert!(matches!(rows[0][0], Value::Null));
    }
}

//! Contract tests inspired by DataFusion's limit, sort and join test matrices.
//! Expectations follow ASAP's IR (notably row-count and IEEE NaN equality).
//! Reference: apache/datafusion e2ca7f3, physical-plan/src/{limit.rs,sorts/sort.rs}.
use asap_physical_operators::{
    expressions::CompiledExpression,
    operators::{Expression, Operator, Reduction, SortKey},
    plan::PhysicalDAG,
    runtime::{Limits, RunContext, Scope},
    values::{Batch, SchemaRef, Value},
};
use futures::{executor::block_on, StreamExt};
use planner_types::ir::physical_export::{PhysicalASAPDAGNode, PhysicalASAPOperatorPayload};
use planner_types::ir::NonASAPOp;
use planner_types::ir::Predicate;
use planner_types::ir::ScalarExpr;
use planner_types::{
    post_asap::{Field, FieldDataType},
    pre_asap::{CompareOpKind, DataType, JoinKind},
};
use std::sync::Arc;

fn schema(fields: &[(&str, DataType, bool)]) -> SchemaRef {
    Arc::new(planner_types::pre_asap::Schema {
        unique_keys: vec![],
        closed: false,
        fields: fields
            .iter()
            .map(|(name, dtype, nullable)| Field {
                table: None,
                name: (*name).into(),
                dtype: FieldDataType::Plain(dtype.clone()),
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
fn collect(dag: &PhysicalDAG<'_, Batch, SchemaRef>, root: u64) -> Vec<Vec<Value>> {
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
fn unary(input: SchemaRef, batches: Vec<Vec<Vec<Value>>>, op: Operator) -> Vec<Vec<Value>> {
    let mut dag = PhysicalDAG::default();
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
    Predicate(ScalarExpr::Compare {
        semantics: planner_types::ir::ExprSemantics::Sql,
        left: Box::new(ScalarExpr::Column(0)),
        op: CompareOpKind::Eq,
        right: Box::new(ScalarExpr::Column(1)),
    })
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
    let mut dag = PhysicalDAG::default();
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
    let expr = CompiledExpression::compile(&ScalarExpr::Column(1), &original).unwrap();
    assert!(Operator::project(current, vec![("b".into(), Expression::planner(expr))]).is_err());
}

// A valid Planner MIN/MAX schema must bind even for a non-null input column.
#[test]
fn global_extrema_bind_with_planner_derived_schema() {
    use asap_physical_operators::physical_planner::compile_node;
    use planner_types::{
        post_asap::*,
        pre_asap::{AggIntent, GroupKeys, Reduction as PlanReduction},
    };
    let input = schema(&[("v", DataType::Int64, false)]);
    for measure in [
        AggIntent::Min { col: Some(0) },
        AggIntent::Max { col: Some(0) },
    ] {
        let planner_input =
            planner_types::pre_asap::Schema::new(vec![planner_types::pre_asap::Field::plain(
                "v",
                DataType::Int64,
                false,
            )]);
        let derived = planner_types::pre_asap::aggregate_output_schema(
            &planner_input,
            &PlanReduction::Reduce(GroupKeys::by(vec![])),
            std::slice::from_ref(&measure),
            &[],
        )
        .unwrap();
        let result = derived.fields[0].clone();
        let output = schema(&[(
            &result.name,
            result.plain_dtype().unwrap().clone(),
            result.nullable,
        )]);
        let node = PhysicalASAPDAGNode {
            id: 1,
            payload: PhysicalASAPOperatorPayload::NonASAP(NonASAPOp::Aggregate {
                reduction: PlanReduction::Reduce(GroupKeys::by(vec![])),
                measures: vec![measure],
                output_names: vec![result.name],
                filters: vec![],
                having: None,
                child: 0,
            }),
            output_state: ExecutionDataState::QUERY_ROWS,
            materialization: Default::default(),
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
        let expression = ScalarExpr::Compare {
            semantics: planner_types::ir::ExprSemantics::Sql,
            left: Box::new(ScalarExpr::Column(0)),
            op: op.clone(),
            right: Box::new(ScalarExpr::Column(1)),
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
    let mut dag = PhysicalDAG::default();
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
    let expr = ScalarExpr::Compare {
        semantics: planner_types::ir::ExprSemantics::Sql,
        left: Box::new(ScalarExpr::Column(0)),
        op: CompareOpKind::Gt,
        right: Box::new(ScalarExpr::Column(1)),
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
                let parts = vec![ScalarExpr::Column(0), ScalarExpr::Column(1)];
                let planner = if and {
                    ScalarExpr::BoolAnd(parts)
                } else {
                    ScalarExpr::BoolOr(parts)
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
fn kll_partial_merge_and_multiple_evaluations_preserve_population() {
    use planner_types::post_asap::{SketchAlgorithm, SketchKind, SketchParams};

    let input = schema(&[("v", DataType::Float64, false)]);
    let family = FieldDataType::Sketch(
        SketchKind::new(
            SketchAlgorithm::Kll,
            SketchParams::Kll {
                k: 512,
                failure_probability: None,
            },
        ),
        Default::default(),
    );
    let mut dag = PhysicalDAG::default();
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
                Operator::evaluation(
                    state.clone(),
                    0,
                    asap_physical_operators::operators::SummaryEvaluation::Sketch(
                        planner_types::post_asap::SketchStatistic::Quantile { q },
                    ),
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

// KLL panes that each compact merge into a state within the calibrated
// single-build rank error: 60 panes of 1000 values at k = 200.
#[test]
fn compacted_kll_panes_merge_within_the_calibrated_rank_error() {
    use planner_types::post_asap::{SketchAlgorithm, SketchKind, SketchParams, SketchStatistic};
    const PANES: u64 = 60;
    const PER_PANE: u64 = 1000;
    const K: u32 = 200;
    // The planner's empirical 99% KLL rank error, `2.296 / k^0.9723`
    // (`asap_aware_mapping`'s `kll_rank_error_99`).
    let bound = 2.296 / f64::from(K).powf(0.9723);
    let n = (PANES * PER_PANE) as f64;
    let input = schema(&[("v", DataType::Float64, false)]);
    let family = FieldDataType::Sketch(
        SketchKind::new(
            SketchAlgorithm::Kll,
            SketchParams::Kll {
                k: K,
                failure_probability: None,
            },
        ),
        Default::default(),
    );
    let build = || Operator::summary_build(input.clone(), family.clone(), 0, None, vec![]).unwrap();
    let state = build().schema();
    let mut dag = PhysicalDAG::default();
    for pane in 0..PANES {
        // Pane `p` holds `[p, p + 1) * 1000`, as tumbling panes of a
        // rising value would, so a lost pane shifts every rank.
        let rows = (0..PER_PANE)
            .map(|j| vec![Value::Float64((pane * PER_PANE + j) as f64)])
            .collect();
        dag.add(
            pane,
            vec![],
            Operator::source(
                input.clone(),
                vec![Batch::try_new(input.clone(), rows).unwrap()],
            )
            .unwrap(),
        )
        .unwrap();
        dag.add(PANES + pane, vec![pane], build()).unwrap();
    }
    let union = 2 * PANES;
    dag.add(
        union,
        (PANES..2 * PANES).collect(),
        Operator::union(state.clone(), PANES as usize).unwrap(),
    )
    .unwrap();
    dag.add(
        union + 1,
        vec![union],
        Operator::summary_merge(state.clone(), 0, vec![]).unwrap(),
    )
    .unwrap();
    for (i, q) in [0.01, 0.1, 0.25, 0.5, 0.75, 0.9, 0.99]
        .into_iter()
        .enumerate()
    {
        let id = union + 2 + i as u64;
        dag.add(
            id,
            vec![union + 1],
            Operator::evaluation(
                state.clone(),
                0,
                asap_physical_operators::operators::SummaryEvaluation::Sketch(
                    SketchStatistic::Quantile { q },
                ),
            )
            .unwrap(),
        )
        .unwrap();
        let Value::Float64(estimate) = collect(&dag, id)[0][0] else {
            panic!("quantile must be Float64");
        };
        // Value `v` has rank `v / n`.
        let error = (estimate / n - q).abs();
        assert!(error <= bound, "q={q}: rank error {error} > {bound}");
    }
}

// Counter panes merged in any input order (here A, C, B) give the increase
// of one build over the whole window, including the jumps between panes
// and a counter reset inside B.
#[test]
fn counter_panes_merge_in_any_order() {
    use asap_physical_operators::summary_kernels::exact::ExactAccumulator;
    use planner_types::post_asap::{ExactKind, ExactParams};
    let input = schema(&[
        ("v", DataType::Float64, false),
        ("t", DataType::Timestamp, false),
    ]);
    let family = FieldDataType::ExactAggregate(ExactKind::Increase, ExactParams::Increase);
    let build =
        || Operator::summary_build(input.clone(), family.clone(), 0, Some(1), vec![]).unwrap();
    let state = build().schema();
    let samples = |range: std::ops::Range<i64>| -> Vec<Vec<Value>> {
        range
            .map(|i| {
                // Rises by 3 per sample and resets once, at sample 7.
                let value = if i < 7 { 3 * i } else { 3 * (i - 7) + 1 };
                vec![Value::Float64(value as f64), Value::Timestamp(i * 1_000)]
            })
            .collect()
    };
    let increase = |dag: &PhysicalDAG<'_, Batch, SchemaRef>, root: u64| -> f64 {
        let rows = collect(dag, root);
        let Value::Summary { state, .. } = &rows[0][0] else {
            panic!("summary state expected");
        };
        state
            .as_any()
            .downcast_ref::<ExactAccumulator>()
            .unwrap()
            .evaluation(asap_physical_operators::Statistic::Increase, None, None)
            .unwrap()
            .unwrap()
    };
    let mut dag = PhysicalDAG::default();
    // Panes A = [0, 4), B = [4, 8), C = [8, 12), added as A, C, B.
    for (id, range) in [(0, 0..4), (1, 8..12), (2, 4..8), (3, 0..12)] {
        let batch = Batch::try_new(input.clone(), samples(range)).unwrap();
        dag.add(
            id,
            vec![],
            Operator::source(input.clone(), vec![batch]).unwrap(),
        )
        .unwrap();
        dag.add(id + 4, vec![id], build()).unwrap();
    }
    dag.add(8, vec![4, 5, 6], Operator::union(state.clone(), 3).unwrap())
        .unwrap();
    dag.add(
        9,
        vec![8],
        Operator::summary_merge(state, 0, vec![]).unwrap(),
    )
    .unwrap();
    assert_eq!(increase(&dag, 9), increase(&dag, 7));
}

// Retained zero-column rows still own Vec headers and must consume the output budget.
#[test]
fn zero_column_output_obeys_memory_limit() {
    use asap_physical_operators::Error;
    let input = schema(&[]);
    let batch = Batch::try_new(input.clone(), vec![vec![]; 200]).unwrap();
    let mut dag = PhysicalDAG::default();
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
            FieldDataType::ExactAggregate(kind, params),
            0,
            None,
            vec![],
        )
        .unwrap();
        let state = build.schema();
        let mut dag = PhysicalDAG::default();
        dag.add(0, vec![], Operator::source(input.clone(), vec![]).unwrap())
            .unwrap();
        dag.add(1, vec![0], build).unwrap();
        dag.add(
            2,
            vec![1],
            Operator::evaluation(
                state,
                0,
                asap_physical_operators::operators::SummaryEvaluation::Exact(
                    asap_physical_operators::summary_kernels::exact::ExactEvaluation {
                        statistic,
                        lookback_ms: None,
                    },
                ),
            )
            .unwrap(),
        )
        .unwrap();
        let rows = collect(&dag, 2);
        assert_eq!(rows.len(), 1);
        assert!(matches!(rows[0][0], Value::Null));
    }
}

/// Rows of `g`, `x` and a nullable Boolean `keep`; `b` has no kept row,
/// and a NULL `keep` drops its row as SQL FILTER does.
fn filtered_input() -> (SchemaRef, Vec<Vec<Value>>) {
    let input = schema(&[
        ("g", DataType::Utf8, false),
        ("x", DataType::Float64, false),
        ("keep", DataType::Bool, true),
    ]);
    let rows = [
        ("a", 1.0, Some(true)),
        ("a", 2.0, None),
        ("b", 3.0, Some(false)),
        ("c", 4.0, Some(true)),
    ]
    .into_iter()
    .map(|(g, x, keep)| {
        vec![
            Value::Utf8(g.into()),
            Value::Float64(x),
            keep.map_or(Value::Null, Value::Bool),
        ]
    })
    .collect();
    (input, rows)
}

fn printed(rows: &[Vec<Value>]) -> Vec<String> {
    let mut rows: Vec<_> = rows
        .iter()
        .map(|row| {
            row.iter()
                .map(|v| match v {
                    Value::Utf8(s) => s.to_string(),
                    Value::Int64(n) => n.to_string(),
                    Value::Float64(x) => format!("{x:?}"),
                    Value::Null => "NULL".into(),
                    other => panic!("unexpected value {other:?}"),
                })
                .collect::<Vec<_>>()
                .join(" ")
        })
        .collect();
    rows.sort();
    rows
}

fn keep() -> Option<Predicate<usize>> {
    Some(Predicate(ScalarExpr::Column(2)))
}

fn planner_node(
    payload: PhysicalASAPOperatorPayload,
    fields: &[(&str, FieldDataType, bool)],
) -> PhysicalASAPDAGNode {
    PhysicalASAPDAGNode {
        id: 1,
        payload,
        output_state: planner_types::post_asap::ExecutionDataState::QUERY_ROWS,
        materialization: Default::default(),
        output_schema: planner_types::pre_asap::Schema::new(
            fields
                .iter()
                .map(|(name, dtype, nullable)| Field {
                    table: None,
                    name: (*name).into(),
                    dtype: dtype.clone(),
                    nullable: *nullable,
                })
                .collect(),
        ),
        guarantee: None,
    }
}

fn round_trip(operator: Operator) -> Operator {
    serde_json::from_slice(&serde_json::to_vec(&operator).unwrap()).unwrap()
}

/// Per-measure filters keep every group: a filtered COUNT reads 0 and a
/// filtered SUM reads NULL for a group with no kept row, and an unfiltered
/// measure beside them still sees every row. The filters survive
/// serialization.
#[test]
fn measure_filters_keep_groups_without_matching_rows() {
    use asap_physical_operators::physical_planner::compile_node;
    use planner_types::pre_asap::{AggIntent, GroupKeys, Reduction as Plan};
    use planner_types::types::AccuracyTarget;
    let (input, rows) = filtered_input();
    let plain = |t: DataType| FieldDataType::Plain(t);
    let node = planner_node(
        PhysicalASAPOperatorPayload::NonASAP(NonASAPOp::Aggregate {
            reduction: Plan::Reduce(GroupKeys::by(vec![0])),
            measures: vec![
                AggIntent::Count {
                    accuracy: AccuracyTarget::Exact,
                },
                AggIntent::Sum { col: Some(1) },
                AggIntent::Count {
                    accuracy: AccuracyTarget::Exact,
                },
            ],
            output_names: vec!["c".into(), "s".into(), "all".into()],
            filters: vec![keep(), keep(), None],
            having: None,
            child: 0,
        }),
        &[
            ("g", plain(DataType::Utf8), false),
            ("c", plain(DataType::Int64), false),
            ("s", plain(DataType::Float64), true),
            ("all", plain(DataType::Int64), false),
        ],
    );
    let operator = round_trip(compile_node(&node, std::slice::from_ref(&input)).unwrap());
    assert_eq!(
        printed(&unary(input, vec![rows], operator)),
        ["a 1 1.0 2", "b 0 NULL 1", "c 1 4.0 1"]
    );
}

/// A filtered SUM is NULL for a group without a kept row, so a plan that
/// declares it non-null does not bind.
#[test]
fn filtered_sum_declared_non_null_is_rejected() {
    use asap_physical_operators::physical_planner::compile_node;
    use planner_types::pre_asap::{AggIntent, GroupKeys, Reduction as Plan};
    let (input, _) = filtered_input();
    let node = planner_node(
        PhysicalASAPOperatorPayload::NonASAP(NonASAPOp::Aggregate {
            reduction: Plan::Reduce(GroupKeys::by(vec![0])),
            measures: vec![AggIntent::Sum { col: Some(1) }],
            output_names: vec!["s".into()],
            filters: vec![keep()],
            having: None,
            child: 0,
        }),
        &[
            ("g", FieldDataType::Plain(DataType::Utf8), false),
            ("s", FieldDataType::Plain(DataType::Float64), false),
        ],
    );
    assert!(compile_node(&node, &[input]).is_err());
}

/// A filtered `SummaryAgg` updates its state only from kept rows but keeps
/// a state for every group: an exact count reads 0 and a KLL quantile,
/// declared nullable, reads NULL for the group with no kept row. The
/// filter survives serialization.
#[test]
fn filtered_summary_build_keeps_empty_groups() {
    use asap_physical_operators::physical_planner::compile_node;
    use planner_types::ir::ASAPOp;
    use planner_types::post_asap::{
        ExactKind, ExactParams, SketchAlgorithm, SketchKind, SketchParams, SketchStatistic,
        SummaryUpdate,
    };
    use planner_types::pre_asap::{ColumnRef, GroupKeys, Reduction as Plan};
    let (input, rows) = filtered_input();
    let kll = FieldDataType::Sketch(
        SketchKind::new(
            SketchAlgorithm::Kll,
            SketchParams::Kll {
                k: 200,
                failure_probability: None,
            },
        ),
        Default::default(),
    );
    let count = FieldDataType::ExactAggregate(ExactKind::Count, ExactParams::Count);
    let cases = [
        (
            kll,
            ASAPOp::SummaryEstimate {
                summary_input: 0,
                query: SketchStatistic::Quantile { q: 0.5 },
            },
            (DataType::Float64, true),
            ["a 1.0", "b NULL", "c 4.0"],
        ),
        (
            count,
            ASAPOp::FinalizeExactAccumulator { child: 0 },
            (DataType::Int64, false),
            ["a 1", "b 0", "c 1"],
        ),
    ];
    for (family, read, (result, nullable), expected) in cases {
        let build = planner_node(
            PhysicalASAPOperatorPayload::ASAP(ASAPOp::SummaryAgg {
                child: 0,
                family: family.clone(),
                input: SummaryUpdate::column(ColumnRef::Named("x".into())),
                reduction: Plan::Reduce(GroupKeys::by(vec![0])),
                grouping: Default::default(),
                filter: keep(),
            }),
            &[
                ("g", FieldDataType::Plain(DataType::Utf8), false),
                ("state", family, false),
            ],
        );
        let build = round_trip(compile_node(&build, std::slice::from_ref(&input)).unwrap());
        let read = planner_node(
            PhysicalASAPOperatorPayload::ASAP(read),
            &[
                ("g", FieldDataType::Plain(DataType::Utf8), false),
                ("value", FieldDataType::Plain(result), nullable),
            ],
        );
        let read = compile_node(&read, &[build.schema()]).unwrap();
        let mut dag = PhysicalDAG::default();
        dag.add(
            0,
            vec![],
            Operator::source(
                input.clone(),
                vec![Batch::try_new(input.clone(), rows.clone()).unwrap()],
            )
            .unwrap(),
        )
        .unwrap();
        dag.add(1, vec![0], build).unwrap();
        dag.add(2, vec![1], read).unwrap();
        assert_eq!(printed(&collect(&dag, 2)), expected);
    }
}

/// A filtered `SummaryAgg` timed at ingestion does not bind: only a
/// query-time build applies its filter. The same build unfiltered binds.
#[test]
fn filtered_summary_build_is_rejected_at_ingestion_time() {
    use asap_physical_operators::physical_planner::compile_node;
    use planner_types::ir::ASAPOp;
    use planner_types::post_asap::{ExactKind, ExactParams, ExecutionDataState, SummaryUpdate};
    use planner_types::pre_asap::{ColumnRef, GroupKeys, Reduction as Plan};
    let (input, _) = filtered_input();
    let family = FieldDataType::ExactAggregate(ExactKind::Count, ExactParams::Count);
    let build = |filter| {
        let mut node = planner_node(
            PhysicalASAPOperatorPayload::ASAP(ASAPOp::SummaryAgg {
                child: 0,
                family: family.clone(),
                input: SummaryUpdate::column(ColumnRef::Named("x".into())),
                reduction: Plan::Reduce(GroupKeys::by(vec![0])),
                grouping: Default::default(),
                filter,
            }),
            &[
                ("g", FieldDataType::Plain(DataType::Utf8), false),
                ("state", family.clone(), false),
            ],
        );
        node.output_state = ExecutionDataState::INGESTION_SUMMARY;
        compile_node(&node, std::slice::from_ref(&input))
    };
    assert!(build(None).is_ok());
    let error = build(keep()).err().expect("filtered ingestion-time build");
    assert!(error.to_string().contains("only at query time"), "{error}");
}

/// PromQL never emits per-measure filters, and over PromQL series identity a
/// filtered measure's empty group would be a NULL its non-nullable output
/// cannot hold, so the filter is rejected there.
#[test]
fn measure_filters_are_rejected_over_promql_series_identity() {
    let mut input = (*schema(&[
        (
            planner_types::pre_asap::schema::PROMQL_SERIES_IDENTITY,
            DataType::Utf8,
            false,
        ),
        ("x", DataType::Float64, false),
        ("keep", DataType::Bool, true),
    ]))
    .clone();
    input.closed = true;
    let input = Arc::new(input);
    assert!(input.has_promql_series_identity());
    let aggregate = Operator::aggregate(
        input.clone(),
        vec![0],
        vec![("m".into(), Reduction::Min(1))],
    )
    .unwrap();
    let error = aggregate
        .with_measure_filters(vec![Some(Expression::Column(2))])
        .err()
        .expect("filtered measure over PromQL series identity");
    assert!(error.to_string().contains("PromQL"), "{error}");
}

/// An exact aggregate node over `input` with one `measure` named `name`,
/// whose output is one non-null `dtype` column.
fn exact_aggregate_node(
    measure: planner_types::pre_asap::AggIntent,
    name: &str,
    dtype: DataType,
) -> PhysicalASAPDAGNode {
    use planner_types::{
        post_asap::ExecutionDataState,
        pre_asap::{GroupKeys, Reduction as PlanReduction},
    };
    PhysicalASAPDAGNode {
        id: 1,
        payload: PhysicalASAPOperatorPayload::NonASAP(NonASAPOp::Aggregate {
            reduction: PlanReduction::Reduce(GroupKeys::by(vec![])),
            measures: vec![measure],
            output_names: vec![name.into()],
            filters: vec![],
            having: None,
            child: 0,
        }),
        output_state: ExecutionDataState::QUERY_ROWS,
        materialization: Default::default(),
        output_schema: (*schema(&[(name, dtype, false)])).clone(),
        guarantee: None,
    }
}

// Exact frequency intents bind to native reducers over typed keys, without a
// sketch or numeric key conversion; empty and all-NULL input give 0.
#[test]
fn exact_frequency_intents_execute_typed_keys_and_empty_input() {
    use asap_physical_operators::physical_planner::compile_node;
    use planner_types::pre_asap::AggIntent;
    use planner_types::types::AccuracyTarget;
    for (dtype, values) in [
        (
            DataType::Utf8,
            vec![Value::Utf8("a".into()), Value::Utf8("b".into())],
        ),
        (
            DataType::Int64,
            vec![
                Value::Int64(9_007_199_254_740_992),
                Value::Int64(9_007_199_254_740_993),
            ],
        ),
        (DataType::Bool, vec![Value::Bool(false), Value::Bool(true)]),
        (
            DataType::Float64,
            vec![Value::Float64(-0.0), Value::Float64(1.0)],
        ),
    ] {
        let input = schema(&[("key", dtype, true)]);
        for (measure, name, expected) in [
            (
                AggIntent::FrequencyL2 {
                    col: Some(0),
                    accuracy: AccuracyTarget::Exact,
                },
                "frequency_l2",
                8.0_f64.sqrt(),
            ),
            (
                AggIntent::FrequencyEntropy {
                    col: Some(0),
                    accuracy: AccuracyTarget::Exact,
                },
                "frequency_entropy",
                1.0,
            ),
        ] {
            let node = exact_aggregate_node(measure, name, DataType::Float64);
            let operator = compile_node(&node, std::slice::from_ref(&input))
                .expect("exact frequency intent binds");
            let rows = values
                .iter()
                .flat_map(|v| [vec![v.clone()], vec![v.clone()]])
                .chain([vec![Value::Null]])
                .collect();
            let result = unary(input.clone(), vec![rows], operator.clone());
            assert!(
                matches!(result[0][0], Value::Float64(v) if (v - expected).abs() < 1e-12),
                "{name}: {result:?}"
            );
            for batches in [vec![], vec![vec![vec![Value::Null]]]] {
                let result = unary(input.clone(), batches, operator.clone());
                assert!(matches!(result[0][0], Value::Float64(v) if v == 0.0));
            }
        }
    }
}

// Each group gets its own frequency population, with one canonical
// signed-zero identity; entropy is in bits.
#[test]
fn exact_frequency_grouping_and_entropy_bits() {
    let input = schema(&[
        ("group", DataType::Int64, false),
        ("key", DataType::Float64, true),
    ]);
    let operator = Operator::aggregate(
        input.clone(),
        vec![0],
        vec![
            ("l2".into(), Reduction::FrequencyL2(1)),
            ("entropy".into(), Reduction::FrequencyEntropy(1)),
        ],
    )
    .unwrap();
    let rows = vec![
        vec![Value::Int64(1), Value::Float64(-0.0)],
        vec![Value::Int64(1), Value::Float64(0.0)],
        vec![Value::Int64(1), Value::Float64(0.0)],
        vec![Value::Int64(1), Value::Float64(1.0)],
        vec![Value::Int64(2), Value::Float64(2.0)],
        vec![Value::Int64(2), Value::Null],
        vec![Value::Int64(3), Value::Null],
    ];
    let result = unary(input.clone(), vec![rows], operator.clone());
    assert_eq!(result.len(), 3);
    let expected_entropy = -0.75_f64 * 0.75_f64.log2() - 0.25_f64 * 0.25_f64.log2();
    for (row, l2, entropy) in [
        (&result[0], 10.0_f64.sqrt(), expected_entropy),
        (&result[1], 1.0, 0.0),
        (&result[2], 0.0, 0.0),
    ] {
        assert!(matches!(row[1], Value::Float64(v) if (v - l2).abs() < 1e-12));
        assert!(matches!(row[2], Value::Float64(v) if (v - entropy).abs() < 1e-12));
    }
    assert!(unary(input, vec![], operator).is_empty());
}

// A non-finite floating identity is an error, not a frequency key.
#[test]
fn exact_frequency_rejects_non_finite_identities() {
    let input = schema(&[("key", DataType::Float64, false)]);
    let operator = Operator::aggregate(
        input.clone(),
        vec![],
        vec![("l2".into(), Reduction::FrequencyL2(0))],
    )
    .unwrap();
    let mut dag = PhysicalDAG::default();
    dag.add(
        0,
        vec![],
        Operator::source(
            input.clone(),
            vec![Batch::try_new(input, vec![vec![Value::Float64(f64::INFINITY)]]).unwrap()],
        )
        .unwrap(),
    )
    .unwrap();
    dag.add(1, vec![0], operator).unwrap();
    let run = context();
    let result = block_on(async {
        let mut stream = dag.execute(&[1], run.clone()).unwrap().remove(0);
        stream.next().await.unwrap()
    });
    assert!(result.is_err());
}

// Exact distinct binding keeps typed tuples, skips any tuple with a NULL
// and returns zero on no input.
#[test]
fn exact_cardinality_binds_and_executes_typed_tuples() {
    use asap_physical_operators::physical_planner::compile_node;
    use planner_types::pre_asap::AggIntent;
    use planner_types::types::AccuracyTarget;
    let input = schema(&[
        ("key", DataType::Int64, true),
        ("tag", DataType::Utf8, true),
    ]);
    for (cols, expected) in [(vec![0], 2), (vec![0, 1], 3)] {
        let node = exact_aggregate_node(
            AggIntent::Cardinality {
                cols,
                accuracy: AccuracyTarget::Exact,
            },
            "distinct",
            DataType::Int64,
        );
        let operator =
            compile_node(&node, std::slice::from_ref(&input)).expect("exact distinct intent binds");
        let rows = vec![
            vec![Value::Int64(9_007_199_254_740_992), Value::Utf8("a".into())],
            vec![Value::Int64(9_007_199_254_740_992), Value::Utf8("a".into())],
            vec![Value::Int64(9_007_199_254_740_992), Value::Utf8("b".into())],
            vec![Value::Int64(9_007_199_254_740_993), Value::Utf8("a".into())],
            vec![Value::Null, Value::Utf8("c".into())],
        ];
        let result = unary(input.clone(), vec![rows], operator.clone());
        assert!(matches!(result[0][0], Value::Int64(v) if v == expected));
        for rows in [vec![], vec![vec![Value::Null, Value::Null]]] {
            let result = unary(input.clone(), vec![rows], operator.clone());
            assert!(matches!(result[0][0], Value::Int64(0)));
        }
    }
}

// Distinct uses grouped equality: signed zero and NaN payloads each form one
// identity.
#[test]
fn exact_cardinality_grouping_normalizes_float_identities() {
    let input = schema(&[
        ("group", DataType::Int64, false),
        ("key", DataType::Float64, true),
    ]);
    let operator = Operator::aggregate(
        input.clone(),
        vec![0],
        vec![("distinct".into(), Reduction::Cardinality(vec![1]))],
    )
    .unwrap();
    let result = unary(
        input,
        vec![vec![
            vec![Value::Int64(1), Value::Float64(0.0)],
            vec![Value::Int64(1), Value::Float64(-0.0)],
            vec![Value::Int64(1), Value::Float64(f64::NAN)],
            vec![
                Value::Int64(1),
                Value::Float64(f64::from_bits(f64::NAN.to_bits() + 1)),
            ],
            vec![Value::Int64(2), Value::Null],
        ]],
        operator,
    );
    assert!(matches!(
        result[0].as_slice(),
        [Value::Int64(1), Value::Int64(2)]
    ));
    assert!(matches!(
        result[1].as_slice(),
        [Value::Int64(2), Value::Int64(0)]
    ));
}

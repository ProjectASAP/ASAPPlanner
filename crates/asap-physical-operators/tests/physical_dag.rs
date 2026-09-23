//! Acceptance tests use the library directly, without either backend engine.
use asap_physical_operators::{
    dag::{
        operators::{Expression, Operator, Reduction, SortKey},
        values::{Batch, Schema, Value},
        Limits, PhysicalDag, RunContext, Scope,
    },
    Statistic,
};
use futures::{executor::block_on, StreamExt};
use planner_types::{
    post_asap::{ExactKind, ExactParams, SummaryFamilyType, SummaryField, SummarySchema},
    pre_asap::DataType,
};
use std::sync::Arc;
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
fn run(dag: &PhysicalDag<'_, Batch, Schema>, root: u64, scope: Scope) -> Vec<Vec<Value>> {
    let context = RunContext::new(
        scope,
        Limits {
            max_buffered_batches: 1,
            ..Limits::default()
        },
    )
    .unwrap();
    block_on(async {
        let mut stream = dag.execute(&[root], context.clone()).unwrap().remove(0);
        let mut rows = vec![];
        while let Some(batch) = stream.next().await {
            rows.extend(batch.unwrap().rows().iter().cloned());
        }
        assert_eq!(context.retained_bytes(), 0);
        rows
    })
}
fn query() -> Scope {
    Scope::Query {
        evaluation_time_ms: 1000,
        revision: 2,
    }
}
fn floats(rows: &[Vec<Value>], column: usize) -> Vec<f64> {
    rows.iter()
        .map(|r| {
            if let Value::Float64(v) = r[column] {
                v
            } else {
                panic!("not Float64")
            }
        })
        .collect()
}

// Sort followed by partitioned Limit implements ranking independently per group.
#[test]
fn grouped_sort_limit_across_batches() {
    let schema = schema(&[
        ("group", DataType::Int64, false),
        ("score", DataType::Float64, false),
    ]);
    let batches = [
        vec![(1, 1.), (2, 4.), (1, 9.)],
        vec![(2, 8.), (1, 5.), (2, 2.)],
    ]
    .into_iter()
    .map(|rows| {
        Batch::try_new(
            schema.clone(),
            rows.into_iter()
                .map(|(g, v)| vec![Value::Int64(g), Value::Float64(v)])
                .collect(),
        )
        .unwrap()
    })
    .collect();
    let mut dag = PhysicalDag::default();
    dag.add(
        0,
        vec![],
        Operator::source(schema.clone(), batches).unwrap(),
    )
    .unwrap();
    dag.add(
        1,
        vec![0],
        Operator::sort(
            schema.clone(),
            vec![SortKey {
                column: 1,
                descending: true,
                nulls_first: false,
            }],
            vec![0],
        )
        .unwrap(),
    )
    .unwrap();
    dag.add(2, vec![1], Operator::limit(schema, 1, 1, vec![0]).unwrap())
        .unwrap();
    assert_eq!(floats(&run(&dag, 2, query()), 1), vec![5., 4.]);
}

// The same computation runs in either engine scope with fresh per-run state.
#[test]
fn summary_construction_merge_and_readout_at_both_phases() {
    let schema = schema(&[("v", DataType::Float64, false)]);
    let batches = (1..=20)
        .map(|v| Batch::try_new(schema.clone(), vec![vec![Value::Float64(v as f64)]]).unwrap())
        .collect();
    let family = SummaryFamilyType::ExactAggregate(ExactKind::Sum, ExactParams::Sum);
    let build = Operator::summary_build(schema.clone(), family, 0, None, vec![]).unwrap();
    let state = build.schema();
    let mut dag = PhysicalDag::default();
    dag.add(0, vec![], Operator::source(schema, batches).unwrap())
        .unwrap();
    dag.add(1, vec![0], build).unwrap();
    dag.add(2, vec![1, 1], Operator::union(state.clone(), 2).unwrap())
        .unwrap();
    dag.add(
        3,
        vec![2],
        Operator::summary_merge(state.clone(), 0, vec![]).unwrap(),
    )
    .unwrap();
    dag.add(
        4,
        vec![3],
        Operator::readout(state, 0, Statistic::Sum, Default::default()).unwrap(),
    )
    .unwrap();
    for scope in [
        query(),
        Scope::Ingestion {
            window_start_ms: 0,
            window_end_ms: 1000,
            revision: 2,
        },
    ] {
        assert_eq!(floats(&run(&dag, 4, scope), 0), vec![420.]);
    }
}

// A semi-join can consume two branches of one producer with a one-batch buffer.
#[test]
fn diamond_semijoin_preserves_left_values_and_multiplicity() {
    let schema = schema(&[("key", DataType::Int64, false)]);
    let batches = [1, 2, 2, 3]
        .into_iter()
        .map(|v| Batch::try_new(schema.clone(), vec![vec![Value::Int64(v)]]).unwrap())
        .collect();
    let filter = Operator::filter(
        schema.clone(),
        Expression::Equal(
            Box::new(Expression::Column(0)),
            Box::new(Expression::Literal {
                value: Value::Int64(2),
                dtype: DataType::Int64,
            }),
        ),
    )
    .unwrap();
    let mut dag = PhysicalDag::default();
    dag.add(
        0,
        vec![],
        Operator::source(schema.clone(), batches).unwrap(),
    )
    .unwrap();
    dag.add(1, vec![0], filter).unwrap();
    dag.add(
        2,
        vec![0, 1],
        Operator::semi_join(schema.clone(), schema, vec![(0, 0)]).unwrap(),
    )
    .unwrap();
    let rows = run(&dag, 2, query());
    assert_eq!(rows.len(), 2);
    assert!(rows.iter().all(|r| matches!(r[0], Value::Int64(2))));
}

// Integer aggregation must not silently lose precision through Float64.
#[test]
fn exact_integer_and_empty_extrema() {
    let schema = schema(&[("v", DataType::Int64, false)]);
    let aggregate = Operator::aggregate(
        schema.clone(),
        vec![],
        vec![("sum".into(), Reduction::Sum(0))],
    )
    .unwrap();
    let mut dag = PhysicalDag::default();
    let value = 9_007_199_254_740_993;
    dag.add(
        0,
        vec![],
        Operator::source(
            schema.clone(),
            vec![Batch::try_new(
                schema.clone(),
                vec![vec![Value::Int64(value)], vec![Value::Int64(2)]],
            )
            .unwrap()],
        )
        .unwrap(),
    )
    .unwrap();
    dag.add(1, vec![0], aggregate).unwrap();
    assert!(matches!(run(&dag,1,query())[0][0],Value::Int64(v) if v==value+2));
    let mut empty = PhysicalDag::default();
    empty
        .add(0, vec![], Operator::source(schema.clone(), vec![]).unwrap())
        .unwrap();
    empty
        .add(
            1,
            vec![0],
            Operator::aggregate(schema, vec![], vec![("min".into(), Reduction::Min(0))]).unwrap(),
        )
        .unwrap();
    assert!(matches!(run(&empty, 1, query())[0][0], Value::Null));
}

// Plain value operators are library implementations, including NaN comparison.
#[test]
fn scalar_negation_and_vector_conversion() {
    let scalar = Operator::scalar(Value::Float64(7.), DataType::Float64).unwrap();
    let project = Operator::project(
        scalar.schema(),
        vec![(
            "v".into(),
            Expression::Negate(Box::new(Expression::Column(0))),
        )],
    )
    .unwrap();
    let convert = Operator::vector_to_scalar(project.schema(), 0).unwrap();
    let mut dag = PhysicalDag::default();
    dag.add(0, vec![], scalar).unwrap();
    dag.add(1, vec![0], project).unwrap();
    dag.add(2, vec![1], convert).unwrap();
    assert_eq!(floats(&run(&dag, 2, query()), 0), vec![-7.]);
    let scalar = Operator::scalar(Value::Float64(f64::NAN), DataType::Float64).unwrap();
    let predicate = Expression::Equal(
        Box::new(Expression::Column(0)),
        Box::new(Expression::Column(0)),
    );
    let filter = Operator::filter(scalar.schema(), predicate).unwrap();
    let mut dag = PhysicalDag::default();
    dag.add(0, vec![], scalar).unwrap();
    dag.add(1, vec![0], filter).unwrap();
    assert!(run(&dag, 1, query()).is_empty());
}

// Invalid operations fail at binding rather than becoming external fallbacks.
#[test]
fn binding_rejects_unsupported_operations() {
    let schema = schema(&[("v", DataType::Float64, false)]);
    assert!(Operator::summary_build(
        schema.clone(),
        SummaryFamilyType::ExactAggregate(ExactKind::Rate, ExactParams::Rate),
        0,
        None,
        vec![]
    )
    .is_err());
    let sum = Operator::summary_build(
        schema.clone(),
        SummaryFamilyType::ExactAggregate(ExactKind::Sum, ExactParams::Sum),
        0,
        None,
        vec![],
    )
    .unwrap();
    assert!(Operator::readout(sum.schema(), 0, Statistic::Quantile, Default::default()).is_err());
    assert!(Operator::filter(schema, Expression::Column(0)).is_err());
}

// KLL is one family example: precomputation changes input sources, not operators.
#[test]
fn kll_raw_partial_and_precomputed_are_native_dags() {
    use planner_types::post_asap::{GroupingStrategy, SketchAlgorithm, SketchKind, SketchParams};
    let input = schema(&[("value", DataType::Float64, false)]);
    let family = SummaryFamilyType::Sketch(
        SketchKind::new(SketchAlgorithm::Kll, SketchParams::Kll { k: 512 }),
        GroupingStrategy::PerSubpopulationInstance,
    );
    let build = Operator::summary_build(input.clone(), family, 0, None, vec![]).unwrap();
    let state = build.schema();
    let build_range = |start: u32, end: u32| {
        let mut dag = PhysicalDag::default();
        let batch = Batch::try_new(
            input.clone(),
            (start..end)
                .map(|v| vec![Value::Float64(f64::from(v))])
                .collect(),
        )
        .unwrap();
        dag.add(
            0,
            vec![],
            Operator::source(input.clone(), vec![batch]).unwrap(),
        )
        .unwrap();
        dag.add(1, vec![0], build.clone()).unwrap();
        run(
            &dag,
            1,
            Scope::Ingestion {
                window_start_ms: 0,
                window_end_ms: 1000,
                revision: 1,
            },
        )
    };
    let prefix = build_range(0, 64);
    let complete = build_range(0, 128);
    let query_plan = |stored: Option<Vec<Vec<Value>>>, raw_start: Option<u32>| {
        let mut dag = PhysicalDag::default();
        let mut states = vec![];
        if let Some(rows) = stored {
            dag.add(
                0,
                vec![],
                Operator::source(
                    state.clone(),
                    vec![Batch::try_new(state.clone(), rows).unwrap()],
                )
                .unwrap(),
            )
            .unwrap();
            states.push(0);
        }
        if let Some(start) = raw_start {
            dag.add(
                1,
                vec![],
                Operator::source(
                    input.clone(),
                    vec![Batch::try_new(
                        input.clone(),
                        (start..128)
                            .map(|v| vec![Value::Float64(f64::from(v))])
                            .collect(),
                    )
                    .unwrap()],
                )
                .unwrap(),
            )
            .unwrap();
            dag.add(2, vec![1], build.clone()).unwrap();
            states.push(2);
        }
        dag.add(
            3,
            states.clone(),
            Operator::union(state.clone(), states.len()).unwrap(),
        )
        .unwrap();
        dag.add(
            4,
            vec![3],
            Operator::summary_merge(state.clone(), 0, vec![]).unwrap(),
        )
        .unwrap();
        dag.add(
            5,
            vec![4],
            Operator::readout(
                state.clone(),
                0,
                Statistic::Quantile,
                std::collections::HashMap::from([("quantile".into(), "0.5".into())]),
            )
            .unwrap(),
        )
        .unwrap();
        floats(&run(&dag, 5, query()), 0)[0]
    };
    let raw = query_plan(None, Some(0));
    let partial = query_plan(Some(prefix), Some(64));
    let full = query_plan(Some(complete), None);
    assert_eq!(raw, partial);
    assert_eq!(partial, full);
    assert!((raw - 64.).abs() <= 1.);
}

// Restored state must retain its family; a mislabeled state is rejected.
#[test]
fn restored_exact_state_and_family_validation() {
    use asap_physical_operators::{
        accumulators::exact_accumulator::ExactAccumulator, SerializableToSink,
    };
    let family = SummaryFamilyType::ExactAggregate(ExactKind::Sum, ExactParams::Sum);
    let mut acc = ExactAccumulator::new(family.clone(), false).unwrap();
    acc.update(None, 7., 0);
    let acc = ExactAccumulator::deserialize_from_bytes(&acc.serialize_to_bytes()).unwrap();
    let schema = Arc::new(SummarySchema {
        fields: vec![SummaryField {
            name: "state".into(),
            dtype: family.clone(),
            nullable: false,
        }],
        time_index: None,
    });
    let value = Value::Summary {
        family: family.clone(),
        state: Arc::new(acc),
    };
    let mut dag = PhysicalDag::default();
    dag.add(
        0,
        vec![],
        Operator::source(
            schema.clone(),
            vec![Batch::try_new(schema.clone(), vec![vec![value]]).unwrap()],
        )
        .unwrap(),
    )
    .unwrap();
    dag.add(
        1,
        vec![0],
        Operator::readout(schema.clone(), 0, Statistic::Sum, Default::default()).unwrap(),
    )
    .unwrap();
    assert_eq!(floats(&run(&dag, 1, query()), 0), vec![7.]);
    let wrong = ExactAccumulator::new(
        SummaryFamilyType::ExactAggregate(ExactKind::Max, ExactParams::Max),
        false,
    )
    .unwrap();
    assert!(Batch::try_new(
        schema,
        vec![vec![Value::Summary {
            family,
            state: Arc::new(wrong)
        }]]
    )
    .is_err());
}

// Planner binding rejects unknown computation instead of accepting a fallback.
#[test]
fn bind_post_asap_before_execution() {
    use asap_physical_operators::dag::planner::bind;
    use planner_types::{
        post_asap::{
            EdgeRole, ExecutableDag, ExecutableDagEdge, ExecutableDagNode,
            ExecutableOperatorPayload, ExecutionDataState, GroupingEdgeCompatibility,
            PostAsapNodeId, ValueOperation, WindowEdgeCompatibility,
        },
        pre_asap::{ArithmeticOpKind, ProjectItem, QueryExpr, ScalarValue},
    };
    use std::{collections::BTreeMap, rc::Rc};
    let schema = schema(&[("value", DataType::Float64, false)]);
    let node = |id, payload| ExecutableDagNode {
        id: PostAsapNodeId(id),
        payload,
        output_state: ExecutionDataState::QUERY_ROWS,
        output_schema: (*schema).clone(),
        guarantee: None,
    };
    let mut dag = ExecutableDag {
        nodes: vec![
            node(
                0,
                ExecutableOperatorPayload::Fallback {
                    expression: QueryExpr::promql_scalar(1.),
                },
            ),
            node(
                1,
                ExecutableOperatorPayload::Value {
                    operation: ValueOperation::Project {
                        cols: vec![ProjectItem {
                            alias: None,
                            expr: QueryExpr::Arithmetic {
                                op: ArithmeticOpKind::Add,
                                left: Rc::new(QueryExpr::Column(0)),
                                right: Rc::new(QueryExpr::Literal(ScalarValue::Float64(2.))),
                            },
                        }],
                        qualifier: None,
                    },
                },
            ),
        ],
        edges: vec![ExecutableDagEdge {
            producer: PostAsapNodeId(0),
            consumer: PostAsapNodeId(1),
            role: EdgeRole::Input,
            intermediate_schema: (*schema).clone(),
            data_state: ExecutionDataState::QUERY_ROWS,
            grouping: GroupingEdgeCompatibility::NotApplicable,
            window: WindowEdgeCompatibility::NotApplicable,
        }],
        root: PostAsapNodeId(1),
    };
    let sources = || -> BTreeMap<u64, asap_physical_operators::dag::planner::Source<'static>> {
        BTreeMap::from([(
            0,
            Box::new(
                Operator::source(
                    schema.clone(),
                    vec![Batch::try_new(schema.clone(), vec![vec![Value::Float64(1.)]]).unwrap()],
                )
                .unwrap(),
            ) as asap_physical_operators::dag::planner::Source<'static>,
        )])
    };
    let native = bind(&dag, sources(), &[1]).unwrap();
    assert_eq!(floats(&run(&native, 1, query()), 0), vec![3.]);
    assert!(bind(&dag, BTreeMap::new(), &[1]).is_err());
    dag.nodes[1].payload = ExecutableOperatorPayload::Value {
        operation: ValueOperation::Extension {
            name: "unknown".into(),
        },
    };
    assert!(bind(&dag, sources(), &[1]).is_err());
}

// A completed empty population has an exact zero count, with integer output.
#[test]
fn empty_exact_count_is_an_integer_state_readout() {
    let input = schema(&[("value", DataType::Float64, false)]);
    let build = Operator::summary_build(
        input.clone(),
        SummaryFamilyType::ExactAggregate(ExactKind::Count, ExactParams::Count),
        0,
        None,
        vec![],
    )
    .unwrap();
    let read = Operator::readout(build.schema(), 0, Statistic::Count, Default::default()).unwrap();
    let mut dag = PhysicalDag::default();
    dag.add(0, vec![], Operator::source(input, vec![]).unwrap())
        .unwrap();
    dag.add(1, vec![0], build).unwrap();
    dag.add(2, vec![1], read).unwrap();
    assert!(matches!(run(&dag, 2, query())[0][0], Value::Int64(0)));
}

// A deployment source cannot pass a different row shape to bound expressions.
#[test]
fn source_batches_must_match_the_bound_schema() {
    use asap_physical_operators::dag::{self, PhysicalOperator};
    use planner_types::{
        post_asap::{
            ExecutableDag, ExecutableDagNode, ExecutableOperatorPayload, ExecutionDataState,
            PostAsapNodeId,
        },
        pre_asap::QueryExpr,
    };
    use std::{cell::Cell, collections::BTreeMap, rc::Rc};
    struct WrongSource {
        schema: Schema,
        starts: Rc<Cell<usize>>,
    }
    impl PhysicalOperator<Batch, Schema> for WrongSource {
        fn name(&self) -> &str {
            "ExternalSource"
        }
        fn input_schemas(&self) -> Vec<Schema> {
            vec![]
        }
        fn output_schema(&self) -> Schema {
            self.schema.clone()
        }
        fn output_bytes(&self, value: &Batch) -> usize {
            value.bytes()
        }
        fn start<'a>(
            &'a self,
            _: Vec<dag::Input<'a, Batch>>,
            _: RunContext,
        ) -> Result<dag::OutputStream<'a, Batch>, dag::Error> {
            self.starts.set(self.starts.get() + 1);
            Ok(
                futures::stream::once(async { Batch::try_new(schema(&[]), vec![vec![]]) })
                    .boxed_local(),
            )
        }
    }
    let expected = schema(&[("value", DataType::Float64, false)]);
    let starts = Rc::new(Cell::new(0));
    let plan = ExecutableDag {
        nodes: vec![ExecutableDagNode {
            id: PostAsapNodeId(0),
            payload: ExecutableOperatorPayload::Fallback {
                expression: QueryExpr::promql_scalar(1.),
            },
            output_state: ExecutionDataState::QUERY_ROWS,
            output_schema: (*expected).clone(),
            guarantee: None,
        }],
        edges: vec![],
        root: PostAsapNodeId(0),
    };
    let source = Box::new(WrongSource {
        schema: expected,
        starts: starts.clone(),
    }) as dag::planner::Source<'static>;
    let native = dag::planner::bind(&plan, BTreeMap::from([(0, source)]), &[0]).unwrap();
    assert_eq!(starts.get(), 0);
    let context = RunContext::new(query(), Limits::default()).unwrap();
    let mut output = native.execute(&[0], context).unwrap().remove(0);
    assert!(matches!(
        block_on(output.next()),
        Some(Err(dag::Error::AtNode { node: 0, .. }))
    ));
    assert_eq!(starts.get(), 1);
}

// Float extrema have the same NaN behavior as the exact summary kernels.
#[test]
fn extrema_preserve_numeric_values_in_the_presence_of_nan() {
    let input = schema(&[("v", DataType::Float64, false)]);
    let mut dag = PhysicalDag::default();
    dag.add(
        0,
        vec![],
        Operator::source(
            input.clone(),
            vec![Batch::try_new(
                input.clone(),
                vec![vec![Value::Float64(-f64::NAN)], vec![Value::Float64(5.)]],
            )
            .unwrap()],
        )
        .unwrap(),
    )
    .unwrap();
    dag.add(
        1,
        vec![0],
        Operator::aggregate(
            input,
            vec![],
            vec![
                ("min".into(), Reduction::Min(0)),
                ("max".into(), Reduction::Max(0)),
            ],
        )
        .unwrap(),
    )
    .unwrap();
    let rows = run(&dag, 1, query());
    assert_eq!(floats(&rows, 0), vec![5.]);
    assert_eq!(floats(&rows, 1), vec![5.]);
}

// Planner wire nodes, including grouping and edge roles, are executable at either phase.
#[test]
fn planner_semijoin_sort_limit_contract_at_both_phases() {
    use asap_physical_operators::dag::planner::{bind, Source};
    use planner_types::{
        post_asap::*,
        pre_asap::{CompareOpKind, GroupKeys, JoinKind, Predicate, QueryExpr, SortKey},
    };
    use std::{collections::BTreeMap, rc::Rc};
    let rows_schema = schema(&[
        ("group", DataType::Utf8, false),
        ("key", DataType::Utf8, false),
        ("score", DataType::Float64, false),
    ]);
    let keys_schema = schema(&[("key", DataType::Utf8, false)]);
    let node = |id, payload, schema: &Schema| ExecutableDagNode {
        id: PostAsapNodeId(id),
        payload,
        output_schema: (**schema).clone(),
        output_state: ExecutionDataState::QUERY_ROWS,
        guarantee: None,
    };
    let edge = |producer, consumer, role, schema: &Schema| ExecutableDagEdge {
        producer: PostAsapNodeId(producer),
        consumer: PostAsapNodeId(consumer),
        role,
        intermediate_schema: (**schema).clone(),
        data_state: ExecutionDataState::QUERY_ROWS,
        grouping: GroupingEdgeCompatibility::NotApplicable,
        window: WindowEdgeCompatibility::NotApplicable,
    };
    let groups = GroupKeys::by(vec![0]);
    let dag = ExecutableDag {
        nodes: vec![
            node(
                0,
                ExecutableOperatorPayload::Fallback {
                    expression: QueryExpr::promql_scalar(0.),
                },
                &rows_schema,
            ),
            node(
                1,
                ExecutableOperatorPayload::Fallback {
                    expression: QueryExpr::promql_scalar(0.),
                },
                &keys_schema,
            ),
            node(
                2,
                ExecutableOperatorPayload::RelationalJoin {
                    join_kind: JoinKind::Semi,
                    pruning: None,
                    pred: Predicate(Rc::new(QueryExpr::Compare {
                        left: Rc::new(QueryExpr::Column(1)),
                        op: CompareOpKind::Eq,
                        right: Rc::new(QueryExpr::Column(3)),
                    })),
                },
                &rows_schema,
            ),
            node(
                3,
                ExecutableOperatorPayload::Value {
                    operation: ValueOperation::Sort {
                        keys: vec![SortKey {
                            expr: QueryExpr::Column(2),
                            ascending: false,
                            nulls_first: false,
                        }],
                        partition_by: groups.clone(),
                    },
                },
                &rows_schema,
            ),
            node(
                4,
                ExecutableOperatorPayload::Value {
                    operation: ValueOperation::Limit {
                        n: 1,
                        offset: 0,
                        partition_by: groups,
                    },
                },
                &rows_schema,
            ),
        ],
        // Deliberately put Right before Left: list order must not swap inputs.
        edges: vec![
            edge(1, 2, EdgeRole::Right, &keys_schema),
            edge(0, 2, EdgeRole::Left, &rows_schema),
            edge(2, 3, EdgeRole::Input, &rows_schema),
            edge(3, 4, EdgeRole::Input, &rows_schema),
        ],
        root: PostAsapNodeId(4),
    };
    let text = |v: &str| Value::Utf8(v.into());
    for (phase, scope) in [
        (ExecutionTiming::QueryTime, query()),
        (
            ExecutionTiming::IngestionTime,
            Scope::Ingestion {
                window_start_ms: 0,
                window_end_ms: 1000,
                revision: 2,
            },
        ),
    ] {
        let dag = dag
            .with_execution_phases(&dag.nodes.iter().map(|node| (node.id, phase)).collect())
            .unwrap();
        let sources: BTreeMap<u64, Source<'static>> = BTreeMap::from([
            (
                0,
                Box::new(
                    Operator::source(
                        rows_schema.clone(),
                        vec![Batch::try_new(
                            rows_schema.clone(),
                            vec![
                                vec![text("a"), text("x"), Value::Float64(8.)],
                                vec![text("a"), text("y"), Value::Float64(9.)],
                                vec![text("b"), text("x"), Value::Float64(2.)],
                                vec![text("b"), text("z"), Value::Float64(99.)],
                            ],
                        )
                        .unwrap()],
                    )
                    .unwrap(),
                ) as Source<'static>,
            ),
            (
                1,
                Box::new(
                    Operator::source(
                        keys_schema.clone(),
                        vec![Batch::try_new(
                            keys_schema.clone(),
                            vec![vec![text("x")], vec![text("y")]],
                        )
                        .unwrap()],
                    )
                    .unwrap(),
                ) as Source<'static>,
            ),
        ]);
        let native = bind(&dag, sources, &[4]).unwrap();
        let mut scores = floats(&run(&native, 4, scope), 2);
        scores.sort_by(f64::total_cmp);
        assert_eq!(scores, vec![2., 9.]);
    }
}

// Planner scalar signatures, collection access and null predicates share native execution.
#[test]
fn planner_expressions_preserve_collection_and_nullable_types() {
    use asap_physical_operators::dag::expressions::CompiledExpression;
    use planner_types::pre_asap::{CompareOpKind, QueryExpr, ScalarValue};
    use std::rc::Rc;
    let input_schema = schema(&[(
        "items",
        DataType::Map {
            key: Box::new(DataType::Utf8),
            value: Box::new(DataType::Int64),
            value_nullable: false,
        },
        false,
    )]);
    let access = QueryExpr::FunctionCall {
        name: "asap_element_access".into(),
        args: vec![
            QueryExpr::Column(0),
            QueryExpr::Literal(ScalarValue::Utf8("count".into())),
        ],
    };
    let project = Operator::project(
        input_schema.clone(),
        vec![(
            "count".into(),
            Expression::planner(CompiledExpression::compile(&access, &input_schema).unwrap()),
        )],
    )
    .unwrap();
    let mut dag = PhysicalDag::default();
    dag.add(
        0,
        vec![],
        Operator::source(
            input_schema.clone(),
            vec![Batch::try_new(
                input_schema.clone(),
                vec![
                    vec![Value::Map(
                        vec![(Value::Utf8("count".into()), Value::Int64(7))].into(),
                    )],
                    vec![Value::Map(Arc::from([]))],
                ],
            )
            .unwrap()],
        )
        .unwrap(),
    )
    .unwrap();
    let projected = project.schema();
    dag.add(1, vec![0], project).unwrap();
    let predicate = QueryExpr::Compare {
        left: Rc::new(QueryExpr::Column(0)),
        op: CompareOpKind::Ge,
        right: Rc::new(QueryExpr::Literal(ScalarValue::Int64(1))),
    };
    dag.add(
        2,
        vec![1],
        Operator::filter(
            projected.clone(),
            Expression::planner(CompiledExpression::compile(&predicate, &projected).unwrap()),
        )
        .unwrap(),
    )
    .unwrap();
    let rows = run(&dag, 2, query());
    assert!(matches!(rows.as_slice(),[row] if matches!(row.as_slice(),[Value::Int64(7)])));
    let unknown = QueryExpr::FunctionCall {
        name: "unregistered_function".into(),
        args: vec![QueryExpr::Column(0)],
    };
    assert!(CompiledExpression::compile(&unknown, &input_schema).is_err());
}

// Outer, semi and anti joins share Planner predicates and preserve SQL null behavior.
#[test]
fn native_relational_join_kinds_preserve_unmatched_rows() {
    use planner_types::pre_asap::{CompareOpKind, JoinKind, Predicate, QueryExpr};
    use std::rc::Rc;
    let input = schema(&[("key", DataType::Int64, true)]);
    let predicate = Predicate(Rc::new(QueryExpr::Compare {
        left: Rc::new(QueryExpr::Column(0)),
        op: CompareOpKind::Eq,
        right: Rc::new(QueryExpr::Column(1)),
    }));
    for (kind, count) in [
        (JoinKind::Inner, 1),
        (JoinKind::Left, 3),
        (JoinKind::Right, 3),
        (JoinKind::Full, 5),
        (JoinKind::Semi, 1),
        (JoinKind::Anti, 2),
        (JoinKind::Cross, 9),
    ] {
        let output = if matches!(kind, JoinKind::Semi | JoinKind::Anti) {
            input.clone()
        } else {
            schema(&[
                ("left", DataType::Int64, true),
                ("right", DataType::Int64, true),
            ])
        };
        let mut dag = PhysicalDag::default();
        for (id, rows) in [
            (
                0,
                vec![
                    vec![Value::Int64(1)],
                    vec![Value::Int64(2)],
                    vec![Value::Null],
                ],
            ),
            (
                1,
                vec![
                    vec![Value::Int64(2)],
                    vec![Value::Int64(3)],
                    vec![Value::Null],
                ],
            ),
        ] {
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
        }
        dag.add(
            2,
            vec![0, 1],
            Operator::relational_join(
                input.clone(),
                input.clone(),
                kind.clone(),
                &predicate,
                output,
            )
            .unwrap(),
        )
        .unwrap();
        assert_eq!(run(&dag, 2, query()).len(), count, "{kind:?}");
    }
}

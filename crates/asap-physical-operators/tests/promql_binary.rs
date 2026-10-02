//! Binary computation must be fully compiled before deployment binds values.
use asap_physical_operators::{
    operators::Operator,
    physical_planner::{compile_node, CompiledPhysicalDAG, InputContract, Source},
    runtime::{Limits, RunContext, Scope},
    values::{Batch, Schema, Value},
};
use futures::{executor::block_on, StreamExt};
use planner_types::{
    post_asap::{
        BinaryOperator, ExecutionDataState, Field, FieldDataType, PostAsapDAGNode, PostAsapNodeId,
        PostAsapOperatorPayload, Schema as PlannerSchema,
    },
    pre_asap::{ArithmeticOpKind, BinaryOpKind, DataType},
};
use std::{collections::BTreeMap, sync::Arc};

fn schema() -> Schema {
    Arc::new(PlannerSchema {
        closed: true,
        unique_keys: vec![],
        fields: vec![
            Field {
                table: None,
                name: "labels".into(),
                dtype: FieldDataType::Plain(DataType::Map {
                    key: Box::new(DataType::Utf8),
                    value: Box::new(DataType::Utf8),
                    value_nullable: false,
                }),
                nullable: false,
            },
            Field {
                table: None,
                name: "value".into(),
                dtype: FieldDataType::Plain(DataType::Float64),
                nullable: false,
            },
        ],
        time_index: None,
    })
}
fn row(name: &str, job: &str, value: f64) -> Vec<Value> {
    vec![
        Value::Map(
            vec![
                (Value::Utf8("__name__".into()), Value::Utf8(name.into())),
                (Value::Utf8("job".into()), Value::Utf8(job.into())),
            ]
            .into(),
        ),
        Value::Float64(value),
    ]
}
fn program() -> CompiledPhysicalDAG {
    program_for(BinaryOperator {
        kind: BinaryOpKind::Arithmetic(ArithmeticOpKind::Div),
        vector_match: None,
        checked_relative_division: true,
        checked_finite_division: false,
    })
}
fn program_for(operator: BinaryOperator) -> CompiledPhysicalDAG {
    let schema = schema();
    let node = PostAsapDAGNode {
        id: PostAsapNodeId(2),
        payload: PostAsapOperatorPayload::Binary { operator },
        output_state: ExecutionDataState::QUERY_ROWS,
        output_schema: (*schema).clone(),
        guarantee: None,
    };
    let operator = compile_node(&node, &[schema.clone(), schema.clone()]).unwrap();
    let physical_dag = CompiledPhysicalDAG::from_operators(
        BTreeMap::from([
            (0, InputContract::bounded(schema.clone())),
            (1, InputContract::bounded(schema)),
        ]),
        BTreeMap::from([(2, (vec![0, 1], operator))]),
        vec![2],
    )
    .unwrap();
    serde_json::from_slice::<CompiledPhysicalDAG>(&serde_json::to_vec(&physical_dag).unwrap())
        .unwrap()
}
fn evaluate(
    left: Vec<Vec<Value>>,
    right: Vec<Vec<Value>>,
) -> Result<Vec<Vec<Value>>, asap_physical_operators::Error> {
    evaluate_with(program(), left, right)
}
fn evaluate_with(
    physical_dag: CompiledPhysicalDAG,
    left: Vec<Vec<Value>>,
    right: Vec<Vec<Value>>,
) -> Result<Vec<Vec<Value>>, asap_physical_operators::Error> {
    let sources = [left, right]
        .into_iter()
        .enumerate()
        .map(|(id, rows)| {
            let batch = Batch::try_new(schema(), rows).unwrap();
            (
                id as u64,
                Box::new(Operator::source(schema(), vec![batch]).unwrap()) as Source<'_>,
            )
        })
        .collect();
    let bound = physical_dag.instantiate(sources)?;
    let ctx = RunContext::new(
        Scope::Query {
            evaluation_time_ms: 1,
            revision: 0,
        },
        Limits::default(),
    )?;
    block_on(async {
        let mut stream = bound.execute(&[2], ctx)?.remove(0);
        let mut rows = Vec::new();
        while let Some(batch) = stream.next().await {
            rows.extend(batch?.rows().iter().cloned());
        }
        Ok(rows)
    })
}

#[test]
fn compiled_binary_matches_series_and_preserves_checked_division() {
    let rows = evaluate(
        vec![row("left", "api", 6.), row("left", "unmatched", 8.)],
        vec![row("right", "api", 2.)],
    )
    .unwrap();
    assert_eq!(
        serde_json::to_value(&rows).unwrap(),
        serde_json::to_value(vec![vec![
            Value::Map(vec![(Value::Utf8("job".into()), Value::Utf8("api".into()))].into()),
            Value::Float64(3.)
        ]])
        .unwrap()
    );
    assert!(evaluate(vec![row("a", "api", 1.)], vec![row("b", "api", 0.)]).is_err());
}

#[test]
fn duplicate_matching_identity_is_rejected() {
    assert!(evaluate(
        vec![row("a", "api", 1.)],
        vec![row("b", "api", 2.), row("c", "api", 3.)]
    )
    .is_err());
}

// Scalar broadcasting and comparison filtering keep the vector operand's value.
#[test]
fn scalar_broadcast_and_bool_comparison_are_distinct() {
    use asap_physical_operators::physical_planner::promql_values;
    use planner_types::pre_asap::CompareOpKind;
    for return_bool in [false, true] {
        let physical_dag = promql_values::compile_binary(
            &BinaryOperator {
                kind: BinaryOpKind::Compare(CompareOpKind::Lt),
                vector_match: None,
                checked_relative_division: false,
                checked_finite_division: false,
            },
            return_bool,
            true,
            false,
        )
        .unwrap();
        let physical_dag = serde_json::from_slice::<CompiledPhysicalDAG>(
            &serde_json::to_vec(&physical_dag).unwrap(),
        )
        .unwrap();
        let scalar = promql_values::scalar_schema();
        let vector = promql_values::vector_schema();
        let sources = BTreeMap::from([
            (
                0,
                Box::new(
                    Operator::source(
                        scalar.clone(),
                        vec![Batch::try_new(scalar, vec![vec![Value::Float64(2.)]]).unwrap()],
                    )
                    .unwrap(),
                ) as Source<'_>,
            ),
            (
                1,
                Box::new(
                    Operator::source(
                        vector.clone(),
                        vec![Batch::try_new(
                            vector,
                            vec![row("requests", "api", 4.), row("requests", "worker", 1.)],
                        )
                        .unwrap()],
                    )
                    .unwrap(),
                ) as Source<'_>,
            ),
        ]);
        let bound = physical_dag.instantiate(sources).unwrap();
        let context = RunContext::new(
            Scope::Query {
                evaluation_time_ms: 1,
                revision: 0,
            },
            Limits::default(),
        )
        .unwrap();
        let result = block_on(async {
            bound
                .execute(&[2], context)
                .unwrap()
                .remove(0)
                .next()
                .await
                .unwrap()
                .unwrap()
        });
        assert_eq!(result.rows().len(), if return_bool { 2 } else { 1 });
        assert!(
            matches!(result.rows()[0].last(), Some(Value::Float64(v)) if *v == if return_bool { 1. } else { 4. })
        );
        let Value::Map(labels) = &result.rows()[0][0] else {
            panic!("missing labels")
        };
        assert_eq!(
            labels
                .iter()
                .any(|(key, _)| matches!(key, Value::Utf8(s) if s.as_ref() == "__name__")),
            !return_bool
        );
    }
}

// Terminal request controls retain their native error classification.
#[test]
fn binary_obeys_memory_and_cancellation() {
    for cancel in [false, true] {
        let physical_dag = program();
        let sources = (0..2)
            .map(|id| {
                (
                    id,
                    Box::new(
                        Operator::source(
                            schema(),
                            vec![Batch::try_new(schema(), vec![row("x", "api", 1.)]).unwrap()],
                        )
                        .unwrap(),
                    ) as Source<'_>,
                )
            })
            .collect();
        let bound = physical_dag.instantiate(sources).unwrap();
        let context = RunContext::new(
            Scope::Query {
                evaluation_time_ms: 1,
                revision: 0,
            },
            Limits {
                max_bytes: if cancel { 10000 } else { 1 },
                ..Limits::default()
            },
        )
        .unwrap();
        if cancel {
            context.cancel();
        }
        let result = block_on(async {
            match bound.execute(&[2], context) {
                Err(error) => Err(error),
                Ok(mut streams) => streams.remove(0).next().await.unwrap().map(|_| ()),
            }
        });
        assert!(matches!(
            (cancel, result),
            (true, Err(asap_physical_operators::Error::Cancelled))
                | (false, Err(asap_physical_operators::Error::MemoryLimit))
        ));
    }
}

// A `bool` comparison over label-map vectors yields 1 or 0 and drops the name.
#[test]
fn label_map_bool_comparison_drops_the_name() {
    let program = program_for(BinaryOperator {
        kind: BinaryOpKind::CompareBool(planner_types::pre_asap::CompareOpKind::Gt),
        vector_match: None,
        checked_relative_division: false,
        checked_finite_division: false,
    });
    let rows = evaluate_with(
        program,
        vec![row("a", "api", 6.)],
        vec![row("b", "api", 2.)],
    )
    .unwrap();
    let [row] = rows.as_slice() else {
        panic!("expected one row, got {}", rows.len());
    };
    let Value::Map(labels) = &row[0] else {
        panic!("expected labels");
    };
    assert!(labels
        .iter()
        .all(|(k, _)| !matches!(k, Value::Utf8(k) if &**k == "__name__")));
    assert!(matches!(row[1], Value::Float64(v) if v == 1.));
}

// Stored temporal readouts drop metric names before filter comparisons and set matching.
#[test]
fn stored_series_readouts_support_filters_and_sets() {
    use asap_physical_operators::{
        physical_planner::compile, summary_kernels::exact::ExactAccumulator,
    };
    use planner_types::post_asap::*;
    use planner_types::pre_asap::{
        schema::PROMQL_SERIES_IDENTITY, CompareOpKind, PromQLVectorSetOpKind,
    };
    for (exact_kind, params) in [
        (ExactKind::Sum, ExactParams::Sum),
        (ExactKind::Count, ExactParams::Count),
    ] {
        let family = FieldDataType::ExactAggregate(exact_kind.clone(), params);
        let state_schema = Arc::new(PlannerSchema {
            closed: true,
            unique_keys: vec![],
            fields: vec![
                Field {
                    table: None,
                    name: PROMQL_SERIES_IDENTITY.into(),
                    dtype: FieldDataType::Plain(DataType::Utf8),
                    nullable: false,
                },
                Field {
                    table: None,
                    name: "value".into(),
                    dtype: family.clone(),
                    nullable: false,
                },
            ],
            time_index: None,
        });
        let mut value_schema = (*state_schema).clone();
        value_schema.fields[1].dtype = FieldDataType::Plain(DataType::Float64);
        for kind in [
            BinaryOpKind::Compare(CompareOpKind::Gt),
            BinaryOpKind::Set(PromQLVectorSetOpKind::And),
            BinaryOpKind::Set(PromQLVectorSetOpKind::Or),
        ] {
            let nodes = (0..5)
                .map(|id| PostAsapDAGNode {
                    id: PostAsapNodeId(id),
                    payload: match id {
                        0 | 1 => PostAsapOperatorPayload::SummaryMerge,
                        2 | 3 => PostAsapOperatorPayload::Value {
                            operation: ValueOperation::FinalizeExactAccumulator,
                        },
                        _ => PostAsapOperatorPayload::Binary {
                            operator: BinaryOperator {
                                kind: kind.clone(),
                                vector_match: None,
                                checked_relative_division: false,
                                checked_finite_division: false,
                            },
                        },
                    },
                    output_state: if id < 2 {
                        ExecutionDataState::INGESTION_SUMMARY
                    } else {
                        ExecutionDataState::QUERY_ROWS
                    },
                    output_schema: if id < 2 {
                        (*state_schema).clone()
                    } else {
                        value_schema.clone()
                    },
                    guarantee: None,
                })
                .collect::<Vec<_>>();
            let edges = [
                (0, 2, EdgeRole::Input),
                (1, 3, EdgeRole::Input),
                (2, 4, EdgeRole::Left),
                (3, 4, EdgeRole::Right),
            ]
            .into_iter()
            .map(|(producer, consumer, role)| PostAsapDAGEdge {
                producer: PostAsapNodeId(producer),
                consumer: PostAsapNodeId(consumer),
                role,
                intermediate_schema: nodes[producer as usize].output_schema.clone(),
                data_state: nodes[producer as usize].output_state,
                grouping: GroupingEdgeCompatibility::NotApplicable,
                window: WindowEdgeCompatibility::NotApplicable,
            })
            .collect();
            let dag = PostAsapDAG {
                nodes,
                edges,
                root: PostAsapNodeId(4),
            };
            let physical_dag = compile(
                &dag,
                BTreeMap::from([
                    (0, InputContract::bounded(state_schema.clone())),
                    (1, InputContract::bounded(state_schema.clone())),
                ]),
                &[4],
            )
            .unwrap();
            let physical_dag: CompiledPhysicalDAG =
                serde_json::from_slice(&serde_json::to_vec(&physical_dag).unwrap()).unwrap();
            let sources = [(0, "a", 6.), (1, "b", 2.)]
                .into_iter()
                .map(|(id, name, value)| {
                    let mut state = ExactAccumulator::new(family.clone(), false).unwrap();
                    if exact_kind == ExactKind::Count {
                        for _ in 0..value as usize {
                            state.update(None, 1.0, 0);
                        }
                    } else {
                        state.update(None, value, 0);
                    }
                    let identity = serde_json::to_string(&BTreeMap::from([
                        ("__name__", name),
                        ("job", "api"),
                    ]))
                    .unwrap();
                    let batch = Batch::try_new(
                        state_schema.clone(),
                        vec![vec![
                            Value::Utf8(identity.into()),
                            Value::Summary {
                                family: family.clone(),
                                state: Arc::new(state),
                            },
                        ]],
                    )
                    .unwrap();
                    (
                        id,
                        Box::new(Operator::source(state_schema.clone(), vec![batch]).unwrap())
                            as Source<'_>,
                    )
                })
                .collect();
            let bound = physical_dag.instantiate(sources).unwrap();
            let context = RunContext::new(
                Scope::Query {
                    evaluation_time_ms: 1,
                    revision: 0,
                },
                Limits::default(),
            )
            .unwrap();
            let result = block_on(async {
                let mut stream = bound.execute(&[4], context).unwrap().remove(0);
                let mut rows = vec![];
                while let Some(batch) = stream.next().await {
                    rows.extend(batch.unwrap().rows().iter().cloned());
                }
                rows
            });
            assert_eq!(result.len(), 1, "{kind:?}");
            assert!(
                matches!(&result[0][0], Value::Utf8(labels) if labels.as_ref()==r#"{"job":"api"}"#)
            );
            assert!(matches!(result[0][1], Value::Float64(6.)));
        }
    }
}

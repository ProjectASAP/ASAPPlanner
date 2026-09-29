//! Binary computation must be fully compiled before deployment binds values.
use asap_physical_operators::{
    operators::Operator,
    physical_planner::{compile_node, CompiledPhysicalDag, InputContract, Source},
    runtime::{Limits, RunContext, Scope},
    values::{Batch, Schema, Value},
};
use futures::{executor::block_on, StreamExt};
use planner_types::{
    post_asap::{
        BinaryOperator, ExecutableDagNode, ExecutableOperatorPayload, ExecutionDataState,
        PostAsapNodeId, SummaryFamilyType, SummaryField, SummarySchema,
    },
    pre_asap::{ArithmeticOpKind, BinaryOpKind, DataType},
};
use std::{collections::BTreeMap, sync::Arc};

fn schema() -> Schema {
    Arc::new(SummarySchema {
        fields: vec![
            SummaryField {
                name: "labels".into(),
                dtype: SummaryFamilyType::Plain(DataType::Map {
                    key: Box::new(DataType::Utf8),
                    value: Box::new(DataType::Utf8),
                    value_nullable: false,
                }),
                nullable: false,
            },
            SummaryField {
                name: "value".into(),
                dtype: SummaryFamilyType::Plain(DataType::Float64),
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
fn program() -> CompiledPhysicalDag {
    let schema = schema();
    let node = ExecutableDagNode {
        id: PostAsapNodeId(2),
        payload: ExecutableOperatorPayload::Binary {
            operator: BinaryOperator {
                kind: BinaryOpKind::Arithmetic(ArithmeticOpKind::Div),
                vector_match: None,
                checked_relative_division: true,
                checked_finite_division: false,
            },
        },
        output_state: ExecutionDataState::QUERY_ROWS,
        output_schema: (*schema).clone(),
        guarantee: None,
    };
    let operator = compile_node(&node, &[schema.clone(), schema.clone()]).unwrap();
    let graph = CompiledPhysicalDag::from_operators(
        BTreeMap::from([
            (0, InputContract::bounded(schema.clone())),
            (1, InputContract::bounded(schema)),
        ]),
        BTreeMap::from([(2, (vec![0, 1], operator))]),
        vec![2],
    )
    .unwrap();
    CompiledPhysicalDag::decode(&graph.encode().unwrap()).unwrap()
}
fn evaluate(
    left: Vec<Vec<Value>>,
    right: Vec<Vec<Value>>,
) -> Result<Vec<Vec<Value>>, asap_physical_operators::Error> {
    let graph = program();
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
    let bound = graph.instantiate(sources)?;
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
        let graph = promql_values::compile_binary(
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
        let graph = CompiledPhysicalDag::decode(&graph.encode().unwrap()).unwrap();
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
        let bound = graph.instantiate(sources).unwrap();
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
        let graph = program();
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
        let bound = graph.instantiate(sources).unwrap();
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

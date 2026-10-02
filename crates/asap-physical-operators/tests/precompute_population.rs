//! Persisted precompute DAGs preserve group/window identity and execute state-to-state computation.
use asap_physical_operators::{
    factory::create_planner_accumulator,
    operators::Operator,
    physical_planner::{precompute, CompiledPhysicalDAG, Source},
    runtime::{Limits, RunContext, Scope},
    values::{Batch, Value},
    Statistic,
};
use futures::{executor::block_on, StreamExt};
use planner_types::pre_asap::Schema as PlannerSchema;
use planner_types::{
    post_asap::*,
    pre_asap::{ArithmeticOpKind, BinaryOpKind, ColumnRef, DataType, GroupKeys, Reduction},
};
use std::{collections::BTreeMap, sync::Arc};

// Typed series identity survives finalization and derived precompute through population metadata.
#[test]
fn finalized_shared_panes_rebuild_one_global_summary_after_recovery() {
    let family = FieldDataType::ExactAggregate(ExactKind::Sum, ExactParams::Sum);
    let schema = |dtype| PlannerSchema {
        closed: true,
        unique_keys: vec![],
        fields: vec![Field {
            table: None,
            name: "value".into(),
            dtype,
            nullable: false,
        }],
        time_index: None,
    };
    let state_schema = schema(family.clone());
    let mut value_schema = schema(FieldDataType::Plain(DataType::Float64));
    value_schema.fields.push(Field {
        table: None,
        name: "time".into(),
        dtype: FieldDataType::Plain(DataType::Timestamp),
        nullable: false,
    });
    value_schema.time_index = Some(1);
    value_schema.fields.push(Field {
        table: None,
        name: planner_types::pre_asap::schema::PROMQL_SERIES_IDENTITY.into(),
        dtype: FieldDataType::Plain(DataType::Utf8),
        nullable: false,
    });
    for (weight, expected) in [
        (SummaryInputExpr::Column(ColumnRef::SampleValue), 60.),
        (SummaryInputExpr::Constant(1.), 4.),
    ] {
        let nodes = vec![
            PostAsapDAGNode {
                id: PostAsapNodeId(0),
                payload: PostAsapOperatorPayload::SummaryMerge,
                output_state: ExecutionDataState::INGESTION_SUMMARY,
                output_schema: state_schema.clone(),
                guarantee: None,
            },
            PostAsapDAGNode {
                id: PostAsapNodeId(1),
                payload: PostAsapOperatorPayload::Value {
                    operation: ValueOperation::FinalizeExactAccumulator,
                },
                output_state: ExecutionDataState::INGESTION_ROWS,
                output_schema: value_schema.clone(),
                guarantee: None,
            },
            PostAsapDAGNode {
                id: PostAsapNodeId(2),
                payload: PostAsapOperatorPayload::Binary {
                    operator: BinaryOperator {
                        kind: BinaryOpKind::Arithmetic(ArithmeticOpKind::Add),
                        vector_match: None,
                        checked_relative_division: false,
                        checked_finite_division: false,
                    },
                },
                output_state: ExecutionDataState::INGESTION_ROWS,
                output_schema: value_schema.clone(),
                guarantee: None,
            },
            PostAsapDAGNode {
                id: PostAsapNodeId(3),
                payload: PostAsapOperatorPayload::SummaryAgg {
                    family: family.clone(),
                    input: SummaryUpdate {
                        weight,
                        ..SummaryUpdate::column(ColumnRef::SampleValue)
                    },
                    reduction: Reduction::Reduce(GroupKeys::by(vec![])),
                    grouping: GroupingStrategy::PerSubpopulationInstance,
                    filter: None,
                },
                output_state: ExecutionDataState::INGESTION_SUMMARY,
                output_schema: state_schema.clone(),
                guarantee: None,
            },
        ];
        let edges = [
            (0, 1, EdgeRole::Input),
            (1, 2, EdgeRole::Left),
            (1, 2, EdgeRole::Right),
            (2, 3, EdgeRole::Input),
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
            root: PostAsapNodeId(3),
        };
        // Identity metadata must remain one non-null Utf8 column.
        for mutation in 0..3 {
            let mut invalid_identity = dag.clone();
            let fields = &mut invalid_identity.nodes[1].output_schema.fields;
            match mutation {
                0 => fields[2].nullable = true,
                1 => fields[2].dtype = FieldDataType::Plain(DataType::Float64),
                _ => fields.push(fields[2].clone()),
            }
            assert!(precompute::compile(&invalid_identity, &[0], &[3]).is_err());
        }
        let mut invalid_grouping = dag.clone();
        let PostAsapOperatorPayload::SummaryAgg { reduction, .. } =
            &mut invalid_grouping.nodes[3].payload
        else {
            unreachable!()
        };
        *reduction = Reduction::Reduce(GroupKeys::by(vec![0]));
        assert!(
            precompute::compile(&invalid_grouping, &[0], &[3]).is_err(),
            "numeric values cannot be reinterpreted as population labels"
        );
        let program = precompute::compile(&dag, &[0], &[3]).unwrap();
        let program =
            serde_json::from_slice::<CompiledPhysicalDAG>(&serde_json::to_vec(&program).unwrap())
                .unwrap();
        assert_eq!(program.input_contracts().count(), 1);
        for revision in [1, 2] {
            let rows = [
                ("a", 1000, 2.),
                ("a", 2000, 4.),
                ("b", 1000, 8.),
                ("b", 2000, 16.),
            ]
            .into_iter()
            .map(|(group, time, value)| {
                let mut state = create_planner_accumulator(
                    &family,
                    &SummaryUpdate::column(ColumnRef::SampleValue),
                    &GroupingStrategy::PerSubpopulationInstance,
                )
                .unwrap();
                state.update_single(value, time);
                vec![
                    Value::Map(
                        vec![(Value::Utf8("instance".into()), Value::Utf8(group.into()))].into(),
                    ),
                    Value::Timestamp(time),
                    Value::Summary {
                        family: family.clone(),
                        state: Arc::from(state.into_accumulator()),
                    },
                ]
            })
            .collect();
            let input =
                Batch::try_new(precompute::population_schema(family.clone()), rows).unwrap();
            let sources = BTreeMap::from([(
                0,
                Box::new(Operator::source(input.schema().clone(), vec![input]).unwrap())
                    as Source<'_>,
            )]);
            let physical_dag = program.instantiate(sources).unwrap();
            let context = RunContext::new(
                Scope::Ingestion {
                    window_start_ms: 0,
                    window_end_ms: 2000,
                    revision,
                },
                Limits::default(),
            )
            .unwrap();
            let output = block_on(async {
                let mut stream = physical_dag
                    .execute(program.roots(), context)
                    .unwrap()
                    .remove(0);
                let batch = stream.next().await.unwrap().unwrap();
                assert!(stream.next().await.is_none());
                batch
            });
            assert_eq!(output.rows().len(), 1);
            assert!(matches!(&output.rows()[0][0], Value::Map(labels) if labels.is_empty()));
            assert!(matches!(&output.rows()[0][1], Value::Timestamp(2000)));
            let Value::Summary { state, .. } = &output.rows()[0][2] else {
                panic!("state expected")
            };
            assert_eq!(
                state
                    .as_any()
                    .downcast_ref::<asap_physical_operators::summary_kernels::exact::ExactAccumulator>()
                    .unwrap()
                    .readout(Statistic::Sum, None, None)
                    .unwrap()
                    .unwrap(),
                expected
            );
        }
    }
}

fn logical_schema(family: FieldDataType) -> PlannerSchema {
    PlannerSchema {
        closed: true,
        unique_keys: vec![],
        fields: vec![Field {
            table: None,
            name: "value".into(),
            dtype: family,
            nullable: false,
        }],
        time_index: None,
    }
}
fn state_dag(
    family: FieldDataType,
    target: Option<FieldDataType>,
    merge: bool,
) -> CompiledPhysicalDAG {
    let mut nodes = vec![PostAsapDAGNode {
        id: PostAsapNodeId(0),
        payload: PostAsapOperatorPayload::SummaryMerge,
        output_state: ExecutionDataState::INGESTION_SUMMARY,
        output_schema: logical_schema(family.clone()),
        guarantee: None,
    }];
    if merge {
        nodes.push(PostAsapDAGNode {
            id: PostAsapNodeId(1),
            payload: PostAsapOperatorPayload::SummaryMerge,
            ..nodes[0].clone()
        });
    }
    let read_id = nodes.len() as u32;
    nodes.push(PostAsapDAGNode {
        id: PostAsapNodeId(read_id),
        payload: PostAsapOperatorPayload::Value {
            operation: ValueOperation::FinalizeExactAccumulator,
        },
        output_state: ExecutionDataState::INGESTION_ROWS,
        output_schema: logical_schema(FieldDataType::Plain(DataType::Float64)),
        guarantee: None,
    });
    if let Some(target) = target {
        nodes.push(PostAsapDAGNode {
            id: PostAsapNodeId(nodes.len() as u32),
            payload: PostAsapOperatorPayload::SummaryAgg {
                family: target.clone(),
                input: SummaryUpdate::column(ColumnRef::SampleValue),
                reduction: Reduction::by(vec![]),
                grouping: GroupingStrategy::default(),
                filter: None,
            },
            output_state: ExecutionDataState::INGESTION_SUMMARY,
            output_schema: logical_schema(target),
            guarantee: None,
        });
    }
    let edges = (1..nodes.len())
        .map(|i| PostAsapDAGEdge {
            producer: nodes[i - 1].id,
            consumer: nodes[i].id,
            role: EdgeRole::Input,
            intermediate_schema: nodes[i - 1].output_schema.clone(),
            data_state: nodes[i - 1].output_state,
            grouping: GroupingEdgeCompatibility::NotApplicable,
            window: WindowEdgeCompatibility::NotApplicable,
        })
        .collect();
    let root = nodes.last().unwrap().id;
    precompute::compile(
        &PostAsapDAG { nodes, edges, root },
        &[0],
        &[u64::from(root.0)],
    )
    .unwrap()
}
fn native_run(
    program: &CompiledPhysicalDAG,
    family: FieldDataType,
    states: Vec<Arc<dyn asap_physical_operators::AggregateCore>>,
    context: RunContext,
) -> Result<Vec<Vec<Value>>, asap_physical_operators::Error> {
    let program =
        serde_json::from_slice::<CompiledPhysicalDAG>(&serde_json::to_vec(&program).unwrap())
            .unwrap();
    let rows = states
        .into_iter()
        .enumerate()
        .map(|(i, state)| {
            vec![
                Value::Map(vec![].into()),
                Value::Timestamp((i as i64 + 1) * 1000),
                Value::Summary {
                    family: family.clone(),
                    state,
                },
            ]
        })
        .collect();
    let input = Batch::try_new(precompute::population_schema(family), rows)?;
    let physical_dag = program.instantiate(BTreeMap::from([(
        0,
        Box::new(Operator::source(input.schema().clone(), vec![input])?) as Source<'_>,
    )]))?;
    block_on(async {
        let mut rows = Vec::new();
        let mut stream = physical_dag.execute(program.roots(), context)?.remove(0);
        while let Some(batch) = stream.next().await {
            rows.extend(batch?.rows().iter().cloned());
        }
        Ok(rows)
    })
}
fn ingestion_context(limits: Limits) -> RunContext {
    RunContext::new(
        Scope::Ingestion {
            window_start_ms: 0,
            window_end_ms: 2000,
            revision: 1,
        },
        limits,
    )
    .unwrap()
}
fn sum_state(value: f64) -> Arc<dyn asap_physical_operators::AggregateCore> {
    let mut state = asap_physical_operators::summary_kernels::exact::ExactAccumulator::new(
        planner_types::post_asap::FieldDataType::ExactAggregate(
            planner_types::post_asap::ExactKind::Sum,
            planner_types::post_asap::ExactParams::Sum,
        ),
        false,
    )
    .unwrap();
    state.update(None, value, 0);
    Arc::new(state)
}

// Only an explicit merge may collapse distinct pane updates before finalization.
#[test]
fn explicit_merge_changes_pane_cardinality() {
    let family = FieldDataType::ExactAggregate(ExactKind::Sum, ExactParams::Sum);
    for (merge, expected) in [(false, vec![2., 7.]), (true, vec![9.])] {
        let program = state_dag(family.clone(), None, merge);
        let rows = native_run(
            &program,
            family.clone(),
            vec![sum_state(2.), sum_state(7.)],
            ingestion_context(Limits::default()),
        )
        .unwrap();
        let values = rows
            .iter()
            .map(|row| match row[2] {
                Value::Float64(v) => v,
                _ => panic!("numeric readout expected"),
            })
            .collect::<Vec<_>>();
        assert_eq!(values, expected);
        assert!(matches!(rows.last().unwrap()[1], Value::Timestamp(2000)));
    }
}

// Typed updates reject invalid domains before publishing any target state.
#[test]
fn precompute_rejects_nonfinite_and_nonpositive_dds_updates() {
    let source = FieldDataType::ExactAggregate(ExactKind::Sum, ExactParams::Sum);
    let target = FieldDataType::Sketch(
        SketchKind::new(
            SketchAlgorithm::DDSketch,
            SketchParams::DDSketch { alpha: 0.01 },
        ),
        GroupingStrategy::default(),
    );
    let program = state_dag(source.clone(), Some(target), false);
    assert!(native_run(
        &program,
        source.clone(),
        vec![sum_state(20.)],
        ingestion_context(Limits::default())
    )
    .is_ok());
    for value in [-20., 0., f64::MAX, f64::NAN, f64::INFINITY] {
        assert!(native_run(
            &program,
            source.clone(),
            vec![sum_state(value)],
            ingestion_context(Limits::default())
        )
        .is_err());
    }
}

// An exact count must not silently lose units when exposed through Float64 rows.
#[test]
fn precompute_count_conversion_checks_precision() {
    use asap_physical_operators::summary_kernels::exact::ExactAccumulator;
    let family = FieldDataType::ExactAggregate(ExactKind::Count, ExactParams::Count);
    let program = state_dag(family.clone(), None, false);
    for (count, valid) in [(3u64, true), ((1u64 << 53) + 1, false)] {
        let mut state =
            serde_json::to_value(ExactAccumulator::new(family.clone(), false).unwrap()).unwrap();
        state["scalar"]["Count"] = count.into();
        let state: ExactAccumulator = serde_json::from_value(state).unwrap();
        let result = native_run(
            &program,
            family.clone(),
            vec![Arc::new(state)],
            ingestion_context(Limits::default()),
        );
        assert_eq!(result.is_ok(), valid);
    }
}

// DAG execution retains terminal cancellation and shared workspace limits.
#[test]
fn precompute_dag_enforces_cancellation_and_budget() {
    let family = FieldDataType::ExactAggregate(ExactKind::Sum, ExactParams::Sum);
    let program = state_dag(family.clone(), None, true);
    let context = ingestion_context(Limits::default());
    context.cancel();
    let error = native_run(&program, family.clone(), vec![sum_state(1.)], context).unwrap_err();
    assert!(format!("{error:?}").contains("Cancelled"));
    let error = native_run(
        &program,
        family,
        vec![sum_state(1.)],
        ingestion_context(Limits {
            max_bytes: 1,
            ..Limits::default()
        }),
    )
    .unwrap_err();
    assert!(format!("{error:?}").contains("MemoryLimit"));
}

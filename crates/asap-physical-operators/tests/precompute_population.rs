//! Persisted precompute graphs preserve group/window identity and execute state-to-state computation.
use asap_physical_operators::{
    factory::create_planner_accumulator,
    operators::Operator,
    physical_planner::{precompute, CompiledPhysicalDag, Source},
    runtime::{Limits, RunContext, Scope},
    values::{Batch, Value},
    Statistic,
};
use futures::{executor::block_on, StreamExt};
use planner_types::{
    post_asap::*,
    pre_asap::{ArithmeticOpKind, BinaryOpKind, ColumnRef, DataType, GroupKeys, Reduction},
};
use std::{collections::BTreeMap, sync::Arc};

#[test]
fn finalized_shared_panes_rebuild_one_global_summary_after_recovery() {
    let family = SummaryFamilyType::ExactAggregate(ExactKind::Sum, ExactParams::Sum);
    let schema = |dtype| SummarySchema {
        fields: vec![SummaryField {
            name: "value".into(),
            dtype,
            nullable: false,
        }],
        time_index: None,
    };
    let state_schema = schema(family.clone());
    let mut value_schema = schema(SummaryFamilyType::Plain(DataType::Float64));
    value_schema.fields.push(SummaryField {
        name: "time".into(),
        dtype: SummaryFamilyType::Plain(DataType::Timestamp),
        nullable: false,
    });
    value_schema.time_index = Some(1);
    for (weight, expected) in [
        (SummaryInputExpr::Column(ColumnRef::SampleValue), 60.),
        (SummaryInputExpr::Constant(1.), 4.),
    ] {
        let nodes = vec![
            ExecutableDagNode {
                id: PostAsapNodeId(0),
                payload: ExecutableOperatorPayload::SummaryMerge,
                output_state: ExecutionDataState::INGESTION_SUMMARY,
                output_schema: state_schema.clone(),
                guarantee: None,
            },
            ExecutableDagNode {
                id: PostAsapNodeId(1),
                payload: ExecutableOperatorPayload::Value {
                    operation: ValueOperation::FinalizeExactAccumulator,
                },
                output_state: ExecutionDataState::INGESTION_ROWS,
                output_schema: value_schema.clone(),
                guarantee: None,
            },
            ExecutableDagNode {
                id: PostAsapNodeId(2),
                payload: ExecutableOperatorPayload::Binary {
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
            ExecutableDagNode {
                id: PostAsapNodeId(3),
                payload: ExecutableOperatorPayload::SummaryAgg {
                    family: family.clone(),
                    input: SummaryUpdate {
                        weight,
                        ..SummaryUpdate::column(ColumnRef::SampleValue)
                    },
                    reduction: Reduction::Reduce(GroupKeys::by(vec![])),
                    grouping: GroupingStrategy::PerSubpopulationInstance,
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
        .map(|(producer, consumer, role)| ExecutableDagEdge {
            producer: PostAsapNodeId(producer),
            consumer: PostAsapNodeId(consumer),
            role,
            intermediate_schema: nodes[producer as usize].output_schema.clone(),
            data_state: nodes[producer as usize].output_state,
            grouping: GroupingEdgeCompatibility::NotApplicable,
            window: WindowEdgeCompatibility::NotApplicable,
        })
        .collect();
        let dag = ExecutableDag {
            nodes,
            edges,
            root: PostAsapNodeId(3),
        };
        let program = precompute::compile(&dag, &[0], &[3]).unwrap();
        let program = CompiledPhysicalDag::decode(&program.encode().unwrap()).unwrap();
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
            let graph = program.instantiate(sources).unwrap();
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
                let mut stream = graph.execute(program.roots(), context).unwrap().remove(0);
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
                    .query_statistic(Statistic::Sum, &None, &Default::default())
                    .unwrap(),
                expected
            );
        }
    }
}

//! Opaque state travels through a retained physical projection without scalar decoding.
use asap_physical_operators::{
    expressions::Expression,
    factory::create_planner_accumulator,
    operators::Operator,
    physical_planner::{compile, CompiledPhysicalDag, InputContract, Source},
    runtime::{Limits, RunContext, Scope},
    values::{Batch, Value},
};
use futures::{executor::block_on, StreamExt};
use planner_types::{
    post_asap::*,
    pre_asap::{ColumnRef, DataType, ProjectItem, QueryExpr},
};
use std::{collections::BTreeMap, sync::Arc};

// A Post-ASAP projection may reorder/rename summary columns; recovery must retain
// the family and pass through the same immutable state, without decoding the payload.
#[test]
fn post_asap_summary_projection_survives_recovery() {
    let family = SummaryFamilyType::ExactAggregate(ExactKind::Sum, ExactParams::Sum);
    let schema = Arc::new(SummarySchema {
        fields: vec![
            SummaryField {
                name: "state".into(),
                dtype: family.clone(),
                nullable: false,
            },
            SummaryField {
                name: "service".into(),
                dtype: SummaryFamilyType::Plain(DataType::Utf8),
                nullable: false,
            },
        ],
        time_index: None,
    });
    let output = SummarySchema {
        fields: vec![
            schema.fields[1].clone(),
            SummaryField {
                name: "renamed".into(),
                ..schema.fields[0].clone()
            },
        ],
        time_index: None,
    };
    let dag = PostAsapDag {
        nodes: vec![
            PostAsapDagNode {
                id: PostAsapNodeId(0),
                payload: PostAsapOperatorPayload::SummaryMerge,
                output_schema: (*schema).clone(),
                output_state: ExecutionDataState::INGESTION_SUMMARY,
                guarantee: None,
            },
            PostAsapDagNode {
                id: PostAsapNodeId(1),
                payload: PostAsapOperatorPayload::Value {
                    operation: ValueOperation::Project {
                        cols: vec![1, 0]
                            .into_iter()
                            .map(|index| ProjectItem {
                                alias: None,
                                expr: QueryExpr::Column(index),
                            })
                            .collect(),
                        qualifier: None,
                    },
                },
                output_schema: output.clone(),
                output_state: ExecutionDataState::INGESTION_SUMMARY,
                guarantee: None,
            },
        ],
        edges: vec![PostAsapDagEdge {
            producer: PostAsapNodeId(0),
            consumer: PostAsapNodeId(1),
            role: EdgeRole::Input,
            intermediate_schema: (*schema).clone(),
            data_state: ExecutionDataState::INGESTION_SUMMARY,
            grouping: GroupingEdgeCompatibility::NotApplicable,
            window: WindowEdgeCompatibility::NotApplicable,
        }],
        root: PostAsapNodeId(1),
    };
    let program = compile(
        &dag,
        BTreeMap::from([(0, InputContract::bounded(schema.clone()))]),
        &[1],
    )
    .unwrap();
    let encoded = serde_json::to_vec(&program).unwrap();
    let program = serde_json::from_slice::<CompiledPhysicalDag>(&encoded).unwrap();
    let mut forged: serde_json::Value = serde_json::from_slice(&encoded).unwrap();
    forged["nodes"]["1"]["Operator"]["operator"]["output"]["fields"][1]["dtype"] =
        serde_json::json!({"Plain": "float64"});
    assert!(
        serde_json::from_slice::<CompiledPhysicalDag>(&serde_json::to_vec(&forged).unwrap())
            .is_err()
    );
    assert!(Operator::project(
        schema.clone(),
        vec![("invalid".into(), Expression::Column(2))]
    )
    .is_err());
    assert!(Operator::project(
        schema.clone(),
        vec![(
            "invalid".into(),
            Expression::Negate(Box::new(Expression::Column(0)))
        )]
    )
    .is_err());
    assert_eq!(*program.output_contract(1).unwrap().schema, output);
    let mut accumulator = create_planner_accumulator(
        &family,
        &SummaryUpdate::column(ColumnRef::SampleValue),
        &GroupingStrategy::PerSubpopulationInstance,
    )
    .unwrap();
    accumulator.update_single(7., 1);
    let state = Arc::from(accumulator.into_accumulator());
    let batch = Batch::try_new(
        schema.clone(),
        vec![vec![
            Value::Summary {
                family,
                state: Arc::clone(&state),
            },
            Value::Utf8("api".into()),
        ]],
    )
    .unwrap();
    let graph = program
        .instantiate(BTreeMap::from([(
            0,
            Box::new(Operator::source(schema, vec![batch]).unwrap()) as Source<'_>,
        )]))
        .unwrap();
    let context = RunContext::new(
        Scope::Query {
            evaluation_time_ms: 1,
            revision: 1,
        },
        Limits::default(),
    )
    .unwrap();
    block_on(async {
        let mut output = graph.execute(&[1], context).unwrap().remove(0);
        let batch = output.next().await.unwrap().unwrap();
        assert!(matches!(&batch.rows()[0][0], Value::Utf8(label) if label.as_ref() == "api"));
        let Value::Summary {
            state: projected, ..
        } = &batch.rows()[0][1]
        else {
            panic!("missing summary")
        };
        assert!(Arc::ptr_eq(&state, projected));
        assert!(output.next().await.is_none());
    });
}

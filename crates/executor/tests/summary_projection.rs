//! Opaque state travels through a retained physical projection without scalar decoding.
use asap_executor::sources::{BindingDataSources, MemorySource, TableProvider};
use asap_executor::{
    expressions::Expression,
    factory::create_planner_accumulator,
    operators::Operator,
    physical_planner::compile,
    runtime::{Limits, RunContext, Scope},
    values::{Batch, Value},
};
use futures::{executor::block_on, StreamExt};
use planner_types::ir::physical_export::{
    EdgeRole, GroupingEdgeCompatibility, PhysicalASAPDAG, PhysicalASAPDAGEdge, PhysicalASAPDAGNode,
    PhysicalASAPOperatorPayload, WindowEdgeCompatibility,
};
use planner_types::ir::properties::*;
use planner_types::ir::scalar::ColumnRef;
use planner_types::ir::schema::{DataType, *};
use planner_types::ir::ASAPOp;
use planner_types::ir::NonASAPOp;
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::Arc,
};

// A projection over a summary may reorder/rename its columns; recovery must retain
// the family and pass through the same immutable state, without decoding the payload.
#[test]
fn summary_projection_survives_recovery() {
    let family = FieldDataType::ExactAggregate(ExactKind::Sum, ExactParams::Sum);
    let schema = Arc::new(planner_types::ir::schema::Schema {
        unique_keys: vec![],
        closed: false,
        fields: vec![
            Field {
                table: None,
                name: "state".into(),
                dtype: family.clone(),
                nullable: false,
            },
            Field {
                table: None,
                name: "service".into(),
                dtype: FieldDataType::Plain(DataType::Utf8),
                nullable: false,
            },
        ],
        time_index: None,
    });
    let output = planner_types::ir::schema::Schema {
        unique_keys: vec![],
        closed: false,
        fields: vec![
            schema.fields[1].clone(),
            Field {
                name: "renamed".into(),
                ..schema.fields[0].clone()
            },
        ],
        time_index: None,
    };
    let dag = PhysicalASAPDAG {
        nodes: vec![
            PhysicalASAPDAGNode {
                id: 0,
                payload: PhysicalASAPOperatorPayload::ASAP(ASAPOp::SummaryMerge {
                    children: vec![],
                    newest_panes: None,
                    group_by: None,
                }),
                output_schema: (*schema).clone(),
                output_state: ExecutionDataState::INGESTION_SUMMARY,
                materialization: planner_types::physical::Materialization::MatAtIngestion(
                    planner_types::physical::Medium::Memory,
                ),
                guarantee: None,
                retention_ms: None,
            },
            PhysicalASAPDAGNode {
                id: 1,
                payload: PhysicalASAPOperatorPayload::NonASAP(NonASAPOp::Project {
                    cols: vec![1, 0]
                        .into_iter()
                        .map(|index| planner_types::ir::ProjectItem {
                            alias: None,
                            expr: planner_types::ir::ScalarExpr::Column(index),
                        })
                        .collect(),
                    qualifier: None,
                    child: 0,
                }),
                output_schema: output.clone(),
                output_state: ExecutionDataState::INGESTION_SUMMARY,
                materialization: planner_types::physical::Materialization::MatAtIngestion(
                    planner_types::physical::Medium::Memory,
                ),
                guarantee: None,
                retention_ms: None,
            },
        ],
        edges: vec![PhysicalASAPDAGEdge {
            producer: 0,
            consumer: 1,
            role: EdgeRole::Input,
            intermediate_schema: (*schema).clone(),
            data_state: ExecutionDataState::INGESTION_SUMMARY,
            grouping: GroupingEdgeCompatibility::NotApplicable,
            window: WindowEdgeCompatibility::NotApplicable,
        }],
        roots: vec![1],
        scalar_roots: vec![],
    };
    let program = compile(&dag, &BTreeSet::from([0]), &[1]).unwrap();
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
    assert_eq!(*program.output_schema(1).unwrap(), output);
    let mut accumulator = create_planner_accumulator(
        &family,
        &SummaryUpdate::column(ColumnRef::SampleValue),
        &GroupingStrategy::PerSubpopulationInstance,
    )
    .unwrap();
    accumulator.update_single(7., 1).unwrap();
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
    let data_sources = BindingDataSources {
        materialized_views: BTreeMap::from([(
            0,
            Arc::new(MemorySource::new(schema, vec![batch]).unwrap()) as Arc<dyn TableProvider>,
        )]),
        ..Default::default()
    };
    let physical_dag = program.instantiate(&data_sources).unwrap();
    let context = RunContext::new(
        Scope::Query {
            evaluation_time_ms: 1,
            revision: 1,
        },
        Limits::default(),
    )
    .unwrap();
    block_on(async {
        let mut output = physical_dag.execute(&[1], context).unwrap().remove(0);
        let batch = output.next().await.unwrap().unwrap();
        assert!(matches!(&batch.to_rows()[0][0], Value::Utf8(label) if label.as_ref() == "api"));
        let Value::Summary {
            state: projected, ..
        } = &batch.to_rows()[0][1]
        else {
            panic!("missing summary")
        };
        assert!(Arc::ptr_eq(&state, projected));
        assert!(output.next().await.is_none());
    });
}

// A `SummaryMerge` with no inputs is an invalid DAG, not a panic.
#[test]
fn summary_merge_without_inputs_is_rejected() {
    let family = FieldDataType::ExactAggregate(ExactKind::Sum, ExactParams::Sum);
    let schema = planner_types::ir::schema::Schema {
        unique_keys: vec![],
        closed: false,
        fields: vec![Field {
            table: None,
            name: "state".into(),
            dtype: family,
            nullable: false,
        }],
        time_index: None,
    };
    let dag = PhysicalASAPDAG {
        nodes: vec![PhysicalASAPDAGNode {
            id: 0,
            payload: PhysicalASAPOperatorPayload::ASAP(ASAPOp::SummaryMerge {
                children: vec![],
                newest_panes: None,
                group_by: None,
            }),
            output_schema: schema,
            output_state: ExecutionDataState::INGESTION_SUMMARY,
            materialization: Default::default(),
            guarantee: None,
            retention_ms: None,
        }],
        edges: vec![],
        roots: vec![0],
        scalar_roots: vec![],
    };
    let error = compile(&dag, &BTreeSet::new(), &[0])
        .err()
        .expect("rejected");
    assert!(
        error.to_string().contains("summary merge requires inputs"),
        "{error}"
    );
}

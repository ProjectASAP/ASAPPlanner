//! Opaque state travels through a retained physical projection without scalar decoding.
use asap_executor::sources::InMemoryDataStore;
use asap_executor::{
    expressions::Expression,
    factory::create_planner_accumulator,
    operators::Operator,
    physical_planner::{compile, test_support::views_at},
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
use planner_types::ir::NonASAPOp;
use planner_types::ir::{ASAPOp, TimeRangeKind};
use std::{sync::Arc, time::Duration};

/// The raw samples a stored summary is built from: `ts`, `value` and
/// `service` rows.
fn sample_schema() -> planner_types::ir::schema::Schema {
    planner_types::ir::schema::Schema::with_time_index(
        vec![
            Field::plain("ts", DataType::Timestamp, false),
            Field::plain("value", DataType::Float64, false),
            Field::plain("service", DataType::Utf8, false),
        ],
        0,
        vec![],
    )
}

/// A `Scan` of the raw samples.
fn scan() -> NonASAPOp<usize> {
    NonASAPOp::Scan {
        source: planner_types::ir::operator::Source::TimeSeries {
            metric: "requests".into(),
        },
        schema: sample_schema(),
        predicates: vec![],
    }
}

/// A node over raw samples (`views_at` moves it to ingestion time).
fn raw_node(id: usize, payload: NonASAPOp<usize>) -> PhysicalASAPDAGNode {
    PhysicalASAPDAGNode {
        id,
        payload: PhysicalASAPOperatorPayload::NonASAP(payload),
        output_schema: sample_schema(),
        output_state: ExecutionDataState::QUERY_ROWS,
        materialization: Default::default(),
        guarantee: None,
        retention_ms: None,
    }
}

/// An edge carrying raw samples.
fn raw_edge(producer: usize, consumer: usize) -> PhysicalASAPDAGEdge {
    PhysicalASAPDAGEdge {
        producer,
        consumer,
        role: EdgeRole::Input,
        intermediate_schema: sample_schema(),
        data_state: ExecutionDataState::QUERY_ROWS,
        grouping: GroupingEdgeCompatibility::NotApplicable,
        window: WindowEdgeCompatibility::NotApplicable,
    }
}

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
                payload: PhysicalASAPOperatorPayload::ASAP(ASAPOp::SummaryAgg {
                    child: 3,
                    family: family.clone(),
                    input: SummaryUpdate::column(ColumnRef::SampleValue),
                    reduction: planner_types::ir::operator::Reduction::Reduce(
                        planner_types::ir::operator::GroupKeys::by(vec![2]),
                    ),
                    grouping: GroupingStrategy::PerSubpopulationInstance,
                    filter: None,
                }),
                output_schema: (*schema).clone(),
                output_state: ExecutionDataState::INGESTION_SUMMARY,
                materialization: Default::default(),
                guarantee: None,
                retention_ms: None,
            },
            raw_node(2, scan()),
            raw_node(
                3,
                NonASAPOp::TimeRange {
                    range: Duration::from_secs(60),
                    kind: TimeRangeKind::Range,
                    child: 2,
                },
            ),
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
                materialization: Default::default(),
                guarantee: None,
                retention_ms: None,
            },
        ],
        edges: vec![
            raw_edge(2, 3),
            raw_edge(3, 0),
            PhysicalASAPDAGEdge {
                producer: 0,
                consumer: 1,
                role: EdgeRole::Input,
                intermediate_schema: (*schema).clone(),
                data_state: ExecutionDataState::INGESTION_SUMMARY,
                grouping: GroupingEdgeCompatibility::NotApplicable,
                window: WindowEdgeCompatibility::NotApplicable,
            },
        ],
        roots: vec![1],
        scalar_roots: vec![],
    };
    let mut dag = dag;
    let views = views_at(&mut dag, &[0]).unwrap();
    let program = compile(&dag, &views, &[1]).unwrap();
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
    let store = InMemoryDataStore::default()
        .with_materialized_view(0, vec![batch])
        .unwrap();
    let physical_dag = program.instantiate(Arc::new(store)).unwrap();
    let context = RunContext::new(
        Scope::Query {
            evaluation_time_ms: 1,
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
    let error = compile(&dag, &[], &[0]).err().expect("rejected");
    assert!(
        error.to_string().contains("summary merge requires inputs"),
        "{error}"
    );
}

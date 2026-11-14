//! Ingestion DAGs over a stored summary state execute state-to-state computation.
use asap_executor::sources::InMemoryDataStore;
use asap_executor::{
    physical_planner::{compile, test_support::views_at, CompiledPhysicalDAG},
    runtime::{Limits, RunContext, Scope},
    values::{Batch, Value},
};

use planner_types::ir::operator::{GroupKeys, Reduction};
use planner_types::ir::properties::*;
use planner_types::ir::scalar::ColumnRef;
use planner_types::ir::schema::{DataType, *};
use planner_types::ir::ASAPOp;
use planner_types::plan::materialized_view::MaterializedView;
use planner_types::plan::physical_asap_dag::{
    EdgeRole, GroupingEdgeCompatibility, PhysicalASAPDAG, PhysicalASAPDAGEdge, PhysicalASAPDAGNode,
    PhysicalASAPOperatorPayload, WindowEdgeCompatibility,
};
use planner_types::summary_type::*;
use std::sync::Arc;

fn logical_schema(family: FieldDataType) -> Schema {
    planner_types::ir::schema::Schema {
        unique_keys: vec![],
        closed: false,
        fields: vec![Field {
            name: "value".into(),
            dtype: family,
            nullable: false,
        }],
        time_index: None,
    }
}
/// The stored state at node 0 (materialized at ingestion time): a global `family`
/// `SummaryAgg` over a minute of raw `m` samples (`Scan` node `scan`,
/// `TimeRange` node `scan + 1`).
fn view_over_samples(family: FieldDataType, scan: usize) -> [PhysicalASAPDAGNode; 3] {
    let samples = Schema::with_time_index(
        vec![
            Field::plain("time", DataType::Timestamp, false),
            Field::plain("value", DataType::Float64, false),
        ],
        0,
        vec![],
    );
    let node = |id, payload, output_schema: Schema, output_state| PhysicalASAPDAGNode {
        id,
        payload,
        output_state,
        lifetime: Default::default(),
        parameters: None,
        output_schema,
        retention_ms: None,
        selected_physical_candidate: None,
        view_coverage: None,
    };
    [
        PhysicalASAPDAGNode {
            lifetime: planner_types::stages::Lifetime::AcrossEvaluations(
                planner_types::stages::StorageMedium::Memory,
            ),
            ..node(
                0,
                PhysicalASAPOperatorPayload::ASAP(ASAPOp::SummaryAgg {
                    summary_type: family.clone(),
                    input: SummaryUpdate::column(ColumnRef::SampleValue),
                    reduction: Reduction::Reduce(GroupKeys::by(vec![])),
                    filter: None,
                    child: scan + 1,
                }),
                logical_schema(family),
                ExecutionDataState::INGESTION_SUMMARY,
            )
        },
        node(
            scan,
            PhysicalASAPOperatorPayload::NonASAP(planner_types::ir::NonASAPOp::Scan {
                source: planner_types::ir::operator::Source::TimeSeries { metric: "m".into() },
                predicates: vec![],
                schema: samples.clone(),
            }),
            samples.clone(),
            ExecutionDataState::INGESTION_ROWS,
        ),
        node(
            scan + 1,
            PhysicalASAPOperatorPayload::NonASAP(planner_types::ir::NonASAPOp::TimeRange {
                range: std::time::Duration::from_secs(60),
                kind: planner_types::ir::TimeRangeKind::Range,
                child: scan,
            }),
            samples,
            ExecutionDataState::INGESTION_ROWS,
        ),
    ]
}
fn edge(
    nodes: &[PhysicalASAPDAGNode],
    producer: usize,
    consumer: usize,
    role: EdgeRole,
) -> PhysicalASAPDAGEdge {
    PhysicalASAPDAGEdge::new(
        producer,
        consumer,
        role,
        nodes[producer].output_schema.clone(),
        nodes[producer].output_state,
        GroupingEdgeCompatibility::NotApplicable,
        WindowEdgeCompatibility::NotApplicable,
    )
}
fn state_dag(
    family: FieldDataType,
    target: Option<FieldDataType>,
    merge: bool,
) -> (CompiledPhysicalDAG, MaterializedView) {
    try_state_dag(family, target, merge).unwrap()
}
fn try_state_dag(
    family: FieldDataType,
    target: Option<FieldDataType>,
    merge: bool,
) -> Result<(CompiledPhysicalDAG, MaterializedView), asap_executor::Error> {
    let scan = 2 + usize::from(merge) + usize::from(target.is_some());
    let [view, scan_node, range_node] = view_over_samples(family.clone(), scan);
    let mut nodes = vec![view];
    if merge {
        nodes.push(PhysicalASAPDAGNode {
            id: 1,
            payload: PhysicalASAPOperatorPayload::ASAP(ASAPOp::SummaryMerge {
                children: vec![0],
                group_by: None,
            }),
            output_state: ExecutionDataState::INGESTION_SUMMARY,
            lifetime: planner_types::stages::Lifetime::AcrossEvaluations(
                planner_types::stages::StorageMedium::Memory,
            ),
            parameters: None,
            output_schema: logical_schema(family.clone()),
            retention_ms: None,
            selected_physical_candidate: None,
            view_coverage: None,
        });
    }
    let read_id = nodes.len();
    nodes.push(PhysicalASAPDAGNode {
        id: read_id,
        payload: PhysicalASAPOperatorPayload::ASAP(ASAPOp::FinalizeExactAccumulator {
            child: read_id - 1,
        }),
        output_state: ExecutionDataState::INGESTION_ROWS,
        lifetime: Default::default(),
        parameters: None,
        output_schema: logical_schema(FieldDataType::Plain(DataType::Float64)),
        retention_ms: None,
        selected_physical_candidate: None,
        view_coverage: None,
    });
    if let Some(target) = target {
        nodes.push(PhysicalASAPDAGNode {
            id: nodes.len(),
            payload: PhysicalASAPOperatorPayload::ASAP(ASAPOp::SummaryAgg {
                summary_type: target.clone(),
                input: SummaryUpdate::column(ColumnRef::SampleValue),
                reduction: Reduction::by(vec![]),
                filter: None,
                child: read_id,
            }),
            output_state: ExecutionDataState::INGESTION_SUMMARY,
            lifetime: planner_types::stages::Lifetime::AcrossEvaluations(
                planner_types::stages::StorageMedium::Memory,
            ),
            parameters: None,
            output_schema: logical_schema(target),
            retention_ms: None,
            selected_physical_candidate: None,
            view_coverage: None,
        });
    }
    let root = nodes.last().unwrap().id;
    nodes.extend([scan_node, range_node]);
    let mut edges: Vec<_> = (1..scan)
        .map(|i| edge(&nodes, i - 1, i, EdgeRole::Input))
        .collect();
    edges.push(edge(&nodes, scan, scan + 1, EdgeRole::Input));
    edges.push(edge(&nodes, scan + 1, 0, EdgeRole::Input));
    let mut dag = PhysicalASAPDAG {
        nodes,
        edges,
        roots: vec![root],
        scalar_roots: vec![],
    };
    asap_executor::physical_planner::test_support::set_build_parameters(&mut dag.nodes, None);
    let views = views_at(&mut dag, &[0])?;
    Ok((compile(&dag, &views, &[root as u64])?, views[0].clone()))
}
fn native_run(
    (program, view): &(CompiledPhysicalDAG, MaterializedView),
    family: FieldDataType,
    states: Vec<Arc<dyn asap_executor::AggregateCore>>,
    context: RunContext,
) -> Result<Vec<Vec<Value>>, asap_executor::Error> {
    let (_, schema) = program.input_slots().next().unwrap();
    let rows = states
        .into_iter()
        .map(|state| {
            vec![Value::Summary {
                family: family.clone(),
                state,
            }]
        })
        .collect();
    let input = Batch::try_new(schema.clone(), rows)?;
    let mut store = InMemoryDataStore::default();
    // Every run reads the view's one-minute window ending at the ingestion
    // run's end.
    store.append_materialized_view(view, (2000 - 60_000, 2000), vec![input])?;
    let physical_dag = program.instantiate(Arc::new(store))?;
    let batches = crate::common::run_roots(&physical_dag, program.roots(), context)?.remove(0);
    Ok(batches.iter().flat_map(Batch::to_rows).collect())
}
fn ingestion_context(limits: Limits) -> RunContext {
    RunContext::new(
        Scope::Ingestion {
            window_start_ms: 0,
            window_end_ms: 2000,
        },
        limits,
    )
    .unwrap()
}
fn sum_state(value: f64) -> Arc<dyn asap_executor::AggregateCore> {
    let mut state = asap_executor::summary_kernels::exact::ExactAccumulator::new(
        planner_types::ir::schema::FieldDataType::ExactAggregate(
            planner_types::summary_type::ExactKind::Sum,
            planner_types::summary_type::ExactParams::Sum,
        ),
        false,
    )
    .unwrap();
    state.update(None, value, 0);
    Arc::new(state)
}

// Only an explicit merge may collapse distinct tumbling window updates before finalization.
#[test]
fn precompute_merge_requires_a_family_with_a_sound_merge() {
    // IRate reads the window's last two samples, so the family-capability
    // table gives it no merge. No summary build produces IRate state either,
    // so the DAG is rejected when its state is built, before the merge.
    let irate = FieldDataType::ExactAggregate(ExactKind::IRate, ExactParams::IRate);
    let error = try_state_dag(irate, None, true).err().expect("rejected");
    assert!(error.to_string().contains("IRate"), "{error}");
    let sum = FieldDataType::ExactAggregate(ExactKind::Sum, ExactParams::Sum);
    try_state_dag(sum, None, true).unwrap();
}

#[test]
fn explicit_merge_changes_tumbling_window_cardinality() {
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
            .map(|row| match row[0] {
                Value::Float64(v) => v,
                _ => panic!("numeric evaluation expected"),
            })
            .collect::<Vec<_>>();
        assert_eq!(values, expected);
    }
}

// Typed updates reject invalid domains before publishing any target state:
// a DDSketch takes zero and negative values (its zero bucket and negative
// store), and refuses NaN, ±Inf and magnitudes beyond its indexable range.
#[test]
fn precompute_rejects_nonfinite_and_out_of_range_dds_updates() {
    let source = FieldDataType::ExactAggregate(ExactKind::Sum, ExactParams::Sum);
    let target = FieldDataType::Sketch(SketchAlgorithm::DDSketch);
    let program = state_dag(source.clone(), Some(target), false);
    for value in [20., -20., 0.] {
        assert!(
            native_run(
                &program,
                source.clone(),
                vec![sum_state(value)],
                ingestion_context(Limits::default())
            )
            .is_ok(),
            "{value}"
        );
    }
    for value in [
        f64::MAX,
        -f64::MAX,
        f64::NAN,
        f64::INFINITY,
        f64::NEG_INFINITY,
    ] {
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
    use asap_executor::summary_kernels::exact::ExactAccumulator;
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
        ingestion_context(Limits { max_bytes: 1 }),
    )
    .unwrap_err();
    assert!(format!("{error:?}").contains("MemoryLimit"));
}

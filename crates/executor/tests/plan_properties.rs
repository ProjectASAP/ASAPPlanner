//! A blocking parent over a raw data slot is finite, and binding it opens no reader.
use asap_executor::{
    physical_planner::compile,
    plan::{Boundedness, Emission},
    runtime::{Limits, OutputStream, RunContext, Scope},
    sources::{DataStore, MaterializedViewRequest, RawDataRequest},
    values::Batch,
    Error,
};
use planner_types::ir::materialized_view::MaterializedView;
use planner_types::ir::operator::{GroupKeys, Source};
use planner_types::ir::physical_export::{
    EdgeRole, GroupingEdgeCompatibility, PhysicalASAPDAG, PhysicalASAPDAGEdge, PhysicalASAPDAGNode,
    PhysicalASAPOperatorPayload, WindowEdgeCompatibility,
};
use planner_types::ir::properties::ExecutionDataState;
use planner_types::ir::schema::{DataType, FieldDataType, Schema};
use planner_types::ir::NonASAPOp;
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};
/// A store of empty raw data that counts the reads it opens.
struct CountingStore {
    opens: Arc<AtomicUsize>,
}
impl DataStore for CountingStore {
    fn read_raw_data(
        &self,
        _: &Source,
        _: &RawDataRequest,
        _: RunContext,
    ) -> Result<OutputStream<'_, Batch>, Error> {
        self.opens.fetch_add(1, Ordering::SeqCst);
        Ok(Box::pin(futures::stream::empty()))
    }
    fn read_materialized_view(
        &self,
        view: &MaterializedView,
        _: &MaterializedViewRequest,
        _: RunContext,
    ) -> Result<OutputStream<'_, Batch>, Error> {
        Err(Error::Invalid(format!("no view {}", view.node_id)))
    }
}
/// `Sort(Scan(t))` over `schema`.
fn sorted_scan(schema: &Schema, identity: &Source) -> PhysicalASAPDAG {
    let node = |id, payload| PhysicalASAPDAGNode {
        id,
        payload,
        output_state: ExecutionDataState::QUERY_ROWS,
        materialization: Default::default(),
        output_schema: schema.clone(),
        guarantee: None,
        retention_ms: None,
    };
    PhysicalASAPDAG {
        nodes: vec![
            node(
                0,
                PhysicalASAPOperatorPayload::NonASAP(NonASAPOp::Scan {
                    source: identity.clone(),
                    schema: schema.clone(),
                    predicates: vec![],
                }),
            ),
            node(
                1,
                PhysicalASAPOperatorPayload::NonASAP(NonASAPOp::Sort {
                    keys: vec![planner_types::ir::SortKey {
                        expr: planner_types::ir::ScalarExpr::Column(0),
                        ascending: true,
                        nulls_first: false,
                    }],
                    partition_by: GroupKeys::by(vec![]),
                    child: 0,
                }),
            ),
        ],
        edges: vec![PhysicalASAPDAGEdge::new(
            0,
            1,
            EdgeRole::Input,
            schema.clone(),
            ExecutionDataState::QUERY_ROWS,
            GroupingEdgeCompatibility::NotApplicable,
            WindowEdgeCompatibility::NotApplicable,
        )],
        roots: vec![1],
        scalar_roots: vec![],
    }
}
// A blocking parent over a raw data slot is Bounded and emits after its
// input: the `Scan` reads a finite request. Instantiation opens no reader;
// the run opens it once.
#[test]
fn blocking_inputs_over_a_raw_data_slot_are_finite() {
    let logical = Schema::new(vec![planner_types::ir::schema::Field::plain(
        "v",
        DataType::Int64,
        false,
    )]);
    let identity = Source::Table {
        table_ref: "t".into(),
    };
    let compiled = compile(&sorted_scan(&logical, &identity), &[], &[1]).unwrap();
    let opens = Arc::new(AtomicUsize::new(0));
    let dag = compiled
        .instantiate(Arc::new(CountingStore {
            opens: opens.clone(),
        }))
        .unwrap();
    let properties = dag.properties(&[1]).unwrap();
    assert_eq!(properties[&1].emission, Emission::AfterInput);
    assert_eq!(properties[&1].boundedness, Boundedness::Bounded);
    assert_eq!(opens.load(Ordering::SeqCst), 0);
    let run = RunContext::new(
        Scope::Query {
            evaluation_time_ms: 0,
        },
        Limits::default(),
    )
    .unwrap();
    let rows: Vec<_> = futures::executor::block_on(futures::StreamExt::collect::<Vec<_>>(
        dag.execute(&[1], run).unwrap().remove(0),
    ));
    assert!(rows.iter().all(Result::is_ok));
    assert_eq!(opens.load(Ordering::SeqCst), 1);
}

/// A KLL kernel accepts `k` only up to the 26 602 `asap_sketchlib` caps it at.
#[test]
fn kll_kernel_rejects_a_k_above_the_library_cap() {
    use asap_executor::capability::validate_summary_kernel;
    use planner_types::ir::scalar::ColumnRef;
    use planner_types::ir::schema::{
        GroupingStrategy, SketchAlgorithm, SketchKind, SketchParams, SummaryInputExpr,
        SummaryUpdate,
    };
    let grouping = GroupingStrategy::default();
    let update = SummaryUpdate {
        item: None,
        weight: SummaryInputExpr::Column(ColumnRef::SampleValue),
        weight_domain: Default::default(),
    };
    let kll = |k| {
        FieldDataType::Sketch(
            SketchKind::new(
                SketchAlgorithm::Kll,
                SketchParams::Kll {
                    k,
                    failure_probability: None,
                },
            ),
            grouping.clone(),
        )
    };
    assert!(validate_summary_kernel(&kll(26_602), &update, &grouping).is_ok());
    assert!(validate_summary_kernel(&kll(26_603), &update, &grouping).is_err());
}

/// A plain CountSketch reads the median of its rows, so plan validation and
/// kernel construction both require a positive odd depth.
#[test]
fn count_sketch_kernel_requires_an_odd_depth() {
    use asap_executor::{capability::validate_summary_kernel, factory::create_planner_accumulator};
    use planner_types::ir::scalar::ColumnRef;
    use planner_types::ir::schema::{
        GroupingStrategy, SketchAlgorithm, SketchKind, SketchParams, SummaryInputExpr,
        SummaryUpdate,
    };
    let grouping = GroupingStrategy::default();
    let mut update = SummaryUpdate::column(ColumnRef::SampleValue);
    update.item = Some(SummaryInputExpr::Column(ColumnRef::Named("host".into())));
    let count_sketch = |depth| {
        FieldDataType::Sketch(
            SketchKind::new(
                SketchAlgorithm::CountSketch,
                SketchParams::CountSketch { width: 64, depth },
            ),
            grouping.clone(),
        )
    };
    for (depth, valid) in [(5, true), (4, false)] {
        let family = count_sketch(depth);
        assert_eq!(
            validate_summary_kernel(&family, &update, &grouping).is_ok(),
            valid
        );
        assert_eq!(
            create_planner_accumulator(&family, &update, &grouping).is_ok(),
            valid
        );
    }
}

// Kernel support must not be mistaken for executable native state/evaluation support.
#[test]
fn summary_capability_levels_are_distinct() {
    use asap_executor::{
        capability::{validate_native_family, validate_sketch_evaluation, validate_summary_kernel},
        planner::ir::schema::SketchStatistic,
    };
    use planner_types::ir::scalar::ColumnRef;
    use planner_types::ir::schema::{
        GroupingStrategy, SketchAlgorithm, SketchKind, SketchParams, SummaryUpdate,
    };
    let grouping = GroupingStrategy::default();
    let cms = FieldDataType::Sketch(
        SketchKind::new(
            SketchAlgorithm::Cms,
            SketchParams::Cms {
                width: 64,
                depth: 4,
            },
        ),
        grouping.clone(),
    );
    let update = SummaryUpdate {
        item: Some(planner_types::ir::schema::SummaryInputExpr::Column(
            ColumnRef::Named("host".into()),
        )),
        weight: planner_types::ir::schema::SummaryInputExpr::Constant(1.0),
        weight_domain: Default::default(),
    };
    assert!(validate_summary_kernel(&cms, &update, &grouping).is_ok());
    // Stored Count-Min state reads only its bare count natively.
    assert!(validate_native_family(&cms).is_ok());
    let bare_count = SketchStatistic::PointCount {
        key: ColumnRef::SampleValue,
        value: None,
    };
    assert!(validate_sketch_evaluation(&cms, &bare_count).is_ok());
    assert!(validate_sketch_evaluation(
        &cms,
        &SketchStatistic::PointCount {
            key: ColumnRef::Named("host".into()),
            value: Some("a".into()),
        }
    )
    .is_err());
    let kll = FieldDataType::Sketch(
        SketchKind::new(
            SketchAlgorithm::Kll,
            SketchParams::Kll {
                k: 128,
                failure_probability: None,
            },
        ),
        grouping,
    );
    assert!(validate_native_family(&kll).is_ok());
    assert!(validate_sketch_evaluation(&kll, &SketchStatistic::Quantile { q: 1.5 }).is_err());
    assert!(validate_sketch_evaluation(&kll, &SketchStatistic::Cardinality).is_err());
    assert!(validate_sketch_evaluation(&kll, &SketchStatistic::Quantile { q: 0.5 }).is_ok());
}

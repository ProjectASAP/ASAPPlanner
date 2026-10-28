//! Finite-input requirements are validated before source execution.
use asap_executor::{
    physical_planner::compile,
    plan::{Boundedness, Emission},
    runtime::{Limits, OutputStream, RunContext, Scope},
    sources::{BindingDataSources, ScanRequest, TableProvider},
    values::{Batch, SchemaRef},
    Error,
};
use planner_types::ir::operator::{GroupKeys, Source};
use planner_types::ir::physical_export::{
    EdgeRole, GroupingEdgeCompatibility, PhysicalASAPDAG, PhysicalASAPDAGEdge, PhysicalASAPDAGNode,
    PhysicalASAPOperatorPayload, WindowEdgeCompatibility,
};
use planner_types::ir::properties::ExecutionDataState;
use planner_types::ir::schema::{DataType, FieldDataType, Schema};
use planner_types::ir::NonASAPOp;
use std::collections::BTreeSet;
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};
struct DeclaredSource {
    schema: SchemaRef,
    boundedness: Boundedness,
    opens: Arc<AtomicUsize>,
}
impl TableProvider for DeclaredSource {
    fn schema(&self) -> SchemaRef {
        self.schema.clone()
    }
    fn boundedness(&self) -> Boundedness {
        self.boundedness
    }
    fn scan(&self, _: &ScanRequest, _: RunContext) -> Result<OutputStream<'_, Batch>, Error> {
        self.opens.fetch_add(1, Ordering::SeqCst);
        Ok(Box::pin(futures::stream::empty()))
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
        edges: vec![PhysicalASAPDAGEdge {
            producer: 0,
            consumer: 1,
            role: EdgeRole::Input,
            intermediate_schema: schema.clone(),
            data_state: ExecutionDataState::QUERY_ROWS,
            grouping: GroupingEdgeCompatibility::NotApplicable,
            window: WindowEdgeCompatibility::NotApplicable,
        }],
        roots: vec![1],
        scalar_roots: vec![],
    }
}
// A blocking parent over a table slot runs only over a provider that
// declares Bounded; an unknown or unbounded one is rejected at
// instantiation, without opening a reader.
#[test]
fn blocking_inputs_require_an_explicit_finite_source() {
    let logical = Schema::new(vec![planner_types::ir::schema::Field::plain(
        "v",
        DataType::Int64,
        false,
    )]);
    let schema = Arc::new(logical.clone());
    let identity = Source::Table {
        table_ref: "t".into(),
    };
    let compiled = compile(&sorted_scan(&logical, &identity), &BTreeSet::new(), &[1]).unwrap();
    for boundedness in [
        Boundedness::Unknown,
        Boundedness::Unbounded,
        Boundedness::Bounded,
    ] {
        let opens = Arc::new(AtomicUsize::new(0));
        let mut data_sources = BindingDataSources::default();
        data_sources
            .tables
            .register(
                identity.clone(),
                Arc::new(DeclaredSource {
                    schema: schema.clone(),
                    boundedness,
                    opens: opens.clone(),
                }),
            )
            .unwrap();
        let run = RunContext::new(
            Scope::Query {
                evaluation_time_ms: 0,
                revision: 0,
            },
            Limits::default(),
        )
        .unwrap();
        match compiled.instantiate(&data_sources) {
            Ok(dag) => {
                assert_eq!(boundedness, Boundedness::Bounded);
                let properties = dag.properties(&[1]).unwrap();
                assert_eq!(properties[&1].emission, Emission::AfterInput);
                assert_eq!(properties[&1].boundedness, Boundedness::Bounded);
                assert!(dag.execute(&[1], run).is_ok());
            }
            Err(error) => {
                assert_ne!(boundedness, Boundedness::Bounded);
                assert!(
                    error.to_string().contains("does not declare Bounded"),
                    "{error}"
                );
            }
        }
        assert_eq!(opens.load(Ordering::SeqCst), 0);
    }
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

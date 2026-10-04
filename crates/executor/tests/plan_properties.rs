//! Finite-input contracts are validated before source execution.
use asap_executor::{
    operators::{Operator, SortKey},
    plan::{Boundedness, Emission, PhysicalDAG},
    runtime::{Limits, OutputStream, RunContext, Scope},
    sources::{DataSources, RawSource},
    values::{Batch, SchemaRef},
    Error,
};
use planner_types::ir::operator::Source;
use planner_types::ir::schema::{DataType, Field, FieldDataType, Schema};
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};
struct DeclaredSource {
    schema: SchemaRef,
    boundedness: Boundedness,
    opens: Arc<AtomicUsize>,
}
impl RawSource for DeclaredSource {
    fn schema(&self) -> SchemaRef {
        self.schema.clone()
    }
    fn boundedness(&self) -> Boundedness {
        self.boundedness
    }
    fn scan(&self, _: RunContext) -> Result<OutputStream<'_, Batch>, Error> {
        self.opens.fetch_add(1, Ordering::SeqCst);
        Ok(Box::pin(futures::stream::empty()))
    }
}
// A blocking parent must reject unknown and unbounded Scan inputs without opening a reader.
#[test]
fn blocking_inputs_require_an_explicit_finite_source() {
    let schema = Arc::new(planner_types::ir::schema::Schema {
        unique_keys: vec![],
        closed: false,
        fields: vec![Field {
            table: None,
            name: "v".into(),
            dtype: FieldDataType::Plain(DataType::Int64),
            nullable: false,
        }],
        time_index: None,
    });
    for boundedness in [
        Boundedness::Unknown,
        Boundedness::Unbounded,
        Boundedness::Bounded,
    ] {
        let opens = Arc::new(AtomicUsize::new(0));
        let mut registry = DataSources::default();
        let identity = Source::Table {
            table_ref: "t".into(),
        };
        registry
            .register(
                identity.clone(),
                Arc::new(DeclaredSource {
                    schema: schema.clone(),
                    boundedness,
                    opens: opens.clone(),
                }),
            )
            .unwrap();
        let scan = registry
            .bind(
                &planner_types::ir::OperatorNode::new_shared(planner_types::ir::Operator::NonASAP(
                    planner_types::ir::NonASAPOp::Scan {
                        source: identity,
                        schema: Schema::new(vec![planner_types::ir::schema::Field::plain(
                            "v",
                            DataType::Int64,
                            false,
                        )]),
                        predicates: vec![],
                    },
                ))
                .unwrap(),
            )
            .unwrap();
        let mut dag = PhysicalDAG::default();
        dag.add(0, vec![], scan).unwrap();
        dag.add(
            1,
            vec![0],
            Operator::sort(
                schema.clone(),
                vec![SortKey {
                    column: 0,
                    descending: false,
                    nulls_first: false,
                }],
                vec![],
            )
            .unwrap(),
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
        if boundedness == Boundedness::Bounded {
            let properties = dag.properties(&[1]).unwrap();
            assert_eq!(properties[&1].emission, Emission::AfterInput);
            assert_eq!(properties[&1].boundedness, Boundedness::Bounded);
            assert!(dag.execute(&[1], run).is_ok());
        } else {
            assert!(
                matches!(dag.execute(&[1], run), Err(Error::Invalid(message)) if message.contains("requires bounded inputs"))
            );
        }
        assert_eq!(opens.load(Ordering::SeqCst), 0);
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
        SketchKind::new(SketchAlgorithm::Kll, SketchParams::Kll { k: 128 }),
        grouping,
    );
    assert!(validate_native_family(&kll).is_ok());
    assert!(validate_sketch_evaluation(&kll, &SketchStatistic::Quantile { q: 1.5 }).is_err());
    assert!(validate_sketch_evaluation(&kll, &SketchStatistic::Cardinality).is_err());
    assert!(validate_sketch_evaluation(&kll, &SketchStatistic::Quantile { q: 0.5 }).is_ok());
}

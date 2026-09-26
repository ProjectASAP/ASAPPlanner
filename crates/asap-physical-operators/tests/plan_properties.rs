//! Finite-input contracts are validated before source execution.
use asap_physical_operators::{
    operators::{Operator, SortKey},
    plan::{Boundedness, Emission, PhysicalDag},
    runtime::{Limits, OutputStream, RunContext, Scope},
    sources::{DataSources, RawSource},
    values::{Batch, Schema},
    Error,
};
use planner_types::{
    post_asap::{SummaryFamilyType, SummaryField, SummarySchema},
    pre_asap::{Column, DataType, QueryExpr, Schema as LogicalSchema, Source},
};
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};
struct DeclaredSource {
    schema: Schema,
    boundedness: Boundedness,
    opens: Arc<AtomicUsize>,
}
impl RawSource for DeclaredSource {
    fn schema(&self) -> Schema {
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
    let schema = Arc::new(SummarySchema {
        fields: vec![SummaryField {
            name: "v".into(),
            dtype: SummaryFamilyType::Plain(DataType::Int64),
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
            .bind(&QueryExpr::Scan {
                source: identity,
                schema: LogicalSchema::new(vec![Column::new("v", DataType::Int64, false)]),
                predicates: vec![],
            })
            .unwrap();
        let mut dag = PhysicalDag::default();
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

// Kernel support must not be mistaken for executable native state/readout support.
#[test]
fn summary_capability_levels_are_distinct() {
    use asap_physical_operators::{
        capability::{validate_native_family, validate_native_readout, validate_summary_kernel},
        Statistic,
    };
    use planner_types::{
        post_asap::{GroupingStrategy, SketchAlgorithm, SketchKind, SketchParams, SummaryUpdate},
        pre_asap::ColumnRef,
    };
    let grouping = GroupingStrategy::default();
    let cms = SummaryFamilyType::Sketch(
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
        item: Some(planner_types::post_asap::SummaryInputExpr::Column(
            ColumnRef::Named("host".into()),
        )),
        weight: planner_types::post_asap::SummaryInputExpr::Constant(1.0),
        weight_domain: Default::default(),
    };
    assert!(validate_summary_kernel(&cms, &update, &grouping).is_ok());
    assert!(validate_native_family(&cms).is_err());
    let kll = SummaryFamilyType::Sketch(
        SketchKind::new(SketchAlgorithm::Kll, SketchParams::Kll { k: 128 }),
        grouping,
    );
    assert!(validate_native_family(&kll).is_ok());
    assert!(validate_native_readout(&kll, Statistic::Quantile, &Default::default()).is_err());
    assert!(validate_native_readout(
        &kll,
        Statistic::Quantile,
        &[("quantile".into(), "0.5".into())].into()
    )
    .is_ok());
}

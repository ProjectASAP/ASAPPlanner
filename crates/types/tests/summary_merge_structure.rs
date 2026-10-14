//! Window composition merges compatible summary states without consuming raw rows.
use asap_types::{
    ir::operator_properties::{Reduction, Source},
    ir::{apply_materialization_timings, MaterializationAssignment, TimingMemo},
    ir::{ASAPOp, ExprSemantics, NonASAPOp, Operator, OperatorNode, Predicate, ScalarExpr},
    post_asap::{
        ExactKind, ExactParams, ExecutionTiming, FamilyCapability, GroupingStrategy, HydraKind,
        HydraParams, Materialization, Medium, MergedError, SelectionRelation, SketchAlgorithm,
        SketchKind, SketchParams, SummaryUpdate,
    },
    pre_asap::expr_ir::{CompareOpKind, ScalarValue},
    pre_asap::{ColumnRef, DataType, Field, FieldDataType, Schema},
};
use std::rc::Rc;
/// KLL over `value` for one `region`, so states of different regions are
/// disjoint.
fn state(k: u32, region: &str) -> Rc<OperatorNode> {
    family_state(
        FieldDataType::Sketch(
            SketchKind::new(
                SketchAlgorithm::Kll,
                SketchParams::Kll {
                    k,
                    failure_probability: None,
                },
            ),
            Default::default(),
        ),
        region,
    )
}
/// A `family` state over `value` for one `region`.
fn family_state(family: FieldDataType, region: &str) -> Rc<OperatorNode> {
    let scan = OperatorNode::new_shared(Operator::NonASAP(NonASAPOp::Scan {
        source: Source::Table {
            table_ref: "latencies".into(),
        },
        predicates: vec![],
        schema: Schema::new(vec![
            Field::plain("region", DataType::Utf8, false),
            Field::plain("value", DataType::Float64, false),
        ]),
    }))
    .unwrap();
    let only_region = OperatorNode::new_shared(Operator::NonASAP(NonASAPOp::Filter {
        pred: Predicate(ScalarExpr::Compare {
            left: Box::new(ScalarExpr::Column(0)),
            op: CompareOpKind::Eq,
            right: Box::new(ScalarExpr::Literal(ScalarValue::Utf8(region.into()))),
            semantics: ExprSemantics::Sql,
        }),
        child: scan,
    }))
    .unwrap();
    OperatorNode::new_shared(Operator::ASAP(ASAPOp::SummaryAgg {
        child: only_region,
        family,
        input: SummaryUpdate::column(ColumnRef::SampleValue),
        reduction: Reduction::by(vec![]),
        grouping: GroupingStrategy::default(),
        filter: None,
    }))
    .unwrap()
}
/// Two KLL panes compose into one typed logical state without timing assignment.
#[test]
fn compatible_panes_merge_structurally() {
    let root = OperatorNode::new_shared(Operator::ASAP(ASAPOp::SummaryMerge {
        children: vec![state(200, "us"), state(200, "eu")],
    }))
    .unwrap();
    root.validate_structure().unwrap();
    assert_eq!(root.schema.fields.len(), 1);
}
/// An empty merge, raw rows and differently sized state cannot masquerade as compatible panes.
#[test]
fn incompatible_merge_inputs_fail() {
    for children in [
        vec![],
        vec![state(200, "us"), state(300, "eu")],
        vec![state(200, "us").children()[0].clone()],
    ] {
        assert!(
            OperatorNode::new_shared(Operator::ASAP(ASAPOp::SummaryMerge { children })).is_err()
        );
    }
}

fn sketch(algorithm: SketchAlgorithm, params: SketchParams) -> FieldDataType {
    FieldDataType::Sketch(SketchKind::new(algorithm, params), Default::default())
}
fn hydra(
    algorithm: SketchAlgorithm,
    params: SketchParams,
    kind: HydraKind,
    hydra_params: HydraParams,
) -> FieldDataType {
    FieldDataType::Sketch(
        SketchKind::new(algorithm, params),
        GroupingStrategy::SharedMultiSubpopulation {
            kind,
            params: hydra_params,
        },
    )
}
fn exact(kind: ExactKind, params: ExactParams) -> FieldDataType {
    FieldDataType::ExactAggregate(kind, params)
}
fn kll() -> FieldDataType {
    sketch(
        SketchAlgorithm::Kll,
        SketchParams::Kll {
            k: 200,
            failure_probability: None,
        },
    )
}
fn hll() -> FieldDataType {
    sketch(SketchAlgorithm::Hll, SketchParams::Hll { precision: 12 })
}
fn cms_with_heap() -> FieldDataType {
    sketch(
        SketchAlgorithm::CmsWithHeap,
        SketchParams::CmsWithHeap {
            width: 64,
            depth: 4,
            heap_size: 10,
        },
    )
}
fn merge(children: Vec<Rc<OperatorNode>>) -> Result<Rc<OperatorNode>, String> {
    OperatorNode::new_shared(Operator::ASAP(ASAPOp::SummaryMerge { children }))
        .map_err(|e| e.to_string())
}

/// The family-capability table: which families merge, how their inputs'
/// selections must relate, how the merged error relates to the parts', and
/// which subtract.
#[test]
fn family_capability_table() {
    use MergedError::{Exact, SameAsUnionBuild};
    use SelectionRelation::{Disjoint, OverlapAllowed};
    let (width, depth, heap_size) = (64, 4, 10);
    for (family, merge, subtractable) in [
        (
            exact(ExactKind::Sum, ExactParams::Sum),
            Some((Disjoint, Exact)),
            true,
        ),
        (
            exact(ExactKind::Count, ExactParams::Count),
            Some((Disjoint, Exact)),
            true,
        ),
        (
            exact(ExactKind::Min, ExactParams::Min),
            Some((OverlapAllowed, Exact)),
            false,
        ),
        (
            exact(ExactKind::Max, ExactParams::Max),
            Some((OverlapAllowed, Exact)),
            false,
        ),
        (exact(ExactKind::Rate, ExactParams::Rate), None, false),
        (
            exact(ExactKind::Increase, ExactParams::Increase),
            None,
            false,
        ),
        (exact(ExactKind::IRate, ExactParams::IRate), None, false),
        (kll(), Some((Disjoint, SameAsUnionBuild)), false),
        (
            sketch(
                SketchAlgorithm::DDSketch,
                SketchParams::DDSketch { alpha: 0.01 },
            ),
            Some((Disjoint, SameAsUnionBuild)),
            false,
        ),
        (hll(), Some((OverlapAllowed, SameAsUnionBuild)), false),
        (
            sketch(SketchAlgorithm::Cms, SketchParams::Cms { width, depth }),
            Some((Disjoint, SameAsUnionBuild)),
            true,
        ),
        (
            sketch(
                SketchAlgorithm::CountSketch,
                SketchParams::CountSketch { width, depth },
            ),
            Some((Disjoint, SameAsUnionBuild)),
            true,
        ),
        (
            sketch(
                SketchAlgorithm::UnivMon,
                SketchParams::UnivMon {
                    heap_size,
                    sketch_rows: depth,
                    sketch_cols: width,
                    layers: 8,
                },
            ),
            Some((Disjoint, SameAsUnionBuild)),
            false,
        ),
        (cms_with_heap(), None, false),
        (
            sketch(
                SketchAlgorithm::CountSketchWithHeap,
                SketchParams::CountSketchWithHeap {
                    width,
                    depth,
                    heap_size,
                },
            ),
            None,
            false,
        ),
        // Hydra over a linear sketch is a grid of linear sketches; Hydra
        // over KLL has no accuracy model and fails closed.
        (
            hydra(
                SketchAlgorithm::Cms,
                SketchParams::Cms { width, depth },
                HydraKind::HydraCms,
                HydraParams::HydraCms {
                    width,
                    depth,
                    shared_rows: depth,
                    shared_columns: width,
                },
            ),
            Some((Disjoint, SameAsUnionBuild)),
            true,
        ),
        (
            hydra(
                SketchAlgorithm::CountSketch,
                SketchParams::CountSketch { width, depth },
                HydraKind::HydraCountSketch,
                HydraParams::HydraCountSketch {
                    width,
                    depth,
                    shared_rows: depth,
                    shared_columns: width,
                },
            ),
            Some((Disjoint, SameAsUnionBuild)),
            true,
        ),
        (
            hydra(
                SketchAlgorithm::Kll,
                SketchParams::Kll {
                    k: 200,
                    failure_probability: None,
                },
                HydraKind::HydraKll,
                HydraParams::HydraKll {
                    k: 200,
                    shared_buckets: width,
                },
            ),
            None,
            false,
        ),
        (FieldDataType::Plain(DataType::Float64), None, false),
    ] {
        let capability = FamilyCapability::of(&family);
        assert_eq!(
            capability.merge.map(|m| (m.inputs, m.error)),
            merge,
            "{family:?}"
        );
        assert_eq!(capability.subtractable, subtractable, "{family:?}");
    }
}

/// The IR does not reject a merge by family: a heap state has no sound
/// merge, but only the table's consumers (the planner rules and the
/// executor) decide that.
#[test]
fn summary_merge_is_permissive_about_the_family() {
    let merged = merge(vec![
        family_state(cms_with_heap(), "us"),
        family_state(cms_with_heap(), "eu"),
    ])
    .unwrap();
    merged.validate_structure().unwrap();
    assert_eq!(FamilyCapability::of(&cms_with_heap()).merge, None);
}

/// Idempotent families (HLL, exact Min/Max) merge states that may share
/// rows, and the union of identical selections is that selection; counting
/// families (exact Sum, KLL) still need disjoint selections.
#[test]
fn idempotent_families_merge_overlapping_states() {
    let same_region = |family: FieldDataType| {
        merge(vec![
            family_state(family.clone(), "us"),
            family_state(family, "us"),
        ])
    };
    for family in [hll(), exact(ExactKind::Max, ExactParams::Max)] {
        let merged = same_region(family.clone()).unwrap_or_else(|e| panic!("{family:?}: {e}"));
        merged.validate_structure().unwrap();
        assert_eq!(
            merged.coverage().unwrap().selection,
            family_state(family, "us").coverage().unwrap().selection
        );
    }
    for family in [exact(ExactKind::Sum, ExactParams::Sum), kll()] {
        let error = same_region(family).unwrap_err();
        assert!(error.contains("not proven disjoint"), "{error}");
    }
}

/// `SummaryMerge` takes the timing of its consumer, and reads pane states
/// stored at ingestion time (#509 Example 3/4 Pattern B1) or built at query
/// time (B2). Merging at ingestion time needs every pane at ingestion time.
#[test]
fn summary_merge_timing() {
    let panes = [state(200, "us"), state(200, "eu")];
    let root = merge(panes.to_vec()).unwrap();
    let timings = |assignment: &MaterializationAssignment| {
        let timed =
            apply_materialization_timings(&root, assignment, &mut TimingMemo::new()).unwrap();
        let children = timed.children();
        (
            timed.timing.unwrap(),
            children
                .iter()
                .map(|c| c.timing.unwrap())
                .collect::<Vec<_>>(),
        )
    };
    use ExecutionTiming::{IngestionTime, QueryTime};
    assert_eq!(
        timings(&MaterializationAssignment::default()),
        (QueryTime, vec![QueryTime, QueryTime])
    );
    let at_ingestion = Materialization::MatAtIngestion(Medium::Memory);
    let mut stored_panes = MaterializationAssignment::default();
    for pane in &panes {
        stored_panes.set(pane, at_ingestion);
    }
    assert_eq!(
        timings(&stored_panes),
        (QueryTime, vec![IngestionTime, IngestionTime])
    );
    // A `SummaryAgg` without an assignment runs at query time, so a merge
    // stored at ingestion time needs its panes there too.
    let mut only_merge = MaterializationAssignment::default();
    only_merge.set(&root, at_ingestion);
    assert!(apply_materialization_timings(&root, &only_merge, &mut TimingMemo::new()).is_err());
    let mut stored_merge = MaterializationAssignment::with_default(at_ingestion);
    stored_merge.set(&root, at_ingestion);
    assert_eq!(
        timings(&stored_merge),
        (IngestionTime, vec![IngestionTime, IngestionTime])
    );
}

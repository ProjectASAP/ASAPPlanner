//! Window composition merges compatible summary states without consuming raw rows.
use asap_types::ir::operator::{Reduction, Source};
use asap_types::ir::scalar::ColumnRef;
use asap_types::ir::scalar::{CompareOpKind, ScalarValue};
use asap_types::ir::schema::{DataType, Field, FieldDataType, Schema};
use asap_types::ir::schema::{
    GroupingStrategy, SketchAlgorithm, SketchKind, SketchParams, SummaryUpdate,
};
use asap_types::ir::{
    ASAPOp, ExprSemantics, NonASAPOp, Operator, OperatorNode, Predicate, ScalarExpr,
};
use std::rc::Rc;
/// KLL over `value` for one `region`, so states of different regions are
/// disjoint.
fn state(k: u32, region: &str) -> Rc<OperatorNode> {
    family_state(
        FieldDataType::Sketch(
            SketchKind::new(SketchAlgorithm::Kll, SketchParams::Kll { k }),
            Default::default(),
        ),
        region,
    )
}
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

fn merge_regions(family: FieldDataType) -> Result<Rc<OperatorNode>, impl std::fmt::Debug> {
    OperatorNode::new_shared(Operator::ASAP(ASAPOp::SummaryMerge {
        children: vec![
            family_state(family.clone(), "us"),
            family_state(family, "eu"),
        ],
    }))
}
/// Heap top-k states have no sound merge model, so disjoint states still cannot
/// merge; KLL states with the same definition can.
#[test]
fn summary_merge_requires_a_mergeable_family() {
    let heap = FieldDataType::Sketch(
        SketchKind::new(
            SketchAlgorithm::CmsWithHeap,
            SketchParams::CmsWithHeap {
                width: 64,
                depth: 4,
                heap_size: 10,
            },
        ),
        Default::default(),
    );
    let error = format!("{:?}", merge_regions(heap).unwrap_err());
    assert!(error.contains("no sound merge"), "{error}");
    let kll = FieldDataType::Sketch(
        SketchKind::new(SketchAlgorithm::Kll, SketchParams::Kll { k: 200 }),
        Default::default(),
    );
    merge_regions(kll).unwrap().validate_structure().unwrap();
}
/// Sound-merge capability is a closed list per family (W6).
#[test]
fn family_merge_capability() {
    use asap_types::ir::schema::{ExactKind, ExactParams};
    let exact = |kind, params| FieldDataType::ExactAggregate(kind, params);
    for family in [
        exact(ExactKind::Sum, ExactParams::Sum),
        exact(ExactKind::Count, ExactParams::Count),
        exact(ExactKind::Min, ExactParams::Min),
        exact(ExactKind::Max, ExactParams::Max),
    ] {
        assert!(family.family_merges(), "{family:?}");
    }
    for family in [
        exact(ExactKind::Rate, ExactParams::Rate),
        exact(ExactKind::Increase, ExactParams::Increase),
        exact(ExactKind::IRate, ExactParams::IRate),
        FieldDataType::Plain(DataType::Float64),
    ] {
        assert!(!family.family_merges(), "{family:?}");
    }
    let sketch = |algorithm, params| {
        FieldDataType::Sketch(SketchKind::new(algorithm, params), Default::default())
    };
    use SketchAlgorithm as A;
    use SketchParams as P;
    let (width, depth, heap_size) = (64, 4, 10);
    for (family, merges) in [
        (sketch(A::Kll, P::Kll { k: 200 }), true),
        (sketch(A::DDSketch, P::DDSketch { alpha: 0.01 }), true),
        (sketch(A::Hll, P::Hll { precision: 12 }), true),
        (sketch(A::Cms, P::Cms { width, depth }), true),
        (
            sketch(A::CountSketch, P::CountSketch { width, depth }),
            true,
        ),
        (
            sketch(
                A::UnivMon,
                P::UnivMon {
                    heap_size,
                    sketch_rows: depth,
                    sketch_cols: width,
                    layers: 8,
                },
            ),
            true,
        ),
        (
            sketch(
                A::CmsWithHeap,
                P::CmsWithHeap {
                    width,
                    depth,
                    heap_size,
                },
            ),
            false,
        ),
        (
            sketch(
                A::CountSketchWithHeap,
                P::CountSketchWithHeap {
                    width,
                    depth,
                    heap_size,
                },
            ),
            false,
        ),
    ] {
        assert_eq!(family.family_merges(), merges, "{family:?}");
    }
}
/// Idempotent families (HLL, exact Min/Max) merge states that may share rows;
/// counting families (KLL, exact Sum) still need disjoint selections.
#[test]
fn idempotent_families_merge_overlapping_states() {
    use asap_types::ir::schema::{ExactKind, ExactParams};
    let hll = FieldDataType::Sketch(
        SketchKind::new(SketchAlgorithm::Hll, SketchParams::Hll { precision: 12 }),
        Default::default(),
    );
    let max = FieldDataType::ExactAggregate(ExactKind::Max, ExactParams::Max);
    let sum = FieldDataType::ExactAggregate(ExactKind::Sum, ExactParams::Sum);
    let kll = FieldDataType::Sketch(
        SketchKind::new(SketchAlgorithm::Kll, SketchParams::Kll { k: 200 }),
        Default::default(),
    );
    let same_region = |family: FieldDataType| {
        OperatorNode::new_shared(Operator::ASAP(ASAPOp::SummaryMerge {
            children: vec![
                family_state(family.clone(), "us"),
                family_state(family, "us"),
            ],
        }))
    };
    for family in [hll, max] {
        let merged = same_region(family.clone()).unwrap_or_else(|e| panic!("{family:?}: {e}"));
        merged.validate_structure().unwrap();
        assert_eq!(
            merged.coverage().unwrap().selection,
            family_state(family, "us").coverage().unwrap().selection
        );
    }
    for family in [sum, kll] {
        assert!(same_region(family).is_err());
    }
}
/// The selection relation a merge requires is declared per family.
#[test]
fn family_merge_selection_relation() {
    use asap_types::ir::schema::{ExactKind, ExactParams, SelectionRelation};
    let exact = |kind, params| FieldDataType::ExactAggregate(kind, params);
    let sketch = |algorithm, params| {
        FieldDataType::Sketch(SketchKind::new(algorithm, params), Default::default())
    };
    for (family, relation) in [
        (
            exact(ExactKind::Sum, ExactParams::Sum),
            Some(SelectionRelation::Disjoint),
        ),
        (
            exact(ExactKind::Count, ExactParams::Count),
            Some(SelectionRelation::Disjoint),
        ),
        (
            exact(ExactKind::Min, ExactParams::Min),
            Some(SelectionRelation::OverlapAllowed),
        ),
        (
            exact(ExactKind::Max, ExactParams::Max),
            Some(SelectionRelation::OverlapAllowed),
        ),
        (exact(ExactKind::Rate, ExactParams::Rate), None),
        (
            sketch(SketchAlgorithm::Kll, SketchParams::Kll { k: 200 }),
            Some(SelectionRelation::Disjoint),
        ),
        (
            sketch(
                SketchAlgorithm::Cms,
                SketchParams::Cms {
                    width: 64,
                    depth: 4,
                },
            ),
            Some(SelectionRelation::Disjoint),
        ),
        (
            sketch(SketchAlgorithm::Hll, SketchParams::Hll { precision: 12 }),
            Some(SelectionRelation::OverlapAllowed),
        ),
    ] {
        assert_eq!(family.merge_relation(), relation, "{family:?}");
    }
}

/// Shared Hydra grids merge only for linear counter cells (#580 W7).
#[test]
fn hydra_merge_capability() {
    use asap_types::ir::schema::{HydraKind, HydraParams};
    let hydra = |algorithm, params, kind, hydra_params| {
        FieldDataType::Sketch(
            SketchKind::new(algorithm, params),
            GroupingStrategy::SharedMultiSubpopulation {
                kind,
                params: hydra_params,
            },
        )
    };
    let (width, depth) = (64, 3);
    let cms = hydra(
        SketchAlgorithm::Cms,
        SketchParams::Cms { width, depth },
        HydraKind::HydraCms,
        HydraParams::HydraCms {
            width,
            depth,
            shared_rows: 3,
            shared_columns: 64,
        },
    );
    assert!(cms.family_merges());
    let kll = hydra(
        SketchAlgorithm::Kll,
        SketchParams::Kll { k: 200 },
        HydraKind::HydraKll,
        HydraParams::HydraKll {
            k: 200,
            shared_buckets: 200,
        },
    );
    assert!(!kll.family_merges());
}

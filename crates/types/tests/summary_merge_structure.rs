//! Window composition merges compatible summary states without consuming raw rows.
use asap_types::ir::operator::operator_properties::{Reduction, Source};
use asap_types::ir::scalar::ColumnRef;
use asap_types::ir::schema::{
    DataType, Field, FieldDataType, GroupingStrategy, Schema, SketchAlgorithm, SketchKind,
    SketchParams, SummaryUpdate,
};
use asap_types::ir::{ASAPOp, NonASAPOp, Operator, OperatorNode};
use std::rc::Rc;
fn state(k: u32) -> Rc<OperatorNode> {
    family_state(
        FieldDataType::Sketch(
            SketchKind::new(SketchAlgorithm::Kll, SketchParams::Kll { k }),
            Default::default(),
        ),
        0..1,
    )
}
fn family_state(family: FieldDataType, time: std::ops::Range<i64>) -> Rc<OperatorNode> {
    let scan = OperatorNode::new_shared(Operator::NonASAP(NonASAPOp::Scan {
        source: Source::Table {
            table_ref: "latencies".into(),
        },
        predicates: vec![],
        schema: Schema::new(vec![Field::plain("value", DataType::Float64, false)]),
    }))
    .unwrap();
    let summary = OperatorNode::new(Operator::ASAP(ASAPOp::SummaryAgg {
        child: scan,
        family,
        input: SummaryUpdate::column(ColumnRef::SampleValue),
        reduction: Reduction::by(vec![]),
        grouping: GroupingStrategy::default(),
        filter: None,
    }))
    .unwrap();
    std::rc::Rc::new(
        summary
            .with_coverage(
                asap_types::ir::properties::summary_coverage::SummaryCoverage {
                    source: Source::Table {
                        table_ref: "latencies".into(),
                    },
                    regions: vec![
                        asap_types::ir::properties::summary_coverage::CoverageRegion {
                            time_ms: Some(time.into()),
                            population: Default::default(),
                        },
                    ],
                },
            )
            .unwrap(),
    )
}
/// Two KLL panes compose into one typed logical state without timing assignment.
#[test]
fn compatible_panes_merge_structurally() {
    let root = OperatorNode::new_shared(Operator::ASAP(ASAPOp::SummaryMerge {
        children: vec![state(200), shifted_state(200, 1, 2)],
    }))
    .unwrap();
    root.validate_structure().unwrap();
    assert_eq!(root.schema.fields.len(), 1);
    assert_eq!(
        root.coverage.as_ref().unwrap().regions[0].time_ms,
        Some((0..2).into())
    );
}
/// An empty merge, raw rows and differently sized state cannot masquerade as compatible panes.
#[test]
fn incompatible_merge_inputs_fail() {
    for children in [
        vec![],
        vec![state(200), state(300)],
        vec![state(200).children()[0].clone()],
    ] {
        assert!(
            OperatorNode::new_shared(Operator::ASAP(ASAPOp::SummaryMerge { children })).is_err()
        );
    }
}

fn shifted_state(k: u32, start: i64, end: i64) -> Rc<OperatorNode> {
    let mut node = (*state(k)).clone();
    let region = &mut node.coverage.as_mut().unwrap().regions[0];
    region.time_ms = Some((start..end).into());
    Rc::new(node)
}
/// Schema equality cannot authorize overlapping or unknown observation coverage.
#[test]
fn unsafe_coverage_merge_is_rejected() {
    let mut unknown = (*state(200)).clone();
    unknown.coverage = None;
    for children in [
        vec![state(200), state(200)],
        vec![state(200), Rc::new(unknown)],
    ] {
        assert!(
            OperatorNode::new_shared(Operator::ASAP(ASAPOp::SummaryMerge { children })).is_err()
        );
    }
}

/// Gapped time coverage remains disconnected, and forged output metadata is rejected.
#[test]
fn merge_derives_coverage_and_validates_retained_metadata() {
    let root = OperatorNode::new_shared(Operator::ASAP(ASAPOp::SummaryMerge {
        children: vec![state(200), shifted_state(200, 2, 3)],
    }))
    .unwrap();
    assert_eq!(root.coverage.as_ref().unwrap().regions.len(), 2);
    let mut forged = (*root).clone();
    forged.coverage.as_mut().unwrap().regions[0].time_ms = Some((0..2).into());
    assert!(Rc::new(forged).validate_structure().is_err());
}

fn merge_panes(family: FieldDataType) -> Result<Rc<OperatorNode>, impl std::fmt::Debug> {
    OperatorNode::new_shared(Operator::ASAP(ASAPOp::SummaryMerge {
        children: vec![
            family_state(family.clone(), 0..1),
            family_state(family, 1..2),
        ],
    }))
}
/// Heap top-k states have no sound merge model, so disjoint panes still cannot
/// merge; KLL panes over the same coverage can.
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
    let error = format!("{:?}", merge_panes(heap).unwrap_err());
    assert!(error.contains("no sound merge"), "{error}");
    let kll = FieldDataType::Sketch(
        SketchKind::new(SketchAlgorithm::Kll, SketchParams::Kll { k: 200 }),
        Default::default(),
    );
    merge_panes(kll).unwrap().validate_structure().unwrap();
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

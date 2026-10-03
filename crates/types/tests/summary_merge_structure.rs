//! Window composition merges compatible summary states without consuming raw rows.
use asap_types::{
    ir::operator_properties::{Reduction, Source},
    ir::{ASAPOp, NonASAPOp, Operator, OperatorNode},
    post_asap::{GroupingStrategy, SketchAlgorithm, SketchKind, SketchParams, SummaryUpdate},
    pre_asap::{ColumnRef, DataType, Field, FieldDataType, Schema},
};
use std::rc::Rc;
fn state(k: u32) -> Rc<OperatorNode> {
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
        family: FieldDataType::Sketch(
            SketchKind::new(SketchAlgorithm::Kll, SketchParams::Kll { k }),
            Default::default(),
        ),
        input: SummaryUpdate::column(ColumnRef::SampleValue),
        reduction: Reduction::by(vec![]),
        grouping: GroupingStrategy::default(),
        filter: None,
    }))
    .unwrap();
    std::rc::Rc::new(
        summary
            .with_coverage(asap_types::ir::summary_coverage::SummaryCoverage {
                source: "latencies:timestamp".into(),
                input: SummaryUpdate::column(ColumnRef::SampleValue),
                reduction: Reduction::by(vec![]),
                regions: vec![asap_types::ir::summary_coverage::CoverageRegion {
                    time_ms: Some(0..1),
                    population: Default::default(),
                }],
            })
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
        Some(0..2)
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
    region.time_ms = Some(start..end);
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
    forged.coverage.as_mut().unwrap().regions[0].time_ms = Some(0..2);
    assert!(Rc::new(forged).validate_structure().is_err());
}

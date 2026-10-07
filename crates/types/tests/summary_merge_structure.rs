//! Window composition merges compatible summary states without consuming raw rows.
use asap_types::{
    ir::operator_properties::{Reduction, Source},
    ir::{ASAPOp, NonASAPOp, Operator, OperatorNode, SchemaDerivationError},
    post_asap::{GroupingStrategy, SketchAlgorithm, SketchKind, SketchParams, SummaryUpdate},
    pre_asap::{ColumnRef, DataType, Field, FieldDataType, Schema},
};
use std::rc::Rc;
fn state(k: u32) -> Rc<OperatorNode> {
    state_with(FieldDataType::Sketch(
        SketchKind::new(SketchAlgorithm::Kll, SketchParams::Kll { k }),
        Default::default(),
    ))
}

fn state_with(family: FieldDataType) -> Rc<OperatorNode> {
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
            .with_coverage(asap_types::ir::summary_coverage::SummaryCoverage {
                source: Source::Table {
                    table_ref: "latencies".into(),
                },
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
    shifted(&state(k), start, end)
}

fn shifted(state: &Rc<OperatorNode>, start: i64, end: i64) -> Rc<OperatorNode> {
    let mut node = (**state).clone();
    let region = &mut node.coverage.as_mut().unwrap().regions[0];
    region.time_ms = Some(start..end);
    Rc::new(node)
}

fn merge(children: Vec<Rc<OperatorNode>>) -> Result<Rc<OperatorNode>, SchemaDerivationError> {
    OperatorNode::new_shared(Operator::ASAP(ASAPOp::SummaryMerge { children }))
}

/// A merge is a state, so merges nest structurally.
#[test]
fn merges_nest() {
    let inner = merge(vec![state(200)]).unwrap();
    let outer = merge(vec![inner, shifted_state(200, 1, 2)]).unwrap();
    outer.validate_structure().unwrap();
}

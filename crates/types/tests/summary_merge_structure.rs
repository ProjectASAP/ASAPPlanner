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
    state_over(family, SummaryUpdate::column(ColumnRef::SampleValue))
}

fn state_over(family: FieldDataType, input: SummaryUpdate) -> Rc<OperatorNode> {
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
        input,
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

/// Equal schemas do not prove both states summarize the same expression.
#[test]
fn different_update_expressions_do_not_merge() {
    let kll = || {
        FieldDataType::Sketch(
            SketchKind::new(SketchAlgorithm::Kll, SketchParams::Kll { k: 200 }),
            Default::default(),
        )
    };
    let named = state_over(
        kll(),
        SummaryUpdate::column(ColumnRef::Named("value".into())),
    );
    let result = OperatorNode::new_shared(Operator::ASAP(ASAPOp::SummaryMerge {
        children: vec![state(200), shifted(&named, 1, 2)],
    }));
    assert!(matches!(
        result,
        Err(SchemaDerivationError::InvalidScalarSignature(message))
            if message.contains("update expression and reduction")
    ));
}

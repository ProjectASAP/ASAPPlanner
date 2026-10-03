//! Window composition merges compatible summary states without consuming raw rows.
use asap_types::{
    ir::operator_properties::{Reduction, Source},
    ir::{ASAPOp, NonASAPOp, Operator, OperatorNode},
    post_asap::{
        ExecutionTiming, GroupingStrategy, SketchAlgorithm, SketchKind, SketchParams, SummaryUpdate,
    },
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
    OperatorNode::new_shared(Operator::ASAP(ASAPOp::SummaryAgg {
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
    .unwrap()
}
/// Two KLL panes compose into one typed state at either execution phase.
#[test]
fn compatible_panes_merge_and_export() {
    let root = OperatorNode::new_shared(Operator::ASAP(ASAPOp::SummaryMerge {
        children: vec![state(200), state(200)],
    }))
    .unwrap();
    root.validate_structure().unwrap();
    assert_eq!(root.schema.fields.len(), 1);
    let timed = asap_types::ir::apply_lifecycle_timings(
        &root,
        &Default::default(),
        &mut Default::default(),
    )
    .unwrap();
    asap_types::ir::export::compile_post_asap_dag(&timed)
        .unwrap()
        .validate()
        .unwrap();
    for timing in [ExecutionTiming::IngestionTime, ExecutionTiming::QueryTime] {
        asap_types::ir::timing::validate_default(&root, timing).unwrap();
        assert_eq!(
            asap_types::ir::timing::planned_data_state(&root, timing).primitive,
            asap_types::post_asap::DataPrimitive::SummaryState
        );
    }
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

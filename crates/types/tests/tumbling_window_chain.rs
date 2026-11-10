//! A `SummaryTumblingWindowChain`: one state per window of its width, with
//! no range and no window count (concepts doc, section 3, "Chain groups").
use std::rc::Rc;
use std::time::Duration;

use asap_types::ir::operator::arguments::{Reduction, Source};
use asap_types::ir::scalar::ColumnRef;
use asap_types::ir::schema::{DataType, Field, FieldDataType, Schema};
use asap_types::ir::{ASAPOp, NonASAPOp, Operator, OperatorNode, TimeRangeKind};
use asap_types::summary_type::{SketchAlgorithm, SummaryUpdate};

const MINUTE: u64 = 60_000;

fn scan() -> Rc<OperatorNode> {
    OperatorNode::new_shared(Operator::NonASAP(NonASAPOp::Scan {
        source: Source::TimeSeries { metric: "m".into() },
        predicates: vec![],
        schema: Schema::with_time_index(
            vec![
                Field::plain("ts", DataType::Timestamp, false),
                Field::plain("value", DataType::Float64, false),
            ],
            0,
            vec![],
        ),
    }))
    .unwrap()
}

fn kll() -> FieldDataType {
    FieldDataType::Sketch(SketchAlgorithm::Kll)
}

/// 1-min KLL tumbling windows per series over `input`.
fn chain(input: Rc<OperatorNode>) -> Rc<OperatorNode> {
    OperatorNode::new_shared(Operator::ASAP(ASAPOp::SummaryTumblingWindowChain {
        child: input,
        summary_type: kll(),
        input: SummaryUpdate::column(ColumnRef::SampleValue),
        reduction: Reduction::PerEntity,
        filter: None,
        tumbling_window_width_ms: MINUTE,
    }))
    .unwrap()
}

/// A chain needs a positive width, and reads the instant vector a range
/// selector would.
#[test]
fn a_chain_reads_an_instant_vector() {
    let ranged = OperatorNode::new_shared(Operator::NonASAP(NonASAPOp::TimeRange {
        range: Duration::from_millis(MINUTE),
        kind: TimeRangeKind::Range,
        child: scan(),
    }))
    .unwrap();
    assert!(chain(ranged).validate_structure().is_err());
    let Some(ASAPOp::SummaryTumblingWindowChain { .. }) = chain(scan()).asap() else {
        unreachable!()
    };
    let empty = OperatorNode::new_shared(Operator::ASAP(ASAPOp::SummaryTumblingWindowChain {
        child: scan(),
        summary_type: kll(),
        input: SummaryUpdate::column(ColumnRef::SampleValue),
        reduction: Reduction::PerEntity,
        filter: None,
        tumbling_window_width_ms: 0,
    }))
    .unwrap();
    assert!(empty.validate_structure().is_err());
}

/// JSON written before #1163 names the chain `PaneChain` and its width
/// `pane_width_ms`; it reads as today's operator.
#[test]
fn names_before_the_tumbling_window_rename_still_read() {
    let operator = chain(scan()).operator.map_children(|_| ());
    let old = serde_json::to_string(&operator)
        .unwrap()
        .replace("SummaryTumblingWindowChain", "PaneChain")
        .replace("tumbling_window_width_ms", "pane_width_ms");
    assert!(!old.contains("umbling"), "{old}");
    assert_eq!(
        serde_json::from_str::<Operator<()>>(&old).unwrap(),
        operator
    );
}

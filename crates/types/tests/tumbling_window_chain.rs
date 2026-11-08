//! A `SummaryTumblingWindowChain`: one state per window of its width, with
//! no range and no window count (concepts doc, section 3, "Chain groups").
use std::rc::Rc;
use std::time::Duration;

use asap_types::ir::operator::arguments::{Reduction, Source};
use asap_types::ir::scalar::ColumnRef;
use asap_types::ir::schema::{DataType, Field, FieldDataType, Schema};
use asap_types::ir::{
    ASAPOp, NonASAPOp, Operator, OperatorNode, OperatorResultKind, TimeRangeKind,
};
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

/// A scan with a `host` label, then the chain, the plain build and the
/// time selections over it by host.
fn scan_with_host() -> Rc<OperatorNode> {
    OperatorNode::new_shared(Operator::NonASAP(NonASAPOp::Scan {
        source: Source::TimeSeries { metric: "m".into() },
        predicates: vec![],
        schema: Schema::with_time_index(
            vec![
                Field::plain("ts", DataType::Timestamp, false),
                Field::plain("host", DataType::Utf8, false),
                Field::plain("value", DataType::Float64, false),
            ],
            0,
            vec![],
        ),
    }))
    .unwrap()
}

fn node(operator: Operator) -> Rc<OperatorNode> {
    let node = OperatorNode::new_shared(operator).unwrap();
    node.validate_structure().unwrap();
    node
}

/// The `SummaryAgg` (`chain` false) or the 1-min chain (`chain` true) of
/// `summary_type` by host over `input`.
fn by_host(input: Rc<OperatorNode>, summary_type: FieldDataType, chain: bool) -> Operator {
    let reduction = Reduction::by(vec![1]);
    let update = SummaryUpdate::column(ColumnRef::SampleValue);
    Operator::ASAP(if chain {
        ASAPOp::SummaryTumblingWindowChain {
            child: input,
            summary_type,
            input: update,
            reduction,
            filter: None,
            tumbling_window_width_ms: MINUTE,
        }
    } else {
        ASAPOp::SummaryAgg {
            child: input,
            summary_type,
            input: update,
            reduction,
            filter: None,
        }
    })
}

/// `TimeRange(5m, TimeShift(1h, input))`.
fn five_minutes_an_hour_ago(input: Rc<OperatorNode>) -> Rc<OperatorNode> {
    let shifted = node(Operator::NonASAP(NonASAPOp::TimeShift {
        shift: asap_types::ir::operator::arguments::TimeShift {
            offset_ms: 60 * MINUTE as i64,
            at: None,
        },
        child: input,
    }));
    node(Operator::NonASAP(NonASAPOp::TimeRange {
        range: Duration::from_millis(5 * MINUTE),
        kind: TimeRangeKind::Range,
        child: shifted,
    }))
}

fn merge(children: Vec<Rc<OperatorNode>>, group_by: Option<Vec<usize>>) -> Operator {
    Operator::ASAP(ASAPOp::SummaryMerge { children, group_by })
}

/// A chain's output is a state with a time index, one row per window and
/// key: (window_end, keys, state), unique by (window_end, keys), for a
/// keyed chain and a per-series one alike. `window_end` replaces the
/// input's `ts`. The plain build by host has no time index.
#[test]
fn a_chain_is_a_state_with_a_time_index() {
    let names = |node: &Rc<OperatorNode>| -> Vec<String> {
        node.schema.fields.iter().map(|f| f.name.clone()).collect()
    };
    let by_host_chain = node(by_host(scan_with_host(), kll(), true));
    let per_series = node(Operator::ASAP(ASAPOp::SummaryTumblingWindowChain {
        child: scan_with_host(),
        summary_type: kll(),
        input: SummaryUpdate::column(ColumnRef::SampleValue),
        reduction: Reduction::PerEntity,
        filter: None,
        tumbling_window_width_ms: MINUTE,
    }));
    for chain in [&by_host_chain, &per_series] {
        assert_eq!(chain.result_kind, OperatorResultKind::State);
        assert_eq!(chain.schema.time_index, Some(0));
        assert_eq!(names(chain), ["window_end", "host", "state"]);
        assert_eq!(chain.schema.unique_keys, [vec![0, 1]]);
    }
    let build = node(by_host(scan_with_host(), kll(), false));
    assert_eq!(build.schema.time_index, None);
    let without_labels = chain(scan());
    assert_eq!(names(&without_labels), ["window_end", "state"]);
    assert_eq!(without_labels.schema.unique_keys, [vec![0]]);
}

/// `TimeRange` and `TimeShift` accept a time-indexed state, a chain or a
/// selection over one, and keep it a state with the chain's schema; over
/// a state without a time index (a plain build) they are rejected.
#[test]
fn time_range_and_time_shift_select_a_chains_windows() {
    let windows = node(by_host(scan_with_host(), kll(), true));
    let selected = five_minutes_an_hour_ago(windows.clone());
    assert_eq!(selected.result_kind, OperatorResultKind::State);
    assert_eq!(selected.schema, windows.schema);
    let build = node(by_host(scan_with_host(), kll(), false));
    let over_build = OperatorNode::new_shared(Operator::NonASAP(NonASAPOp::TimeRange {
        range: Duration::from_millis(5 * MINUTE),
        kind: TimeRangeKind::Range,
        child: build,
    }))
    .unwrap();
    assert!(over_build.validate_structure().is_err());
}

/// A `TimeRange` over a chain selects a range of windows; an instant one
/// is rejected.
#[test]
fn an_instant_time_range_over_a_chain_is_rejected() {
    let instant = OperatorNode::new_shared(Operator::NonASAP(NonASAPOp::TimeRange {
        range: Duration::from_millis(5 * MINUTE),
        kind: TimeRangeKind::Instant,
        child: node(by_host(scan_with_host(), kll(), true)),
    }))
    .unwrap();
    assert!(instant.validate_structure().is_err());
}

/// Merging the windows `TimeRange(5m, TimeShift(1h, chain(X)))` selects
/// gives the state of `SummaryAgg(TimeRange(5m, TimeShift(1h, X)))`: the
/// same schema and kind, and so the same schema read out, finalized
/// (an exact sum) and rolled up.
#[test]
fn merged_windows_are_the_build_over_the_selected_range() {
    let sum = FieldDataType::exact(asap_types::summary_type::ExactKind::Sum);
    for summary_type in [kll(), sum.clone()] {
        let merged = node(merge(
            vec![five_minutes_an_hour_ago(node(by_host(
                scan_with_host(),
                summary_type.clone(),
                true,
            )))],
            None,
        ));
        let build = node(by_host(
            five_minutes_an_hour_ago(scan_with_host()),
            summary_type.clone(),
            false,
        ));
        assert_eq!(merged.schema, build.schema, "{summary_type:?}");
        assert_eq!(merged.result_kind, build.result_kind);
        let read = |state: &Rc<OperatorNode>| {
            node(if summary_type == sum {
                Operator::ASAP(ASAPOp::FinalizeExactAccumulator {
                    child: state.clone(),
                })
            } else {
                Operator::ASAP(ASAPOp::SummaryEstimate {
                    summary_input: state.clone(),
                    query: asap_types::summary_type::SketchStatistic::Quantile { q: 0.95 },
                })
            })
        };
        let (read_merged, read_build) = (read(&merged), read(&build));
        assert_eq!(read_merged.schema, read_build.schema, "{summary_type:?}");
        assert_eq!(read_merged.result_kind, read_build.result_kind);
        // Rolled up to one state over every host.
        let (rolled_merged, rolled_build) = (
            node(merge(vec![merged], Some(vec![]))),
            node(merge(vec![build], Some(vec![]))),
        );
        assert_eq!(rolled_merged.schema, rolled_build.schema);
        let (read_merged, read_build) = (read(&rolled_merged), read(&rolled_build));
        assert_eq!(read_merged.schema, read_build.schema, "{summary_type:?}");
    }
}

/// A merge over a time-indexed state reads one `TimeRange` over a chain:
/// not the whole chain, not a `TimeShift` without a range, and not beside
/// another input.
#[test]
fn a_merge_over_a_chain_reads_one_time_range() {
    let windows = || node(by_host(scan_with_host(), kll(), true));
    let shifted = node(Operator::NonASAP(NonASAPOp::TimeShift {
        shift: asap_types::ir::operator::arguments::TimeShift {
            offset_ms: MINUTE as i64,
            at: None,
        },
        child: windows(),
    }));
    for children in [
        vec![windows()],
        vec![shifted],
        vec![
            five_minutes_an_hour_ago(windows()),
            five_minutes_an_hour_ago(windows()),
        ],
    ] {
        assert!(OperatorNode::new_shared(merge(children, None)).is_err());
    }
    assert!(
        OperatorNode::new_shared(merge(vec![five_minutes_an_hour_ago(windows())], None)).is_ok()
    );
}

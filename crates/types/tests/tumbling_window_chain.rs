//! A `SummaryTumblingWindowChain` of tumbling windows stands for its tumbling windows: it covers what one
//! build over its whole window does, and expands into the `SummaryAgg`
//! tumbling windows the executor runs (#509 Pass 2, window-composition rule).
use std::ops::Bound;
use std::rc::Rc;
use std::time::Duration;

use asap_types::ir::operator::operator_properties::{Reduction, Source, TimeShift};
use asap_types::ir::operator::tumbling_window_chain::{
    expand_tumbling_window_chains, merged_chain,
};
use asap_types::ir::scalar::ColumnRef;
use asap_types::ir::schema::{DataType, Field, FieldDataType, Schema};
use asap_types::ir::schema::{GroupingStrategy, SketchAlgorithm, SketchStatistic, SummaryUpdate};
use asap_types::ir::{ASAPOp, NonASAPOp, Operator, OperatorNode, TimeRangeKind};
use asap_types::test_support::{tumbling_window, whole_window_build};

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

fn shifted(child: Rc<OperatorNode>, offset_ms: i64) -> Rc<OperatorNode> {
    OperatorNode::new_shared(Operator::NonASAP(NonASAPOp::TimeShift {
        shift: TimeShift {
            offset_ms,
            at: None,
        },
        child,
    }))
    .unwrap()
}

fn kll() -> FieldDataType {
    FieldDataType::Sketch(SketchAlgorithm::Kll, GroupingStrategy::default())
}

/// Five 1-min KLL tumbling windows per series over `input`.
fn chain(input: Rc<OperatorNode>) -> Rc<OperatorNode> {
    OperatorNode::new_shared(Operator::ASAP(ASAPOp::SummaryTumblingWindowChain {
        child: input,
        family: kll(),
        input: SummaryUpdate::column(ColumnRef::SampleValue),
        reduction: Reduction::PerEntity,
        grouping: GroupingStrategy::default(),
        filter: None,
        tumbling_window_width_ms: MINUTE,
        tumbling_window_count: 5,
    }))
    .unwrap()
}

fn merge(children: Vec<Rc<OperatorNode>>) -> Rc<OperatorNode> {
    OperatorNode::new_shared(Operator::ASAP(ASAPOp::SummaryMerge {
        children,
        newest_tumbling_windows: None,
        group_by: None,
    }))
    .unwrap()
}

fn relative_time(node: &OperatorNode) -> (Bound<i64>, Bound<i64>) {
    let coverage = node.coverage().expect("coverage");
    let [selection] = coverage.selection() else {
        panic!("one box: {coverage:?}")
    };
    selection.relative_time().expect("a time window")
}

/// The merged chain is a state of the build over the whole 5-min window:
/// the same schema and coverage `(-5m, 0]`, and it validates.
#[test]
fn merged_tumbling_windows_cover_the_whole_window() {
    let chain = chain(scan());
    let merged = merge(vec![Rc::clone(&chain)]);
    merged.validate_structure().unwrap();
    assert!(merged_chain(&merged).is_some_and(|c| std::ptr::eq(c, &*chain)));
    let whole = whole_window_build(&chain).unwrap();
    let Some(NonASAPOp::TimeRange { range, kind, .. }) = whole.children()[0].non_asap() else {
        panic!("{:?}", whole.operator)
    };
    assert_eq!(
        (*range, *kind),
        (Duration::from_millis(5 * MINUTE), TimeRangeKind::Range)
    );
    assert_eq!(merged.schema, whole.schema);
    assert_eq!(merged.coverage(), whole.coverage());
    assert_eq!(
        relative_time(&merged),
        (Bound::Excluded(-5 * MINUTE as i64), Bound::Included(0))
    );
}

/// Tumbling window `i` is the build over `(-(i+1)·1m, -i·1m]`, the query's own offset
/// folded into one shift; the tumbling windows are disjoint and tile the window.
#[test]
fn each_tumbling_window_is_one_shifted_minute() {
    let offset = 3_600_000;
    let chain = chain(shifted(scan(), offset));
    for i in 0..5 {
        let tumbling_window = tumbling_window(&chain, i).unwrap();
        let back = offset + i64::from(i) * MINUTE as i64;
        assert_eq!(
            relative_time(&tumbling_window),
            (
                Bound::Excluded(-(back + MINUTE as i64)),
                Bound::Included(-back)
            ),
            "tumbling window {i}"
        );
        let range = tumbling_window.children()[0];
        let Some(NonASAPOp::TimeShift { shift, child }) = range.children()[0].non_asap() else {
            panic!("tumbling window {i} reads a shift")
        };
        assert_eq!(shift.offset_ms, back);
        assert!(matches!(child.non_asap(), Some(NonASAPOp::Scan { .. })));
    }
}

/// Expansion gives the executor a merge over the five tumbling windows, newest first,
/// which covers what the merged chain does; a chain read without a merge
/// does not expand.
#[test]
fn expansion_merges_the_tumbling_windows() {
    let chain = chain(scan());
    let merged = merge(vec![Rc::clone(&chain)]);
    let estimate = OperatorNode::new_shared(Operator::ASAP(ASAPOp::SummaryEstimate {
        summary_input: Rc::clone(&merged),
        query: SketchStatistic::Quantile { q: 0.99 },
    }))
    .unwrap();
    let expanded = expand_tumbling_window_chains(&estimate).unwrap();
    expanded.validate_structure().unwrap();
    assert_eq!(expanded.schema, estimate.schema);
    let Some(ASAPOp::SummaryEstimate { summary_input, .. }) = expanded.asap() else {
        panic!("{:?}", expanded.operator)
    };
    let Some(ASAPOp::SummaryMerge { children, .. }) = summary_input.asap() else {
        panic!("{:?}", summary_input.operator)
    };
    assert_eq!(children.len(), 5);
    for (i, child) in children.iter().enumerate() {
        assert_eq!(
            **child,
            tumbling_window(&chain, i as u32).unwrap(),
            "tumbling window {i}"
        );
    }
    assert_eq!(summary_input.coverage(), merged.coverage());
    assert!(expand_tumbling_window_chains(&chain).is_err());
}

/// A chain needs a positive width and count, and reads the instant vector
/// a range selector would.
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
        family: kll(),
        input: SummaryUpdate::column(ColumnRef::SampleValue),
        reduction: Reduction::PerEntity,
        grouping: GroupingStrategy::default(),
        filter: None,
        tumbling_window_width_ms: MINUTE,
        tumbling_window_count: 0,
    }))
    .unwrap();
    assert!(empty.validate_structure().is_err());
}

fn newest(chain: Rc<OperatorNode>, newest_tumbling_windows: u32) -> Rc<OperatorNode> {
    OperatorNode::new_shared(Operator::ASAP(ASAPOp::SummaryMerge {
        children: vec![chain],
        newest_tumbling_windows: Some(newest_tumbling_windows),
        group_by: None,
    }))
    .unwrap()
}

/// A merge of the newest 2 of 5 tumbling windows is a state of one build over the
/// newest 2 minutes: coverage `(-2m, 0]`, the window build's schema, and
/// a valid structure.
#[test]
fn a_merge_of_the_newest_tumbling_windows_covers_their_window() {
    use asap_types::ir::operator::tumbling_window_chain::merged_window_build;
    let partial = newest(chain(scan()), 2);
    partial.validate_structure().unwrap();
    let build = merged_window_build(&partial).unwrap();
    let Some(NonASAPOp::TimeRange { range, .. }) = build.children()[0].non_asap() else {
        panic!("{:?}", build.operator)
    };
    assert_eq!(*range, Duration::from_millis(2 * MINUTE));
    assert_eq!(partial.schema, build.schema);
    assert_eq!(partial.coverage(), build.coverage());
    assert_eq!(
        relative_time(&partial),
        (Bound::Excluded(-2 * MINUTE as i64), Bound::Included(0))
    );
}

/// `newest_tumbling_windows` must name between 1 and fewer than all tumbling windows of the one
/// chain the merge reads; anything else is rejected.
#[test]
fn a_merge_of_the_newest_tumbling_windows_reads_one_chain_and_fewer_than_all_tumbling_windows() {
    let chain = chain(scan());
    let build = whole_window_build(&chain).unwrap();
    for (children, newest_tumbling_windows) in [
        (vec![Rc::clone(&chain)], 0),
        (vec![Rc::clone(&chain)], 5),
        (vec![Rc::clone(&chain)], 6),
        (vec![Rc::clone(&chain), Rc::clone(&chain)], 2),
        (vec![Rc::new(build)], 1),
    ] {
        let merge = OperatorNode::new(Operator::ASAP(ASAPOp::SummaryMerge {
            children,
            newest_tumbling_windows: Some(newest_tumbling_windows),
            group_by: None,
        }));
        assert!(merge.is_err(), "newest {newest_tumbling_windows}");
    }
}

/// `newest_tumbling_windows` is left out of the JSON when `None`, and JSON without it
/// reads as `None`; `Some(n)` round-trips.
#[test]
fn newest_tumbling_windows_serializes_only_when_set() {
    let full: ASAPOp<()> = ASAPOp::SummaryMerge {
        children: vec![()],
        newest_tumbling_windows: None,
        group_by: None,
    };
    let json = serde_json::to_string(&full).unwrap();
    assert!(!json.contains("newest_tumbling_windows"), "{json}");
    let read: ASAPOp<()> = serde_json::from_str(r#"{"SummaryMerge":{"children":[null]}}"#).unwrap();
    assert_eq!(read, full);
    let partial: ASAPOp<()> = ASAPOp::SummaryMerge {
        children: vec![()],
        newest_tumbling_windows: Some(6),
        group_by: None,
    };
    let json = serde_json::to_string(&partial).unwrap();
    assert_eq!(serde_json::from_str::<ASAPOp<()>>(&json).unwrap(), partial);
}

/// A rollup of a chain's tumbling windows to no keys (every series' tumbling windows into one
/// state) is no tumbling-window merge, and expansion keeps its keys: the expanded
/// rollup covers what the planned one does, one build over the whole
/// window and every series.
#[test]
fn expansion_keeps_a_rollup_of_the_tumbling_windows() {
    let chain = chain(scan());
    let rollup = OperatorNode::new_shared(Operator::ASAP(ASAPOp::SummaryMerge {
        children: vec![Rc::clone(&chain)],
        group_by: Some(vec![]),
        newest_tumbling_windows: None,
    }))
    .unwrap();
    assert!(merged_chain(&rollup).is_none());
    let expanded = expand_tumbling_window_chains(&rollup).unwrap();
    let Some(ASAPOp::SummaryMerge {
        children, group_by, ..
    }) = expanded.asap()
    else {
        panic!("{:?}", expanded.operator)
    };
    assert_eq!((children.len(), group_by), (5, &Some(vec![])));
    let coverage = rollup.coverage().unwrap();
    assert_eq!(expanded.coverage(), Some(coverage.clone()));
    let Some(ASAPOp::SummaryAgg { reduction, .. }) = coverage.definition().asap() else {
        panic!("{:?}", coverage.definition().operator)
    };
    assert_eq!(*reduction, Reduction::by(vec![]));
    // A rollup merges every input state: it reads a merge of the newest
    // tumbling windows, it is not one.
    assert!(OperatorNode::new(Operator::ASAP(ASAPOp::SummaryMerge {
        children: vec![chain],
        newest_tumbling_windows: Some(2),
        group_by: Some(vec![]),
    }))
    .is_err());
}

/// A rollup reads a merge of the newest tumbling windows: every series' newest 2 of 5
/// tumbling windows into one state covers one build by no keys over the newest 2
/// minutes.
#[test]
fn a_rollup_of_the_newest_tumbling_windows_covers_their_window() {
    let rollup = OperatorNode::new_shared(Operator::ASAP(ASAPOp::SummaryMerge {
        children: vec![newest(chain(scan()), 2)],
        newest_tumbling_windows: None,
        group_by: Some(vec![]),
    }))
    .unwrap();
    rollup.validate_structure().unwrap();
    let coverage = rollup.coverage().unwrap();
    let Some(ASAPOp::SummaryAgg { reduction, .. }) = coverage.definition().asap() else {
        panic!("{:?}", coverage.definition().operator)
    };
    assert_eq!(*reduction, Reduction::by(vec![]));
    assert_eq!(
        coverage.selection()[0].relative_time(),
        Some((Bound::Excluded(-2 * MINUTE as i64), Bound::Included(0)))
    );
}

/// JSON written before #1163 names the chain `PaneChain`, its fields
/// `tumbling_window_width_ms` and `tumbling_window_count`, and a merge's `newest_tumbling_windows`; it reads
/// as today's operators.
#[test]
fn names_before_the_tumbling_window_rename_still_read() {
    let chain = chain(scan()).operator.map_children(|_| ());
    let merge: Operator<()> = Operator::ASAP(ASAPOp::SummaryMerge {
        children: vec![()],
        newest_tumbling_windows: Some(3),
        group_by: None,
    });
    for operator in [chain, merge] {
        let old = serde_json::to_string(&operator)
            .unwrap()
            .replace("SummaryTumblingWindowChain", "PaneChain")
            .replace("tumbling_window_width_ms", "pane_width_ms")
            .replace("tumbling_window_count", "pane_count")
            .replace("newest_tumbling_windows", "newest_panes");
        assert!(!old.contains("umbling"), "{old}");
        assert_eq!(
            serde_json::from_str::<Operator<()>>(&old).unwrap(),
            operator
        );
    }
}

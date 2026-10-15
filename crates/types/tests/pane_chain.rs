//! A `PaneChain` of tumbling panes stands for its panes: it covers what one
//! build over its whole window does, and expands into the `SummaryAgg`
//! panes the executor runs (#509 Pass 2, window-composition rule).
use std::ops::Bound;
use std::rc::Rc;
use std::time::Duration;

use asap_types::ir::operator_properties::{Reduction, Source, TimeShift};
use asap_types::ir::pane_chain::{expand_pane_chains, merged_chain, pane, whole_window_build};
use asap_types::ir::{ASAPOp, NonASAPOp, Operator, OperatorNode, TimeRangeKind};
use asap_types::post_asap::{
    GroupingStrategy, SketchAlgorithm, SketchKind, SketchParams, SketchStatistic, SummaryUpdate,
};
use asap_types::pre_asap::{ColumnRef, DataType, Field, FieldDataType, Schema};

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
    FieldDataType::Sketch(
        SketchKind::new(
            SketchAlgorithm::Kll,
            SketchParams::Kll {
                k: 200,
                failure_probability: None,
            },
        ),
        GroupingStrategy::default(),
    )
}

/// Five 1-min KLL panes per series over `input`.
fn chain(input: Rc<OperatorNode>) -> Rc<OperatorNode> {
    OperatorNode::new_shared(Operator::ASAP(ASAPOp::PaneChain {
        child: input,
        family: kll(),
        input: SummaryUpdate::column(ColumnRef::SampleValue),
        reduction: Reduction::PerEntity,
        grouping: GroupingStrategy::default(),
        filter: None,
        pane_width_ms: MINUTE,
        pane_count: 5,
    }))
    .unwrap()
}

fn merge(children: Vec<Rc<OperatorNode>>) -> Rc<OperatorNode> {
    OperatorNode::new_shared(Operator::ASAP(ASAPOp::SummaryMerge { children })).unwrap()
}

fn relative_time(node: &OperatorNode) -> (Bound<i64>, Bound<i64>) {
    let coverage = node.coverage().expect("coverage");
    let [selection] = coverage.selection.as_slice() else {
        panic!("one box: {coverage:?}")
    };
    selection.relative_time.expect("a time window")
}

/// The merged chain is a state of the build over the whole 5-min window:
/// the same schema and coverage `(-5m, 0]`, and it validates.
#[test]
fn merged_panes_cover_the_whole_window() {
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

/// Pane `i` is the build over `(-(i+1)·1m, -i·1m]`, the query's own offset
/// folded into one shift; the panes are disjoint and tile the window.
#[test]
fn each_pane_is_one_shifted_minute() {
    let offset = 3_600_000;
    let chain = chain(shifted(scan(), offset));
    for i in 0..5 {
        let pane = pane(&chain, i).unwrap();
        let back = offset + i64::from(i) * MINUTE as i64;
        assert_eq!(
            relative_time(&pane),
            (
                Bound::Excluded(-(back + MINUTE as i64)),
                Bound::Included(-back)
            ),
            "pane {i}"
        );
        let range = pane.children()[0];
        let Some(NonASAPOp::TimeShift { shift, child }) = range.children()[0].non_asap() else {
            panic!("pane {i} reads a shift")
        };
        assert_eq!(shift.offset_ms, back);
        assert!(matches!(child.non_asap(), Some(NonASAPOp::Scan { .. })));
    }
}

/// Expansion gives the executor a merge over the five panes, newest first,
/// which covers what the merged chain does; a chain read without a merge
/// does not expand.
#[test]
fn expansion_merges_the_panes() {
    let chain = chain(scan());
    let merged = merge(vec![Rc::clone(&chain)]);
    let estimate = OperatorNode::new_shared(Operator::ASAP(ASAPOp::SummaryEstimate {
        summary_input: Rc::clone(&merged),
        query: SketchStatistic::Quantile { q: 0.99 },
    }))
    .unwrap();
    let expanded = expand_pane_chains(&estimate).unwrap();
    expanded.validate_structure().unwrap();
    assert_eq!(expanded.schema, estimate.schema);
    let Some(ASAPOp::SummaryEstimate { summary_input, .. }) = expanded.asap() else {
        panic!("{:?}", expanded.operator)
    };
    let Some(ASAPOp::SummaryMerge { children }) = summary_input.asap() else {
        panic!("{:?}", summary_input.operator)
    };
    assert_eq!(children.len(), 5);
    for (i, child) in children.iter().enumerate() {
        assert_eq!(**child, pane(&chain, i as u32).unwrap(), "pane {i}");
    }
    assert_eq!(summary_input.coverage(), merged.coverage());
    assert!(expand_pane_chains(&chain).is_err());
}

/// Expanding a timed chain carries its timing and materialization onto its
/// panes: a chain materialized at ingestion time expands into panes each
/// built and stored at ingestion time, over ranges and shifts that run at
/// ingestion time too, while the merge reading them stays at query time.
/// An unmaterialized chain expands into query-time panes.
#[test]
fn expansion_carries_the_chains_timing_onto_its_panes() {
    use asap_types::ir::{
        apply_materialization_timings, Materialization, MaterializationAssignment, Medium,
        TimingMemo,
    };
    use asap_types::post_asap::ExecutionTiming;
    let at_ingestion = Materialization::MatAtIngestion(Medium::Memory);
    for (materialization, timing) in [
        (at_ingestion, ExecutionTiming::IngestionTime),
        (Materialization::NotMaterialized, ExecutionTiming::QueryTime),
    ] {
        let chain = chain(shifted(scan(), MINUTE as i64));
        let estimate = OperatorNode::new_shared(Operator::ASAP(ASAPOp::SummaryEstimate {
            summary_input: merge(vec![Rc::clone(&chain)]),
            query: SketchStatistic::Quantile { q: 0.99 },
        }))
        .unwrap();
        let mut assignment = MaterializationAssignment::default();
        assignment.set(&chain, materialization);
        let timed =
            apply_materialization_timings(&estimate, &assignment, &mut TimingMemo::new()).unwrap();
        let expanded = expand_pane_chains(&timed).unwrap();
        let merged = expanded.children()[0];
        assert_eq!(merged.timing, Some(ExecutionTiming::QueryTime));
        assert_eq!(merged.children().len(), 5);
        for pane in merged.children() {
            assert_eq!(pane.timing, Some(timing), "{materialization:?}");
            assert_eq!(pane.materialization, materialization);
            for below in OperatorNode::reachable(pane).iter().skip(1) {
                assert_eq!(below.timing, Some(timing), "{}", below.operator.kind_name());
                assert_eq!(below.materialization, Materialization::NotMaterialized);
            }
        }
    }
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
    let Some(ASAPOp::PaneChain { .. }) = chain(scan()).asap() else {
        unreachable!()
    };
    let empty = OperatorNode::new_shared(Operator::ASAP(ASAPOp::PaneChain {
        child: scan(),
        family: kll(),
        input: SummaryUpdate::column(ColumnRef::SampleValue),
        reduction: Reduction::PerEntity,
        grouping: GroupingStrategy::default(),
        filter: None,
        pane_width_ms: MINUTE,
        pane_count: 0,
    }))
    .unwrap();
    assert!(empty.validate_structure().is_err());
}

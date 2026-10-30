//! A `PaneChain` of tumbling panes stands for its panes: it covers what one
//! build over its whole window does, and expands into the `SummaryAgg`
//! panes the executor runs (#509 Pass 2, window-composition rule).
use std::ops::Bound;
use std::rc::Rc;
use std::time::Duration;

use asap_types::ir::operator::operator_properties::{Reduction, Source, TimeShift};
use asap_types::ir::operator::pane_chain::{expand_pane_chains, merged_chain};
use asap_types::ir::scalar::ColumnRef;
use asap_types::ir::schema::{DataType, Field, FieldDataType, Schema};
use asap_types::ir::schema::{
    GroupingStrategy, SketchAlgorithm, SketchKind, SketchParams, SketchStatistic, SummaryUpdate,
};
use asap_types::ir::{ASAPOp, NonASAPOp, Operator, OperatorNode, TimeRangeKind};
use asap_types::test_support::{pane, whole_window_build};

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
    OperatorNode::new_shared(Operator::ASAP(ASAPOp::SummaryMerge {
        children,
        newest_panes: None,
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
    let Some(ASAPOp::SummaryMerge { children, .. }) = summary_input.asap() else {
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
    use asap_types::ir::properties::ExecutionTiming;
    use asap_types::ir::{
        apply_materialization_timings, MaterializationAssignment, NodeToTimedNodeCache,
    };
    use asap_types::physical::{Materialization, Medium};
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
            apply_materialization_timings(&estimate, &assignment, &mut NodeToTimedNodeCache::new())
                .unwrap();
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

/// Two merges over one chain (two queries reading the same panes) expand
/// into one set of panes: the same five `SummaryAgg` nodes, not ten.
#[test]
fn merges_over_one_chain_share_its_panes() {
    use asap_types::ir::operator::pane_chain::expand_pane_chains_in_workload;
    use asap_types::ir::QueryRoot;
    let shared = chain(scan());
    let roots = [
        QueryRoot::Operator(merge(vec![Rc::clone(&shared)])),
        QueryRoot::Operator(merge(vec![shared])),
    ];
    let expansion = expand_pane_chains_in_workload(&roots).unwrap();
    assert_eq!(expansion.chains.len(), 1);
    assert_eq!(expansion.chains[0].panes.len(), 5);
    let expanded = expansion.roots;
    let panes = |root: &QueryRoot| match root {
        QueryRoot::Operator(node) => node.children().into_iter().cloned().collect::<Vec<_>>(),
        QueryRoot::Scalar(_) => unreachable!(),
    };
    let (first, second) = (panes(&expanded[0]), panes(&expanded[1]));
    assert_eq!(first.len(), 5);
    assert!(first.iter().zip(&second).all(|(a, b)| Rc::ptr_eq(a, b)));
}

fn newest(chain: Rc<OperatorNode>, newest_panes: u32) -> Rc<OperatorNode> {
    OperatorNode::new_shared(Operator::ASAP(ASAPOp::SummaryMerge {
        children: vec![chain],
        newest_panes: Some(newest_panes),
        group_by: None,
    }))
    .unwrap()
}

/// A merge of the newest 2 of 5 panes is a state of one build over the
/// newest 2 minutes: coverage `(-2m, 0]`, the window build's schema, and
/// a valid structure.
#[test]
fn a_merge_of_the_newest_panes_covers_their_window() {
    use asap_types::ir::operator::pane_chain::merged_window_build;
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

/// `newest_panes` must name between 1 and fewer than all panes of the one
/// chain the merge reads; anything else is rejected.
#[test]
fn a_merge_of_the_newest_panes_reads_one_chain_and_fewer_than_all_panes() {
    let chain = chain(scan());
    let build = whole_window_build(&chain).unwrap();
    for (children, newest_panes) in [
        (vec![Rc::clone(&chain)], 0),
        (vec![Rc::clone(&chain)], 5),
        (vec![Rc::clone(&chain)], 6),
        (vec![Rc::clone(&chain), Rc::clone(&chain)], 2),
        (vec![Rc::new(build)], 1),
    ] {
        let merge = OperatorNode::new(Operator::ASAP(ASAPOp::SummaryMerge {
            children,
            newest_panes: Some(newest_panes),
            group_by: None,
        }));
        assert!(merge.is_err(), "newest {newest_panes}");
    }
}

/// A merge of the newest 2 panes and a merge of all 5 over one chain
/// expand into one set of panes: the partial merge reads the first 2 of
/// the full merge's 5 nodes, newest first, and the chain lists all 5.
#[test]
fn a_merge_of_the_newest_panes_reads_the_first_panes_of_the_chain() {
    use asap_types::ir::operator::pane_chain::expand_pane_chains_in_workload;
    use asap_types::ir::QueryRoot;
    let shared = chain(scan());
    let roots = [
        QueryRoot::Operator(newest(Rc::clone(&shared), 2)),
        QueryRoot::Operator(merge(vec![shared])),
    ];
    let expansion = expand_pane_chains_in_workload(&roots).unwrap();
    assert_eq!(expansion.chains.len(), 1);
    assert_eq!(expansion.chains[0].panes.len(), 5);
    let panes = |root: &QueryRoot| match root {
        QueryRoot::Operator(node) => node.children().into_iter().cloned().collect::<Vec<_>>(),
        QueryRoot::Scalar(_) => unreachable!(),
    };
    let (partial, full) = (panes(&expansion.roots[0]), panes(&expansion.roots[1]));
    assert_eq!((partial.len(), full.len()), (2, 5));
    assert!(partial.iter().zip(&full).all(|(a, b)| Rc::ptr_eq(a, b)));
    for (root, window) in expansion.roots.iter().zip([2, 5]) {
        let QueryRoot::Operator(node) = root else {
            unreachable!()
        };
        assert_eq!(
            relative_time(node),
            (Bound::Excluded(-window * MINUTE as i64), Bound::Included(0))
        );
    }
}

/// `newest_panes` is left out of the JSON when `None`, and JSON without it
/// reads as `None`; `Some(n)` round-trips.
#[test]
fn newest_panes_serializes_only_when_set() {
    let full: ASAPOp<()> = ASAPOp::SummaryMerge {
        children: vec![()],
        newest_panes: None,
        group_by: None,
    };
    let json = serde_json::to_string(&full).unwrap();
    assert!(!json.contains("newest_panes"), "{json}");
    let read: ASAPOp<()> = serde_json::from_str(r#"{"SummaryMerge":{"children":[null]}}"#).unwrap();
    assert_eq!(read, full);
    let partial: ASAPOp<()> = ASAPOp::SummaryMerge {
        children: vec![()],
        newest_panes: Some(6),
        group_by: None,
    };
    let json = serde_json::to_string(&partial).unwrap();
    assert_eq!(serde_json::from_str::<ASAPOp<()>>(&json).unwrap(), partial);
}

/// A rollup of a chain's panes to no keys (every series' panes into one
/// state) is no pane merge, and expansion keeps its keys: the expanded
/// rollup covers what the planned one does, one build over the whole
/// window and every series.
#[test]
fn expansion_keeps_a_rollup_of_the_panes() {
    let chain = chain(scan());
    let rollup = OperatorNode::new_shared(Operator::ASAP(ASAPOp::SummaryMerge {
        children: vec![Rc::clone(&chain)],
        group_by: Some(vec![]),
        newest_panes: None,
    }))
    .unwrap();
    assert!(merged_chain(&rollup).is_none());
    let expanded = expand_pane_chains(&rollup).unwrap();
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
    // panes, it is not one.
    assert!(OperatorNode::new(Operator::ASAP(ASAPOp::SummaryMerge {
        children: vec![chain],
        newest_panes: Some(2),
        group_by: Some(vec![]),
    }))
    .is_err());
}

/// A rollup reads a merge of the newest panes: every series' newest 2 of 5
/// panes into one state covers one build by no keys over the newest 2
/// minutes.
#[test]
fn a_rollup_of_the_newest_panes_covers_their_window() {
    let rollup = OperatorNode::new_shared(Operator::ASAP(ASAPOp::SummaryMerge {
        children: vec![newest(chain(scan()), 2)],
        newest_panes: None,
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

/// A fine-width chain remains a legal candidate; interrupted expansion is a
/// typed budget stop, never a malformed chain or a partially successful plan.
#[test]
fn fine_pane_expansion_stops_at_the_planning_deadline() {
    use asap_types::ir::operator::pane_chain::PaneChainExpansionError;
    use asap_types::ir::QueryRoot;
    use asap_types::test_support::expand_pane_chains_in_workload_before;
    use std::time::Instant;
    let mut op = chain(scan()).operator.clone();
    let Operator::ASAP(ASAPOp::PaneChain {
        pane_width_ms,
        pane_count,
        ..
    }) = &mut op
    else {
        unreachable!()
    };
    *pane_width_ms = 1;
    *pane_count = 1_000_000;
    let roots = [QueryRoot::Operator(merge(vec![OperatorNode::new_shared(
        op,
    )
    .unwrap()]))];
    let deadline = Instant::now() + Duration::from_millis(1);
    assert!(matches!(
        expand_pane_chains_in_workload_before(&roots, Some(deadline)),
        Err(PaneChainExpansionError::DeadlineExceeded)
    ));
}

/// Supplying ample budget preserves complete expansion and shared pane identity.
#[test]
fn budgeted_expansion_preserves_every_pane_and_shared_reader() {
    use asap_types::ir::QueryRoot;
    use asap_types::test_support::expand_pane_chains_in_workload_before;
    use std::time::Instant;
    let shared = chain(scan());
    let roots = [
        QueryRoot::Operator(merge(vec![shared.clone()])),
        QueryRoot::Operator(merge(vec![shared])),
    ];
    let result = expand_pane_chains_in_workload_before(
        &roots,
        Some(Instant::now() + Duration::from_secs(10)),
    )
    .unwrap();
    assert_eq!(result.chains.len(), 1);
    assert_eq!(result.chains[0].panes.len(), 5);
    let QueryRoot::Operator(first) = &result.roots[0] else {
        unreachable!()
    };
    let QueryRoot::Operator(second) = &result.roots[1] else {
        unreachable!()
    };
    assert!(first
        .children()
        .iter()
        .zip(second.children())
        .all(|(a, b)| Rc::ptr_eq(a, b)));
}

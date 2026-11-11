//! Step 5: chain groups.
use std::collections::BTreeSet;
use std::time::Duration;

use asap_types::accuracy::AccuracyTarget;
use asap_types::ir::operator::arguments::{AtModifier, Reduction, TimeShift};
use asap_types::ir::{ASAPOp, NonASAPOp, Operator, TimeRangeKind};
use asap_types::stages::{
    CandidateForMemoGroupId, Memo, MemoGroupId, OriginOfCandidateForMemoGroup, RootPruneReason,
};
use asap_types::summary_type::parameter_list;
use asap_types::summary_type::{ExactKind, SketchAlgorithm};
use asap_types::workload::{
    DataWorkload, QueryRecurrence, QueryWorkloadEntry, RepeatedDemand, RepetitionInterval,
};

use crate::test_fixtures::*;

/// A mergeable state over a range gets one chain group per width with a
/// build per surviving θ; its readout reads the builds and the merges,
/// one per θ.
#[test]
fn a_mergeable_state_reads_a_chain_with_one_build_per_surviving_parameters() {
    let mut memo = Memo::new(&DataWorkload::default());
    let (_, state, readout) = quantile_over_time(&mut memo, SketchAlgorithm::Kll, 0.9, 300);
    query(&mut memo, readout, None);
    let logical = pass2(memo, &[entry(120_000)]);
    let memo = &logical.memo;
    assert_eq!(chain_widths_serving(memo, state), BTreeSet::from([60_000]));
    let chains = groups_where(memo, |e| {
        matches!(e, Operator::ASAP(ASAPOp::SummaryTumblingWindowChain { .. }))
    });
    let listed = parameter_list(SketchAlgorithm::Kll).len();
    assert_eq!(candidates_of(memo, chains[0]).len(), listed);
    // The builds and one merge per θ.
    assert_eq!(
        edges_into(&logical, own_candidate(readout), 0).len(),
        2 * listed
    );
}

/// No chain for a summary type whose states do not merge (an instant
/// rate), under `@ end()`, or for a query without a known grid.
#[test]
fn no_chain_without_a_merge_an_anchor_or_a_grid() {
    let mut memo = Memo::new(&DataWorkload::default());
    let input = scan(&mut memo, "m", &["job"]);
    let window = range(&mut memo, input, 300);
    let (irate, irate_root) =
        exact_state(&mut memo, window, ExactKind::IRate, Reduction::PerEntity);
    let anchored = shift(&mut memo, input, 0, Some(AtModifier::End));
    let anchored_window = range(&mut memo, anchored, 300);
    let (pinned, pinned_root) = exact_state(
        &mut memo,
        anchored_window,
        ExactKind::Sum,
        Reduction::PerEntity,
    );
    let other = scan(&mut memo, "n", &["job"]);
    let other_window = range(&mut memo, other, 300);
    let (no_grid, no_grid_root) = exact_state(
        &mut memo,
        other_window,
        ExactKind::Sum,
        Reduction::PerEntity,
    );
    for root in [irate_root, pinned_root, no_grid_root] {
        query(&mut memo, root, None);
    }
    let without_phase = QueryWorkloadEntry {
        recurrence: QueryRecurrence::Repeated(RepeatedDemand::FixedInterval(RepetitionInterval(
            60_000,
        ))),
        ..entry(60_000)
    };
    let logical = pass2(memo, &[entry(60_000), entry(60_000), without_phase]);
    for state in [irate, pinned, no_grid] {
        assert!(
            chain_widths_serving(&logical.memo, state).is_empty(),
            "G{}",
            state.0
        );
    }
    assert!(groups_where(&logical.memo, |e| matches!(
        e,
        Operator::ASAP(ASAPOp::SummaryTumblingWindowChain { .. })
    ))
    .is_empty());
}

/// A chain serves a reader only when its width divides the reader's
/// evaluation grid: `quantile_over_time(0.9, m[30s])` read every 15 s and
/// every 10 s is one state group evaluated on the 5 s grid, so of its 5,
/// 10 and 15 s chains only the 5 s one has edges, and the merges over the
/// others are pruned (a 15 s chain would answer the 10 s reader at times
/// it does not end on).
#[test]
fn a_chain_whose_width_does_not_divide_the_readers_grid_fails_its_edge() {
    let mut memo = Memo::new(&DataWorkload::default());
    let (_, state, readout) = quantile_over_time(&mut memo, SketchAlgorithm::Kll, 0.9, 30);
    query(&mut memo, readout, None);
    query(&mut memo, readout, None);
    let logical = pass2(memo, &[entry(15_000), entry(10_000)]);
    let memo = &logical.memo;
    assert_eq!(
        chain_widths_serving(memo, state),
        BTreeSet::from([5_000, 10_000, 15_000])
    );
    for edge in &logical.edges {
        if let Some(width) = chain_width_below(memo, edge.producer_candidate_id.group) {
            assert_eq!(width, 5_000, "{edge:?}");
        }
    }
    let mut live_widths = BTreeSet::new();
    for (index_in_group, merge) in candidates_of(memo, state).iter().enumerate() {
        let Operator::ASAP(ASAPOp::SummaryMerge { children, .. }) = &merge.expression.operator
        else {
            continue;
        };
        let merge = CandidateForMemoGroupId {
            group: state,
            index_in_group,
        };
        match pruned_entry(&logical, merge) {
            None => live_widths.extend(chain_width_below(memo, children[0])),
            Some(pruned) => assert!(
                pruned.causes.iter().any(|cause| matches!(
                    cause.root,
                    RootPruneReason::ChainWidthDoesNotDivideEvaluationGrid {
                        evaluation_period_ms: Some(5_000),
                        ..
                    }
                )),
                "{pruned:?}"
            ),
        }
    }
    assert_eq!(live_widths, BTreeSet::from([5_000]));
}

/// A state under `@ T` adds no width; it reads a chain another query's
/// width created when that width divides T.
#[test]
fn an_absolute_anchor_reuses_only_existing_chains() {
    let mut memo = Memo::new(&DataWorkload::default());
    let input = scan(&mut memo, "m", &["job"]);
    let window = range(&mut memo, input, 300);
    let (state, root) = exact_state(&mut memo, window, ExactKind::Sum, Reduction::PerEntity);
    query(&mut memo, root, None);
    let mut pinned = Vec::new();
    // T = …040 s lies on the 60 s grid; T = …010 s does not.
    for at_ms in [1_700_000_040_000, 1_700_000_010_000] {
        let at = shift(&mut memo, input, 0, Some(AtModifier::Timestamp(at_ms)));
        let at_window = range(&mut memo, at, 300);
        let (state, root) = exact_state(&mut memo, at_window, ExactKind::Sum, Reduction::PerEntity);
        query(&mut memo, root, None);
        pinned.push(state);
    }
    let logical = pass2(memo, &vec![entry(60_000); 3]);
    let widths = |state| chain_widths_serving(&logical.memo, state);
    assert_eq!(widths(state), BTreeSet::from([60_000]));
    assert_eq!(widths(pinned[0]), BTreeSet::from([60_000]));
    assert!(widths(pinned[1]).is_empty());
}

/// Chain groups come after the rollup
/// rule and the reach along candidate edges, so a state's widths come from
/// every query that reaches it through a candidate.
///
/// A `sum by (job, instance) (count_over_time(m[5m]))` every 60 s; B
/// `sum by (job) (count_over_time(m[5m]))` every 90 s (both at phase 0).
/// B's state rolls up A's, so both reach A's state, whose widths are the
/// gcd closure of {60 s, 90 s} with 300 s: 30 s and 60 s.
#[test]
fn chain_groups_run_after_reach_so_a_rolled_up_query_adds_its_grid() {
    let mut memo = Memo::new(&DataWorkload::default());
    let input = scan(&mut memo, "m", &["job", "instance"]);
    let window = range(&mut memo, input, 300);
    let (by_job_instance, a) = exact_state(
        &mut memo,
        window,
        ExactKind::Count,
        Reduction::by(vec![2, 3]),
    );
    let (by_job, b) = exact_state(&mut memo, window, ExactKind::Count, Reduction::by(vec![2]));
    let query_a = query(&mut memo, a, Some(AccuracyTarget::exact()));
    let query_b = query(&mut memo, b, Some(AccuracyTarget::exact()));
    let logical = pass2(memo, &[entry(60_000), entry(90_000)]);
    let memo = &logical.memo;
    assert!(
        candidates_of(memo, by_job).iter().any(|c| {
            c.origin == OriginOfCandidateForMemoGroup::Rollup
                && c.expression.operator.children() == vec![&by_job_instance]
        }),
        "B's state reads A's through a rollup"
    );
    let reached_by = memo.groups()[by_job_instance.0 as usize].reached_by_queries();
    assert!(
        reached_by.contains(&query_a) && reached_by.contains(&query_b),
        "A's state is reached by A and B: {reached_by:?}"
    );
    assert_eq!(
        chain_widths_serving(memo, by_job_instance),
        BTreeSet::from([30_000, 60_000])
    );
    assert_eq!(chain_widths_serving(memo, by_job), BTreeSet::from([30_000]));
}

/// A window read at an `offset` takes the
/// offset into the gcd, since its windows' edges move by it.
/// `sum_over_time(m[5m] offset 45s)` every 60 s (phase 0): widths divide
/// 60 s, 300 s and 45 s, so 15 s only.
#[test]
fn an_offset_enters_the_gcd_of_the_widths() {
    let mut memo = Memo::new(&DataWorkload::default());
    let input = scan(&mut memo, "m", &["job"]);
    let shifted = shift(&mut memo, input, 45, None);
    let window = range(&mut memo, shifted, 300);
    let (state, root) = exact_state(&mut memo, window, ExactKind::Sum, Reduction::PerEntity);
    query(&mut memo, root, Some(AccuracyTarget::exact()));
    let logical = pass2(memo, &[entry(60_000)]);
    assert_eq!(
        chain_widths_serving(&logical.memo, state),
        BTreeSet::from([15_000])
    );
}

/// A tumbling-window chain is a group
/// of its own, `SummaryTumblingWindowChain` over the rows with a width and
/// no range, offset or window count, so every range and offset of one rows
/// and width reads the same chain group through `TimeRange` (and
/// `TimeShift`) groups and a `SummaryMerge`.
///
/// A `sum_over_time(m[5m])`, B `sum_over_time(m[5m] offset 1h)`, C
/// `sum_over_time(m[1h])`, each every 60 s (phase 0): one chain group Gw of
/// width 60 s over the scan of m; the groups `TimeShift(1h, Gw)`,
/// `TimeRange(5m, Gw)`, `TimeRange(5m, TimeShift(1h, Gw))` and
/// `TimeRange(1h, Gw)`; each state group has a `SummaryMerge` candidate
/// over its `TimeRange` group.
#[test]
fn one_range_free_chain_group_serves_every_range_and_offset() {
    let mut memo = Memo::new(&DataWorkload::default());
    let input = scan(&mut memo, "m", &["job"]);
    let five_minutes = range(&mut memo, input, 300);
    let shifted_input = shift(&mut memo, input, 3600, None);
    let five_minutes_an_hour_ago = range(&mut memo, shifted_input, 300);
    let one_hour = range(&mut memo, input, 3600);
    let mut states = Vec::new();
    for window in [five_minutes, five_minutes_an_hour_ago, one_hour] {
        let (state, root) = exact_state(&mut memo, window, ExactKind::Sum, Reduction::PerEntity);
        query(&mut memo, root, Some(AccuracyTarget::exact()));
        states.push(state);
    }
    let logical = pass2(memo, &vec![entry(60_000); 3]);
    let memo = &logical.memo;
    let chains = groups_where(memo, |e| {
        matches!(e, Operator::ASAP(ASAPOp::SummaryTumblingWindowChain { .. }))
    });
    let [chain] = chains[..] else {
        panic!("one chain group, found {chains:?}");
    };
    let Operator::ASAP(ASAPOp::SummaryTumblingWindowChain {
        child,
        tumbling_window_width_ms,
        ..
    }) = &memo.groups()[chain.0 as usize].expression
    else {
        unreachable!();
    };
    assert_eq!(*tumbling_window_width_ms, 60_000);
    assert_eq!(
        *child, input,
        "the chain reads the scan: no range or shift below it"
    );
    let range_over = |range_s: u64, child: MemoGroupId| {
        group_of(
            memo,
            &Operator::NonASAP(NonASAPOp::TimeRange {
                range: Duration::from_secs(range_s),
                kind: TimeRangeKind::Range,
                child,
            }),
        )
    };
    let shifted_chain = group_of(
        memo,
        &Operator::NonASAP(NonASAPOp::TimeShift {
            shift: TimeShift {
                offset_ms: 3_600_000,
                at: None,
            },
            child: chain,
        }),
    )
    .expect("TimeShift(1h, Gw)");
    let ranges = [
        range_over(300, chain).expect("TimeRange(5m, Gw)"),
        range_over(300, shifted_chain).expect("TimeRange(5m, TimeShift(1h, Gw))"),
        range_over(3600, chain).expect("TimeRange(1h, Gw)"),
    ];
    for (state, range) in states.iter().zip(ranges) {
        assert!(
            candidates_of(memo, *state).iter().any(|c| matches!(
                &c.expression.operator,
                Operator::ASAP(ASAPOp::SummaryMerge { children, group_by: None })
                    if *children == vec![range]
            )),
            "G{} reads G{} through a SummaryMerge",
            state.0,
            range.0
        );
    }
}

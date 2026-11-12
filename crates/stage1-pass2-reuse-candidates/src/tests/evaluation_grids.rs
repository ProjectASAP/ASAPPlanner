//! The evaluation grid of each group, per query reaching it.
use std::collections::BTreeSet;
use std::time::Duration;

use asap_types::ir::operator::arguments::Reduction;
use asap_types::ir::operator::AggIntent;
use asap_types::ir::{ASAPOp, NonASAPOp, Operator};
use asap_types::stages::{Memo, MemoGroupId};
use asap_types::summary_type::ExactKind;
use asap_types::workload::{
    DataWorkload, QueryRecurrence, QueryWorkloadEntry, RepeatedDemand, RepetitionInterval,
};

use crate::test_fixtures::*;

/// A group's evaluation grid is the gcd of its reaching queries' grids;
/// `None` when one has no known grid.
#[test]
fn a_groups_evaluation_grid_is_the_gcd_of_its_queries_grids() {
    let mut memo = Memo::new(&DataWorkload::default());
    let input = scan(&mut memo, "m", &["job"]);
    let window = range(&mut memo, input, 300);
    let (_, a) = exact_state(&mut memo, window, ExactKind::Sum, Reduction::PerEntity);
    let other = scan(&mut memo, "n", &["job"]);
    query(&mut memo, a, None);
    query(&mut memo, a, None);
    query(&mut memo, other, None);
    let without_phase = QueryWorkloadEntry {
        recurrence: QueryRecurrence::Repeated(RepeatedDemand::FixedInterval(RepetitionInterval(
            60_000,
        ))),
        ..entry(60_000)
    };
    let logical = pass2(memo, &[entry(60_000), entry(120_000), without_phase]);
    let grid = |group: MemoGroupId| logical.evaluation_grid_of_group[group.0 as usize];
    assert_eq!(grid(window).map(|g| g.period_ms), Some(60_000));
    assert_eq!(grid(other), None);
}

/// `max_over_time(sum_over_time(m[2m])[10m:<resolution>])` read every 60 s
/// over a scan of `m`: the inner sum group and its state, the subquery
/// group and the query's root.
fn sum_over_time_in_a_subquery(
    memo: &mut Memo,
    resolution: Option<Duration>,
) -> (MemoGroupId, MemoGroupId, MemoGroupId) {
    let input = scan(memo, "m", &["job"]);
    let window = range(memo, input, 120);
    let (state, sum) = exact_state(memo, window, ExactKind::Sum, Reduction::PerEntity);
    let subquery = group(
        memo,
        Operator::NonASAP(NonASAPOp::PromqlSubquery {
            range: Duration::from_secs(600),
            resolution,
            child: sum,
        }),
    );
    let root = group(
        memo,
        Operator::NonASAP(NonASAPOp::Aggregate {
            child: subquery,
            reduction: Reduction::PerEntity,
            measures: vec![AggIntent::Max { col: None }],
            output_names: vec![String::new()],
            filters: vec![None],
            having: None,
        }),
    );
    query(memo, root, None);
    (state, subquery, root)
}

/// Below a subquery `[10m:30s]` the inner expression is evaluated at the
/// multiples of 30 s, whatever the outer grid: read every 60 s, the inner
/// sum's state is on the 30 s grid and reads a 30 s chain, not a 60 s one
/// whose windows end only at every other inner evaluation.
#[test]
fn below_a_subquery_the_grid_is_its_resolution() {
    let mut memo = Memo::new(&DataWorkload::default());
    let (state, subquery, root) =
        sum_over_time_in_a_subquery(&mut memo, Some(Duration::from_secs(30)));
    let logical = pass2(memo, &[entry(60_000)]);
    let period = |group: MemoGroupId| {
        logical.evaluation_grid_of_group[group.0 as usize].map(|grid| grid.period_ms)
    };
    assert_eq!(period(root), Some(60_000));
    assert_eq!(period(subquery), Some(60_000));
    assert_eq!(period(state), Some(30_000));
    assert_eq!(
        chain_widths_serving(&logical.memo, state),
        BTreeSet::from([30_000])
    );
}

/// A subquery without a resolution steps at the server's default
/// evaluation interval, unknown to the planner: the groups below it have
/// no known grid and get no chain.
#[test]
fn below_a_subquery_without_a_resolution_there_is_no_grid() {
    let mut memo = Memo::new(&DataWorkload::default());
    let (state, subquery, _) = sum_over_time_in_a_subquery(&mut memo, None);
    let logical = pass2(memo, &[entry(60_000)]);
    assert_eq!(
        logical.evaluation_grid_of_group[subquery.0 as usize].map(|grid| grid.period_ms),
        Some(60_000)
    );
    assert_eq!(logical.evaluation_grid_of_group[state.0 as usize], None);
    assert!(groups_where(&logical.memo, |e| matches!(
        e,
        Operator::ASAP(ASAPOp::SummaryTumblingWindowChain { .. })
    ))
    .is_empty());
}

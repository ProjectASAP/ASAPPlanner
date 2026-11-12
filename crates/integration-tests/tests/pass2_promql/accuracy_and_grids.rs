//! PromQL → Pass 2's error contracts and evaluation grids, through
//! production lowering: the frontend's `Project`s of a function of the
//! value keep a readout's error bounded, and a subquery's resolution is the
//! evaluation grid of the groups below it.
use crate::planning_common::promql_pass2;
use asap_types::ir::operator::asap::summary_type_of_operator;
use asap_types::ir::schema::FieldDataType;
use asap_types::ir::{ASAPOp, NonASAPOp, Operator};
use asap_types::stages::{CandidateForMemoGroupId, LogicalASAPMemoCandidates, MemoGroupId};
use serde_json::{json, Value};

/// Pass 2's output for `query` at `accuracy`.
fn pass2(query: &str, accuracy: Value) -> LogicalASAPMemoCandidates {
    promql_pass2(&[query], accuracy)
}

/// The sketch readouts (`SummaryEstimate` over a sketch state) that
/// survive Pass 2.
fn live_sketch_readouts(logical: &LogicalASAPMemoCandidates) -> usize {
    let memo = &logical.memo;
    let mut live = 0;
    for (group, memo_group) in memo.groups().iter().enumerate() {
        for (index_in_group, candidate) in memo_group.candidates.iter().enumerate() {
            let Operator::ASAP(ASAPOp::SummaryEstimate { summary_input, .. }) =
                &candidate.expression.operator
            else {
                continue;
            };
            let sketch = matches!(
                summary_type_of_operator(&memo.groups()[summary_input.0 as usize].expression),
                Some(FieldDataType::Sketch(_))
            );
            let id = CandidateForMemoGroupId {
                group: MemoGroupId(group as u32),
                index_in_group,
            };
            if sketch && logical.pruned.iter().all(|pruned| pruned.candidate != id) {
                live += 1;
            }
        }
    }
    live
}

/// The frontend lowers `abs(v)`, `-v` and `v / 2` to a `Project` that
/// replaces the value and passes the series identity through
/// `promql_drop_metric_name`. Its contract judges only the value: `abs`
/// and the negation keep an absolute error, scaling by a constant a
/// relative one, so a sketch readout below each still serves the query, as
/// it does for the bare readout.
#[test]
fn a_function_of_the_value_keeps_the_readouts_below_it() {
    let absolute = json!({"max_error": 0.05, "accuracy_metric": "absolute"});
    let relative = json!({"max_error": 0.05, "accuracy_metric": "relative"});
    for (query, accuracy) in [
        ("count_over_time(m[1m])", &absolute),
        ("abs(count_over_time(m[1m]))", &absolute),
        ("-count_over_time(m[1m])", &absolute),
        ("quantile_over_time(0.9, m[5m]) / 2", &relative),
        ("quantile_over_time(0.9, m[5m]) * 2", &relative),
    ] {
        let logical = pass2(query, accuracy.clone());
        assert!(live_sketch_readouts(&logical) > 0, "{query}");
    }
}

/// The groups whose expression is a tumbling-window chain, with its width.
fn chain_widths(logical: &LogicalASAPMemoCandidates) -> Vec<u64> {
    logical
        .memo
        .groups()
        .iter()
        .filter_map(|group| match group.expression {
            Operator::ASAP(ASAPOp::SummaryTumblingWindowChain {
                tumbling_window_width_ms,
                ..
            }) => Some(tumbling_window_width_ms),
            _ => None,
        })
        .collect()
}

/// `quantile_over_time(0.9, rate(c[2m])[10m:30s])` read every 60 s
/// evaluates the inner `rate` every 30 s: its groups are on the 30 s grid
/// and read only chains whose width divides 30 s. Without a resolution the
/// inner step is the server's default, unknown: no grid, no chain.
#[test]
fn a_subquery_s_resolution_is_the_grid_of_the_groups_below_it() {
    let rank = json!({"max_error": 0.01, "accuracy_metric": "rank"});
    let logical = pass2(
        "quantile_over_time(0.9, rate(c[2m])[10m:30s])",
        rank.clone(),
    );
    let subquery = logical
        .memo
        .groups()
        .iter()
        .position(|group| {
            matches!(
                group.expression,
                Operator::NonASAP(NonASAPOp::PromqlSubquery { .. })
            )
        })
        .expect("a subquery group");
    let child = logical.memo.groups()[subquery].expression.children()[0].0 as usize;
    let period = |group: usize| logical.evaluation_grid_of_group[group].map(|grid| grid.period_ms);
    assert_eq!(period(subquery), Some(60_000));
    assert_eq!(period(child), Some(30_000));
    let widths = chain_widths(&logical);
    assert!(!widths.is_empty());
    assert!(widths.iter().all(|width| 30_000 % width == 0), "{widths:?}");
    let without_resolution = pass2("max_over_time(rate(c[2m])[10m:])", rank);
    assert_eq!(chain_widths(&without_resolution), Vec::<u64>::new());
}

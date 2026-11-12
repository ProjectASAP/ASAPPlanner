//! PromQL → Pass 2's rollup rule, through production lowering: a coarse
//! cross-series aggregate is offered from a finer one over the same
//! per-series computation, although the two queries' scans list different
//! labels and an instant or range `TimeRange` (and a per-series function)
//! sits between each scan and its aggregate.
use asap_devtools::planning_stages::{lower_promql_roots, run_stages_until, RunUntil};
use asap_planner::PlanningModels;
use asap_types::ir::{NonASAPOp, Operator, QueryRoot};
use asap_types::stages::{Memo, MemoGroupId, OriginOfCandidateForMemoGroup};
use asap_types::workload::PlanningWorkload;
use serde_json::json;

/// Pass 2's memo of `queries`, each repeating every minute at a 1% error
/// target, over the gauge `m`.
fn pass2_memo(queries: &[&str]) -> Memo {
    let workload: PlanningWorkload = serde_json::from_value(json!({
        "query_workload": {
            "language": "promql",
            "repeating_queries": queries.iter().map(|query| json!({
                "query": query,
                "demand": {"fixed_interval_at": {"interval": 60000, "evaluation_phase": 0}},
                "requirements": {"accuracy": {"max_error": 0.01}},
                "predictability": {"predictable": {"known_at": null}},
            })).collect::<Vec<_>>(),
        },
        "data_workload": {
            "arrival": {"ingesting": {"rows_per_second": 1000.0}},
            "sources": {"m": {"series_count": 1000, "metric_type": "gauge", "sample_interval": 1000}},
        },
    }))
    .unwrap();
    workload.validate().unwrap();
    let roots = lower_promql_roots(&workload).unwrap();
    let entries: Vec<_> = workload.query_workload.entries().collect();
    let data = workload.data_workload.clone().unwrap_or_default();
    let run = run_stages_until(
        roots,
        &entries,
        &data,
        PlanningModels::builtin(),
        RunUntil::Pass2,
    )
    .unwrap();
    run.run.logical_asap_candidates.unwrap().memo
}

/// The root group of query `query`.
fn root(memo: &Memo, query: usize) -> MemoGroupId {
    match memo.queries()[query].root {
        QueryRoot::Operator(group) => group,
        ref other => panic!("query {query} has no operator root: {other:?}"),
    }
}

/// The groups the rollup candidates of `group` read: the finer group
/// itself, or the finer group under the compensating `Filter`.
fn rolled_up_from(memo: &Memo, group: MemoGroupId) -> Vec<MemoGroupId> {
    memo.groups()[group.0 as usize]
        .candidates
        .iter()
        .filter(|candidate| candidate.origin == OriginOfCandidateForMemoGroup::Rollup)
        .map(|candidate| {
            let [input] = candidate.expression.operator.children()[..] else {
                panic!("a rollup reads one input: {:?}", candidate.expression);
            };
            match &memo.groups()[input.0 as usize].expression {
                Operator::NonASAP(NonASAPOp::Filter { child, .. }) => *child,
                _ => *input,
            }
        })
        .collect()
}

/// Every rollup candidate of the memo.
fn rollup_count(memo: &Memo) -> usize {
    memo.groups()
        .iter()
        .flat_map(|group| &group.candidates)
        .filter(|candidate| candidate.origin == OriginOfCandidateForMemoGroup::Rollup)
        .count()
}

#[test]
fn an_instant_sum_rolls_up_from_the_finer_sum_over_a_scan_listing_more_labels() {
    let memo = pass2_memo(&["sum by (job) (m)", "sum by (job, instance) (m)"]);
    assert_eq!(rolled_up_from(&memo, root(&memo, 0)), vec![root(&memo, 1)]);
    assert!(rolled_up_from(&memo, root(&memo, 1)).is_empty());
}

/// A count rolls up as a sum of the finer counts, under the coarse
/// count's own column name (`count`, where the sum's would be `sum`).
#[test]
fn an_instant_count_rolls_up_from_the_finer_count() {
    let memo = pass2_memo(&["count by (job) (m)", "count by (job, instance) (m)"]);
    assert_eq!(rolled_up_from(&memo, root(&memo, 0)), vec![root(&memo, 1)]);
}

#[test]
fn a_sum_of_rates_rolls_up_from_the_finer_sum_of_the_same_rates() {
    let memo = pass2_memo(&[
        "sum by (job) (rate(m[5m]))",
        "sum by (job, instance) (rate(m[5m]))",
    ]);
    assert_eq!(rolled_up_from(&memo, root(&memo, 0)), vec![root(&memo, 1)]);
}

#[test]
fn a_label_matcher_becomes_a_compensating_filter_over_the_finer_sum() {
    let memo = pass2_memo(&[
        r#"sum by (job) (m{instance="i1"})"#,
        "sum by (job, instance) (m)",
    ]);
    let candidates = &memo.groups()[root(&memo, 0).0 as usize].candidates;
    let rollup = candidates
        .iter()
        .find(|candidate| candidate.origin == OriginOfCandidateForMemoGroup::Rollup)
        .expect("a rollup candidate");
    let [filter] = rollup.expression.operator.children()[..] else {
        panic!("a rollup reads one input");
    };
    let Operator::NonASAP(NonASAPOp::Filter { child, .. }) =
        &memo.groups()[filter.0 as usize].expression
    else {
        panic!("the rollup reads a compensating filter");
    };
    assert_eq!(*child, root(&memo, 1));
}

#[test]
fn rates_over_different_ranges_do_not_roll_up() {
    let memo = pass2_memo(&[
        "sum by (job) (rate(m[5m]))",
        "sum by (job, instance) (rate(m[10m]))",
    ]);
    assert_eq!(rollup_count(&memo), 0);
}

#[test]
fn different_per_series_functions_do_not_roll_up() {
    let memo = pass2_memo(&[
        "sum by (job) (rate(m[5m]))",
        "sum by (job, instance) (increase(m[5m]))",
    ]);
    assert_eq!(rollup_count(&memo), 0);
}

#[test]
fn a_standard_deviation_does_not_roll_up() {
    let memo = pass2_memo(&["stddev by (job) (m)", "stddev by (job, instance) (m)"]);
    assert_eq!(rollup_count(&memo), 0);
}

#[test]
fn a_finer_sum_narrowed_by_a_matcher_serves_no_wider_sum() {
    let memo = pass2_memo(&[
        "sum by (job) (m)",
        r#"sum by (job, instance) (m{instance="i1"})"#,
    ]);
    assert!(rolled_up_from(&memo, root(&memo, 0)).is_empty());
}

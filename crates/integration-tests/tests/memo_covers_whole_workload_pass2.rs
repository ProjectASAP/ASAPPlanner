//! The memo search covers the whole-workload Pass 2 (#958 step 3): on each
//! workload one of its rules applies to, the memo search selects a plan no
//! more expensive than the cheapest plan the whole-workload path builds from
//! every Pass 1 assignment. An ignored test is a gap: a capability the
//! memo lacks or a plan its search misses, to be closed before the
//! whole-workload path is deleted. The memo search is bounded by
//! [`MEMO_EVALUATIONS`], with a time limit long enough not to bind, so a
//! slower machine evaluates the same proposals; `shared_pane_chains` is the
//! exception (see there).
use asap_devtools::planner_layering_examples::{
    example1, shared_data_workload, EXAMPLE1_Q1, EXAMPLE1_Q2, SHARED_ROWS_PER_SECOND,
};
use asap_executor::physical_planner::promql_rows;
use asap_integration_tests::fixtures::lower_promql;
use asap_integration_tests::planning::explicit_assignment_plans;
use asap_logical_asap_optimization::per_computation_candidates::{
    generate_per_computation_candidates, PerComputationCandidates,
};
use asap_plan_selection::{
    run_stage1_to_stage3_with, PlanningModels, Stage1ToStage3Run, Stage1ToStage3Search,
};
use asap_types::ir::properties::AccuracyTarget;
use asap_types::ir::QueryRoot;
use asap_types::workload::{
    DataArrival, DataWorkload, QueryRecurrence, QueryRequirements, QueryWorkloadEntry,
    RepeatedDemand, RepetitionInterval,
};

/// Whole-workload assignments enumerated at most; every workload here has
/// fewer, so the whole-workload side is exhaustive.
const MAX_ASSIGNMENTS: usize = 256;

/// Assignments the memo search evaluates at most: the binding limit.
const MEMO_EVALUATIONS: usize = 32;

/// The memo search's time limit, generous so that the evaluation limit binds.
const MEMO_TIME_LIMIT_MS: u64 = 600_000;

/// The memo search's time limit where an evaluation takes tens of seconds,
/// so that the time limit binds before the evaluation limit.
const SLOW_EVALUATION_TIME_LIMIT_MS: u64 = 120_000;

/// The selected plan's cost per second.
fn selected_cost(run: &Stage1ToStage3Run<usize>) -> f64 {
    let selection = match run.stage3_plan_selection.as_ref() {
        Ok(selection) => selection,
        Err(error) => panic!("no selected plan: {error:?}"),
    };
    if let Some(report) = &selection.search.candidate_connections {
        println!(
            "status {} lower bound {:?} gap {:?} pending widths {} refinements {}/{}",
            report.status,
            report.lower_bound,
            report.relative_gap,
            report.pane_widths_pending,
            report.refinements,
            report.max_refinements
        );
    }
    selection.costs[&selection.selected]
        .cost_per_second
        .expect("cost model 1")
}

/// Every Pass 1 assignment of `inventory`: each target's every choice,
/// skipping the duplicates a fused computation makes of the targets it
/// absorbs.
fn every_assignment(inventory: &PerComputationCandidates<usize>) -> Vec<Vec<usize>> {
    let counts: Vec<_> = inventory.targets.iter().map(|t| t.choice_count()).collect();
    let total = counts.iter().product::<usize>();
    assert!(total <= MAX_ASSIGNMENTS, "{total} assignments");
    (0..total)
        .map(|mut n| {
            counts
                .iter()
                .map(|&count| {
                    let digit = n % count;
                    n /= count;
                    digit
                })
                .collect::<Vec<_>>()
        })
        .filter(|choice| !inventory.absorbed_choice_varies(choice))
        .collect()
}

/// One workload's selected plan costs per second.
struct Costs {
    /// The cheapest plan the whole-workload path builds.
    whole_workload: f64,
    /// The memo search's plan.
    memo: f64,
}

fn costs(
    roots: Vec<(usize, QueryRoot)>,
    entries: &[QueryWorkloadEntry],
    data: &DataWorkload,
    time_limit_ms: u64,
) -> Costs {
    let inventory = generate_per_computation_candidates(roots.clone()).unwrap();
    let assignments = every_assignment(&inventory);
    let whole = explicit_assignment_plans(inventory, assignments, entries, data);
    // Same models as `explicit_assignment_plans`, and the default search
    // (seed grids first).
    let memo = run_stage1_to_stage3_with(
        roots,
        entries,
        data,
        PlanningModels::builtin(),
        Stage1ToStage3Search {
            milp_time_limit_ms: Some(time_limit_ms),
            max_evaluations: Some(MEMO_EVALUATIONS),
            ..Default::default()
        },
    )
    .expect("Pass 1 accepts the workload");
    println!("memo side:");
    let memo_cost = selected_cost(&memo);
    println!(
        "whole-workload side ({} plans):",
        whole.stage2_physical_asap_optimization.len()
    );
    Costs {
        whole_workload: selected_cost(&whole),
        memo: memo_cost,
    }
}

/// Asserts the memo plan costs at most the whole-workload plan.
fn assert_memo_covers(
    queries: &[(&str, AccuracyTarget)],
    entries: &[QueryWorkloadEntry],
    data: &DataWorkload,
    time_limit_ms: u64,
) {
    let roots = queries
        .iter()
        .enumerate()
        .map(|(i, (query, accuracy))| {
            let root = lower_promql(query, accuracy.clone()).unwrap();
            let root = promql_rows::with_series_identity(&root).unwrap();
            (i, QueryRoot::Operator(root))
        })
        .collect();
    let Costs {
        whole_workload: whole,
        memo,
    } = costs(roots, entries, data, time_limit_ms);
    println!("whole-workload {whole} memo {memo}");
    assert!(
        memo <= whole * (1.0 + 1e-6),
        "memo {memo} costs more than whole-workload {whole}"
    );
}

/// `m`: 1,000,000 series sampled every 15 s, 100 jobs, and 15 minutes of
/// rows held, which bounds an instant selector's scan.
fn data() -> DataWorkload {
    let mut data = shared_data_workload(
        DataArrival::Ingesting {
            rows_per_second: SHARED_ROWS_PER_SECOND,
        },
        "m",
    );
    data.sources.get_mut("m").unwrap().distinct_values_per_label =
        [("job".to_string(), 100)].into();
    data.sources.get_mut("m").unwrap().rows_held = Some((SHARED_ROWS_PER_SECOND * 900.0) as u64);
    data
}

/// Each of `queries`, evaluated every minute.
fn every_minute(queries: &[(&str, AccuracyTarget)]) -> Vec<QueryWorkloadEntry> {
    queries
        .iter()
        .map(|(_, accuracy)| QueryWorkloadEntry {
            query: Default::default(),
            requirements: QueryRequirements {
                accuracy: accuracy.clone(),
                max_latency_ms: None,
            },
            predictability: Default::default(),
            recurrence: QueryRecurrence::Repeated(RepeatedDemand::FixedInterval(
                RepetitionInterval(60_000),
            )),
            range_evaluation: None,
        })
        .collect()
}

fn check(queries: &[(&str, AccuracyTarget)]) {
    assert_memo_covers(queries, &every_minute(queries), &data(), MEMO_TIME_LIMIT_MS);
}

fn approximate() -> AccuracyTarget {
    AccuracyTarget::max_error(0.01).failure_probability(0.001)
}

/// Identical expression: two aggregates of one per-series rate.
#[test]
fn identical_expression() {
    check(&[
        ("sum by (job) (rate(m[1m]))", AccuracyTarget::exact()),
        ("max by (job) (rate(m[1m]))", AccuracyTarget::exact()),
    ]);
}

/// Union scan labels: one metric read under two label selectors.
#[test]
fn union_scan_labels() {
    check(&[
        (
            "sum(sum_over_time(m{job=\"a\"}[5m]))",
            AccuracyTarget::exact(),
        ),
        (
            "sum(sum_over_time(m{job=\"b\"}[5m]))",
            AccuracyTarget::exact(),
        ),
    ]);
}

/// Shared sizes: two quantiles of one window at different error bounds.
#[test]
// Search miss (#963): the memo holds the whole-workload winner (one
// query-time KLL with k = 345, fp = 0.001, read by both estimates; the
// q = 0.5 reader is rewritten to it), but every query-time KLL plan has
// lower bound 0, so the search does not reach it. Measured: whole-workload
// 2.4145834/s; memo 3.2787667/s or 3.3738667/s depending on the machine
// (default budget), and 3.2787667/s at 300 s / 4096 evaluations (19 made,
// complete catalog, no proof of optimality).
#[ignore = "search miss: #963"]
fn shared_sizes() {
    check(&[
        ("quantile_over_time(0.95, m[5m])", approximate()),
        (
            "quantile_over_time(0.5, m[5m])",
            AccuracyTarget::max_error(0.05).failure_probability(0.001),
        ),
    ]);
}

/// Local panes: one quantile over a window longer than its step.
#[test]
fn local_panes() {
    check(&[("quantile_over_time(0.99, m[5m])", approximate())]);
}

/// Shared pane chains: nested windows of one sum.
#[test]
fn shared_pane_chains() {
    // An evaluation over the expanding pane-width rounds takes tens of
    // seconds (12 in 600 s), so this search is bounded by time: it passes
    // because the seed round already finds the whole-workload cost, and a
    // slower machine finds the same seed plan.
    let queries = [
        ("sum(sum_over_time(m[2m]))", AccuracyTarget::exact()),
        ("sum(sum_over_time(m[1m]))", AccuracyTarget::exact()),
    ];
    assert_memo_covers(
        &queries,
        &every_minute(&queries),
        &data(),
        SLOW_EVALUATION_TIME_LIMIT_MS,
    );
}

/// Rollup: a coarse sum beside the finer sum it can be read from.
#[test]
// Coverage gap (#979): the memo plan rolls up the same way but builds two
// identical finalize nodes over q1's state, so it cannot store q1's
// finalized rows once for both readers, as the whole-workload winner does.
// Measured: whole-workload 0.0126783/s; memo 0.0137854/s at the default
// budget (128-evaluation limit) and at 300 s / 4096 evaluations (202 made,
// time limit).
#[ignore = "coverage gap: #979"]
fn rollup() {
    check(&[
        ("sum by (job) (m)", AccuracyTarget::exact()),
        ("sum by (job, instance) (m)", AccuracyTarget::exact()),
    ]);
}

/// Exact top-k reuse: two top-k of one ranking.
#[test]
// Coverage gap (#979): the memo plan stores the full Sort with two
// identical Limit(10) nodes over it instead of one shared Limit(10) ▸ Sort.
// Measured: whole-workload 0.7756417/s; memo 0.7800940/s at the default
// budget and at 300 s / 4096 evaluations (35 made).
#[ignore = "coverage gap: #979"]
fn topk_reuse() {
    check(&[
        ("topk by (job) (5, rate(m[1m]))", AccuracyTarget::exact()),
        ("topk by (job) (10, rate(m[1m]))", AccuracyTarget::exact()),
    ]);
}

/// The #509 design doc's Example 1: a sum and a top-k of one counter.
#[test]
fn planner_layering_example1() {
    let workload = example1();
    let entries: Vec<_> = workload.query_workload.entries().collect();
    let queries: Vec<_> = [EXAMPLE1_Q1, EXAMPLE1_Q2]
        .into_iter()
        .zip(&entries)
        .map(|(query, entry)| (query, entry.requirements.accuracy.clone()))
        .collect();
    assert_memo_covers(
        &queries,
        &entries,
        workload.data_workload.as_ref().unwrap(),
        MEMO_TIME_LIMIT_MS,
    );
}

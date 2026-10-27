//! The memo search covers the whole-workload Pass 2 (#958 step 3): on each
//! workload one of its rules applies to, the memo search selects a plan no
//! more expensive than the cheapest plan the whole-workload path builds from
//! every Pass 1 assignment. An ignored test is a gap: a capability the
//! memo lacks or a plan its search misses, to be closed before the
//! whole-workload path is deleted. The memo search is time-limited and a
//! longer search only lowers its cost: a slow machine can fail a test,
//! never pass a gap.
use asap_devtools::planner_layering_examples::{
    example1, shared_data_workload, EXAMPLE1_Q1, EXAMPLE1_Q2, SHARED_ROWS_PER_SECOND,
};
use asap_executor::physical_planner::promql_rows;
use asap_integration_tests::fixtures::lower_promql;
use asap_integration_tests::planning::explicit_assignment_plans;
use asap_logical_asap_optimization::per_computation_candidates::generate_per_computation_candidates;
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

/// Every Pass 1 assignment of `roots`: each target's every choice, skipping
/// the duplicates a fused computation makes of the targets it absorbs.
fn every_assignment(roots: &[(usize, QueryRoot)]) -> Vec<Vec<usize>> {
    let inventory = generate_per_computation_candidates(roots.to_vec()).unwrap();
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

/// The cheapest whole-workload plan and the memo search's plan of one
/// workload, as (whole-workload cost, memo cost) per second.
fn costs(
    roots: Vec<(usize, QueryRoot)>,
    entries: &[QueryWorkloadEntry],
    data: &DataWorkload,
) -> (f64, f64) {
    let inventory = generate_per_computation_candidates(roots.clone()).unwrap();
    let whole = explicit_assignment_plans(inventory, every_assignment(&roots), entries, data);
    // Same models as `explicit_assignment_plans`, and the default search
    // (seed grids first).
    let memo = run_stage1_to_stage3_with(
        roots,
        entries,
        data,
        PlanningModels::builtin(),
        Stage1ToStage3Search {
            milp_time_limit_ms: Some(120_000),
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
    (selected_cost(&whole), memo_cost)
}

/// Asserts the memo plan costs at most the whole-workload plan.
fn assert_memo_covers(
    queries: &[(&str, AccuracyTarget)],
    entries: &[QueryWorkloadEntry],
    data: &DataWorkload,
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
    let (whole, memo) = costs(roots, entries, data);
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
    assert_memo_covers(queries, &every_minute(queries), &data());
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
// Gap: memo 3.2787667/s vs whole-workload 2.4145834/s (+36%); the memo
// search stops at its time limit with lower bound 0, so whether the memo
// lacks the plan or the search misses it is open.
#[ignore = "memo costs more than the whole-workload path"]
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
    check(&[
        ("sum(sum_over_time(m[2m]))", AccuracyTarget::exact()),
        ("sum(sum_over_time(m[1m]))", AccuracyTarget::exact()),
    ]);
}

/// Rollup: a coarse sum beside the finer sum it can be read from.
#[test]
// Gap: memo 0.0137854/s vs whole-workload 0.0126783/s (+8.7%), at the
// 128-evaluation limit with lower bound 0.
#[ignore = "memo costs more than the whole-workload path"]
fn rollup() {
    check(&[
        ("sum by (job) (m)", AccuracyTarget::exact()),
        ("sum by (job, instance) (m)", AccuracyTarget::exact()),
    ]);
}

/// Exact top-k reuse: two top-k of one ranking.
#[test]
// Gap: memo 0.7800940/s vs whole-workload 0.7756417/s (+0.6%), at the
// time limit with lower bound 0.
#[ignore = "memo costs more than the whole-workload path"]
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
    assert_memo_covers(&queries, &entries, workload.data_workload.as_ref().unwrap());
}

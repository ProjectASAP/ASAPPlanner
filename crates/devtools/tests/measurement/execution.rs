//! §2.7: the reference driver over hand-made data. Four 1 Hz series of
//! `m`, constant 1 and 2 (`job="a"`) and 3 and 4 (`job="b"`); every
//! expected answer is counted by hand over the window `(t − range, t]`.

use std::sync::mpsc;
use std::time::Duration;

use asap_devtools::measurement::execution::{
    execute, DataInput, EvaluationAnswers, ExecuteError, ExecuteRequest, ExecutionReport,
    RootDrainOrder,
};
use asap_devtools::measurement::samples::Sample;
use asap_planner::{DeploymentCapabilities, MaterializedViewRefresh, PlanningWorkload};

use super::common::{
    exact, exact_match, facts, four_constant_series, four_series_facts, job, no_raw_data,
    per_value, sample, workload,
};

const SUM_BY_JOB: &str = "sum by (job) (sum_over_time(m[1m]))";

fn sum_by_job() -> PlanningWorkload {
    workload(
        &[(SUM_BY_JOB, exact())],
        10_000,
        &[("m", four_series_facts())],
    )
}

fn run(request: &ExecuteRequest<'_>) -> ExecutionReport {
    execute(request).unwrap()
}

/// Root `root`'s value for `job` at evaluation `evaluation`.
fn value(report: &ExecutionReport, evaluation: usize, root: usize, name: &str) -> f64 {
    report.evaluations[evaluation].per_root[root].values[&job(name)]
}

/// At t = 600 s the window (540 s, 600 s] holds 60 samples per series:
/// a: 60 · (1 + 2) = 180, b: 60 · (3 + 4) = 420. A window closed on both
/// ends would hold 61 (183 / 427).
#[test]
fn execute_sum_over_time_hand() {
    let workload = sum_by_job();
    let request = ExecuteRequest::new(
        &workload,
        DataInput::Samples(four_constant_series(600_000, |_, _| None)),
        vec![600_000],
    );
    let report = run(&request);
    assert_eq!(value(&report, 0, 0, "a"), 180.0);
    assert_eq!(value(&report, 0, 0, "b"), 420.0);
    assert_eq!(report.evaluations[0].per_root[0].values.len(), 2);
}

/// Series 1's sample at exactly t − 60 s = 540 s is 1000: excluded, so a
/// is still 180; the sample at t = 600 s is included (else 177).
#[test]
fn execute_window_boundary() {
    let data = four_constant_series(600_000, |series, ts| {
        (series == 0 && ts == 540_000).then_some(1_000.0)
    });
    let workload = sum_by_job();
    let request = ExecuteRequest::new(&workload, DataInput::Samples(data), vec![600_000]);
    let report = run(&request);
    assert_eq!(value(&report, 0, 0, "a"), 180.0);
}

/// A counter `c_total` = t / 1000 on every series rises 1 per second:
/// `sum by (job) (rate(c_total[1m]))` is 2.0 for each job.
#[test]
fn execute_rate_linear_counter() {
    let mut samples = Vec::new();
    for ts in (0..=600_000).step_by(1_000) {
        for (job, instance) in [("a", "1"), ("a", "2"), ("b", "3"), ("b", "4")] {
            samples.push(sample(
                "c_total",
                &[("job", job), ("instance", instance)],
                ts,
                ts as f64 / 1000.0,
            ));
        }
    }
    let workload = workload(
        &[("sum by (job) (rate(c_total[1m]))", exact())],
        10_000,
        &[("c_total", four_series_facts())],
    );
    let report = run(&ExecuteRequest::new(
        &workload,
        DataInput::Samples(samples),
        vec![600_000],
    ));
    assert!(exact_match(value(&report, 0, 0, "a"), 2.0), "{report:?}");
    assert!(exact_match(value(&report, 0, 0, "b"), 2.0), "{report:?}");
}

/// Series 1 changes to 10 after t = 605 s. At 600 s: a = 180; at 610 s the
/// window holds 5 samples of 10: 180 − 5 + 50 = 225; at 620 s, 15:
/// 180 − 15 + 150 = 315. b stays 420.
#[test]
fn execute_several_eval_times() {
    let data = four_constant_series(620_000, |series, ts| {
        (series == 0 && ts > 605_000).then_some(10.0)
    });
    let workload = sum_by_job();
    let request = ExecuteRequest::new(
        &workload,
        DataInput::Samples(data),
        vec![600_000, 610_000, 620_000],
    );
    let report = run(&request);
    let a: Vec<f64> = (0..3).map(|i| value(&report, i, 0, "a")).collect();
    assert_eq!(a, vec![180.0, 225.0, 315.0]);
    assert!((0..3).all(|i| value(&report, i, 0, "b") == 420.0));
}

/// The same query planned at query time over raw data and as an
/// ingestion-time chain (no raw data) answers the same.
// provisional answer 21
#[test]
fn execute_ingestion_equals_query_time() {
    let data = four_constant_series(620_000, |series, ts| {
        (series == 0 && ts > 605_000).then_some(10.0)
    });
    let workload = sum_by_job();
    let times = vec![600_000, 610_000, 620_000];
    let query_time = run(&ExecuteRequest::new(
        &workload,
        DataInput::Samples(data.clone()),
        times.clone(),
    ));
    let mut chained = ExecuteRequest::new(&workload, DataInput::Samples(data), times);
    chained.deployment_capabilities = no_raw_data();
    let chained = run(&chained);
    for (left, right) in query_time.evaluations.iter().zip(&chained.evaluations) {
        for (l, r) in left.per_root.iter().zip(&right.per_root) {
            assert_eq!(
                l.values.keys().collect::<Vec<_>>(),
                r.values.keys().collect::<Vec<_>>()
            );
            for (key, value) in &l.values {
                assert!(
                    exact_match(*value, r.values[key]),
                    "{key:?}: {value} vs {}",
                    r.values[key]
                );
            }
        }
    }
}

/// Two queries over one producer: `sum` and `count` of each job's
/// samples in the last minute.
fn joint_workload() -> PlanningWorkload {
    workload(
        &[
            (SUM_BY_JOB, exact()),
            ("sum by (job) (count_over_time(m[1m]))", exact()),
        ],
        10_000,
        &[("m", four_series_facts())],
    )
}

/// Run `workload` over `data` at t = 600 s with `order` on another thread
/// and return its answers, failing after 10 s: a deadlock guard, not a
/// timing assertion.
fn answers_with_deadlock_guard(
    workload: &'static PlanningWorkload,
    data: Vec<Sample>,
    order: RootDrainOrder,
) -> Vec<EvaluationAnswers> {
    let (sender, receiver) = mpsc::channel();
    std::thread::spawn(move || {
        let mut request = ExecuteRequest::new(workload, DataInput::Samples(data), vec![600_000]);
        request.root_drain_order = order;
        let _ = sender.send(execute(&request).map(|report| report.evaluations));
    });
    receiver
        .recv_timeout(Duration::from_secs(10))
        .expect("the run did not finish within 10 s")
        .unwrap()
}

/// Draining the roots of a joint plan in any order gives the same answers
/// and finishes: sum 180 / 420, count 120 / 120.
#[test]
fn execute_drain_order() {
    let workload: &'static PlanningWorkload = Box::leak(Box::new(joint_workload()));
    let data = four_constant_series(600_000, |_, _| None);
    let mut answers = Vec::new();
    for order in [
        RootDrainOrder::Reversed,
        RootDrainOrder::WorkloadOrder,
        RootDrainOrder::Concurrent,
    ] {
        let evaluations = answers_with_deadlock_guard(workload, data.clone(), order);
        assert_eq!(evaluations[0].per_root[0].values[&job("a")], 180.0);
        assert_eq!(evaluations[0].per_root[1].values[&job("a")], 120.0);
        assert_eq!(evaluations[0].per_root[1].values[&job("b")], 120.0);
        answers.push(evaluations);
    }
    assert!(answers.windows(2).all(|pair| pair[0] == pair[1]));
}

/// Without raw data both queries read ingestion-time views fed by one
/// scan: at ingestion time the scan runs once per window and each batch
/// reaches both chain builds, so both views are complete (sum 180 / 420,
/// count 120 / 120), not one view missing the batches the other read first.
#[test]
fn execute_shared_ingestion_scan_feeds_every_consumer() {
    let mut request = ExecuteRequest::new(
        Box::leak(Box::new(joint_workload())),
        DataInput::Samples(four_constant_series(600_000, |_, _| None)),
        vec![600_000],
    );
    request.deployment_capabilities = no_raw_data();
    let report = run(&request);
    assert_eq!(value(&report, 0, 0, "a"), 180.0);
    assert_eq!(value(&report, 0, 0, "b"), 420.0);
    assert_eq!(value(&report, 0, 1, "a"), 120.0);
    assert_eq!(value(&report, 0, 1, "b"), 120.0);
}

/// A 1m window over a 10 s chain evaluated 20 times every 10 s: after the
/// warm-up the view holds a constant number of states, 6 windows × 2
/// groups.
#[test]
fn execute_retention_evicts() {
    let times: Vec<i64> = (0..20).map(|i| 600_000 + i * 10_000).collect();
    let mut request = ExecuteRequest::new(
        Box::leak(Box::new(sum_by_job())),
        DataInput::Samples(four_constant_series(*times.last().unwrap(), |_, _| None)),
        times,
    );
    request.deployment_capabilities = no_raw_data();
    let report = run(&request);
    let chain = report
        .views
        .iter()
        .find(|view| view.refresh == MaterializedViewRefresh::AtIngestion)
        .expect("an ingestion-time view");
    let after_warm_up = &chain.states_after_evaluation[1..];
    assert!(
        after_warm_up.iter().all(|&states| states == 12),
        "{after_warm_up:?}"
    );
}

/// An evaluation at t = 30 s, before a 1m window of data exists, is
/// refused, never answered from half a window.
// provisional answer 18
#[test]
fn execute_before_warmup() {
    let workload = sum_by_job();
    let request = ExecuteRequest::new(
        &workload,
        DataInput::Samples(four_constant_series(600_000, |_, _| None)),
        vec![30_000],
    );
    assert!(matches!(
        execute(&request),
        Err(ExecuteError::BeforeWarmUp { at_ms: 30_000, .. })
    ));
}

/// A plan with a view refreshed on first read (query-time results kept
/// across evaluations) is run, not refused: the view is refreshed once
/// and the second evaluation reuses it.
#[test]
fn execute_on_first_read_view() {
    let workload = workload(
        &[("sum by (job) (sum_over_time(m[1m] offset 1m))", exact())],
        10_000,
        &[("m", four_series_facts())],
    );
    let mut request = ExecuteRequest::new(
        Box::leak(Box::new(workload)),
        DataInput::Samples(four_constant_series(620_000, |_, _| None)),
        vec![610_000, 620_000],
    );
    request.deployment_capabilities = DeploymentCapabilities {
        query_time_results_retained: true,
        ..DeploymentCapabilities::default()
    };
    let report = run(&request);
    let view = report
        .views
        .iter()
        .find(|view| view.refresh == MaterializedViewRefresh::OnFirstRead)
        .expect("the plan has an on-first-read view");
    assert_eq!(view.refresh_count, 1, "{view:?}");
    assert_eq!(value(&report, 1, 0, "a"), 180.0);
}

/// `quantile_over_time(0.5, m[1m])` over values 1 … 60: exactly, 30.5 (the
/// interpolated median); a DDSketch with α = 0.01 answers within 1% of the
/// nearest-rank bracket [30, 31].
// provisional answer 4
#[test]
fn execute_quantile_reference() {
    // Values 1 … 60 in the window (540 s, 600 s].
    let samples: Vec<Sample> = (0..=600)
        .map(|second| {
            let value = if second > 540 {
                (second - 540) as f64
            } else {
                1.0
            };
            sample("lat", &[("job", "a")], second * 1_000, value)
        })
        .collect();
    let query = "quantile_over_time(0.5, lat[1m])";
    let exact_workload = workload(&[(query, exact())], 10_000, &[("lat", facts(1, 1_000))]);
    let exact_report = run(&ExecuteRequest::new(
        &exact_workload,
        DataInput::Samples(samples.clone()),
        vec![600_000],
    ));
    assert_eq!(value(&exact_report, 0, 0, "a"), 30.5);
    let sketch_workload = workload(
        &[(query, per_value("relative", 0.01, 0.0))],
        10_000,
        &[("lat", facts(1, 1_000))],
    );
    let sketch_report = run(&ExecuteRequest::new(
        &sketch_workload,
        DataInput::Samples(samples),
        vec![600_000],
    ));
    let answer = value(&sketch_report, 0, 0, "a");
    assert!((30.0 * 0.99..=31.0 * 1.01).contains(&answer), "{answer}");
}

/// Two runs, and two drain orders, give identical answer maps (I9).
#[test]
fn execute_answers_bit_identical() {
    let workload: &'static PlanningWorkload = Box::leak(Box::new(joint_workload()));
    let data = four_constant_series(620_000, |series, ts| {
        (series == 2 && ts % 7_000 == 0).then_some(0.1 * ts as f64)
    });
    let mut runs = Vec::new();
    for order in [
        RootDrainOrder::WorkloadOrder,
        RootDrainOrder::WorkloadOrder,
        RootDrainOrder::Reversed,
    ] {
        let mut request = ExecuteRequest::new(
            workload,
            DataInput::Samples(data.clone()),
            vec![600_000, 620_000],
        );
        request.root_drain_order = order;
        runs.push(run(&request).evaluations);
    }
    for pair in runs.windows(2) {
        for (left, right) in pair[0].iter().zip(&pair[1]) {
            for (l, r) in left.per_root.iter().zip(&right.per_root) {
                assert_eq!(
                    l.values
                        .iter()
                        .map(|(k, v)| (k.clone(), v.to_bits()))
                        .collect::<Vec<_>>(),
                    r.values
                        .iter()
                        .map(|(k, v)| (k.clone(), v.to_bits()))
                        .collect::<Vec<_>>()
                );
            }
        }
    }
}

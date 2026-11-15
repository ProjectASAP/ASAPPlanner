//! §2.10: regression tests for the mis-rankings the feasibility run found,
//! with tables calibrated on the same machine. The small variants (100
//! series, 70 minutes of data so a 1h window is covered) are T2; the full
//! ones (1,000 series) are T3.

use asap_devtools::measurement::benchmark::{
    bench, bench_showcase, ArmMeasurement, BenchArm, BenchReport, BenchRequest, DataSetting,
};
use asap_devtools::measurement::calibration_tables::{calibrate, CalibrateRequest, PlanningTables};
use asap_devtools::measurement::cost_calibration::CalibrationFamilies;
use asap_planner::PlanningWorkload;
use asap_types::ir::properties::ExecutionTiming;

use super::common::{exact, facts, per_value, workload};

fn calibrated_tables() -> PlanningTables {
    let calibration = calibrate(&CalibrateRequest {
        families: CalibrationFamilies::All,
        small: false,
        seed: 7,
    })
    .unwrap();
    PlanningTables {
        cost_inputs: asap_cost::CostInputs {
            measured_operation_costs: calibration.costs,
            ..asap_cost::CostInputs::default()
        },
        accuracy: calibration.accuracy,
        warnings: Vec::new(),
    }
}

fn arm(report: &BenchReport, which: BenchArm) -> &ArmMeasurement {
    report
        .arms
        .iter()
        .find(|arm| arm.arm == which)
        .expect("the arm ran")
}

fn cost(arm: &ArmMeasurement) -> f64 {
    arm.measured_cost_per_second.expect("a measured cost")
}

fn run(workload: &PlanningWorkload, series: u64, evaluation_interval_ms: u64) -> BenchReport {
    bench(&BenchRequest {
        workload,
        arms: vec![BenchArm::Exact, BenchArm::Planned, BenchArm::NoRaw],
        settings: vec![DataSetting {
            sample_interval_ms: 1_000,
            evaluation_interval_ms,
            raw_data_retained: true,
        }],
        series_count: series,
        duration_ms: 70 * 60_000,
        repetitions: 5,
        tables: calibrated_tables(),
        seed: 1,
        cost_gate: true,
        output_root: None,
    })
    .unwrap()
}

/// `sum by (job) (rate(requests_total[1h]))` at 1 Hz every 300 s with raw
/// data: today a query-time `SummaryAgg` (331 ms p50) is selected though
/// the stored chain is 45× faster, and predicted wrongly ranked. The
/// selected plan must cost at most 1.05 × the cheaper of exact and the
/// chain, and the predicted order of the two must be the measured one.
// provisional answer 1 and provisional answer 20
fn rate_every_300s(series: u64) {
    let workload = workload(
        &[("sum by (job) (rate(requests_total[1h]))", exact())],
        300_000,
        &[("requests_total", facts(series, 1_000))],
    );
    let report = run(&workload, series, 300_000);
    let (exact_arm, planned, chain) = (
        arm(&report, BenchArm::Exact),
        arm(&report, BenchArm::Planned),
        arm(&report, BenchArm::NoRaw),
    );
    assert!(cost(planned) <= 1.05 * cost(exact_arm).min(cost(chain)));
    let predicted = |arm: &ArmMeasurement| arm.predicted.cost_per_second.unwrap();
    assert_eq!(
        predicted(exact_arm) < predicted(chain),
        cost(exact_arm) < cost(chain),
        "predicted and measured disagree on query time against the chain"
    );
}

#[test]
#[ignore = "timing: run on a dedicated host"]
fn regression_rate_1hz_every_300s() {
    rate_every_300s(100);
}

#[test]
#[ignore = "milestone scale"]
fn regression_rate_1hz_every_300s_full() {
    rate_every_300s(1_000);
}

/// `quantile_over_time(0.99, latency[1h])` at 1 Hz every 10 s, 1% relative
/// at δ = 0.01, raw data: today a DDSketch built from raw data at query
/// time runs at 0.50× exact. The selected plan must cost at most 1.05 ×
/// exact (speedup ≥ 0.95×).
// provisional answer 1
fn quantile_not_slower(series: u64) {
    let workload = workload(
        &[(
            "quantile_over_time(0.99, latency[1h])",
            per_value("relative", 0.01, 0.01),
        )],
        10_000,
        &[("latency", facts(series, 1_000))],
    );
    let report = run(&workload, series, 10_000);
    let (exact_arm, planned) = (
        arm(&report, BenchArm::Exact),
        arm(&report, BenchArm::Planned),
    );
    assert!(cost(planned) <= 1.05 * cost(exact_arm));
    let speedup = exact_arm.measured.as_ref().unwrap().p50_query_ms
        / planned.measured.as_ref().unwrap().p50_query_ms;
    assert!(speedup >= 0.95, "speedup {speedup}");
}

#[test]
#[ignore = "timing: run on a dedicated host"]
fn regression_quantile_half_speed() {
    quantile_not_slower(100);
}

#[test]
#[ignore = "milestone scale"]
fn regression_quantile_half_speed_full() {
    quantile_not_slower(1_000);
}

/// Over the full showcase, the planner ranks the selected plan against
/// exact as measured in at least 90% of the rows, and measured / predicted
/// stays within 1.5× per plan kind (by the timing of the plan's first
/// root).
#[test]
#[ignore = "milestone scale"]
fn regression_ranking_rate() {
    let report = bench_showcase(false, &calibrated_tables(), None).unwrap();
    let mut right = 0;
    let mut rows = 0;
    for exact_arm in report.arms.iter().filter(|arm| arm.arm == BenchArm::Exact) {
        let Some(planned) = report.arms.iter().find(|arm| {
            arm.arm == BenchArm::Planned
                && arm.setting == exact_arm.setting
                && arm.plan.query_texts == exact_arm.plan.query_texts
        }) else {
            continue;
        };
        rows += 1;
        let predicted_order =
            planned.predicted.cost_per_second < exact_arm.predicted.cost_per_second;
        let measured_order = cost(planned) < cost(exact_arm);
        right += usize::from(predicted_order == measured_order);
    }
    assert!(rows > 0);
    assert!(
        right as f64 >= 0.9 * rows as f64,
        "{right} of {rows} ranked right"
    );
    for kind in [ExecutionTiming::QueryTime, ExecutionTiming::IngestionTime] {
        for arm in report.arms.iter().filter(|arm| {
            let root = arm.plan.dag.roots[0];
            arm.plan
                .dag
                .nodes
                .iter()
                .any(|node| node.id == root && node.output_state.timing == kind)
        }) {
            let ratio = cost(arm) / arm.predicted.cost_per_second.unwrap();
            assert!(
                (1.0 / 1.5..=1.5).contains(&ratio),
                "{kind:?}: measured / predicted {ratio}"
            );
        }
    }
}

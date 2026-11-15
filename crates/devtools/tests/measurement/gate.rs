//! §2.9: the bench gate's rule, and runs whose tables lie.

use std::path::PathBuf;

use asap_accuracy::{ErrorAtFailureProbability, MeasuredAccuracyEntry, MeasuredSketchAccuracy};
use asap_cost::{CostInputs, MeasuredOperationCosts, OperatorCost, OperatorKind};
use asap_devtools::measurement::benchmark::{
    bench, gate_verdict, BenchArm, BenchRequest, DataSetting, GateFailure, GateRow, GateVerdict,
};
use asap_devtools::measurement::calibration_tables::PlanningTables;
use asap_planner::PlanningWorkload;
use asap_types::accuracy::AccuracyMetric;
use asap_types::summary_type::{SketchParams, SketchStatistic};

use super::common::{facts, per_value, workload, TempDir};

fn setting() -> DataSetting {
    DataSetting {
        sample_interval_ms: 1_000,
        evaluation_interval_ms: 10_000,
        raw_data_retained: true,
    }
}

fn row(exact: Option<f64>, selected: Result<f64, String>, error: f64, target: f64) -> GateRow {
    GateRow {
        workload: "w".into(),
        setting: setting(),
        exact_cost_per_second: exact,
        selected_cost_per_second: selected,
        measured_error: error,
        target_error: target,
    }
}

fn verdict_of(row: GateRow) -> GateVerdict {
    gate_verdict(&[("w".into(), setting())], &[row], true)
}

fn fails(verdict: &GateVerdict) -> bool {
    matches!(verdict, GateVerdict::Fail(failures) if !failures.is_empty())
}

/// The threshold arithmetic: cost 105 against exact 100 passes (≤ 1.05 ×),
/// 105.01 fails; error 0.0100 against 0.01 passes (`within_limit`), 0.0101
/// fails; a missing exact arm, a failed selected arm, a NaN error, and a
/// missing or extra row each fail.
// provisional answer 1
#[test]
fn gate_rule_pure() {
    assert_eq!(
        verdict_of(row(Some(100.0), Ok(105.0), 0.0, 0.01)),
        GateVerdict::Pass
    );
    assert!(fails(&verdict_of(row(Some(100.0), Ok(105.01), 0.0, 0.01))));
    assert_eq!(
        verdict_of(row(Some(100.0), Ok(50.0), 0.0100, 0.01)),
        GateVerdict::Pass
    );
    assert!(fails(&verdict_of(row(Some(100.0), Ok(50.0), 0.0101, 0.01))));
    assert!(matches!(
        verdict_of(row(None, Ok(50.0), 0.0, 0.01)),
        GateVerdict::Fail(failures) if failures.iter().any(|f| matches!(f, GateFailure::ExactArmMissing { .. }))
    ));
    assert!(matches!(
        verdict_of(row(Some(100.0), Err("panicked".into()), 0.0, 0.01)),
        GateVerdict::Fail(failures) if failures.iter().any(|f| matches!(f, GateFailure::SelectedArmFailed { .. }))
    ));
    assert!(matches!(
        verdict_of(row(Some(100.0), Ok(50.0), f64::NAN, 0.01)),
        GateVerdict::Fail(failures) if failures.iter().any(|f| matches!(f, GateFailure::ErrorNotANumber { .. }))
    ));
    let missing = gate_verdict(
        &[("w".into(), setting()), ("v".into(), setting())],
        &[row(Some(100.0), Ok(50.0), 0.0, 0.01)],
        true,
    );
    assert!(
        matches!(&missing, GateVerdict::Fail(failures) if failures.iter().any(|f| matches!(f, GateFailure::MissingRow { .. })))
    );
    let extra = gate_verdict(&[], &[row(Some(100.0), Ok(50.0), 0.0, 0.01)], true);
    assert!(
        matches!(&extra, GateVerdict::Fail(failures) if failures.iter().any(|f| matches!(f, GateFailure::ExtraRow { .. })))
    );
    // Without the cost gate, a costly plan with a met target passes.
    assert_eq!(
        gate_verdict(
            &[("w".into(), setting())],
            &[row(Some(100.0), Ok(500.0), 0.0, 0.01)],
            false
        ),
        GateVerdict::Pass
    );
}

fn tables(costs: MeasuredOperationCosts, accuracy: MeasuredSketchAccuracy) -> PlanningTables {
    PlanningTables {
        cost_inputs: CostInputs {
            measured_operation_costs: costs,
            ..CostInputs::default()
        },
        accuracy,
        warnings: Vec::new(),
    }
}

fn request<'a>(
    workload: &'a PlanningWorkload,
    tables: PlanningTables,
    output_root: Option<PathBuf>,
) -> BenchRequest<'a> {
    BenchRequest {
        workload,
        arms: vec![BenchArm::Exact, BenchArm::Planned],
        settings: vec![setting()],
        series_count: 100,
        duration_ms: 600_000,
        repetitions: 5,
        tables,
        seed: 1,
        cost_gate: true,
        output_root,
    }
}

/// A cost table claiming the exact scan costs 1e4 ns per row and the
/// DDSketch build 1e-3 ns per row makes the planner pick the DDSketch
/// query-time build, which measures about 2× the exact plan (217 against
/// 82–107 ns per row in the feasibility run): the gate fails.
// provisional answer 1 and provisional answer 23
#[test]
#[ignore = "timing: run on a dedicated host"]
fn gate_fails_on_lying_cost_table() {
    let workload = workload(
        &[(
            "quantile_over_time(0.99, m[10m])",
            per_value("relative", 0.01, 0.01),
        )],
        10_000,
        &[("m", facts(100, 1_000))],
    );
    let lie = |operator, sketch, ns_per_unit| OperatorCost {
        operator,
        phase: None,
        sketch,
        exact: None,
        measure: None,
        ns_per_unit,
        ns_per_invocation: 0.0,
        memory_bytes_per_state: None,
    };
    let costs = MeasuredOperationCosts {
        operators: vec![
            lie(OperatorKind::Scan, None, 1e4),
            lie(
                OperatorKind::SummaryAgg,
                Some(SketchParams::DDSketch { alpha: 0.01 }),
                1e-3,
            ),
        ],
        ..MeasuredOperationCosts::default()
    };
    let report = bench(&request(
        &workload,
        tables(costs, MeasuredSketchAccuracy::default()),
        None,
    ))
    .unwrap();
    assert!(
        matches!(&report.verdict, GateVerdict::Fail(failures)
            if failures.iter().any(|f| matches!(f, GateFailure::CostAboveLimit { .. }))),
        "{:?}",
        report.verdict
    );
}

/// An accuracy table claiming KLL k = 26 has rank error 1e-4 makes the
/// planner pick k = 26 for rank target 0.005 at δ = 0.01; its measured
/// rank error is about 0.05–0.1: the gate fails on error (deterministic
/// under a seed).
// provisional answer 3
#[test]
fn gate_fails_on_lying_accuracy_table() {
    let workload = workload(
        &[(
            "quantile_over_time(0.99, m[10m])",
            per_value("rank", 0.005, 0.01),
        )],
        10_000,
        &[("m", facts(100, 1_000))],
    );
    let accuracy = MeasuredSketchAccuracy {
        entries: vec![MeasuredAccuracyEntry {
            sketch: SketchParams::Kll { k: 26 },
            statistic: SketchStatistic::Quantile { q: 0.99 },
            accuracy_metric: AccuracyMetric::Rank,
            trials: 1_000,
            error_at: vec![ErrorAtFailureProbability {
                failure_probability: 0.01,
                error: 1e-4,
            }],
        }],
    };
    let mut request = request(
        &workload,
        tables(MeasuredOperationCosts::default(), accuracy),
        None,
    );
    request.cost_gate = false;
    let report = bench(&request).unwrap();
    let planned = report
        .arms
        .iter()
        .find(|arm| arm.arm == BenchArm::Planned)
        .unwrap();
    assert!(
        planned
            .plan
            .dag
            .nodes
            .iter()
            .any(|node| node.parameters == Some(SketchParams::Kll { k: 26 })),
        "the lying table did not select k = 26"
    );
    assert!(
        matches!(&report.verdict, GateVerdict::Fail(failures)
            if failures.iter().any(|f| matches!(f, GateFailure::ErrorAboveTarget { .. }))),
        "{:?}",
        report.verdict
    );
}

/// The 1m-window counterexample: `quantile_over_time(0.99, m[1m])` at 1 Hz
/// every 10 s selects exact, or a plan within 1.05× of it: the gate passes.
// provisional answer 23
#[test]
#[ignore = "timing: run on a dedicated host"]
fn gate_counterexample_passes_by_exact() {
    let workload = workload(
        &[(
            "quantile_over_time(0.99, m[1m])",
            per_value("relative", 0.01, 0.01),
        )],
        10_000,
        &[("m", facts(100, 1_000))],
    );
    let report = bench(&request(
        &workload,
        tables(
            MeasuredOperationCosts::default(),
            MeasuredSketchAccuracy::default(),
        ),
        None,
    ))
    .unwrap();
    assert_eq!(report.verdict, GateVerdict::Pass);
}

/// A run writes `runs/<machine-key>/<run-id>/{workload.json, plan.txt,
/// results.csv, summary.md}`, one results row per workload and setting.
#[test]
fn gate_output_files() {
    let root = TempDir::new("runs");
    let workload = workload(
        &[(
            "sum by (job) (sum_over_time(m[1m]))",
            super::common::exact(),
        )],
        10_000,
        &[("m", super::common::four_series_facts())],
    );
    let mut request = request(
        &workload,
        tables(
            MeasuredOperationCosts::default(),
            MeasuredSketchAccuracy::default(),
        ),
        Some(root.0.clone()),
    );
    request.series_count = 4;
    request.cost_gate = false;
    let report = bench(&request).unwrap();
    let run_dir = report.run_dir.expect("the run was written");
    assert!(run_dir.starts_with(root.0.join("runs")), "{run_dir:?}");
    assert_eq!(
        run_dir.parent().unwrap().parent().unwrap(),
        root.0.join("runs")
    );
    for file in ["workload.json", "plan.txt", "results.csv", "summary.md"] {
        assert!(
            run_dir.join(file).is_file(),
            "{file} missing in {run_dir:?}"
        );
    }
    let results = std::fs::read_to_string(run_dir.join("results.csv")).unwrap();
    // A header and one row for one workload at one setting.
    assert_eq!(results.lines().count(), 2, "{results}");
}

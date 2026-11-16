//! Benchmarks (`asap_run bench`): each arm of a workload run end to end
//! over the same data, its measured usage and error beside the planner's
//! predicted cost, and the showcase gate.
//!
//! Gate example: an exact arm measured at cost 100 per second and a
//! selected plan at 105 pass (105 ≤ 1.05 × 100); 105.01 fails. A selected
//! plan whose measured error exceeds its target fails whatever its cost.

use std::collections::BTreeSet;
use std::path::PathBuf;

use asap_planner::{InstallablePlan, PlanningWorkload, SelectedPlanReport};
use asap_types::stages::{PhysicalASAPMemoCandidates, PhysicalCandidateId, PhysicalEdgeId};
use asap_types::workload::QueryWorkloadEntry;

use crate::measurement::calibration_tables::PlanningTables;

/// The plans one benchmark compares over the same data.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum BenchArm {
    /// The exact plan: the reference every error is measured against.
    Exact,
    /// The plan selected with the resolved tables.
    Planned,
    /// The plan selected with `raw_data_retained: false`.
    NoRaw,
}

/// The data setting of one gate row, e.g. 1 Hz evaluated every 10 s with
/// raw data retained.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct DataSetting {
    pub sample_interval_ms: u64,
    pub evaluation_interval_ms: u64,
    pub raw_data_retained: bool,
}

/// One `asap_run bench` run.
#[derive(Debug, Clone)]
pub struct BenchRequest<'a> {
    pub workload: &'a PlanningWorkload,
    pub arms: Vec<BenchArm>,
    pub settings: Vec<DataSetting>,
    pub series_count: u64,
    pub duration_ms: u64,
    /// Runs per arm; each measurement is their median.
    pub repetitions: u32,
    pub tables: PlanningTables,
    pub seed: u64,
    /// Whether the gate checks cost (`--no-cost-gate` turns it off; error
    /// is always checked).
    pub cost_gate: bool,
    /// Where `runs/<machine-key>/<run-id>/` goes; `None` writes nothing.
    pub output_root: Option<PathBuf>,
}

/// What a run measured of one arm, per second of wall time unless the
/// name says otherwise.
#[derive(Debug, Clone, PartialEq)]
pub struct MeasuredUsage {
    pub p50_query_ms: f64,
    pub p95_query_ms: f64,
    pub ingestion_cpu_seconds_per_second: f64,
    pub query_cpu_seconds_per_second: f64,
    pub average_memory_bytes: f64,
    pub peak_memory_bytes: u64,
    pub scanned_bytes_per_second: f64,
}

/// One arm of one setting.
#[derive(Debug, Clone)]
pub struct ArmMeasurement {
    pub arm: BenchArm,
    pub setting: DataSetting,
    pub plan: InstallablePlan,
    /// The planner's prediction for this plan, with the same cost inputs.
    pub predicted: SelectedPlanReport,
    /// `Err` when the arm did not run.
    pub measured: Result<MeasuredUsage, String>,
    /// Cost model 1 over the measured usage at the planner's own prices:
    /// the median of `repetition_costs_per_second`.
    pub measured_cost_per_second: Option<f64>,
    /// The same, per repetition, for the noise band.
    pub repetition_costs_per_second: Vec<f64>,
    /// The arm's error against the exact arm in the target's metric, at
    /// the target's failure probability.
    pub measured_error: Option<f64>,
}

/// One gate row: one workload at one setting.
#[derive(Debug, Clone, PartialEq)]
pub struct GateRow {
    pub workload: String,
    pub setting: DataSetting,
    /// `None` when the exact arm did not run.
    pub exact_cost_per_second: Option<f64>,
    /// `Err` when the selected arm did not run.
    pub selected_cost_per_second: Result<f64, String>,
    pub measured_error: f64,
    pub target_error: f64,
}

/// Why a gate failed.
#[derive(Debug, Clone, PartialEq)]
pub enum GateFailure {
    /// The selected plan cost more than 1.05 × the exact plan.
    CostAboveLimit {
        workload: String,
        setting: DataSetting,
    },
    /// Its error exceeded the target.
    ErrorAboveTarget {
        workload: String,
        setting: DataSetting,
    },
    /// Its error is not a number.
    ErrorNotANumber {
        workload: String,
        setting: DataSetting,
    },
    ExactArmMissing {
        workload: String,
        setting: DataSetting,
    },
    SelectedArmFailed {
        workload: String,
        setting: DataSetting,
    },
    /// An expected row is absent.
    MissingRow {
        workload: String,
        setting: DataSetting,
    },
    /// A row nobody expected.
    ExtraRow {
        workload: String,
        setting: DataSetting,
    },
}

/// The gate's verdict over a run's rows.
#[derive(Debug, Clone, PartialEq)]
pub enum GateVerdict {
    Pass,
    Fail(Vec<GateFailure>),
}

/// The gate over `rows`, which must be exactly the `expected` (workload,
/// setting) pairs; `cost_gate` false checks error and completeness only.
pub fn gate_verdict(
    expected: &[(String, DataSetting)],
    rows: &[GateRow],
    cost_gate: bool,
) -> GateVerdict {
    let _ = (expected, rows, cost_gate);
    todo!("asap_run: implement after test approval")
}

/// What one benchmark run produced.
#[derive(Debug, Clone)]
pub struct BenchReport {
    pub arms: Vec<ArmMeasurement>,
    pub rows: Vec<GateRow>,
    pub verdict: GateVerdict,
    /// `runs/<machine-key>/<run-id>/`, when written.
    pub run_dir: Option<PathBuf>,
}

/// Run `request`.
pub fn bench(request: &BenchRequest<'_>) -> Result<BenchReport, String> {
    let _ = request;
    todo!("asap_run: implement after test approval")
}

/// The fixed showcase suite (`--suite showcase`), at the per-PR scale
/// when `small` (100 series, 10 minutes of data).
pub fn showcase_suite(small: bool) -> Vec<(String, PlanningWorkload, Vec<DataSetting>)> {
    let _ = small;
    todo!("asap_run: implement after test approval")
}

/// Run the showcase suite with `tables`.
pub fn bench_showcase(
    small: bool,
    tables: &PlanningTables,
    output_root: Option<PathBuf>,
) -> Result<BenchReport, String> {
    let _ = (small, tables, output_root);
    todo!("asap_run: implement after test approval")
}

/// Measure one given plan of `workload` at `setting` (the exhaustive
/// oracle runs every enumerated plan through it).
pub fn bench_plan(
    plan: &InstallablePlan,
    workload: &PlanningWorkload,
    setting: &DataSetting,
    request: &BenchRequest<'_>,
) -> Result<ArmMeasurement, String> {
    let _ = (plan, workload, setting, request);
    todo!("asap_run: implement after test approval")
}

/// The installable plan of one plan over Stage 2's candidates: the
/// `candidates` chosen and the `edges` feeding each of their inputs (the
/// exhaustive oracle builds every enumerated plan with it).
pub fn installable_plan_of_selection(
    physical: &PhysicalASAPMemoCandidates,
    entries: &[QueryWorkloadEntry],
    candidates: &BTreeSet<PhysicalCandidateId>,
    edges: &BTreeSet<PhysicalEdgeId>,
) -> Result<InstallablePlan, String> {
    let _ = (physical, entries, candidates, edges);
    todo!("asap_run: implement after test approval")
}

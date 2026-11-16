//! A machine's calibration tables: running a calibration and writing its
//! directory, and finding and loading the tables a planning run reads
//! (shared by `asap_run` and `asap-planner`).
//!
//! Layout, under `ASAP_CALIBRATION_DIR` or `<repo>/calibration`:
//! `<machine-key>/{machine.json, costs.json, accuracy.json,
//! calibrate-run.json}`. Example: with `ASAP_CALIBRATION_DIR=/data/cal` on
//! machine `x86_64-intel-xeon-gold-6142-56c-registered-ddr4`, the cost table is
//! `/data/cal/x86_64-intel-xeon-gold-6142-56c-registered-ddr4/costs.json`, unless `--costs <path>`
//! names another.

use std::path::{Path, PathBuf};

use asap_accuracy::MeasuredSketchAccuracy;
use asap_cost::{CostInputs, MeasuredOperationCosts};

use crate::measurement::accuracy_calibration::MergeCountErrors;
use crate::measurement::cost_calibration::{CalibrationFamilies, CostFitRecord};
use crate::measurement::machine::MachineDescription;

/// The environment variable naming the calibration directory.
pub const CALIBRATION_DIR_ENV: &str = "ASAP_CALIBRATION_DIR";

/// One `asap_run calibrate` run.
#[derive(Debug, Clone, PartialEq)]
pub struct CalibrateRequest {
    pub families: CalibrationFamilies,
    /// The per-PR scale: fewer sizes and trials.
    pub small: bool,
    pub seed: u64,
}

/// What one calibration measured.
#[derive(Debug, Clone)]
pub struct Calibration {
    pub machine: MachineDescription,
    pub costs: MeasuredOperationCosts,
    pub accuracy: MeasuredSketchAccuracy,
    /// `calibrate-run.json`: every case's samples and fit.
    pub cost_fits: Vec<CostFitRecord>,
    /// `calibrate-run.json`: the errors per merge count.
    pub accuracy_by_merge_count: Vec<MergeCountErrors>,
}

/// Run a calibration on this machine.
pub fn calibrate(request: &CalibrateRequest) -> Result<Calibration, String> {
    let _ = request;
    todo!("asap_run: implement after test approval")
}

/// Write `calibration` to `<dir>/<machine-key>/`, replacing an earlier
/// run's files; returns that directory.
pub fn write_calibration(dir: &Path, calibration: &Calibration) -> Result<PathBuf, String> {
    let _ = (dir, calibration);
    todo!("asap_run: implement after test approval")
}

/// The tables a run was told to read on its command line.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct TableOverrides {
    /// `--costs <path>`.
    pub costs: Option<PathBuf>,
    /// `--accuracy <path>`.
    pub accuracy: Option<PathBuf>,
    /// `--cost-inputs <path>`: cost inputs, possibly with a cost table
    /// inside.
    pub cost_inputs: Option<PathBuf>,
}

/// Where one table is read from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TablePath {
    /// Named on the command line.
    Given(PathBuf),
    /// The machine's file in the calibration directory.
    Calibrated(PathBuf),
    /// No file: the run falls back to the formulas, warning that this
    /// path was looked at.
    Missing { looked_at: PathBuf },
}

/// Where a run's two tables are read from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedTablePaths {
    pub costs: TablePath,
    pub accuracy: TablePath,
}

/// Where the tables of machine `machine_key` are read from:
/// `overrides` first, then the calibration directory (`env_dir`, from
/// [`CALIBRATION_DIR_ENV`], else `<repo_root>/calibration`).
pub fn resolve_table_paths(
    overrides: &TableOverrides,
    env_dir: Option<&Path>,
    repo_root: Option<&Path>,
    machine_key: &str,
) -> ResolvedTablePaths {
    let _ = (overrides, env_dir, repo_root, machine_key);
    todo!("asap_run: implement after test approval")
}

/// The tables a planning run plans with.
#[derive(Debug, Clone)]
pub struct PlanningTables {
    pub cost_inputs: CostInputs,
    pub accuracy: MeasuredSketchAccuracy,
    /// What the run prints to standard error, e.g. that no table exists
    /// for this machine.
    pub warnings: Vec<String>,
}

/// Why a table could not be read: a corrupt or invalid file.
#[derive(Debug, Clone, PartialEq)]
pub struct TableLoadError {
    pub path: PathBuf,
    pub reason: String,
}

/// Load the tables `resolved` names, over the cost inputs
/// `overrides.cost_inputs` names (the defaults without one); a missing
/// table falls back to the formulas with a warning naming `machine_key`
/// and `asap_run calibrate`.
pub fn load_planning_tables(
    resolved: &ResolvedTablePaths,
    overrides: &TableOverrides,
    machine_key: &str,
) -> Result<PlanningTables, TableLoadError> {
    let _ = (resolved, overrides, machine_key);
    todo!("asap_run: implement after test approval")
}

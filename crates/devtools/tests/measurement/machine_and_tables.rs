//! §2.5: the machine key, which tables a run reads, and the fallback to
//! the formulas.

use std::path::Path;
use std::process::Command;

use asap_accuracy::NoMeasuredGuarantees;
use asap_cost::{CostInputs, MeasuredOperationCosts, OperatorCost, OperatorKind};
use asap_devtools::measurement::calibration_tables::{
    load_planning_tables, resolve_table_paths, TableOverrides, TablePath, CALIBRATION_DIR_ENV,
};
use asap_devtools::measurement::machine::MachineDescription;
use asap_planner::DeploymentCapabilities;

use super::common::{exact, facts, plan, workload, TempDir};

const E5_MODEL: &str = "Intel(R) Xeon(R) CPU E5-2683 v3 @ 2.00GHz";

/// `/proc/cpuinfo` of `cores` logical cores of `model`.
fn cpuinfo(model: &str, cores: u32) -> String {
    (0..cores)
        .map(|core| format!("processor\t: {core}\nmodel name\t: {model}\n\n"))
        .collect()
}

fn meminfo(kib: u64) -> String {
    format!("MemTotal:       {kib} kB\nMemFree:        1000 kB\n")
}

/// 56 logical cores of the E5-2683 v3 on x86_64 with `memory_kib` of memory,
/// DIMM types `dimm_types` (EDAC) and an optional override.
fn e5(
    memory_kib: u64,
    dimm_types: &[&str],
    memory_type_override: Option<&str>,
) -> MachineDescription {
    MachineDescription::from_proc(
        "x86_64",
        &cpuinfo(E5_MODEL, 56),
        &meminfo(memory_kib),
        dimm_types,
        memory_type_override,
        "Linux 5.15",
    )
    .unwrap()
}

const E5_MEMORY_KIB: u64 = 263_724_000;
const E5_KEY: &str = "x86_64-intel-xeon-e5-2683-v3-56c-registered-ddr4";

/// Architecture, model, cores and memory type give the slug: `(R)`, `CPU`
/// and `@ 2.00GHz` are dropped from the model, the rest is lowercased and
/// dash-joined.
// decided 2026-11-16: key = architecture + model + logical cores + memory type
#[test]
fn machine_key_from_cpu_cores_memory() {
    let machine = e5(E5_MEMORY_KIB, &["Registered-DDR4", "Registered-DDR4"], None);
    assert_eq!(machine.cpu_architecture, "x86_64");
    assert_eq!(machine.logical_cores, 56);
    assert_eq!(machine.memory_type, "Registered-DDR4");
    assert_eq!(machine.memory_kib, E5_MEMORY_KIB);
    assert_eq!(machine.machine_key(), E5_KEY);
}

/// The key is a function of architecture, CPU model, cores and memory type
/// only: this machine's detected key contains no token of its hostname that
/// the CPU model does not also contain, and the description has no hostname
/// to change.
#[test]
fn machine_key_ignores_hostname() {
    let fixture = e5(E5_MEMORY_KIB, &["Registered-DDR4"], None).machine_key();
    for hostname in ["a", "xeon-e5-2683", "node-17"] {
        // The fixture's inputs do not include a hostname, so every
        // hostname gives the same key, and it is never the hostname.
        assert_ne!(fixture, hostname);
    }
    let detected = MachineDescription::detect(None).unwrap();
    let key = detected.machine_key();
    let hostname = std::fs::read_to_string("/proc/sys/kernel/hostname").unwrap_or_default();
    let model = detected.cpu_model.to_lowercase();
    for token in hostname
        .trim()
        .to_lowercase()
        .split(|c: char| !c.is_ascii_alphanumeric())
    {
        if token.len() >= 3 && !model.contains(token) {
            assert!(
                !key.contains(token),
                "key {key} contains hostname token {token}"
            );
        }
    }
}

/// Total memory is not part of the key: 64 GiB and 256 GiB of the same
/// DIMM type give the same key, and `MemTotal` jitter across boots too.
#[test]
fn machine_key_ignores_memory_size() {
    let dimms = ["Registered-DDR4"];
    let key = e5(E5_MEMORY_KIB, &dimms, None).machine_key();
    assert_eq!(key, E5_KEY);
    assert_eq!(e5(64 * 1024 * 1024, &dimms, None).machine_key(), key);
    assert_eq!(e5(256 * 1024 * 1024, &dimms, None).machine_key(), key);
    assert_eq!(
        e5(E5_MEMORY_KIB * 1005 / 1000, &dimms, None).machine_key(),
        key
    );
}

/// A VM without EDAC and without an override: `unknown-memory` ends the
/// key, and the warning names it and the override.
#[test]
fn machine_key_without_edac_is_unknown_memory() {
    let machine = e5(E5_MEMORY_KIB, &[], None);
    assert_eq!(
        machine.machine_key(),
        "x86_64-intel-xeon-e5-2683-v3-56c-unknown-memory"
    );
    let warning = machine.warning().expect("a warning for unknown memory");
    assert!(warning.contains("unknown-memory"), "{warning}");
    assert!(warning.contains("ASAP_MEMORY_TYPE"), "{warning}");
    assert!(e5(E5_MEMORY_KIB, &["Registered-DDR4"], None)
        .warning()
        .is_none());
}

/// `--memory-type` / `ASAP_MEMORY_TYPE` supplies the type when EDAC has
/// none, and no warning is given.
#[test]
fn machine_key_memory_type_override() {
    let machine = e5(E5_MEMORY_KIB, &[], Some("Registered-DDR4"));
    assert_eq!(machine.machine_key(), E5_KEY);
    assert!(machine.warning().is_none());
}

/// DIMMs of different types: the sorted unique values joined, whatever the
/// DIMM order.
#[test]
fn machine_key_mixed_dimm_types() {
    for dimms in [
        ["Registered-DDR4", "LRDIMM-DDR4", "Registered-DDR4"],
        ["LRDIMM-DDR4", "Registered-DDR4", "LRDIMM-DDR4"],
    ] {
        assert_eq!(
            e5(E5_MEMORY_KIB, &dimms, None).machine_key(),
            "x86_64-intel-xeon-e5-2683-v3-56c-lrdimm-ddr4-registered-ddr4"
        );
    }
}

/// Only `[a-z0-9-]`, no leading, trailing or double dashes.
#[test]
fn machine_key_slug_charset() {
    let machine = MachineDescription::from_proc(
        "aarch64",
        &cpuinfo("AMD EPYC(TM) 7452 32-Core Processor  @ 2.35GHz (R)", 8),
        &meminfo(16_000_000),
        &["Registered-DDR4"],
        None,
        "Linux",
    )
    .unwrap();
    let key = machine.machine_key();
    assert!(
        key.chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-'),
        "{key}"
    );
    assert!(
        !key.contains("--") && !key.starts_with('-') && !key.ends_with('-'),
        "{key}"
    );
}

const KEY: &str = E5_KEY;

/// `--costs` first, then `ASAP_CALIBRATION_DIR`, then `<repo>/calibration`,
/// then nothing (the formulas).
// provisional answer 15: an `ASAP_CALIBRATION_DIR` without the machine's
// file falls back to the formulas; the repo default is read only when the
// variable is unset.
#[test]
fn path_precedence() {
    let env = TempDir::new("env");
    let repo = TempDir::new("repo");
    let given = env.write("given/costs.json", "{}");
    let env_costs = env.write(&format!("{KEY}/costs.json"), "{}");
    let env_accuracy = env.write(&format!("{KEY}/accuracy.json"), "{}");
    let repo_costs = repo.write(&format!("calibration/{KEY}/costs.json"), "{}");
    let overrides = TableOverrides {
        costs: Some(given.clone()),
        ..TableOverrides::default()
    };
    let resolved = resolve_table_paths(&overrides, Some(&env.0), Some(&repo.0), KEY);
    assert_eq!(resolved.costs, TablePath::Given(given));
    assert_eq!(resolved.accuracy, TablePath::Calibrated(env_accuracy));
    let resolved =
        resolve_table_paths(&TableOverrides::default(), Some(&env.0), Some(&repo.0), KEY);
    assert_eq!(resolved.costs, TablePath::Calibrated(env_costs));
    let resolved = resolve_table_paths(&TableOverrides::default(), None, Some(&repo.0), KEY);
    assert_eq!(resolved.costs, TablePath::Calibrated(repo_costs));
    assert_eq!(
        resolved.accuracy,
        TablePath::Missing {
            looked_at: repo.0.join(format!("calibration/{KEY}/accuracy.json"))
        }
    );
    let empty = TempDir::new("empty");
    let resolved = resolve_table_paths(
        &TableOverrides::default(),
        Some(&empty.0),
        Some(&repo.0),
        KEY,
    );
    assert_eq!(
        resolved.costs,
        TablePath::Missing {
            looked_at: empty.0.join(format!("{KEY}/costs.json"))
        }
    );
}

/// A small workload whose plan the tables can change.
fn rate_workload() -> asap_planner::PlanningWorkload {
    workload(
        &[("sum by (job) (rate(requests_total[5m]))", exact())],
        10_000,
        &[("requests_total", facts(100, 1_000))],
    )
}

/// No `costs.json` for this machine: one warning naming the machine key
/// and `asap_run calibrate`, and the plan is the default cost inputs'
/// plan, byte for byte.
#[test]
fn missing_costs_warns_and_falls_back() {
    let dir = TempDir::new("missing");
    let resolved = resolve_table_paths(&TableOverrides::default(), Some(&dir.0), None, KEY);
    let tables = load_planning_tables(&resolved, &TableOverrides::default(), KEY).unwrap();
    let costs_warnings: Vec<_> = tables
        .warnings
        .iter()
        .filter(|warning| warning.contains("costs"))
        .collect();
    assert_eq!(costs_warnings.len(), 1, "{:?}", tables.warnings);
    assert!(costs_warnings[0].contains(KEY), "{costs_warnings:?}");
    assert!(
        costs_warnings[0].contains("asap_run calibrate"),
        "{costs_warnings:?}"
    );
    let caps = DeploymentCapabilities::default();
    let fallback = plan(
        &rate_workload(),
        &tables.cost_inputs,
        &tables.accuracy,
        &caps,
    );
    let default = plan(
        &rate_workload(),
        &CostInputs::default(),
        &NoMeasuredGuarantees,
        &caps,
    );
    assert_eq!(
        serde_json::to_string(&fallback.0).unwrap(),
        serde_json::to_string(&default.0).unwrap()
    );
}

fn scan_table() -> MeasuredOperationCosts {
    MeasuredOperationCosts {
        operators: vec![OperatorCost {
            operator: OperatorKind::Scan,
            phase: None,
            sketch: None,
            exact: None,
            measure: None,
            ns_per_unit: 20.0,
            ns_per_invocation: 0.0,
            memory_bytes_per_state: None,
        }],
        ..MeasuredOperationCosts::default()
    }
}

/// Costs present, accuracy absent: exactly one warning, for the accuracy
/// table, and the plan is the costs-only plan with no measured accuracy.
#[test]
fn missing_accuracy_warns_separately() {
    let dir = TempDir::new("costs_only");
    dir.write(
        &format!("{KEY}/costs.json"),
        &serde_json::to_string(&scan_table()).unwrap(),
    );
    let resolved = resolve_table_paths(&TableOverrides::default(), Some(&dir.0), None, KEY);
    let tables = load_planning_tables(&resolved, &TableOverrides::default(), KEY).unwrap();
    assert_eq!(tables.warnings.len(), 1, "{:?}", tables.warnings);
    assert!(
        tables.warnings[0].contains("accuracy"),
        "{:?}",
        tables.warnings
    );
    assert_eq!(tables.cost_inputs.measured_operation_costs, scan_table());
    assert!(tables.accuracy.entries.is_empty());
    let costs_only = CostInputs {
        measured_operation_costs: scan_table(),
        ..CostInputs::default()
    };
    let caps = DeploymentCapabilities::default();
    assert_eq!(
        plan(
            &rate_workload(),
            &tables.cost_inputs,
            &tables.accuracy,
            &caps
        ),
        plan(&rate_workload(), &costs_only, &NoMeasuredGuarantees, &caps),
    );
}

/// `--costs` wins over the table inside `--cost-inputs`, with a warning.
// provisional answer 15
#[test]
fn costs_flag_wins_over_cost_inputs_table() {
    let dir = TempDir::new("precedence");
    let inner = MeasuredOperationCosts {
        ns_per_evaluation: 1_000.0,
        ..scan_table()
    };
    let cost_inputs = dir.write(
        "cost_inputs.json",
        &serde_json::to_string(&CostInputs {
            measured_operation_costs: inner,
            ..CostInputs::default()
        })
        .unwrap(),
    );
    let costs = dir.write("costs.json", &serde_json::to_string(&scan_table()).unwrap());
    let overrides = TableOverrides {
        costs: Some(costs),
        cost_inputs: Some(cost_inputs),
        ..TableOverrides::default()
    };
    let resolved = resolve_table_paths(&overrides, Some(&dir.0), None, KEY);
    let tables = load_planning_tables(&resolved, &overrides, KEY).unwrap();
    assert_eq!(tables.cost_inputs.measured_operation_costs, scan_table());
    assert!(
        tables
            .warnings
            .iter()
            .any(|warning| warning.contains("--costs")),
        "{:?}",
        tables.warnings
    );
}

/// A truncated or invalid table is an error naming its path, never a
/// silent fallback.
// provisional answer 15
#[test]
fn corrupt_costs_is_an_error() {
    for (name, contents) in [
        (
            "truncated",
            r#"{"operators": [{"operator": "Scan", "ns_per_unit""#,
        ),
        (
            "negative",
            r#"{"operators": [{"operator": "Scan", "ns_per_unit": -5.0}]}"#,
        ),
    ] {
        let dir = TempDir::new(name);
        let path = dir.write(&format!("{KEY}/costs.json"), contents);
        let resolved = resolve_table_paths(&TableOverrides::default(), Some(&dir.0), None, KEY);
        let error = load_planning_tables(&resolved, &TableOverrides::default(), KEY).unwrap_err();
        assert_eq!(error.path, path, "{name}");
    }
}

/// A run of `bin` on `workload_path` with the calibration directory `dir`.
fn run_with_calibration_dir(bin: &str, args: &[&str], dir: &Path) -> String {
    let output = Command::new(bin)
        .args(args)
        .env(CALIBRATION_DIR_ENV, dir)
        .output()
        .unwrap();
    String::from_utf8_lossy(&output.stderr).into_owned()
}

/// `asap-planner` and `asap_run` resolve the same tables: with the same
/// empty calibration directory, both print the same missing-table
/// warnings.
#[test]
fn asap_planner_cli_uses_same_resolution() {
    let dir = TempDir::new("cli");
    let workload = dir.write("w.json", &serde_json::to_string(&rate_workload()).unwrap());
    let workload = workload.to_str().unwrap();
    let warnings = |stderr: String| -> Vec<String> {
        stderr
            .lines()
            .filter(|line| line.contains("asap_run calibrate"))
            .map(str::to_owned)
            .collect()
    };
    let planner = warnings(run_with_calibration_dir(
        env!("CARGO_BIN_EXE_asap_planner"),
        &["--workload", workload],
        &dir.0,
    ));
    let run = warnings(run_with_calibration_dir(
        env!("CARGO_BIN_EXE_asap_run"),
        &[
            "execute",
            "--workload",
            workload,
            "--data",
            "facts",
            "--at",
            "600000",
        ],
        &dir.0,
    ));
    assert!(
        !planner.is_empty(),
        "asap-planner printed no missing-table warning"
    );
    assert_eq!(planner, run);
}

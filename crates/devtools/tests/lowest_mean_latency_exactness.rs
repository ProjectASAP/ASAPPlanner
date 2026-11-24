//! The speed-ups of cost model 3's `lowest_mean_latency` keep its optimum
//! (`docs/design_docs/proposals/stage3-lowest-mean-latency.md`, section
//! 2.5): with the pre-solve exclusions (θ dominance, the feasible-plan
//! bound), the flow-formulated reads and the lazy tie-break solves, the
//! selected plan's mean latency is the one the full MILP finds without
//! them, on sketch plan snapshot workloads and on a query that reads one
//! state twice (an in-query diamond: a ratio of two quantiles). These
//! comparisons solve exactly (`ASAP_STAGE3_EXACT=1`); the default stop
//! rule (a relative gap of 1 % or 10,000 branch-and-bound nodes) is
//! checked against the exact optimum. The MILP's latencies are the plan's
//! own: `plan_cost` of the selected plan gives each query the latency the
//! MILP does.

use std::path::PathBuf;
use std::process::Command;

use serde_json::Value;

/// The repository root.
fn root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")
}

/// What a selection under cost model 3 gives: the MILP's mean latency and
/// its size.
struct Selected {
    mean_latency_ms: f64,
    /// The solver's lower bound on any plan's mean latency, ms.
    lower_bound_ms: f64,
    /// The reported relative gap.
    relative_gap: f64,
    variables: u64,
    rows: u64,
}

/// The selection for `workload` on `deployment` (a sketch plan snapshot
/// deployment), cost model 3 with the viewer's cluster inputs, with the
/// speed-ups or without (`full`), solved exactly or with the default stop
/// rule.
fn selected_with(
    workload: &str,
    deployment: &str,
    ignore_accuracy: bool,
    full: bool,
    exact: bool,
) -> Selected {
    let root = root();
    let mut command = Command::new(env!("CARGO_BIN_EXE_asap_planner"));
    command
        .current_dir(&root)
        .args(["--workload", workload, "--run-until", "stage3"])
        .args([
            "--deployment-capabilities",
            &format!("crates/devtools/tests/sketch_plan_snapshots/deployments/{deployment}.capabilities.json"),
        ])
        .args(["--cost-inputs", cost_inputs_of(workload)])
        .args(["--format", "json"]);
    if ignore_accuracy {
        command.arg("--ignore-accuracy-guarantees");
    }
    if exact {
        command.env("ASAP_STAGE3_EXACT", "1");
    }
    if full {
        command
            .env("ASAP_STAGE3_THETA_DOMINANCE", "off")
            .env("ASAP_STAGE3_FEASIBLE_PLAN_BOUND", "off")
            .env("ASAP_STAGE3_TIE_BREAKS", "full")
            .env("ASAP_STAGE3_PATH_READS", "off");
    }
    let output = command.output().unwrap();
    let document: Value = serde_json::from_slice(&output.stdout)
        .unwrap_or_else(|_| panic!("{}", String::from_utf8_lossy(&output.stderr)));
    let stage3 = &document["stage3_plan_selection"];
    Selected {
        mean_latency_ms: stage3["milp_objective_mean_latency_ms"]
            .as_f64()
            .unwrap_or_else(|| panic!("{workload}: no plan: {}", stage3["error"])),
        lower_bound_ms: stage3["milp_optimality_gap"]["mean_latency_lower_bound_ms"]
            .as_f64()
            .unwrap(),
        relative_gap: stage3["milp_optimality_gap"]["relative_gap"]
            .as_f64()
            .unwrap(),
        variables: stage3["milp_variables"].as_u64().unwrap(),
        rows: stage3["milp_constraints"].as_u64().unwrap(),
    }
}

/// The exact selection, with the speed-ups or without (`full`).
fn selected(workload: &str, deployment: &str, ignore_accuracy: bool, full: bool) -> Selected {
    selected_with(workload, deployment, ignore_accuracy, full, true)
}

/// The default stop rule's plan is within its reported gap of the exact
/// optimum `optimum_ms`: its lower bound is at most the optimum, and its
/// mean latency at most the optimum plus the gap, which is at most 1 %
/// (every workload here solves within the node limit).
fn assert_within_gap(workload: &str, deployment: &str, ignore_accuracy: bool, optimum_ms: f64) {
    let stopped = selected_with(workload, deployment, ignore_accuracy, false, false);
    let tolerance = 1e-6 * optimum_ms.max(1.0);
    assert!(
        stopped.relative_gap <= 0.01 + 1e-9,
        "{workload}: gap {} over 1 %",
        stopped.relative_gap
    );
    assert!(
        stopped.lower_bound_ms <= optimum_ms + tolerance,
        "{workload}: bound {} ms over the optimum {optimum_ms} ms",
        stopped.lower_bound_ms
    );
    assert!(
        stopped.mean_latency_ms - optimum_ms
            <= stopped.relative_gap * stopped.mean_latency_ms + tolerance,
        "{workload}: {} ms, optimum {optimum_ms} ms, gap {}",
        stopped.mean_latency_ms,
        stopped.relative_gap
    );
    assert!(
        (stopped.relative_gap
            - (stopped.mean_latency_ms - stopped.lower_bound_ms) / stopped.mean_latency_ms)
            .abs()
            <= 1e-9,
        "{workload}: the reported gap is not (plan - bound) / plan"
    );
}

/// Assert the mean latency is the full MILP's, to the solver's precision.
fn assert_same_optimum(workload: &str, deployment: &str, ignore_accuracy: bool) {
    let fast = selected(workload, deployment, ignore_accuracy, false).mean_latency_ms;
    let full = selected(workload, deployment, ignore_accuracy, true).mean_latency_ms;
    assert!(
        (fast - full).abs() <= 1e-6 * full.abs().max(1.0),
        "{workload}: {fast} ms with the speed-ups, {full} ms without"
    );
}

/// The workloads below solve in seconds; the rest of the snapshot set,
/// slower without the speed-ups, is in the ignored test.
#[test]
fn the_speed_ups_keep_the_optimum() {
    let workloads = "crates/devtools/tests/sketch_plan_snapshots/workloads";
    for (workload, deployment, ignore_accuracy) in [
        ("topk-rate.json", "mvp", true),
        ("mixed-quantiles.json", "raw-retained", false),
        ("distinct-two-forms.json", "raw-retained", false),
        // An in-query diamond: one state read by both quantiles.
        ("ratio-of-quantiles-1h.json", "mvp", false),
    ] {
        assert_same_optimum(
            &format!("{workloads}/{workload}"),
            deployment,
            ignore_accuracy,
        );
    }
}

/// Every PromQL sketch plan snapshot workload (minutes without the
/// speed-ups), exact and with the default stop rule:
/// `cargo test -p asap-devtools --test devtools -- --ignored the_speed_ups_keep_every`.
#[test]
#[ignore]
fn the_speed_ups_keep_every_snapshot_workloads_optimum() {
    let root = root();
    let manifest: Value = serde_json::from_str(
        &std::fs::read_to_string(
            root.join("crates/devtools/tests/sketch_plan_snapshots/manifest.json"),
        )
        .unwrap(),
    )
    .unwrap();
    // The PromQL workloads: a SQL one needs its catalog tables.
    for entry in manifest["workloads"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|entry| entry.get("tables").is_none())
    {
        let deployment = match entry["deployment"].as_str().unwrap() {
            "raw_retained" => "raw-retained",
            other => other,
        };
        let ignore = entry["accuracy_guarantees"] == "ignored";
        let workload = entry["workload"].as_str().unwrap();
        assert_same_optimum(workload, deployment, ignore);
        let optimum = selected(workload, deployment, ignore, false).mean_latency_ms;
        assert_within_gap(workload, deployment, ignore, optimum);
    }
}

/// The four workloads the solve time was measured on keep their optimum
/// (pinned, as the full MILP found it) in a model no larger than measured
/// with the speed-ups (plus a quarter): a change that loses a speed-up or
/// moves the optimum fails here.
#[test]
fn the_measured_workloads_keep_their_optimum_in_a_bounded_model() {
    let snapshots = "crates/devtools/tests/sketch_plan_snapshots/workloads";
    for (workload, deployment, ignore_accuracy, mean_latency_ms, variables, rows) in [
        (
            format!("{snapshots}/topk-rate.json"),
            "mvp",
            true,
            186.89267,
            TOPK_VARIABLES,
            TOPK_ROWS,
        ),
        (
            format!("{snapshots}/mixed-quantiles.json"),
            "raw-retained",
            false,
            44.6893333333,
            MIXED_VARIABLES,
            MIXED_ROWS,
        ),
        (
            format!("{snapshots}/distinct-two-forms.json"),
            "raw-retained",
            false,
            36.7112080558,
            DISTINCT_VARIABLES,
            DISTINCT_ROWS,
        ),
        (
            "tools/dag-viewer/workloads/random-mixed-seed-862/workload-01.json".to_string(),
            "mvp",
            false,
            1457.15173913,
            SEED_VARIABLES,
            SEED_ROWS,
        ),
        // A dashboard at 10,000 series, priced with measured operation
        // costs (`cluster-lowest-mean-latency-measured.json`).
        (
            TYPICAL_DASHBOARD.to_string(),
            "mvp",
            false,
            825.602741972,
            TYPICAL_VARIABLES,
            TYPICAL_ROWS,
        ),
    ] {
        let selected = selected(&workload, deployment, ignore_accuracy, false);
        assert!(
            (selected.mean_latency_ms - mean_latency_ms).abs() <= 1e-6 * mean_latency_ms,
            "{workload}: mean latency {} ms, pinned {mean_latency_ms} ms",
            selected.mean_latency_ms
        );
        assert!(
            selected.variables <= variables * 5 / 4 && selected.rows <= rows * 5 / 4,
            "{workload}: {} variables and {} rows, measured {variables} and {rows}",
            selected.variables,
            selected.rows
        );
        assert_within_gap(&workload, deployment, ignore_accuracy, mean_latency_ms);
    }
}

/// θ-family dominance leaves out dominated families: with guarantees
/// ignored, the top-k of a rate has θ-families of its CMS heap that others
/// match or beat in every quantity, so its model is smaller with the
/// pruning (the feasible-plan bound, which would leave most of them out
/// too, off in both), at the same optimum.
#[test]
fn dominated_theta_families_are_left_out() {
    let workload = "crates/devtools/tests/sketch_plan_snapshots/workloads/topk-rate.json";
    let run = |theta_dominance: &str| -> Value {
        let output = Command::new(env!("CARGO_BIN_EXE_asap_planner"))
            .current_dir(root())
            .env("ASAP_STAGE3_FEASIBLE_PLAN_BOUND", "off")
            .env("ASAP_STAGE3_THETA_DOMINANCE", theta_dominance)
            .env("ASAP_STAGE3_EXACT", "1")
            .args(["--workload", workload, "--run-until", "stage3"])
            .args([
                "--deployment-capabilities",
                "crates/devtools/tests/sketch_plan_snapshots/deployments/mvp.capabilities.json",
                "--cost-inputs",
                "tools/dag-viewer/cost-inputs/cluster-lowest-mean-latency.json",
                "--ignore-accuracy-guarantees",
                "--format",
                "json",
            ])
            .output()
            .unwrap();
        let document: Value = serde_json::from_slice(&output.stdout).unwrap();
        document["stage3_plan_selection"].clone()
    };
    let (pruned, unpruned) = (run("on"), run("off"));
    let variables = |stage3: &Value| stage3["milp_variables"].as_u64().unwrap();
    let mean = |stage3: &Value| stage3["milp_objective_mean_latency_ms"].as_f64().unwrap();
    assert!(
        variables(&pruned) < variables(&unpruned),
        "{} variables with the pruning, {} without",
        variables(&pruned),
        variables(&unpruned)
    );
    assert!((mean(&pruned) - mean(&unpruned)).abs() <= 1e-6 * mean(&unpruned));
}

/// The dashboard workload at 10,000 series and its measured cost inputs.
const TYPICAL_DASHBOARD: &str = "tools/dag-viewer/workloads/typical-dashboard-10k/workload.json";

/// The cost inputs a workload is planned with: the viewer's cluster
/// inputs, or the measured ones for the dashboard at 10,000 series.
fn cost_inputs_of(workload: &str) -> &'static str {
    match workload == TYPICAL_DASHBOARD {
        true => "tools/dag-viewer/cost-inputs/cluster-lowest-mean-latency-measured.json",
        false => "tools/dag-viewer/cost-inputs/cluster-lowest-mean-latency.json",
    }
}

/// The model sizes measured with the speed-ups (variables, rows), with the
/// compressed-sample chains and decodes every raw range is offered.
const TOPK_VARIABLES: u64 = 1244;
const TOPK_ROWS: u64 = 2461;
const MIXED_VARIABLES: u64 = 942;
const MIXED_ROWS: u64 = 1589;
const DISTINCT_VARIABLES: u64 = 550;
const DISTINCT_ROWS: u64 = 990;
const SEED_VARIABLES: u64 = 3766;
const SEED_ROWS: u64 = 6871;
/// Each query's latency in the MILP, and the frequency-weighted mean, are
/// what `plan_cost` gives the selected plan (`--price-plan`), exactly and
/// with the default stop rule, on the five measured workloads.
#[test]
fn the_milp_latencies_are_the_plan_costs() {
    let snapshots = "crates/devtools/tests/sketch_plan_snapshots/workloads";
    for (workload, deployment, ignore_accuracy) in [
        (format!("{snapshots}/topk-rate.json"), "mvp", true),
        (
            format!("{snapshots}/mixed-quantiles.json"),
            "raw-retained",
            false,
        ),
        (
            format!("{snapshots}/distinct-two-forms.json"),
            "raw-retained",
            false,
        ),
        (
            "tools/dag-viewer/workloads/random-mixed-seed-862/workload-01.json".to_string(),
            "mvp",
            false,
        ),
        (TYPICAL_DASHBOARD.to_string(), "mvp", false),
    ] {
        for exact in [true, false] {
            assert_milp_latencies_are_plan_costs(&workload, deployment, ignore_accuracy, exact);
        }
    }
}

/// The same on every PromQL sketch plan snapshot workload (a SQL one needs
/// its catalog tables), exactly and with the default stop rule.
#[test]
fn the_milp_latencies_are_the_plan_costs_on_every_snapshot_workload() {
    for (workload, deployment, ignore_accuracy) in promql_snapshot_workloads() {
        for exact in [true, false] {
            assert_milp_latencies_are_plan_costs(&workload, &deployment, ignore_accuracy, exact);
        }
    }
}

/// The PromQL sketch plan snapshot workloads: (workload, deployment,
/// accuracy guarantees ignored).
fn promql_snapshot_workloads() -> Vec<(String, String, bool)> {
    let manifest: Value = serde_json::from_str(
        &std::fs::read_to_string(
            root().join("crates/devtools/tests/sketch_plan_snapshots/manifest.json"),
        )
        .unwrap(),
    )
    .unwrap();
    manifest["workloads"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|entry| entry.get("tables").is_none())
        .map(|entry| {
            let deployment = match entry["deployment"].as_str().unwrap() {
                "raw_retained" => "raw-retained",
                other => other,
            };
            (
                entry["workload"].as_str().unwrap().to_string(),
                deployment.to_string(),
                entry["accuracy_guarantees"] == "ignored",
            )
        })
        .collect()
}

/// `workload`'s selected plan under cost model 3, solved exactly or with
/// the default stop rule, has each query's MILP latency and the
/// frequency-weighted mean that `plan_cost` gives it (`--price-plan`).
fn assert_milp_latencies_are_plan_costs(
    workload: &str,
    deployment: &str,
    ignore_accuracy: bool,
    exact: bool,
) {
    let root = root();
    let name = workload.rsplit('/').next().unwrap();
    // A directory of its own per call (tests run in parallel, and a
    // workload may appear twice, on two deployments), removed at the end.
    static CALLS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    let call = CALLS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let scratch = std::env::temp_dir().join(format!("lml_plan_cost_{}_{call}", std::process::id()));
    std::fs::create_dir_all(&scratch).unwrap();
    let plan = scratch.join(format!("{name}-{exact}.plan.json"));
    let report = scratch.join(format!("{name}-{exact}.report.json"));
    let planner = |extra: &[&str]| {
        let mut command = Command::new(env!("CARGO_BIN_EXE_asap_planner"));
        command
            .current_dir(&root)
            .args(["--workload", workload, "--run-until", "stage3"])
            .args([
                "--deployment-capabilities",
                &format!("crates/devtools/tests/sketch_plan_snapshots/deployments/{deployment}.capabilities.json"),
            ])
            .args(["--cost-inputs", cost_inputs_of(workload)])
            .args(extra);
        if ignore_accuracy {
            command.arg("--ignore-accuracy-guarantees");
        }
        if exact {
            command.env("ASAP_STAGE3_EXACT", "1");
        }
        command.output().unwrap()
    };
    let selected = planner(&["--format", "json", "--plan-file", plan.to_str().unwrap()]);
    let document: Value = serde_json::from_slice(&selected.stdout)
        .unwrap_or_else(|_| panic!("{name}: {}", String::from_utf8_lossy(&selected.stderr)));
    let stage3 = &document["stage3_plan_selection"];
    let milp_mean_ms = stage3["milp_objective_mean_latency_ms"]
        .as_f64()
        .unwrap_or_else(|| panic!("{name}: no plan: {}", stage3["error"]));
    let milp_per_query_ms: Vec<f64> = stage3["milp_latency_ms_per_query"]
        .as_array()
        .unwrap()
        .iter()
        .map(|ms| ms.as_f64().unwrap())
        .collect();
    let priced = planner(&[
        "--out",
        "/dev/null",
        "--price-plan",
        plan.to_str().unwrap(),
        "--report-file",
        report.to_str().unwrap(),
    ]);
    assert!(
        priced.status.success(),
        "{name}: {}",
        String::from_utf8_lossy(&priced.stderr)
    );
    let report: Value = serde_json::from_str(&std::fs::read_to_string(&report).unwrap()).unwrap();
    let per_query = report["predicted_usage"]["per_query"].as_array().unwrap();
    let plan_cost_per_query_ms: Vec<f64> = per_query
        .iter()
        .map(|query| query["latency_ms"].as_f64().unwrap())
        .collect();
    let weights: Vec<f64> = per_query
        .iter()
        .map(|query| query["evaluations_per_second"].as_f64().unwrap())
        .collect();
    let plan_cost_mean_ms = plan_cost_per_query_ms
        .iter()
        .zip(&weights)
        .map(|(ms, weight)| ms * weight)
        .sum::<f64>()
        / weights.iter().sum::<f64>();
    let close = |a: f64, b: f64| (a - b).abs() <= 1e-6 * b.abs().max(1.0);
    assert!(
        close(milp_mean_ms, plan_cost_mean_ms),
        "{name} (exact {exact}): MILP mean {milp_mean_ms} ms, plan_cost {plan_cost_mean_ms} ms"
    );
    assert_eq!(
        milp_per_query_ms.len(),
        plan_cost_per_query_ms.len(),
        "{name}"
    );
    for (query, (milp, plan_cost)) in milp_per_query_ms
        .iter()
        .zip(&plan_cost_per_query_ms)
        .enumerate()
    {
        assert!(
            close(*milp, *plan_cost),
            "{name} (exact {exact}): q{query} MILP {milp} ms, plan_cost {plan_cost} ms"
        );
    }
    std::fs::remove_dir_all(&scratch).ok();
}

/// A first solve that finds no plan within its node limit
/// (`ASAP_STAGE3_NODE_LIMIT=0`) runs once more from cost model 1's plan
/// and selects a plan (one solve more than by default); when cost model
/// 1's plan breaks a limit (raw data on a 1 MB disk), there is no start
/// and the error says which limit.
#[test]
fn a_solve_without_a_plan_in_its_node_limit_retries_from_cost_model_1_s_plan() {
    let root = root();
    let workload = "crates/devtools/tests/sketch_plan_snapshots/workloads/distinct-two-forms.json";
    let cluster = "tools/dag-viewer/cost-inputs/cluster-lowest-mean-latency.json";
    let mut small_disk: Value =
        serde_json::from_str(&std::fs::read_to_string(root.join(cluster)).unwrap()).unwrap();
    small_disk["cost_model"]["cluster_capacity"]["disk_bytes"] = 1_000_000.into();
    let small_disk_path =
        std::env::temp_dir().join(format!("lml_small_disk_{}.json", std::process::id()));
    std::fs::write(&small_disk_path, small_disk.to_string()).unwrap();
    let plan = |cost_inputs: &str, node_limit: Option<&str>| {
        let mut command = Command::new(env!("CARGO_BIN_EXE_asap_planner"));
        command
            .current_dir(&root)
            .args(["--workload", workload, "--run-until", "stage3"])
            .args([
                "--deployment-capabilities",
                "crates/devtools/tests/sketch_plan_snapshots/deployments/raw-retained.capabilities.json",
            ])
            .args(["--cost-inputs", cost_inputs, "--out", "/dev/stdout"]);
        if let Some(nodes) = node_limit {
            command.env("ASAP_STAGE3_NODE_LIMIT", nodes);
        }
        let output = command.output().unwrap();
        (
            output.status.success(),
            String::from_utf8_lossy(&output.stdout).into_owned(),
            String::from_utf8_lossy(&output.stderr).into_owned(),
        )
    };
    let solves = |stdout: &str| {
        stdout
            .lines()
            .find(|line| line.starts_with("selection MILP:"))
            .and_then(|line| {
                line.split(" (")
                    .nth(1)?
                    .split(' ')
                    .next()?
                    .parse::<u32>()
                    .ok()
            })
    };
    let (ok, by_default, stderr) = plan(cluster, None);
    assert!(ok, "{stderr}");
    let (ok, retried, stderr) = plan(cluster, Some("0"));
    assert!(ok, "{stderr}");
    assert_eq!(
        solves(&retried),
        solves(&by_default).map(|solves| solves + 1)
    );
    let (ok, _, stderr) = plan(small_disk_path.to_str().unwrap(), Some("0"));
    assert!(!ok);
    assert!(
        stderr.contains("no plan within the node limit; cost model 1's plan breaks a limit (DiskBytes), so no start was available"),
        "{stderr}"
    );
    // With the default node limit the plan keeping no raw data fits.
    let (ok, _, stderr) = plan(small_disk_path.to_str().unwrap(), None);
    assert!(ok, "{stderr}");
    std::fs::remove_file(&small_disk_path).ok();
}

const TYPICAL_VARIABLES: u64 = 4940;
const TYPICAL_ROWS: u64 = 9031;

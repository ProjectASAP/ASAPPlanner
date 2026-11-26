// planner_bench --workload <file> [--sql-catalog-table <json>]...
//     [--deployment-capabilities <file>] [--ignore-accuracy-guarantees]
//
// Plans one workload through Stage 3 without keeping inspection copies
// and prints one JSON line: per-stage wall time (the facade's
// `PlanningReport`, plus frontend lowering), per-stage peak RSS, memo /
// candidate / MILP sizes and whether a plan was selected. Unlike
// `asap_planner`, nothing is formatted, so the numbers are the planner's.
//
// Peak RSS per stage comes from a thread sampling /proc/self/statm every
// millisecond; the stages' boundaries are reconstructed from the report's
// durations, so a stage shorter than a few samples reads its neighbours'.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use asap_cost::CostInputs;
use asap_devtools::planning_stages::{lower_sql_roots, sql_catalog};
use asap_planner::{run_planning_until, PlanSearchSettings, PlanningModels, RunUntil};
use asap_types::deployment_capabilities::DeploymentCapabilities;
use asap_types::workload::PlanningWorkload;
use serde_json::json;

fn rss_bytes() -> u64 {
    let statm = std::fs::read_to_string("/proc/self/statm").unwrap_or_default();
    let pages: u64 = statm
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    pages * 4096
}

fn vm_hwm_bytes() -> u64 {
    let status = std::fs::read_to_string("/proc/self/status").unwrap_or_default();
    status
        .lines()
        .find_map(|l| l.strip_prefix("VmHWM:"))
        .and_then(|v| v.trim().trim_end_matches("kB").trim().parse::<u64>().ok())
        .unwrap_or(0)
        * 1024
}

fn main() {
    let mut workload_path = None;
    let mut tables = Vec::new();
    let mut capabilities_path = None;
    let mut ignore_accuracy = false;
    let mut args = std::env::args().skip(1);
    while let Some(flag) = args.next() {
        match flag.as_str() {
            "--workload" => workload_path = args.next(),
            "--sql-catalog-table" => tables.push(args.next().expect("a table")),
            "--deployment-capabilities" => capabilities_path = args.next(),
            "--ignore-accuracy-guarantees" => ignore_accuracy = true,
            other => {
                eprintln!("planner_bench: unknown argument {other}");
                std::process::exit(2);
            }
        }
    }
    let workload_path = workload_path.expect("--workload is required");
    let workload: PlanningWorkload =
        serde_json::from_str(&std::fs::read_to_string(&workload_path).expect("workload reads"))
            .expect("workload parses");
    workload.validate().expect("workload validates");
    let mut capabilities: DeploymentCapabilities = match &capabilities_path {
        Some(path) => serde_json::from_str(&std::fs::read_to_string(path).expect("reads"))
            .expect("capabilities parse"),
        None => DeploymentCapabilities::default(),
    };
    if capabilities.supported_summaries.is_none() {
        capabilities.supported_summaries = Some(asap_executor::supported_summary_families());
    }
    let cost_inputs = CostInputs::default();
    let (catalog, _) = sql_catalog(&tables).expect("catalog");

    let samples: Arc<Mutex<Vec<(Instant, u64)>>> = Arc::new(Mutex::new(Vec::new()));
    let stop = Arc::new(AtomicBool::new(false));
    let sampler = {
        let (samples, stop) = (samples.clone(), stop.clone());
        std::thread::spawn(move || {
            while !stop.load(Ordering::Relaxed) {
                samples.lock().unwrap().push((Instant::now(), rss_bytes()));
                std::thread::sleep(Duration::from_millis(1));
            }
        })
    };

    let rss_start = rss_bytes();
    let lowering_started = Instant::now();
    let lowered = lower_sql_roots(&workload, &catalog);
    let lowering_ms = lowering_started.elapsed().as_secs_f64() * 1000.0;
    let roots = match lowered {
        Ok(roots) => roots,
        Err(error) => {
            println!(
                "{}",
                json!({"workload": workload_path, "status": "lowering_error", "error": error})
            );
            return;
        }
    };
    let entries: Vec<_> = workload.query_workload.entries().collect();
    let data = workload.data_workload.clone().unwrap_or_default();
    let models = PlanningModels::builtin()
        .with_cost_inputs(&cost_inputs)
        .with_deployment_capabilities(&capabilities)
        .with_ignore_accuracy_guarantees(ignore_accuracy);

    let rss_before_planning = rss_bytes();
    let planning_started = Instant::now();
    let run = run_planning_until(
        &roots,
        &entries,
        &data,
        models,
        PlanSearchSettings { inspect: false },
        RunUntil::Stage3,
    );
    let planning_ms = planning_started.elapsed().as_secs_f64() * 1000.0;
    stop.store(true, Ordering::Relaxed);
    sampler.join().unwrap();
    let samples = samples.lock().unwrap();

    // Peak sampled RSS over [from_ms, to_ms) after planning started, counting
    // the last sample before `from_ms` too: a stage shorter than the sampling
    // period otherwise has no sample.
    let ms_of = |t: &Instant| t.saturating_duration_since(planning_started).as_secs_f64() * 1000.0;
    let peak = |from_ms: f64, to_ms: f64| -> u64 {
        let before = samples
            .iter()
            .filter(|(t, _)| *t < planning_started || ms_of(t) < from_ms)
            .last()
            .map(|(_, rss)| *rss)
            .unwrap_or(rss_before_planning);
        samples
            .iter()
            .filter(|(t, _)| *t >= planning_started && ms_of(t) >= from_ms && ms_of(t) < to_ms)
            .map(|(_, rss)| *rss)
            .max()
            .unwrap_or(0)
            .max(before)
    };

    let line = match run {
        Err(error) => json!({
            "workload": workload_path,
            "status": "pass1_error",
            "error": error.to_string(),
            "queries": entries.len(),
            "lowering_ms": lowering_ms,
            "planning_ms": planning_ms,
            "vm_hwm_bytes": vm_hwm_bytes(),
        }),
        Ok(run) => {
            let r = &run.report;
            let b1 = r.pass1_ms;
            let b2 = b1 + r.pass2_ms;
            let b3 = b2 + r.stage2_ms;
            let selection = run.selection.as_ref();
            let (status, error, milp) = match selection {
                Ok((_, s)) => (
                    "ok",
                    None,
                    json!({
                        "variables": s.milp_variable_count,
                        "constraints": s.milp_constraint_count,
                        "construction_ms": s.milp_construction_ms,
                        "solve_ms": s.milp_solve_ms,
                        "solve_count": s.milp_solve_count,
                        "stage3_pruned_candidates": s.pruned_candidates.len(),
                        "objective_cost_per_second": s.selected_plan_cost_per_second,
                    }),
                ),
                Err(e) => ("selection_error", Some(e.to_string()), json!(null)),
            };
            json!({
                "workload": workload_path,
                "status": status,
                "error": error,
                "queries": entries.len(),
                "lowering_ms": lowering_ms,
                "planning_ms": planning_ms,
                "stage0_pass1_ms": r.pass1_ms,
                "pass2_ms": r.pass2_ms,
                "stage2_ms": r.stage2_ms,
                "stage3_ms": r.stage3_ms,
                "memo_groups": r.memo_groups,
                "logical_candidates": r.logical_candidates,
                "physical_candidates": r.physical_candidates,
                "stage2_pruned_candidates": r.stage2_pruned_candidates,
                "milp": milp,
                "rss_start_bytes": rss_start,
                "rss_before_planning_bytes": rss_before_planning,
                "peak_rss_stage0_pass1_bytes": peak(0.0, b1),
                "peak_rss_pass2_bytes": peak(b1, b2),
                "peak_rss_stage2_bytes": peak(b2, b3),
                "peak_rss_stage0_2_bytes": peak(0.0, b3),
                "peak_rss_stage3_bytes": peak(b3, f64::INFINITY),
                "vm_hwm_bytes": vm_hwm_bytes(),
                "rss_samples": samples.len(),
            })
        }
    };
    println!("{line}");
}

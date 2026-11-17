// asap_run: runs the Planner's executor as one lane of the engine comparison
// of sketch-bench's harness (`aqpbm-harness`).
//
//   asap_run bench-external --lane asap --run-dir <dir> [--variant <v>]...
//       [--core <n>] [--warmup <n>] [--runs <n>] [--max-core-busy <f>]
//
// Runs every (workload, variant) cell of the run directory another step
// prepared (`workloads/<w>.json`, `plans/<v>/<w>.plan.json` written by
// `asap_planner --plan-file`) on `asap_lane::AsapEngine`, which installs each
// plan file as given, and writes `results/asap/<w>__<v>.cell.json` and the
// arm results beside them, as `approxbench bench-external run` does for
// its lanes; `approxbench bench-external report` then joins every lane.
//   - `--variant`: only these variants (repeatable); every one by default.
//   - `--core`: the core each step is pinned to (default 0).
//   - `--warmup`, `--runs`: runs discarded, and measured (odd), per step
//     (the harness's defaults).
//   - `--max-core-busy`: a core busier than this fraction before a step
//     refuses it (the harness's default).
// Prints one line per cell. Exits 2 on a usage error, 1 when the lane
// cannot run (a cell that fails is written as not run, and the next runs).

use std::path::PathBuf;
use std::process::ExitCode;

use aqpbm_harness::bench_external::{run_lane, LaneSetup};
use aqpbm_harness::engine::EngineUnderTest;
use aqpbm_harness::protocol::{
    load_probe_for, MeasurementClocks, OsCpuClock, ProcessCpuFiles, Protocol,
};
use aqpbm_harness::rundir::RunDir;
use asap_devtools::asap_lane::AsapEngine;

const USAGE: &str =
    "usage: asap_run bench-external --lane asap --run-dir <dir> [--variant <v>]... \
[--core <n>] [--warmup <n>] [--runs <n>] [--max-core-busy <f>]";

/// The lanes this binary runs.
const LANES: [&str; 1] = ["asap"];

enum Failure {
    Usage(String),
    Run(String),
}

fn main() -> ExitCode {
    match run(std::env::args().skip(1).collect()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(Failure::Usage(error)) => {
            eprintln!("asap_run: {error}\n{USAGE}");
            ExitCode::from(2)
        }
        Err(Failure::Run(error)) => {
            eprintln!("asap_run: {error}");
            ExitCode::FAILURE
        }
    }
}

fn run(args: Vec<String>) -> Result<(), Failure> {
    let mut args = args.into_iter();
    match args.next().as_deref() {
        Some("bench-external") => {}
        Some(other) => return Err(Failure::Usage(format!("unknown command {other}"))),
        None => return Err(Failure::Usage("a command is required".into())),
    }
    let mut lane = None;
    let mut run_dir = None;
    let mut variants = Vec::new();
    let mut core = 0;
    let mut protocol = Protocol::default();
    while let Some(flag) = args.next() {
        let value = args
            .next()
            .ok_or_else(|| Failure::Usage(format!("{flag} needs a value")))?;
        let number = |value: &str| -> Result<u32, Failure> {
            value
                .parse()
                .map_err(|e| Failure::Usage(format!("{flag} {value}: {e}")))
        };
        match flag.as_str() {
            "--lane" => lane = Some(value),
            "--run-dir" => run_dir = Some(PathBuf::from(value)),
            "--variant" => variants.push(value),
            "--core" => core = number(&value)? as usize,
            "--warmup" => protocol.warmup_runs = number(&value)?,
            "--runs" => protocol.measured_runs = number(&value)?,
            "--max-core-busy" => {
                protocol.max_core_busy_fraction = value
                    .parse()
                    .map_err(|e| Failure::Usage(format!("{flag} {value}: {e}")))?
            }
            other => return Err(Failure::Usage(format!("unknown argument {other}"))),
        }
    }
    let engine = match lane.as_deref() {
        Some("asap") => AsapEngine,
        Some(other) => {
            return Err(Failure::Usage(format!(
                "unknown lane {other}; known lanes: {}",
                LANES.join(", ")
            )))
        }
        None => return Err(Failure::Usage("--lane is required".into())),
    };
    let run_dir = run_dir.ok_or_else(|| Failure::Usage("--run-dir is required".into()))?;
    let run_dir = RunDir::open(&run_dir).map_err(|e| Failure::Run(e.to_string()))?;
    let clocks = MeasurementClocks::with_calling_thread(&OsCpuClock);
    let mut probe = load_probe_for(&engine.cpu_accounting(), &ProcessCpuFiles::os())
        .map_err(|e| Failure::Run(e.to_string()))?;
    let only = (!variants.is_empty()).then_some(variants.as_slice());
    let cells = run_lane(
        &run_dir,
        &engine,
        only,
        LaneSetup {
            protocol: &protocol,
            core,
            probe: probe.as_mut(),
            clocks: &clocks,
            args: std::env::args().collect(),
        },
    )
    .map_err(|e| Failure::Run(e.to_string()))?;
    for cell in cells {
        println!("{} {} {:?}", cell.workload, cell.variant, cell.outcome);
    }
    Ok(())
}

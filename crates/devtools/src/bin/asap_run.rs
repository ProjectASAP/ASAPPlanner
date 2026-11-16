//! `asap_run`: execute plans on the Planner's executor, calibrate measured
//! unit costs and sketch accuracy per machine, bench predicted against
//! measured cost, and compute source facts from data
//! (`asap_devtools::measurement`).
//!
//! ```text
//! asap_run execute   --workload w.json [--data facts|file.jsonl] --at <ms>[,<ms>...]
//! asap_run calibrate [--families all|<list>] [--small]
//! asap_run bench     --workload w.json --arms exact,planned,noraw --duration <ms>
//! asap_run bench     --suite showcase [--small] [--no-cost-gate]
//! asap_run facts     --data <file> --workload w.json [--trust-observed-bounds]
//! ```
//!
//! Every subcommand reads the machine's tables as `asap-planner` does:
//! `--costs` / `--accuracy`, else `ASAP_CALIBRATION_DIR` or
//! `<repo>/calibration`, else the formulas with a warning.

fn main() {
    todo!("asap_run: implement after test approval")
}

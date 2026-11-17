//! The in-process ASAP lane of the engine comparison: the Planner's
//! executor as an engine under test of the sketch-bench harness
//! (`aqpbm_core::harness`).
//!
//! [`AsapEngine`] installs a plan file `asap_planner --plan-file` wrote (an
//! [`InstallablePlan`]) as given, so this lane and every other lane of a
//! run execute the same DAG; `asap_run bench-external --lane asap` runs it
//! over a run directory. The lane is a deployment runtime over the
//! executor's public API: it keeps the raw samples and the stored views in
//! one [`InMemoryDataStore`](asap_executor::sources::InMemoryDataStore), refreshes each ingestion refresh group in one
//! ingestion run per window as samples arrive, and answers each evaluation
//! with one query run, all on the calling thread.
//!
//! Example: a plan for `sum_over_time(m[5m])` every 10 s built at ingestion
//! time is a chain of 10 s tumbling windows: each ingested chunk of 10 s
//! refreshes the window ending at its end, and the query run at `T` merges
//! the 30 windows ending in `(T - 5 min, T]`.

mod installed_plan;
mod time_series_batches;

use std::path::PathBuf;

use aqpbm_core::harness::engine::{
    ArmSpec, CpuAccounting, EngineDescription, EngineError, EngineUnderTest, InstalledArm,
    WorkloadSpec,
};
use asap_planner::InstallablePlan;

use installed_plan::InstalledPlan;

/// The Planner's executor as an engine under test: installs the plan file
/// named by `arm.engine_options["plan_file"]`.
///
/// Its version is the commit in `ASAP_PLANNER_COMMIT` at build time
/// (`unknown` without it), dirty when `ASAP_PLANNER_DIRTY` is `1`; e.g.
/// `ASAP_PLANNER_COMMIT=$(git rev-parse HEAD) cargo build --release --bin asap_run`.
#[derive(Debug, Clone, Copy, Default)]
pub struct AsapEngine;

impl EngineUnderTest for AsapEngine {
    fn description(&self) -> EngineDescription {
        EngineDescription {
            id: "asap".into(),
            version: option_env!("ASAP_PLANNER_COMMIT")
                .unwrap_or("unknown")
                .into(),
            version_dirty: option_env!("ASAP_PLANNER_DIRTY") == Some("1"),
            build_profile: if cfg!(debug_assertions) {
                "debug"
            } else {
                "release"
            }
            .into(),
        }
    }

    /// Every ingestion and query run runs on the thread that calls
    /// `ingest` or `evaluate` (`futures::executor::block_on`).
    fn cpu_accounting(&self) -> CpuAccounting {
        CpuAccounting::CallingThread
    }

    /// Reads and validates the plan file, matches its query texts to
    /// `workload`'s queries, and compiles it with the executor. A plan the
    /// executor cannot compile is an error starting `not executable:`, so
    /// the cell reads "not run: not executable: ..." while the other lanes
    /// still run that plan.
    fn install(
        &self,
        workload: &WorkloadSpec,
        arm: &ArmSpec,
    ) -> Result<Box<dyn InstalledArm>, EngineError> {
        let plan_file = plan_file_of(arm)?;
        let plan = read_plan_file(&plan_file)?;
        let query_ids = query_ids_of(&plan, workload)
            .map_err(|error| EngineError(format!("plan file {}: {error}", plan_file.display())))?;
        Ok(Box::new(InstalledPlan::install(plan, query_ids, workload)?))
    }
}

/// `arm.engine_options["plan_file"]`.
fn plan_file_of(arm: &ArmSpec) -> Result<PathBuf, EngineError> {
    arm.engine_options
        .get("plan_file")
        .and_then(serde_json::Value::as_str)
        .map(PathBuf::from)
        .ok_or_else(|| {
            EngineError(format!(
                "arm {}: engine options {} name no plan_file",
                arm.name, arm.engine_options
            ))
        })
}

/// The plan file at `path`, read and validated.
fn read_plan_file(path: &std::path::Path) -> Result<InstallablePlan, EngineError> {
    let error = |error: &dyn std::fmt::Display| {
        EngineError(format!("plan file {}: {error}", path.display()))
    };
    let text = std::fs::read_to_string(path).map_err(|e| error(&e))?;
    let plan = InstallablePlan::from_json(&text).map_err(|e| error(&e))?;
    plan.validate().map_err(|e| error(&e))?;
    Ok(plan)
}

/// The id of the query each root of `plan` answers: root `i` answers query
/// text `i`, which must be one of the texts (in any language) of
/// `workload`'s query `i`. Example: a plan for `sum_over_time(m[5m])` and a
/// workload whose query `sum` reads `sum_over_time(m[5m])` give `["sum"]`.
fn query_ids_of(plan: &InstallablePlan, workload: &WorkloadSpec) -> Result<Vec<String>, String> {
    if plan.query_texts.len() != workload.queries.len() {
        return Err(format!(
            "{} query text(s) for workload {}'s {} queries",
            plan.query_texts.len(),
            workload.name,
            workload.queries.len()
        ));
    }
    plan.query_texts
        .iter()
        .zip(&workload.queries)
        .enumerate()
        .map(|(index, (planned, query))| {
            if query.texts.iter().any(|text| &text.text == planned) {
                return Ok(query.id.clone());
            }
            let texts: Vec<&str> = query.texts.iter().map(|text| text.text.as_str()).collect();
            Err(format!(
                "query {index} ({}) is planned as {planned:?}, but the workload's texts are {texts:?}",
                query.id
            ))
        })
        .collect()
}

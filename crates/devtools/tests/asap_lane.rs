//! The in-process ASAP lane of the engine comparison: `AsapEngine` installs a
//! plan file `asap_planner --plan-file` wrote and runs it with the Planner's
//! executor under the harness of sketch-bench (`aqpbm_core::harness`), as one
//! lane of `asap_run bench-external`.
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::process::Command;

use aqpbm_core::harness::arm_run::ArmSchedule;
use aqpbm_core::harness::bench_external::{run_lane, CellOutcome, LaneSetup};
use aqpbm_core::harness::conformance::{self, ConformanceCheck};
use aqpbm_core::harness::data::{workload_chunks, DataRecord, SeriesLabels};
use aqpbm_core::harness::engine::{
    ArmSpec, CpuAccounting, EngineDescription, EngineError, EngineUnderTest, InstalledArm,
    QueryLanguage, QueryText, WorkloadQuery, WorkloadSpec,
};
use aqpbm_core::harness::protocol::{
    LoadProbe, MeasurementClocks, OsCpuClock, Protocol, ProtocolError,
};
use aqpbm_core::harness::records::ArmResult;
use aqpbm_core::harness::rundir::RunDir;
use aqpbm_datagen::{DataDistribution, SeriesLabelSpec, TimeSeriesDescription, UniformParameter};
use asap_devtools::asap_lane::AsapEngine;
use asap_planner::{plan_installable, FrontendInput, PlanningModels, PlanningWorkload, UserInput};

/// A directory under the build's test temp directory, removed when dropped.
struct TempDir(PathBuf);

impl TempDir {
    fn new(test: &str) -> Self {
        let path = PathBuf::from(env!("CARGO_TARGET_TMPDIR"))
            .join(format!("asap-lane-{}-{test}", std::process::id()));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).unwrap();
        TempDir(path)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Every core idle, so a test never waits on the machine's load.
struct IdleProbe;

impl LoadProbe for IdleProbe {
    fn core_busy_fraction(&mut self, _core: usize) -> Result<f64, ProtocolError> {
        Ok(0.0)
    }
    fn loadavg(&mut self) -> f64 {
        0.0
    }
}

/// The planning workload of one exact PromQL `query` evaluated every 10 s at
/// phase 0 over gauge `metric` with `series_count` series sampled every
/// `sample_interval_ms`, arriving as `arrival` (`{"ingesting": ...}` or
/// `"at_rest"`).
fn planning_workload(
    query: &str,
    metric: &str,
    series_count: u64,
    sample_interval_ms: u64,
    arrival: &str,
) -> String {
    format!(
        r#"{{"query_workload": {{"language": "promql", "repeating_queries": [{{
              "query": {query:?},
              "demand": {{"fixed_interval_at": {{"interval": 10000, "evaluation_phase": 0}}}},
              "requirements": {{"accuracy": {{"max_error": 0.0, "failure_probability": 0.0}}}},
              "predictability": {{"predictable": {{"known_at": null}}}}}}]}},
            "data_workload": {{"arrival": {arrival},
              "sources": {{{metric:?}: {{"series_count": {series_count}, "metric_type": "gauge",
                  "sample_interval": {sample_interval_ms}}}}}}}}}"#
    )
}

/// Writes `plan_file` with `asap_planner --plan-file` for `planning`, checked
/// as `executor_check` says; whether the planner wrote it.
fn write_plan_file(planning: &str, plan_file: &Path, executor_check: &str) -> bool {
    let workload_file = plan_file.with_extension("planning.json");
    std::fs::write(&workload_file, planning).unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_asap_planner"))
        .args(["--workload", workload_file.to_str().unwrap()])
        .args(["--run-until", "stage3", "--format", "json"])
        .args(["--executor-check", executor_check])
        .args(["--plan-file", plan_file.to_str().unwrap()])
        .output()
        .unwrap();
    let written = plan_file.exists();
    assert_eq!(
        output.status.success(),
        written,
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    written
}

/// The query of workload `w`: a 5-minute window, so a plan built at
/// ingestion time reads 30 tumbling windows of 10 s.
const SUM_QUERY: &str = "sum_over_time(m[5m])";

/// Workload `name` of the harness: query `sum` = `query` (PromQL) every 10 s
/// over `window_ms`, over `m` with 2 series (label `s`), one sample per
/// second from 0 for 360 s, values uniform in [0, 10), seed 1.
fn harness_workload(name: &str, query: &str, window_ms: u64) -> WorkloadSpec {
    WorkloadSpec {
        name: name.into(),
        queries: vec![WorkloadQuery {
            id: "sum".into(),
            texts: vec![QueryText {
                language: QueryLanguage::PromQl,
                text: query.into(),
            }],
            window_ms,
            evaluation_interval_ms: 10_000,
        }],
        data: vec![DataRecord {
            metric: "m".into(),
            description: TimeSeriesDescription {
                labels: vec![SeriesLabelSpec {
                    name: "s".into(),
                    cardinality: 2,
                    skewness: 0.0,
                }],
                series_num: 2,
                start_ms: 0,
                sample_interval_ms: 1_000,
                duration_ms: 360_000,
                value_distribution: DataDistribution::Uniform(UniformParameter {
                    lower_bound: 0.0,
                    upper_bound: 10.0,
                    seed: 0,
                }),
                counter: false,
                seed: 1,
            },
        }],
    }
}

fn write_json(path: &Path, value: &impl serde::Serialize) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, serde_json::to_string_pretty(value).unwrap()).unwrap();
}

/// A run directory with `workloads` and, per variant (substitutions not
/// allowed), the plan file of each workload in it.
fn run_dir(temp: &TempDir, workloads: &[&WorkloadSpec], plans: &[(&str, &str, PathBuf)]) -> RunDir {
    let run = RunDir::create_at(&temp.0.join("run")).unwrap();
    for workload in workloads {
        write_json(
            &run.path()
                .join("workloads")
                .join(format!("{}.json", workload.name)),
            workload,
        );
    }
    for (variant, workload, plan_file) in plans {
        let dir = run.path().join("plans").join(variant);
        write_json(
            &dir.join("variant.json"),
            &serde_json::json!({"substitutions_allowed": false}),
        );
        std::fs::copy(plan_file, dir.join(format!("{workload}.plan.json"))).unwrap();
    }
    run
}

/// Runs the ASAP lane over `run` with warm-up `warmup` and `runs` measured
/// runs.
fn run_asap_lane(
    run: &RunDir,
    warmup: u32,
    runs: u32,
) -> Vec<aqpbm_core::harness::bench_external::LaneCell> {
    let protocol = Protocol {
        warmup_runs: warmup,
        measured_runs: runs,
        max_core_busy_fraction: 1.0,
    };
    let clocks = MeasurementClocks::with_calling_thread(&OsCpuClock);
    run_lane(
        run,
        &AsapEngine,
        None,
        LaneSetup {
            protocol: &protocol,
            core: allowed_core(),
            probe: &mut IdleProbe,
            clocks: &clocks,
            args: vec!["test".into()],
        },
    )
    .unwrap()
}

/// A core this process may run on: the first of its `Cpus_allowed_list`.
fn allowed_core() -> usize {
    let status = std::fs::read_to_string("/proc/self/status").unwrap();
    let list = status
        .lines()
        .find_map(|line| line.strip_prefix("Cpus_allowed_list:"))
        .unwrap()
        .trim();
    list.split([',', '-']).next().unwrap().parse().unwrap()
}

/// `labels` without `__name__`: a PromQL `*_over_time` drops the metric name.
fn without_name(labels: &SeriesLabels) -> SeriesLabels {
    let mut labels = labels.clone();
    labels.remove("__name__");
    labels
}

/// Per series (without `__name__`), the sum of `workload`'s samples in
/// `(at_ms - window_ms, at_ms]`, computed from the regenerated samples.
fn sums_over_window(
    workload: &WorkloadSpec,
    window_ms: u64,
    at_ms: i64,
) -> BTreeMap<SeriesLabels, f64> {
    let mut sums = BTreeMap::new();
    for chunk in workload_chunks(&workload.data, 10_000).unwrap() {
        for sample in chunk.unwrap().samples {
            if sample.ts_ms > at_ms - window_ms as i64 && sample.ts_ms <= at_ms {
                *sums.entry(without_name(&sample.labels)).or_insert(0.0) += sample.value;
            }
        }
    }
    sums
}

fn arm_result(run: &RunDir, workload: &str, variant: &str) -> ArmResult {
    let path = run
        .path()
        .join("results/asap")
        .join(format!("{workload}__{variant}.json"));
    serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
}

/// The conformance kit's probe arm has no engine options, so this engine
/// hands `AsapEngine` the plan file of each probe workload, by name.
struct WithPlanFiles {
    plan_files: BTreeMap<String, PathBuf>,
}

impl EngineUnderTest for WithPlanFiles {
    fn description(&self) -> EngineDescription {
        AsapEngine.description()
    }
    fn cpu_accounting(&self) -> CpuAccounting {
        AsapEngine.cpu_accounting()
    }
    fn install(
        &self,
        workload: &WorkloadSpec,
        arm: &ArmSpec,
    ) -> Result<Box<dyn InstalledArm>, EngineError> {
        let arm = ArmSpec {
            engine_options: serde_json::json!({"plan_file": self.plan_files[&workload.name]}),
            ..arm.clone()
        };
        AsapEngine.install(workload, &arm)
    }
}

/// The harness's conformance kit passes on the ASAP lane: deterministic
/// answers, left-open windows (`count_over_time(probe[10s])` at 20 s counts
/// only the sample at 20 s), and the executor's work on the calling thread.
#[test]
fn conformance_kit_passes_on_the_asap_engine() {
    let temp = TempDir::new("conformance");
    let runtime = tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap();
    let mut plan_files = BTreeMap::new();
    for workload in conformance::probe_workloads() {
        let query = &workload.queries[0].texts[0];
        assert_eq!(query.language, QueryLanguage::PromQl);
        let planning: PlanningWorkload = serde_json::from_str(&planning_workload(
            &query.text,
            "probe",
            1,
            10_000,
            r#"{"ingesting": {"rows_per_second": 0.1}}"#,
        ))
        .unwrap();
        let (plan, _) = runtime
            .block_on(plan_installable(UserInput::new(
                &planning,
                FrontendInput::Promql,
                PlanningModels::builtin(),
            )))
            .unwrap();
        let path = temp.0.join(format!("{}.plan.json", workload.name));
        std::fs::write(&path, serde_json::to_string(&plan).unwrap()).unwrap();
        plan_files.insert(workload.name.clone(), path);
    }
    let mut engine = WithPlanFiles { plan_files };
    let report = conformance::check(
        &mut engine,
        &MeasurementClocks::with_calling_thread(&OsCpuClock),
    )
    .unwrap_or_else(|failures| panic!("{failures:#?}"));
    for check in [
        ConformanceCheck::DeterministicAnswers,
        ConformanceCheck::LeftOpenWindow,
        ConformanceCheck::WorkOnCallingThread,
    ] {
        assert!(report.passed.contains(&check), "{report:?}");
    }
}

/// `sum_over_time(m[5m])` every 10 s over 2 series, planned twice: with
/// samples arriving (a chain of 10 s tumbling windows built at ingestion
/// time) and at rest (raw samples scanned at query time). The ASAP lane
/// runs both plans (one warm-up and three measured runs, so four installs
/// interleaved) and answers every scheduled evaluation exactly: per series,
/// the sum of its samples in `(T - 5 min, T]`.
#[test]
fn the_asap_lane_runs_a_planned_sum_over_time_and_answers_exactly() {
    let temp = TempDir::new("sum");
    let workload = harness_workload("w", SUM_QUERY, 300_000);
    let ingesting = temp.0.join("ingesting.plan.json");
    let at_rest = temp.0.join("at-rest.plan.json");
    for (plan_file, arrival) in [
        (&ingesting, r#"{"ingesting": {"rows_per_second": 1000.0}}"#),
        (&at_rest, r#""at_rest""#),
    ] {
        let planning = planning_workload(SUM_QUERY, "m", 1_000, 1_000, arrival);
        assert!(write_plan_file(&planning, plan_file, "compile"));
    }
    let run = run_dir(
        &temp,
        &[&workload],
        &[("ingesting", "w", ingesting), ("at-rest", "w", at_rest)],
    );
    let cells = run_asap_lane(&run, 1, 3);
    assert_eq!(cells.len(), 2, "{cells:?}");
    assert!(
        cells.iter().all(|cell| cell.outcome == CellOutcome::Ran),
        "{cells:?}"
    );

    let schedule = ArmSchedule::for_workload(&workload).unwrap();
    for variant in ["ingesting", "at-rest"] {
        let result = arm_result(&run, "w", variant);
        assert!(result.plan.substitutions.is_empty());
        assert!(result.plan.sharing_lost.is_empty());
        assert!(!result.plan.node_kinds.is_empty());
        let times: Vec<i64> = result.answers.iter().map(|(at_ms, _)| *at_ms).collect();
        assert_eq!(times, schedule.evaluate_at_ms, "{variant}");
        for (at_ms, answers) in &result.answers {
            let expected = sums_over_window(&workload, 300_000, *at_ms);
            assert_eq!(expected.len(), 2);
            let answered = &answers.0["sum"];
            assert_eq!(
                answered.keys().collect::<BTreeSet<_>>(),
                expected.keys().collect::<BTreeSet<_>>(),
                "{variant} at {at_ms}"
            );
            for (series, value) in &expected {
                let got = answered[series];
                assert!(
                    (got - value).abs() <= 1e-9 * value.abs(),
                    "{variant} at {at_ms} {series:?}: {got} vs {value}"
                );
            }
        }
        // The plan's shape, a precondition: if the planner changes it, pick
        // workloads that keep one ingestion-time and one query-time plan.
        match variant {
            "ingesting" => {
                assert!(!result.plan.stored_outputs.is_empty(), "{:?}", result.plan);
                assert!(result.footprint.view_bytes > 0, "{:?}", result.footprint);
            }
            _ => {
                assert!(result.plan.stored_outputs.is_empty(), "{:?}", result.plan);
                assert!(
                    result.footprint.raw_retained_bytes > 0,
                    "{:?}",
                    result.footprint
                );
            }
        }
    }
}

/// `absent_over_time(m[10s])`: Stages 1-3 plan it, but the Planner's
/// executor cannot compile it. The plan file written for a runtime other
/// than the executor (`--executor-check skip`) is a "not executable" cell
/// on the ASAP lane, so the other lanes still run that plan.
#[test]
fn a_plan_the_executor_cannot_compile_is_a_not_executable_cell() {
    const QUERY: &str = "absent_over_time(m[10s])";
    let temp = TempDir::new("absent");
    let planning = planning_workload(QUERY, "m", 2, 1_000, r#""at_rest""#);
    let plan_file = temp.0.join("absent.plan.json");
    // Precondition: an executor-only gap. Once the executor runs
    // `absent_over_time`, pick another query it plans but cannot compile.
    assert!(!write_plan_file(&planning, &plan_file, "compile"));
    assert!(write_plan_file(&planning, &plan_file, "skip"));
    let workload = harness_workload("w", QUERY, 10_000);
    let run = run_dir(&temp, &[&workload], &[("exact", "w", plan_file)]);
    let cells = run_asap_lane(&run, 0, 1);
    let [cell] = cells.as_slice() else {
        panic!("{cells:?}")
    };
    match &cell.outcome {
        CellOutcome::NotRun { error } => {
            assert!(error.starts_with("not executable: "), "{error}")
        }
        other => panic!("{other:?}"),
    }
}

/// `install`'s refusals: no plan file in the engine options, a missing file,
/// a file that is not a plan, and a plan for other queries (query `i` is
/// answered by the plan's root `i`, matched by text in any of the query's
/// languages).
#[test]
fn a_plan_for_other_queries_is_refused_naming_the_query() {
    let temp = TempDir::new("refusals");
    let planning = planning_workload(SUM_QUERY, "m", 2, 1_000, r#""at_rest""#);
    let plan_file = temp.0.join("sum.plan.json");
    assert!(write_plan_file(&planning, &plan_file, "compile"));
    let arm = |options: serde_json::Value| ArmSpec {
        name: "exact".into(),
        raw_data_retained: true,
        engine_options: options,
    };
    let with_plan = arm(serde_json::json!({ "plan_file": plan_file }));
    let install_error =
        |workload: &WorkloadSpec, arm: &ArmSpec| match AsapEngine.install(workload, arm) {
            Ok(_) => panic!("installed"),
            Err(EngineError(message)) => message,
        };
    let workload = harness_workload("w", SUM_QUERY, 300_000);

    let error = install_error(&workload, &arm(serde_json::Value::Null));
    assert!(error.contains("plan_file"), "{error}");
    let missing = temp.0.join("missing.plan.json");
    let error = install_error(&workload, &arm(serde_json::json!({ "plan_file": missing })));
    assert!(error.contains("missing.plan.json"), "{error}");
    let not_a_plan = temp.0.join("not-a-plan.json");
    std::fs::write(&not_a_plan, "{}").unwrap();
    let error = install_error(
        &workload,
        &arm(serde_json::json!({ "plan_file": not_a_plan })),
    );
    assert!(error.contains("not-a-plan.json"), "{error}");

    // Another query text: the error names the query and both texts.
    let other = harness_workload("w", "count_over_time(m[5m])", 300_000);
    let error = install_error(&other, &with_plan);
    for part in ["query 0", "sum", SUM_QUERY, "count_over_time(m[5m])"] {
        assert!(error.contains(part), "{part}: {error}");
    }
    // Two queries against a plan with one root.
    let mut two = workload.clone();
    two.queries.push(WorkloadQuery {
        id: "second".into(),
        ..two.queries[0].clone()
    });
    let error = install_error(&two, &with_plan);
    assert!(
        error.contains("1 query text") && error.contains("2 queries"),
        "{error}"
    );

    // The plan's text as the query's second text (after a SQL text) matches.
    let mut sql_first = workload.clone();
    sql_first.queries[0].texts.insert(
        0,
        QueryText {
            language: QueryLanguage::Sql,
            text: "SELECT s, sum(value) FROM m GROUP BY s".into(),
        },
    );
    assert!(AsapEngine.install(&sql_first, &with_plan).is_ok());
}

/// `asap_run bench-external --lane asap` runs the lane over a run directory
/// and writes each cell; an unknown lane is a usage error naming the known
/// one.
#[test]
fn asap_run_runs_the_asap_lane_over_a_run_dir() {
    let temp = TempDir::new("asap-run");
    let workload = harness_workload("w", SUM_QUERY, 300_000);
    let plan_file = temp.0.join("sum.plan.json");
    let planning = planning_workload(SUM_QUERY, "m", 2, 1_000, r#""at_rest""#);
    assert!(write_plan_file(&planning, &plan_file, "compile"));
    let run = run_dir(&temp, &[&workload], &[("exact", "w", plan_file)]);
    let asap_run = |lane: &str| {
        Command::new(env!("CARGO_BIN_EXE_asap_run"))
            .args(["bench-external", "--lane", lane])
            .args(["--run-dir", run.path().to_str().unwrap()])
            .args(["--core", &allowed_core().to_string()])
            .args(["--warmup", "0", "--runs", "1", "--max-core-busy", "1.0"])
            .output()
            .unwrap()
    };
    let output = asap_run("asap");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let cell: aqpbm_core::harness::bench_external::LaneCell = serde_json::from_str(
        &std::fs::read_to_string(run.path().join("results/asap/w__exact.cell.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(cell.outcome, CellOutcome::Ran);

    let output = asap_run("fake");
    assert_eq!(output.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&output.stderr).contains("asap"));
}

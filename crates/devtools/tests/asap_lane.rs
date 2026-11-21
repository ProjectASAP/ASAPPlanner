//! The in-process ASAP lane of the engine comparison: `AsapEngine` installs a
//! plan file `asap_planner --plan-file` wrote and runs it with the Planner's
//! executor under the harness of sketch-bench (`aqpbm_core::harness`), as one
//! lane of `asap_run bench-external`.
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::process::Command;

use aqpbm_core::harness::arm_run::ArmSchedule;
use aqpbm_core::harness::bench_external::{run_lane, CellOutcome, LaneSetup};
use aqpbm_core::harness::data::{workload_chunks, DataRecord, SeriesLabels};
use aqpbm_core::harness::engine::{
    ArmSpec, EngineError, EngineUnderTest, QueryLanguage, QueryText, WorkloadQuery, WorkloadSpec,
};
use aqpbm_core::harness::protocol::{
    LoadProbe, MeasurementClocks, OsCpuClock, Protocol, ProtocolError,
};
use aqpbm_core::harness::records::ArmResult;
use aqpbm_core::harness::rundir::RunDir;
use aqpbm_datagen::{DataDistribution, SeriesLabelSpec, TimeSeriesDescription, UniformParameter};
use asap_devtools::asap_lane::AsapEngine;

#[path = "asap_lane_support.rs"]
mod support;
use support::{planning_workload, TempDir};

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

/// Writes `plan_file` with `asap_planner --plan-file` for `planning`, which
/// must select a plan; returns the planner's standard error, which notes
/// when the Planner's executor cannot run the plan.
fn write_plan_file(planning: &str, plan_file: &Path) -> String {
    let workload_file = plan_file.with_extension("planning.json");
    std::fs::write(&workload_file, planning).unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_asap_planner"))
        .args(["--workload", workload_file.to_str().unwrap()])
        .args(["--run-until", "stage3", "--format", "json"])
        .args(["--plan-file", plan_file.to_str().unwrap()])
        .output()
        .unwrap();
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(output.status.success(), "{stderr}");
    assert!(plan_file.exists(), "{stderr}");
    stderr
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
        write_plan_file(&planning, plan_file);
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

/// Writes the plan file of `queries` (PromQL, each every 10 s at phase 0,
/// exact) over counter `c`, planned for 1,000 series sampled every second
/// arriving at 1,000 rows/s with no raw data kept, so every query reads
/// ingestion-time chains.
fn write_counter_plan_file(queries: &[&str], plan_file: &Path) {
    let entries: Vec<_> = queries
        .iter()
        .map(|query| {
            serde_json::json!({
                "query": query,
                "demand": {"fixed_interval_at": {"interval": 10000, "evaluation_phase": 0}},
                "requirements": {"accuracy": {"max_error": 0.0, "failure_probability": 0.0}},
                "predictability": {"predictable": {"known_at": null}},
            })
        })
        .collect();
    let planning = serde_json::json!({
        "query_workload": {"language": "promql", "repeating_queries": entries},
        "data_workload": {
            "arrival": {"ingesting": {"rows_per_second": 1000.0}},
            "sources": {"c": {"series_count": 1000, "metric_type": "counter",
                "column_signs": {"value": "non_negative"}, "sample_interval": 1000}},
        },
    });
    let workload_file = plan_file.with_extension("planning.json");
    let capabilities_file = plan_file.with_extension("capabilities.json");
    std::fs::write(&workload_file, planning.to_string()).unwrap();
    std::fs::write(&capabilities_file, r#"{"raw_data_retained": false}"#).unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_asap_planner"))
        .args(["--workload", workload_file.to_str().unwrap()])
        .args([
            "--deployment-capabilities",
            capabilities_file.to_str().unwrap(),
        ])
        .args(["--run-until", "stage3", "--format", "json"])
        .args(["--plan-file", plan_file.to_str().unwrap()])
        .output()
        .unwrap();
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(output.status.success(), "{stderr}");
}

/// The summary types of the tumbling-window chains in `plan_file`.
fn chain_summary_types(plan_file: &Path) -> Vec<serde_json::Value> {
    let plan: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(plan_file).unwrap()).unwrap();
    fn walk(value: &serde_json::Value, found: &mut Vec<serde_json::Value>) {
        match value {
            serde_json::Value::Object(map) => {
                if let Some(chain) = map.get("SummaryTumblingWindowChain") {
                    found.push(chain["summary_type"].clone());
                }
                map.values().for_each(|value| walk(value, found));
            }
            serde_json::Value::Array(values) => values.iter().for_each(|value| walk(value, found)),
            _ => {}
        }
    }
    let mut found = Vec::new();
    walk(&plan, &mut found);
    found
}

/// Workload `name` of the harness: `queries` (id, PromQL) every 10 s over
/// 5 minutes, over counter `c` with 2 series (label `s`), one sample per
/// second from 0 for 360 s, increasing by uniform steps in [0, 10), seed 1.
fn counter_workload(name: &str, queries: &[(&str, &str)]) -> WorkloadSpec {
    let mut workload = harness_workload(name, queries[0].1, 300_000);
    workload.queries = queries
        .iter()
        .map(|(id, text)| WorkloadQuery {
            id: (*id).into(),
            texts: vec![QueryText {
                language: QueryLanguage::PromQl,
                text: (*text).into(),
            }],
            ..workload.queries[0].clone()
        })
        .collect();
    workload.data[0].metric = "c".into();
    workload.data[0].description.counter = true;
    workload
}

/// Each evaluation time's answers: per query id, per series, its value.
type AnswersOverTime = Vec<(i64, BTreeMap<String, BTreeMap<SeriesLabels, f64>>)>;

/// Installs `plan_file` for `workload`, ingests its samples 10 s at a time
/// and evaluates it at each scheduled time once its samples are in: the
/// answers, and the most bytes the stored views held after a chunk.
fn run_installed(workload: &WorkloadSpec, plan_file: &Path) -> (AnswersOverTime, u64) {
    let arm = ArmSpec {
        name: "counter".into(),
        raw_data_retained: false,
        engine_options: serde_json::json!({ "plan_file": plan_file }),
    };
    let mut installed = AsapEngine.install(workload, &arm).unwrap();
    let schedule = ArmSchedule::for_workload(workload).unwrap();
    let mut times = schedule.evaluate_at_ms.iter().copied().peekable();
    let (mut answers, mut view_bytes) = (Vec::new(), 0);
    for chunk in workload_chunks(&workload.data, 10_000).unwrap() {
        let chunk = chunk.unwrap();
        installed.ingest(&chunk).unwrap();
        view_bytes = view_bytes.max(installed.footprint().unwrap().view_bytes);
        while let Some(at_ms) = times.next_if(|&at_ms| at_ms <= chunk.end_ms) {
            answers.push((at_ms, installed.evaluate(at_ms).unwrap().0 .0));
        }
    }
    assert!(times.next().is_none(), "evaluations after the data");
    (answers, view_bytes)
}

/// `answers` as JSON, one entry per time, query and series, each value as
/// the hex of its bits so a comparison is bit for bit.
fn answers_json(answers: &AnswersOverTime) -> serde_json::Value {
    let mut entries = Vec::new();
    for (at_ms, per_query) in answers {
        for (query, per_series) in per_query {
            for (series, value) in per_series {
                entries.push(serde_json::json!({
                    "at_ms": at_ms,
                    "query": query,
                    "series": series,
                    "bits": format!("{:#018x}", value.to_bits()),
                }));
            }
        }
    }
    serde_json::Value::Array(entries)
}

/// The answers of `rate(c[5m])` and `increase(c[5m])` planned together
/// before they shared a state (a `Rate` chain and an `Increase` chain),
/// from `run_installed` over `counter_workload("both", ...)`.
const COUNTER_ANSWERS_BEFORE_SHARING: &str = "asap_lane_fixtures/rate_and_increase_two_chains.json";

/// `rate(c[5m])` and `increase(c[5m])` planned together read one chain of
/// counter states, which the executor finalizes as `rate` and as
/// `increase`. Their answers are bit-identical to those of the two chains
/// the same workload planned before (`COUNTER_ANSWERS_BEFORE_SHARING`) and
/// to each query's planned alone (one chain each), and the shared
/// chain's stored views take at most half the bytes of the two.
#[test]
fn rate_and_increase_of_one_counter_share_one_chain_and_answer_as_apart() {
    const RATE: &str = "rate(c[5m])";
    const INCREASE: &str = "increase(c[5m])";
    let temp = TempDir::new("counter");
    let counter = serde_json::json!({"ExactAggregate": ["Counter", "Counter"]});
    let together = temp.0.join("together.plan.json");
    write_counter_plan_file(&[RATE, INCREASE], &together);
    assert_eq!(chain_summary_types(&together), vec![counter.clone()]);
    let (answers, together_bytes) = run_installed(
        &counter_workload("both", &[("rate", RATE), ("increase", INCREASE)]),
        &together,
    );
    // Bit for bit the answers of the same workload planned before the
    // shared state, as two chains.
    let before: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(
            Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("tests")
                .join(COUNTER_ANSWERS_BEFORE_SHARING),
        )
        .unwrap(),
    )
    .unwrap();
    assert_eq!(answers_json(&answers), before);

    let mut apart_bytes = 0;
    for (id, query) in [("rate", RATE), ("increase", INCREASE)] {
        let alone = temp.0.join(format!("{id}.plan.json"));
        write_counter_plan_file(&[query], &alone);
        assert_eq!(chain_summary_types(&alone), vec![counter.clone()]);
        let (alone_answers, bytes) = run_installed(&counter_workload(id, &[(id, query)]), &alone);
        apart_bytes += bytes;
        assert_eq!(answers.len(), alone_answers.len());
        for ((at_ms, together), (alone_at_ms, alone)) in answers.iter().zip(&alone_answers) {
            assert_eq!(at_ms, alone_at_ms);
            let (together, alone) = (&together[id], &alone[id]);
            assert_eq!(together.len(), 2, "{id} at {at_ms}: {together:?}");
            assert_eq!(
                together.keys().collect::<Vec<_>>(),
                alone.keys().collect::<Vec<_>>()
            );
            for (series, value) in together {
                assert_eq!(
                    value.to_bits(),
                    alone[series].to_bits(),
                    "{id} at {at_ms} {series:?}: {value} vs {}",
                    alone[series]
                );
            }
        }
    }
    assert!(
        together_bytes > 0 && 2 * together_bytes <= apart_bytes + apart_bytes / 50,
        "{together_bytes} together, {apart_bytes} apart"
    );
}

/// `absent_over_time(m[10s])`: Stages 1-3 plan it, but the Planner's
/// executor cannot compile it. `asap_planner` still writes the plan file
/// (with a note), and on the ASAP lane it is a "not executable" cell, so
/// the other lanes still run that plan.
#[test]
fn a_plan_the_executor_cannot_compile_is_a_not_executable_cell() {
    const QUERY: &str = "absent_over_time(m[10s])";
    let temp = TempDir::new("absent");
    let planning = planning_workload(QUERY, "m", 2, 1_000, r#""at_rest""#);
    let plan_file = temp.0.join("absent.plan.json");
    // Precondition: an executor-only gap. Once the executor runs
    // `absent_over_time`, pick another query it plans but cannot compile.
    let stderr = write_plan_file(&planning, &plan_file);
    assert!(
        stderr.contains("note: the Planner's executor cannot run this plan: "),
        "{stderr}"
    );
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
    write_plan_file(&planning, &plan_file);
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
    write_plan_file(&planning, &plan_file);
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

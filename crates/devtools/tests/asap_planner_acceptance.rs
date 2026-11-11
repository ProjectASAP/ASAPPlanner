//! #1161's acceptance tests for `asap_planner`: `--run-until` stops after
//! the stage it names, Pass 2 holds every legal tumbling-window width, Stage 3 selects
//! among the candidates Stage 2 lists, and the CLI refuses what it no
//! longer takes.

use std::collections::BTreeSet;
use std::process::{Command, Output};

use serde_json::{json, Value};

/// One repeating PromQL `query` every `interval_ms`, within `max_error`
/// (failure probability 0.001 when approximate), over `metric`, a counter
/// with 1,000 series sampled every 15 s.
fn workload(query: &str, interval_ms: u64, max_error: f64, metric: &str) -> Value {
    let failure_probability = if max_error == 0.0 { 0.0 } else { 0.001 };
    json!({
        "query_workload": {"language": "promql", "query_batch": null, "repeating_queries": [{
            "query": query,
            "demand": {"fixed_interval": interval_ms},
            "requirements": {"accuracy": {"max_error": max_error, "failure_probability": failure_probability}},
            "predictability": {"predictable": {"known_at": null}},
        }]},
        "data_workload": {
            "arrival": {"ingesting": {"rows_per_second": 1000.0 / 15.0}},
            "sources": {metric: {"series_count": 1000, "sample_interval": 15000, "metric_type": "counter"}},
        },
    })
}

/// A rate every 10 s: Stage 3 solves it in well under a second.
fn rate_workload() -> Value {
    workload(
        "sum by (job) (rate(http_requests_total[1m]))",
        10_000,
        0.0,
        "http_requests_total",
    )
}

/// `asap_planner` on `workload` with `args`.
fn planner(workload: &Value, args: &[&str]) -> Output {
    static CALLS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    let call = CALLS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let path = std::env::temp_dir().join(format!(
        "asap_planner_acceptance_{}_{call}.json",
        std::process::id()
    ));
    std::fs::write(&path, workload.to_string()).unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_asap_planner"))
        .arg("--workload")
        .arg(&path)
        .args(args)
        .output()
        .unwrap();
    std::fs::remove_file(path).unwrap();
    output
}

/// `asap_planner`'s standard output on success.
fn stdout(output: Output) -> String {
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap()
}

/// The text sections, in order, that each `--run-until` prints after the
/// ones before it.
const SECTIONS: [(&str, &[&str]); 5] = [
    (
        "stage0",
        &[
            "Stage 0 output: the frontends' roots",
            "Stage 0 → Stage 1: Memo (the roots hash-consed, without candidates)",
        ],
    ),
    ("stage1_pass1", &["Stage 1 Pass 1 output: Memo"]),
    (
        "stage1_pass2",
        &["Stage 1 Pass 2 output = Stage 2 input: LogicalASAPMemoCandidates"],
    ),
    (
        "stage2",
        &["Stage 2 output = Stage 3 input: PhysicalASAPMemoCandidates"],
    ),
    ("stage3", &["Stage 3 output: Selection"]),
];

/// `--run-until <stage>` prints exactly the sections of every stage up to
/// and including that one, in pipeline order.
#[test]
fn run_until_prints_exactly_the_stages_up_to_it() {
    for (last, (stage, _)) in SECTIONS.iter().enumerate() {
        let text = stdout(planner(&rate_workload(), &["--run-until", stage]));
        let printed: Vec<&str> = text
            .lines()
            .filter_map(|line| line.strip_prefix("==================== "))
            .collect();
        let expected: Vec<&str> = SECTIONS[..=last]
            .iter()
            .flat_map(|(_, headings)| headings.iter().copied())
            .collect();
        assert_eq!(
            printed.len(),
            expected.len(),
            "--run-until {stage}:\n{text}"
        );
        for (line, heading) in printed.iter().zip(&expected) {
            assert!(line.starts_with(heading), "--run-until {stage}: {line}");
        }
    }
}

/// Pass 2 composes tumbling-window candidates over every legal width, not only
/// the seed grid: a 5 m quantile within 1% every 60 s gets a mergeable
/// summary's tumbling-window chain for every width that divides 60 s (in ms), from
/// 1 ms to 60 s.
#[test]
fn pass2_holds_a_tumbling_window_candidate_for_every_width_dividing_the_grid() {
    let quantile = workload("quantile_over_time(0.5, data[5m])", 60_000, 0.01, "data");
    let text = stdout(planner(&quantile, &["--run-until", "stage1_pass2"]));
    let widths: BTreeSet<u64> = text
        .split("tumbling_window_width_ms=")
        .skip(1)
        .map(|rest| {
            let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
            digits.parse().unwrap()
        })
        .collect();
    let divisors: BTreeSet<u64> = (1..=60_000).filter(|w| 60_000 % w == 0).collect();
    assert_eq!(widths, divisors);
}

/// Stage 3 selects among the candidates Stage 2 lists: every candidate a
/// selected node runs (`nodeN:  C<k>`) is a line of Stage 2's output.
#[test]
fn stage3_selects_candidates_stage2_lists() {
    let text = stdout(planner(&rate_workload(), &["--run-until", "stage3"]));
    let (stage2, stage3) = text.split_once("Stage 3 output: Selection").unwrap();
    let selected: Vec<&str> = stage3
        .lines()
        .filter(|line| line.starts_with("node") && line.contains(":  C"))
        .filter_map(|line| line.split_whitespace().nth(1))
        .collect();
    assert!(!selected.is_empty(), "{stage3}");
    for candidate in selected {
        assert!(
            stage2.contains(&format!("\n    {candidate}:")),
            "{candidate} is not in Stage 2's output"
        );
    }
}

/// `--format json` leaves the sections of stages it did not run null: after
/// `stage1_pass2`, Stage 1 and the memo up to Pass 2 are there and Stages 2 and 3
/// are null; after `stage3`, every section is there.
#[test]
fn json_sections_of_stages_not_run_are_null() {
    let document = |stage: &str| -> Value {
        serde_json::from_str(&stdout(planner(
            &rate_workload(),
            &["--run-until", stage, "--format", "json"],
        )))
        .unwrap()
    };
    let pass2 = document("stage1_pass2");
    assert_eq!(pass2["run_until"], "stage1_pass2");
    assert!(pass2["stage0_query_language_frontends"].is_object());
    assert!(pass2["stage1_logical_asap_optimization"].is_object());
    assert_eq!(pass2["stage2_physical_asap_optimization"], Value::Null);
    assert_eq!(pass2["stage3_plan_selection"], Value::Null);
    let steps: Vec<&str> = pass2["memo"]["steps"]
        .as_array()
        .unwrap()
        .iter()
        .map(|step| step["step"].as_str().unwrap())
        .collect();
    assert_eq!(steps, ["stage0", "pass1", "pass2"]);

    let stage3 = document("stage3");
    assert!(stage3["stage2_physical_asap_optimization"].is_object());
    assert!(stage3["stage3_plan_selection"]["selected_dag"].is_object());
    assert_eq!(
        stage3["stage1_logical_asap_optimization"]["candidates"][0]["id"],
        "L1"
    );
    assert_eq!(
        stage3["stage2_physical_asap_optimization"]["candidates"][0]["id"],
        "P1"
    );
}

/// A missing `--run-until`, the old `pass1`/`pass2` stage names, and each
/// flag `stage_pipeline` took that `asap_planner` does not, is a usage error
/// (status 2, with the usage).
#[test]
fn missing_run_until_and_removed_flags_are_usage_errors() {
    let refused = |args: &[&str], message: &str| {
        let output = planner(&rate_workload(), args);
        assert_eq!(output.status.code(), Some(2), "{args:?}");
        let stderr = String::from_utf8(output.stderr).unwrap();
        assert!(stderr.contains(message), "{args:?}: {stderr}");
        assert!(stderr.contains("usage: asap_planner"), "{args:?}: {stderr}");
    };
    refused(&[], "--run-until is required");
    for stage in ["pass1", "pass2"] {
        refused(&["--run-until", stage], &format!("unknown stage {stage}"));
    }
    for flag in [
        "--example",
        "--promql",
        "--sql",
        "--table",
        "--exhaustive",
        "--milp-time-limit-s",
        "--max-displayed",
        "--no-memo",
        "--max-enumerated",
        "--enforce-accuracy-guarantees",
        "--cost-model",
        "--text",
    ] {
        refused(
            &["--run-until", "stage0", flag],
            &format!("unknown argument {flag}"),
        );
    }
}

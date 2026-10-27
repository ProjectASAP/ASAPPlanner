//! Planner snapshots over real-world query corpora (#941).
//!
//! The planner has no reference answer, so these tests pin its output
//! instead: each corpus query's `stage_pipeline` document (default search)
//! is compared with the committed one, and any change fails until the
//! snapshot is regenerated and the change explained in the PR.
//!
//! Each `tests/corpus_snapshots/<corpus>/corpus.json` declares the shared
//! planning inputs and the queries by stable id; `<id>.json` beside it is
//! that query's snapshot. Regenerate with
//! `UPDATE_SNAPSHOTS=1 cargo test --release -p asap-devtools --test corpus_snapshots -- --ignored`.
//!
//! A snapshot must not depend on the machine: the search report's timings are
//! dropped, and a run the wall-clock budget stopped fails instead of being
//! recorded, so only the evaluation budget can end a search.

use std::path::{Path, PathBuf};
use std::process::Command;

use serde_json::{json, Value};

/// Far above any corpus query's planning time, so it never binds.
const TIME_LIMIT_S: &str = "600";

fn corpora_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/corpus_snapshots")
}

/// `query`'s one-query workload under `corpus`'s shared inputs.
fn workload(corpus: &Value, query: &str) -> Value {
    let mut entry = corpus["query_workload"]["repeating_query"].clone();
    entry["query"] = json!(query);
    json!({
        "query_workload": {
            "language": corpus["query_workload"]["language"],
            "repeating_queries": [entry],
        },
        "data_workload": corpus["data_workload"],
    })
}

/// Runs `stage_pipeline` on `workload`; panics (a planner crash) on failure.
fn plan(workload: &Value, args: &[String], scratch: &Path) -> Value {
    let input = scratch.join("workload.json");
    let output = scratch.join("document.json");
    std::fs::write(&input, workload.to_string()).unwrap();
    let mut command = Command::new(env!("CARGO_BIN_EXE_stage_pipeline"));
    command
        .arg("--workload")
        .arg(&input)
        .args(["--milp-time-limit-s", TIME_LIMIT_S])
        .args(args)
        .arg("--out")
        .arg(&output);
    let result = command.output().unwrap();
    assert!(
        result.status.success(),
        "{command:?} failed: {}",
        String::from_utf8_lossy(&result.stderr)
    );
    serde_json::from_str(&std::fs::read_to_string(output).unwrap()).unwrap()
}

/// Whether the wall-clock budget, not the evaluation budget, ended the search.
fn hit_time_limit(document: &Value) -> bool {
    let report = &document["stage3_plan_selection"]["search"]["candidate_connections"];
    match (
        report["full_planning_ms"].as_f64(),
        report["time_limit_ms"].as_f64(),
    ) {
        (Some(elapsed), Some(limit)) => elapsed >= limit,
        _ => false,
    }
}

/// Drops the search report's timings (`*_ms`), the only fields that differ
/// between runs of the same workload. A failed selection's error also embeds
/// the report, timings included; it is cut off there, since `search` has it.
fn strip_timings(document: &mut Value) {
    if let Some(Value::String(error)) = document.pointer_mut("/stage3_plan_selection/error") {
        if let Some(at) = error.find(" Search report: ") {
            error.truncate(at);
        }
    }
    fn strip(value: &mut Value) {
        match value {
            Value::Object(map) => {
                map.retain(|key, _| !key.ends_with("_ms"));
                map.values_mut().for_each(strip);
            }
            Value::Array(items) => items.iter_mut().for_each(strip),
            _ => {}
        }
    }
    if let Some(search) = document.pointer_mut("/stage3_plan_selection/search") {
        strip(search);
    }
}

/// The first line where `expected` and `actual` differ, for the failure message.
fn first_difference(expected: &str, actual: &str) -> String {
    let mut expected_lines = expected.lines();
    let mut actual_lines = actual.lines();
    for line in 1.. {
        match (expected_lines.next(), actual_lines.next()) {
            (None, None) => break,
            (e, a) if e != a => {
                return format!(
                    "line {line}:\n  - {}\n  + {}",
                    e.unwrap_or("<end>"),
                    a.unwrap_or("<end>")
                )
            }
            _ => {}
        }
    }
    String::new()
}

/// Every corpus query's document matches its committed snapshot.
#[test]
#[ignore = "runs the release planner over the corpora: see the module docs"]
fn corpus_plans_match_snapshots() {
    let update = std::env::var_os("UPDATE_SNAPSHOTS").is_some();
    // Regenerated snapshots of failing queries, for CI to upload.
    let regenerated = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("corpus_snapshots");
    let scratch = std::env::temp_dir().join(format!("corpus_snapshots_{}", std::process::id()));
    std::fs::create_dir_all(&scratch).unwrap();
    let mut failures = Vec::new();

    let mut corpora: Vec<_> = std::fs::read_dir(corpora_dir())
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .collect();
    corpora.sort();
    for dir in corpora {
        let name = dir.file_name().unwrap().to_string_lossy().into_owned();
        let corpus: Value =
            serde_json::from_str(&std::fs::read_to_string(dir.join("corpus.json")).unwrap())
                .unwrap();
        let args: Vec<String> =
            serde_json::from_value(corpus["stage_pipeline_args"].clone()).unwrap();
        let queries = corpus["queries"].as_array().unwrap();
        let ids: Vec<&str> = queries.iter().map(|q| q["id"].as_str().unwrap()).collect();

        // A snapshot whose query left the manifest would otherwise go stale unseen.
        for entry in std::fs::read_dir(&dir).unwrap() {
            let file = entry.unwrap().file_name().to_string_lossy().into_owned();
            if let Some(id) = file.strip_suffix(".json") {
                if id != "corpus" && !ids.contains(&id) {
                    failures.push(format!("{name}/{file}: no query with id {id}"));
                }
            }
        }

        for query in queries {
            let id = query["id"].as_str().unwrap();
            let mut document = plan(
                &workload(&corpus, query["query"].as_str().unwrap()),
                &args,
                &scratch,
            );
            if hit_time_limit(&document) {
                failures.push(format!(
                    "{name}/{id}: the {TIME_LIMIT_S} s time limit ended the search, \
                     so its plan depends on machine speed"
                ));
                continue;
            }
            strip_timings(&mut document);
            let actual = serde_json::to_string_pretty(&document).unwrap() + "\n";
            let path = dir.join(format!("{id}.json"));
            if update {
                std::fs::write(&path, &actual).unwrap();
                continue;
            }
            let Ok(expected) = std::fs::read_to_string(&path) else {
                failures.push(format!("{name}/{id}: no snapshot {}", path.display()));
                continue;
            };
            if expected != actual {
                let out = regenerated.join(&name);
                std::fs::create_dir_all(&out).unwrap();
                std::fs::write(out.join(format!("{id}.json")), &actual).unwrap();
                failures.push(format!(
                    "{name}/{id}: plan changed at {}",
                    first_difference(&expected, &actual)
                ));
            }
        }
    }
    std::fs::remove_dir_all(&scratch).unwrap();
    assert!(
        failures.is_empty(),
        "{} snapshot(s) differ; regenerated ones are in {}.\n\
         If the change is intended, run with UPDATE_SNAPSHOTS=1 and explain it \
         under \"Snapshot changes\" in the PR.\n\n{}",
        failures.len(),
        regenerated.display(),
        failures.join("\n\n")
    );
}

/// Only the search report's timings, and the report embedded in a selection
/// error, are dropped; `_ms` inputs elsewhere stay.
#[test]
fn strip_timings_keeps_inputs() {
    let mut document = json!({
        "workload": {"queries": [{"range_evaluation": {"step_ms": 60000}}]},
        "stage3_plan_selection": {
            "pane_retention_ms": {"3": 60000},
            "error": "infeasible. Search report: {\"solver_ms\":1.5}",
            "search": {"candidate_connections": {"solver_ms": 1.5, "refinements": 128}},
        },
    });
    strip_timings(&mut document);
    assert_eq!(
        document,
        json!({
            "workload": {"queries": [{"range_evaluation": {"step_ms": 60000}}]},
            "stage3_plan_selection": {
                "pane_retention_ms": {"3": 60000},
                "error": "infeasible.",
                "search": {"candidate_connections": {"refinements": 128}},
            },
        })
    );
}

/// A search that used its whole wall-clock budget is detected; one stopped by
/// the evaluation budget, or with no search report, is not.
#[test]
fn time_limit_detection() {
    let report = |elapsed: f64| {
        json!({"stage3_plan_selection": {"search": {"candidate_connections": {
            "full_planning_ms": elapsed, "time_limit_ms": 1000.0,
        }}}})
    };
    assert!(hit_time_limit(&report(1000.2)));
    assert!(!hit_time_limit(&report(12.0)));
    assert!(!hit_time_limit(
        &json!({"stage3_plan_selection": {"error": "x"}})
    ));
}

/// The failure message names the first differing line.
#[test]
fn first_difference_names_the_line() {
    assert_eq!(
        first_difference("a\nb\n", "a\nc\n"),
        "line 2:\n  - b\n  + c"
    );
    assert_eq!(
        first_difference("a\n", "a\nb\n"),
        "line 2:\n  - <end>\n  + b"
    );
}

//! Planner snapshots over real-world query corpora (#941).
//!
//! The planner has no reference answer, so these tests pin its output
//! instead: each corpus query's selected plan (default search) is compared
//! with the committed one, and any change fails until the snapshot is
//! regenerated and the change explained in the PR.
//!
//! Each `tests/corpus_snapshots/<corpus>/corpus.json` declares the shared
//! planning inputs and the queries by stable id; `<id>.json` beside it is
//! that query's snapshot. Regenerate with
//! `UPDATE_SNAPSHOTS=1 cargo test --release -p asap-devtools --test corpus_snapshots -- --ignored`.
//!
//! A snapshot holds only the plan, never the search that found it: the
//! selected plan's DAG (Stage 2 candidate `P1`) and display name, its cost
//! and accepted accuracy per query, the materialized views, pane retention,
//! and whether the search proved the plan optimal; for a query that does not
//! plan, the error. Candidate ids, round and evaluation counters, timings, the
//! Stage 1 and Stage 2 candidate lists and the memo vary from run to run
//! (round budgets are wall-clock), so they are left out. Floats are rounded
//! to 12 significant digits. A run that the time limit stopped fails instead
//! of being recorded, unless the query has a `time_limit_reason` in the
//! manifest (its plan is then recorded and compared as usual); a query with a
//! `skip_reason` is skipped.
//!
//! The same binary holds the sketch plan snapshots (`sketch_plan_snapshots.rs`),
//! which reuse these helpers on multi-query workloads with accuracy targets.

use std::path::{Path, PathBuf};
use std::process::Command;

use serde_json::{json, Map, Value};

#[path = "sketch_plan_snapshots.rs"]
mod sketch_plan_snapshots;

/// Per query. The slowest kept queries take about 60 s on one core, so this
/// leaves room for a slower CI runner while still failing a runaway search.
const TIME_LIMIT_S: &str = "240";

/// Held for the whole of each planner snapshot test, so the corpus and
/// sketch plan tests do not run at once (`--include-ignored` would otherwise
/// run them in parallel) and slow each other's searches.
static SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());

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

/// Runs `stage_pipeline` (without the memo section) on `workload`; panics
/// (a planner crash) on failure.
fn plan(workload: &Value, args: &[String], scratch: &Path) -> Value {
    let input = scratch.join("workload.json");
    std::fs::write(&input, workload.to_string()).unwrap();
    let mut input_args = vec!["--workload".to_owned(), input.display().to_string()];
    input_args.extend_from_slice(args);
    stage_pipeline(&input_args, scratch)
}

/// Runs `stage_pipeline` (without the memo section, under the
/// [`TIME_LIMIT_S`] budget) with `args`, which name its input; panics (a
/// planner crash) on failure.
fn stage_pipeline(args: &[String], scratch: &Path) -> Value {
    let output = scratch.join("document.json");
    let mut command = Command::new(env!("CARGO_BIN_EXE_stage_pipeline"));
    command
        .args(["--milp-time-limit-s", TIME_LIMIT_S, "--no-memo"])
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

/// Whether the wall-clock budget ended the search: planning (Pass 1 and
/// every pane-width round) used the whole budget. A round cut short by its
/// own budget is not visible in the report.
fn hit_time_limit(document: &Value) -> bool {
    let report = &document["planning_report"];
    let Some(limit) = report["time_limit_ms"].as_f64() else {
        return false;
    };
    let rounds: f64 = report["rounds"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|round| {
            ["pass2_ms", "stage2_ms", "stage3_ms"]
                .iter()
                .filter_map(|key| round[*key].as_f64())
                .sum::<f64>()
        })
        .sum();
    report["pass1_ms"].as_f64().unwrap_or(0.0) + rounds >= limit
}

/// The selected plan's search status (`optimal` or `deadline_reached`), or
/// null without a plan.
fn search_status(document: &Value) -> &Value {
    &document["stage3_plan_selection"]["search_report"]["status"]
}

/// Whether the plan is proven optimal: its search says so and every legal
/// pane width was searched.
fn proven_optimal(document: &Value) -> bool {
    search_status(document) == "optimal"
        && document["planning_report"]["pane_widths_pending"] != json!(true)
}

/// `value` with every non-integer number rounded to 12 significant digits.
fn rounded(value: &Value) -> Value {
    match value {
        Value::Number(n) if n.is_f64() => {
            let x = n.as_f64().unwrap();
            json!(format!("{x:.11e}").parse::<f64>().unwrap())
        }
        Value::Object(map) => {
            Value::Object(map.iter().map(|(k, v)| (k.clone(), rounded(v))).collect())
        }
        Value::Array(items) => Value::Array(items.iter().map(rounded).collect()),
        other => other.clone(),
    }
}

/// The stable part of a one-query `stage_pipeline` document: the query, the
/// selected plan and its cost, or why there is none. See the module docs.
fn snapshot_of(document: &Value) -> Value {
    let mut snapshot = plan_snapshot(document);
    snapshot.insert(
        "query".into(),
        document["workload"]["queries"][0]["text"].clone(),
    );
    rounded(&Value::Object(snapshot))
}

/// The selected plan and its cost, or why there is none, unrounded: the
/// part of a snapshot the sketch plan snapshots share.
fn plan_snapshot(document: &Value) -> Map<String, Value> {
    let stage3 = &document["stage3_plan_selection"];
    let mut snapshot = Map::new();
    snapshot.insert(
        "accuracy_guarantees".into(),
        document["accuracy_guarantees"].clone(),
    );
    if stage3["cost"].is_object() {
        // The document shows the selected plan as Stage 2 candidate `P1`.
        let candidate = document["stage2_physical_asap_optimization"]["candidates"]
            .as_array()
            .and_then(|candidates| candidates.iter().find(|c| c["id"] == "P1"))
            .unwrap_or_else(|| panic!("the selected plan P1 is not shown"));
        snapshot.insert(
            "selected".into(),
            json!({
                "display_name": candidate["display_name"],
                "dag": candidate["dag"],
                "cost": stage3["cost"],
                "accepted": stage3["accepted_accuracy_per_query"],
            }),
        );
        snapshot.insert("proven_optimal".into(), json!(proven_optimal(document)));
        for key in [
            "materialized_views",
            "materialized_views_error",
            "pane_retention_ms",
            "warnings",
        ] {
            if let Some(value) = stage3.get(key) {
                snapshot.insert(key.into(), value.clone());
            }
        }
    } else {
        snapshot.insert("selected".into(), Value::Null);
        if let Some(error) = stage3["error"].as_str() {
            let error = error.split(" Search report: ").next().unwrap();
            snapshot.insert("error".into(), json!(error));
        }
    }
    snapshot
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

/// The first JSON path where `expected` and `actual` differ, with both
/// values (shortened), for the failure message; `None` when they are equal.
fn first_differing_path(expected: &Value, actual: &Value) -> Option<String> {
    fn short(value: Option<&Value>) -> String {
        let text = value.map_or("<absent>".to_owned(), Value::to_string);
        match text.char_indices().nth(200) {
            Some((end, _)) => format!("{}...", &text[..end]),
            None => text,
        }
    }
    fn walk(path: String, expected: Option<&Value>, actual: Option<&Value>) -> Option<String> {
        match (expected, actual) {
            (Some(Value::Object(e)), Some(Value::Object(a))) => {
                let mut keys: Vec<&String> = e.keys().chain(a.keys()).collect();
                keys.sort();
                keys.dedup();
                keys.into_iter()
                    .find_map(|key| walk(format!("{path}.{key}"), e.get(key), a.get(key)))
            }
            (Some(Value::Array(e)), Some(Value::Array(a))) => (0..e.len().max(a.len()))
                .find_map(|i| walk(format!("{path}[{i}]"), e.get(i), a.get(i))),
            (e, a) if e == a => None,
            (e, a) => Some(format!("{path}:\n  - {}\n  + {}", short(e), short(a))),
        }
    }
    walk(String::new(), Some(expected), Some(actual))
}

/// Every corpus query's plan matches its committed snapshot.
#[test]
#[ignore = "runs the release planner over the corpora: see the module docs"]
fn corpus_plans_match_snapshots() {
    let _serial = SERIAL
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let update = std::env::var_os("UPDATE_SNAPSHOTS").is_some();
    // Regenerated snapshots of failing queries, with their full
    // `stage_pipeline` documents for the DAG viewer, for CI to upload.
    // Each corpus clears only its own directory: the sketch plan test writes
    // beside them.
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
        let _ = std::fs::remove_dir_all(regenerated.join(&name));
        let corpus: Value =
            serde_json::from_str(&std::fs::read_to_string(dir.join("corpus.json")).unwrap())
                .unwrap();
        let args: Vec<String> =
            serde_json::from_value(corpus["stage_pipeline_args"].clone()).unwrap();
        // A query with a `skip_reason` is not planned.
        let queries: Vec<&Value> = corpus["queries"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|q| q.get("skip_reason").is_none())
            .collect();
        let ids: Vec<&str> = queries.iter().map(|q| q["id"].as_str().unwrap()).collect();

        // A snapshot whose query left the manifest, or became known-slow,
        // would otherwise go stale unseen.
        for entry in std::fs::read_dir(&dir).unwrap() {
            let file = entry.unwrap().file_name().to_string_lossy().into_owned();
            if let Some(id) = file.strip_suffix(".json") {
                if id != "corpus" && !ids.contains(&id) {
                    failures.push(format!("{name}/{file}: no checked query with id {id}"));
                }
            }
        }

        for query in queries {
            let id = query["id"].as_str().unwrap();
            let document = plan(
                &workload(&corpus, query["query"].as_str().unwrap()),
                &args,
                &scratch,
            );
            // A `time_limit_reason` accepts a run the time limit ended; the
            // plan and cost are still compared with the snapshot.
            if hit_time_limit(&document) && query.get("time_limit_reason").is_none() {
                failures.push(format!(
                    "{name}/{id}: the {TIME_LIMIT_S} s time limit ended the search, \
                     so its plan depends on machine speed"
                ));
                continue;
            }
            let actual = serde_json::to_string_pretty(&snapshot_of(&document)).unwrap() + "\n";
            let path = dir.join(format!("{id}.json"));
            if update {
                std::fs::write(&path, &actual).unwrap();
                continue;
            }
            let expected = std::fs::read_to_string(&path).unwrap_or_default();
            if expected != actual {
                let out = regenerated.join(&name);
                std::fs::create_dir_all(&out).unwrap();
                std::fs::write(out.join(format!("{id}.json")), &actual).unwrap();
                std::fs::write(
                    out.join(format!("{id}.document.json")),
                    serde_json::to_string_pretty(&document).unwrap(),
                )
                .unwrap();
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

/// The snapshot keeps the selected plan without the search report, and
/// rounds floats.
#[test]
fn snapshot_keeps_only_the_plan() {
    let document = json!({
        "accuracy_guarantees": "enforced",
        "workload": {"queries": [{"text": "sum(up)"}]},
        "stage2_physical_asap_optimization": {"candidates": [
            {"id": "P1", "display_name": "plan A", "dag": {"nodes": [1]}},
        ]},
        "stage3_plan_selection": {
            "selected_dag": {"candidate_of_node": [0]},
            "cost": {"cost_per_second": 0.1 + 0.2},
            "accepted_accuracy_per_query": [],
            "materialized_views": [],
            "pane_retention_ms": {"1": 60000},
            "search_report": {"status": "deadline_reached", "solver_ms": 1},
        },
    });
    assert_eq!(
        snapshot_of(&document),
        json!({
            "query": "sum(up)",
            "accuracy_guarantees": "enforced",
            "selected": {
                "display_name": "plan A",
                "dag": {"nodes": [1]},
                "cost": {"cost_per_second": 0.3},
                "accepted": [],
            },
            "proven_optimal": false,
            "materialized_views": [],
            "pane_retention_ms": {"1": 60000},
        })
    );
}

/// A query without a plan keeps the error up to the embedded search report.
#[test]
fn snapshot_of_a_failed_query() {
    let document = json!({
        "accuracy_guarantees": "enforced",
        "workload": {"queries": [{"text": "sum(up)"}]},
        "stage3_plan_selection": {
            "selected_dag": null,
            "error": "infeasible. Search report: {\"solver_ms\":1.5}",
        },
    });
    assert_eq!(
        snapshot_of(&document),
        json!({
            "query": "sum(up)",
            "accuracy_guarantees": "enforced",
            "selected": null,
            "error": "infeasible.",
        })
    );
}

/// Planning that used its whole wall-clock budget is detected; planning
/// within it, or with no report, is not.
#[test]
fn time_limit_detection() {
    let report = |pass1: f64, stage3: f64| {
        json!({"planning_report": {
            "pass1_ms": pass1, "time_limit_ms": 1000,
            "rounds": [{"pass2_ms": 1.0, "stage2_ms": 1.0, "stage3_ms": stage3}],
        }})
    };
    assert!(hit_time_limit(&report(10.0, 990.0)));
    assert!(!hit_time_limit(&report(10.0, 12.0)));
    assert!(!hit_time_limit(
        &json!({"stage3_plan_selection": {"error": "x"}})
    ));
}

/// An optimal search proves the plan optimal only when no legal pane width
/// was left unsearched.
#[test]
fn proven_optimal_needs_every_pane_width() {
    let document = |status: &str, pending: bool| {
        json!({
            "stage3_plan_selection": {"search_report": {"status": status}},
            "planning_report": {"pane_widths_pending": pending},
        })
    };
    assert!(proven_optimal(&document("optimal", false)));
    assert!(!proven_optimal(&document("optimal", true)));
    assert!(!proven_optimal(&document("deadline_reached", false)));
}

/// The failure message names the first differing JSON path.
#[test]
fn first_differing_path_names_the_path() {
    let expected = json!({"a": {"b": [1, 2]}, "c": 1});
    assert_eq!(first_differing_path(&expected, &expected), None);
    assert_eq!(
        first_differing_path(&expected, &json!({"a": {"b": [1, 3]}, "c": 2})).unwrap(),
        ".a.b[1]:\n  - 2\n  + 3"
    );
    assert_eq!(
        first_differing_path(&expected, &json!({"a": {"b": [1]}, "c": 1})).unwrap(),
        ".a.b[1]:\n  - 2\n  + <absent>"
    );
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

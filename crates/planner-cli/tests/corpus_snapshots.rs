//! Planner snapshots over real-world query corpora (#941).
//!
//! The planner has no reference answer, so these tests pin its output
//! instead: each corpus query's selected plan (default search) is compared
//! with the committed one, and any change fails until the snapshot is
//! regenerated and the change explained in the PR. That includes a plan
//! that changes at the committed plan's cost (#1541): the nightly workflow
//! sets `STRICT_SNAPSHOTS=1`, and any difference fails. Without it, as on a
//! local run, a changed plan whose cost equals the committed plan's
//! (relative difference at most 1e-9), with the same accuracy, warnings and
//! errors, passes with a note on stderr: the selection may return any of
//! several equal-cost plans, and which one could differ between machines.
//!
//! Each `tests/corpus_snapshots/<corpus>/corpus.json` declares the shared
//! planning inputs and the queries by stable id; `<id>.json` beside it is
//! that query's snapshot. Regenerate with
//! `UPDATE_SNAPSHOTS=1 cargo test --release -p asap-planner-cli --test corpus_snapshots -- --ignored`.
//!
//! Each query is planned by `asap_planner --run-until stage3`: one exact
//! search over every legal tumbling-window width, with no time limit.
//!
//! A snapshot holds only the plan, never the search that found it: Stage
//! 3's plan (its `PhysicalASAPDAG`, with the retention and lifetime of its nodes), its cost and
//! accepted accuracy per query, and the materialized views; for a query
//! that does not plan, the error. The selection MILP's size and timings,
//! the Stage 1 and Stage 2 candidate lists and the memo are left out. Floats are as
//! `asap_planner` prints them, rounded to 12 significant digits. A query with a `skip_reason` is skipped.
//!
//! The same binary holds the sketch plan snapshots (`sketch_plan_snapshots.rs`),
//! which reuse these helpers on multi-query workloads with accuracy targets.

use std::path::{Path, PathBuf};
use std::process::Command;

use serde_json::{json, Map, Value};

#[path = "sketch_plan_snapshots.rs"]
mod sketch_plan_snapshots;

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

/// Runs `asap_planner` on `workload`; panics (a planner crash) on failure.
fn plan(workload: &Value, args: &[String], scratch: &Path) -> Value {
    let input = scratch.join("workload.json");
    std::fs::write(&input, workload.to_string()).unwrap();
    let mut input_args = vec!["--workload".to_owned(), input.display().to_string()];
    input_args.extend_from_slice(args);
    asap_planner(&input_args, scratch)
}

/// Runs `asap_planner --run-until stage3 --format json` with `args`, which
/// name its input (paths relative to the repository root). A run whose plan
/// selection failed still writes its document, with Stage 3's `error`,
/// which is returned; any other failure (a planner crash) panics.
fn asap_planner(args: &[String], scratch: &Path) -> Value {
    let output = scratch.join("document.json");
    let mut command = Command::new(env!("CARGO_BIN_EXE_asap_planner"));
    command
        .current_dir(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../.."))
        .args(["--run-until", "stage3", "--format", "json"])
        .args(args)
        .arg("--out")
        .arg(&output);
    let result = command.output().unwrap();
    let document: Option<Value> = std::fs::read_to_string(&output)
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok());
    match document {
        Some(document)
            if result.status.success()
                || document["stage3_plan_selection"]["error"].is_string() =>
        {
            document
        }
        _ => panic!(
            "{command:?} failed: {}",
            String::from_utf8_lossy(&result.stderr)
        ),
    }
}

/// The stable part of a one-query `asap_planner` document: the query, the
/// selected plan and its cost, or why there is none. See the module docs.
fn snapshot_of(document: &Value) -> Value {
    let mut snapshot = plan_snapshot(document);
    snapshot.insert(
        "query".into(),
        document["workload"]["queries"][0]["text"].clone(),
    );
    Value::Object(snapshot)
}

/// The selected plan and its cost, or why there is none: the
/// part of a snapshot the sketch plan snapshots share.
fn plan_snapshot(document: &Value) -> Map<String, Value> {
    let stage3 = &document["stage3_plan_selection"];
    let mut snapshot = Map::new();
    snapshot.insert(
        "accuracy_guarantees".into(),
        document["accuracy_guarantees"].clone(),
    );
    if stage3["plan"].is_object() {
        snapshot.insert(
            "selected".into(),
            json!({
                "dag": stage3["plan"],
                "cost": stage3["cost"],
                "accepted": stage3["accepted_accuracy_per_query"],
            }),
        );
        for key in [
            "materialized_views",
            "materialized_views_error",
            "planner_executor_error",
            "warnings",
        ] {
            if let Some(value) = stage3.get(key) {
                snapshot.insert(key.into(), value.clone());
            }
        }
    } else {
        snapshot.insert("selected".into(), Value::Null);
        if let Some(error) = stage3["error"].as_str() {
            snapshot.insert("error".into(), json!(error));
        }
    }
    snapshot
}

/// The largest relative difference between two selected plans' costs per
/// second for which a changed plan still passes. Snapshot floats carry 12
/// significant digits, and plans this close in cost are equally good:
/// the selection may return either, depending on the machine.
const EQUAL_COST_RELATIVE_TOLERANCE: f64 = 1e-9;

/// Whether equal-cost plan changes fail: `STRICT_SNAPSHOTS` is set, as the
/// nightly workflow sets it.
fn strict_snapshots() -> bool {
    std::env::var_os("STRICT_SNAPSHOTS").is_some()
}

/// Whether a snapshot whose text changed from `expected` to `actual` still
/// passes: never when `strict`; otherwise when both select plans of the
/// same cost ([`equal_cost_plan`]).
fn changed_snapshot_passes(expected: &str, actual: &str, strict: bool) -> bool {
    !strict && equal_cost_plan(expected, actual)
}

/// Whether the snapshot texts `expected` and `actual` select plans of the
/// same cost ([`same_selected_cost`]).
fn equal_cost_plan(expected: &str, actual: &str) -> bool {
    match (
        serde_json::from_str::<Value>(expected),
        serde_json::from_str::<Value>(actual),
    ) {
        (Ok(expected), Ok(actual)) => same_selected_cost(&expected, &actual),
        _ => false,
    }
}

/// Whether both snapshots select a plan, their costs per second are equal
/// within [`EQUAL_COST_RELATIVE_TOLERANCE`], and they agree on everything
/// but the plan itself ([`without_the_plan`]): accuracy guarantees, the
/// accepted accuracy per query, warnings, errors and the queries.
fn same_selected_cost(expected: &Value, actual: &Value) -> bool {
    let cost = |snapshot: &Value| snapshot["selected"]["cost"]["cost_per_second"].as_f64();
    let equal_cost = match (cost(expected), cost(actual)) {
        (Some(expected), Some(actual)) => {
            (expected - actual).abs()
                <= EQUAL_COST_RELATIVE_TOLERANCE * expected.abs().max(actual.abs())
        }
        _ => false,
    };
    equal_cost && without_the_plan(expected) == without_the_plan(actual)
}

/// `snapshot` without the selected plan's DAG, its cost breakdown and the
/// materialized views derived from it: the parts that differ between two
/// equal-cost plans.
fn without_the_plan(snapshot: &Value) -> Value {
    let mut rest = snapshot.clone();
    if let Some(selected) = rest["selected"].as_object_mut() {
        selected.remove("dag");
        selected.remove("cost");
    }
    if let Some(fields) = rest.as_object_mut() {
        fields.remove("materialized_views");
    }
    rest
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
    let strict = strict_snapshots();
    // Regenerated snapshots of failing queries, with their full
    // `asap_planner` documents for the DAG viewer, for CI to upload.
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
            serde_json::from_value(corpus["asap_planner_args"].clone()).unwrap();
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
            let actual = serde_json::to_string_pretty(&snapshot_of(&document)).unwrap() + "\n";
            let path = dir.join(format!("{id}.json"));
            if update {
                std::fs::write(&path, &actual).unwrap();
                continue;
            }
            let expected = std::fs::read_to_string(&path).unwrap_or_default();
            if expected != actual && changed_snapshot_passes(&expected, &actual, strict) {
                eprintln!(
                    "{name}/{id}: plan changed at the committed plan's cost; \
                     passes (fails with STRICT_SNAPSHOTS=1)"
                );
            } else if expected != actual {
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

/// The snapshot keeps the plan without the selection MILP's size.
#[test]
fn snapshot_keeps_only_the_plan() {
    let document = json!({
        "accuracy_guarantees": "enforced",
        "workload": {"queries": [{"text": "sum(up)"}]},
        "stage3_plan_selection": {
            "plan": {"nodes": [1]},
            "cost": {"cost_per_second": 0.3},
            "accepted_accuracy_per_query": [],
            "materialized_views": [],
            "milp_variables": 1,
        },
    });
    assert_eq!(
        snapshot_of(&document),
        json!({
            "query": "sum(up)",
            "accuracy_guarantees": "enforced",
            "selected": {
                "dag": {"nodes": [1]},
                "cost": {"cost_per_second": 0.3},
                "accepted": [],
            },
            "materialized_views": [],
        })
    );
}

/// A query without a plan keeps the error.
#[test]
fn snapshot_of_a_failed_query() {
    let document = json!({
        "accuracy_guarantees": "enforced",
        "workload": {"queries": [{"text": "sum(up)"}]},
        "stage3_plan_selection": {"plan": null, "error": "infeasible"},
    });
    assert_eq!(
        snapshot_of(&document),
        json!({
            "query": "sum(up)",
            "accuracy_guarantees": "enforced",
            "selected": null,
            "error": "infeasible",
        })
    );
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

/// Outside strict mode, a changed plan with the committed plan's cost
/// passes: the planner may pick any of several equal-cost plans, and which
/// one could depend on the machine. A dearer or cheaper plan, or one that
/// no longer plans, fails.
#[test]
fn equal_cost_plans_pass() {
    let plan = |cost: f64| json!({"selected": {"cost": {"cost_per_second": cost}, "dag": {}}});
    assert!(same_selected_cost(
        &plan(7.32192724727e-7),
        &plan(7.32192724727e-7)
    ));
    assert!(same_selected_cost(
        &plan(1.65072582049e-8),
        &plan(1.65072581961e-8)
    ));
    assert!(!same_selected_cost(&plan(1.0e-6), &plan(1.1e-6)));
    assert!(!same_selected_cost(
        &plan(1.0e-6),
        &json!({"selected": null})
    ));
    assert!(!same_selected_cost(
        &json!({"selected": null}),
        &json!({"selected": null})
    ));
    // Equal cost, different plan and views: passes.
    let with = |dag: Value, accepted: Value, warnings: Value| {
        json!({
            "selected": {"cost": {"cost_per_second": 1.0e-6}, "dag": dag, "accepted": accepted},
            "materialized_views": dag,
            "warnings": warnings,
        })
    };
    let committed = with(json!({"k": 1404}), json!([0.01]), json!([]));
    assert!(same_selected_cost(
        &committed,
        &with(json!({"k": 2863}), json!([0.01]), json!([]))
    ));
    // Equal cost, but weaker accepted accuracy or a new warning: fails.
    assert!(!same_selected_cost(
        &committed,
        &with(json!({"k": 1404}), json!([0.05]), json!([]))
    ));
    assert!(!same_selected_cost(
        &committed,
        &with(json!({"k": 1404}), json!([0.01]), json!(["w"]))
    ));
}

/// In strict mode (the nightly workflow) any changed snapshot fails, even
/// one selecting a different plan at the committed plan's cost (#1541):
/// committed `k = 1404`, selected `k = 2863` at the same cost.
#[test]
fn equal_cost_plan_changes_fail_when_strict() {
    let snapshot = |k: u64| {
        serde_json::to_string_pretty(&json!({
            "selected": {"cost": {"cost_per_second": 1.0e-6}, "dag": {"k": k}, "accepted": []},
        }))
        .unwrap()
    };
    let (committed, regenerated) = (snapshot(1404), snapshot(2863));
    assert!(changed_snapshot_passes(&committed, &regenerated, false));
    assert!(!changed_snapshot_passes(&committed, &regenerated, true));
}

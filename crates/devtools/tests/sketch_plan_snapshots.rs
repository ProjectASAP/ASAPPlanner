//! Planner snapshots over workloads that select sketches.
//!
//! The corpus snapshots are single queries without an accuracy target, so
//! their plans are exact. These workloads carry accuracy targets and select
//! sketches (KLL, DDSketch, KMV, a CMS heap), share sketches and pane chains
//! between queries, compose two sketch results, and materialize panes, under
//! each deployment the DAG viewer examples use (default, raw data retained, and the
//! MVP's raw data not retained). A change to sketch kinds, parameters, sizes,
//! sharing, timing, materialization or cost shows up as a snapshot diff.
//!
//! `tests/sketch_plan_snapshots/manifest.json` lists each workload by id:
//! its `workload` file (or built-in `example`) relative to the repository
//! root, SQL `tables`, `deployment`, and whether accuracy guarantees are
//! `ignored` (enforced otherwise). `<id>.json` beside it is its snapshot,
//! the corpus snapshot of the selected plan plus every query's text and the
//! search status. Workloads copied from elsewhere live in `workloads/`.
//! Regenerate with
//! `UPDATE_SNAPSHOTS=1 cargo test --release -p asap-devtools --test corpus_snapshots -- --ignored sketch_plans_match_snapshots`.
//!
//! Each kept workload's search is proven optimal or stopped by an
//! evaluation count (or a numerical stall), never by the wall clock, and
//! gave the same plan in three runs. Workloads whose search used the whole
//! time limit (the raw-retained memo examples, full random-mixed workloads,
//! three or more quantile queries over 10,000 series) were left out; a run
//! that the time limit stops fails rather than being recorded.

use std::path::PathBuf;

use serde_json::{json, Value};

use super::{
    first_difference, first_differing_path, hit_time_limit, plan_snapshot, rounded, stage_pipeline,
};

fn repository_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn snapshots_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/sketch_plan_snapshots")
}

/// The `stage_pipeline` arguments for `entry` under `manifest`'s deployments.
fn arguments(manifest: &Value, entry: &Value) -> Vec<String> {
    let path = |key: &str| repository_root().join(entry[key].as_str().unwrap());
    let mut args = Vec::new();
    match (entry["workload"].as_str(), entry["example"].as_str()) {
        (Some(_), None) => args.extend([
            "--workload".to_owned(),
            path("workload").display().to_string(),
        ]),
        (None, Some(example)) => args.extend(["--example".to_owned(), example.to_owned()]),
        _ => panic!("{entry}: give exactly one of `workload` and `example`"),
    }
    if entry.get("tables").is_some() {
        let tables: Vec<Value> =
            serde_json::from_str(&std::fs::read_to_string(path("tables")).unwrap()).unwrap();
        for table in tables {
            args.extend(["--table".to_owned(), table.to_string()]);
        }
    }
    let deployment = entry["deployment"].as_str().unwrap();
    let deployment: Vec<String> =
        serde_json::from_value(manifest["deployments"][deployment].clone())
            .unwrap_or_else(|_| panic!("{entry}: unknown deployment {deployment}"));
    args.extend(deployment);
    match entry["accuracy_guarantees"].as_str() {
        None | Some("enforced") => args.push("--enforce-accuracy-guarantees".to_owned()),
        Some("ignored") => {}
        Some(other) => {
            panic!("{entry}: accuracy_guarantees must be enforced or ignored, not {other}")
        }
    }
    args
}

/// The stable part of a `stage_pipeline` document: every query's text, the
/// selected plan (see [`plan_snapshot`]) and how its search ended.
fn snapshot_of(document: &Value) -> Value {
    let mut snapshot = plan_snapshot(document);
    let queries: Vec<Value> = document["workload"]["queries"]
        .as_array()
        .unwrap()
        .iter()
        .map(|query| query["text"].clone())
        .collect();
    snapshot.insert("queries".into(), Value::Array(queries));
    let status = &document["stage3_plan_selection"]["search"]["candidate_connections"]["status"];
    if !status.is_null() {
        snapshot.insert("search_status".into(), status.clone());
    }
    rounded(&Value::Object(snapshot))
}

/// Every manifest workload's plan matches its committed snapshot.
#[test]
#[ignore = "runs the release planner over the sketch workloads: see the module docs"]
fn sketch_plans_match_snapshots() {
    let update = std::env::var_os("UPDATE_SNAPSHOTS").is_some();
    // Beside the corpus snapshots' regenerated files, which CI uploads.
    let regenerated = PathBuf::from(env!("CARGO_TARGET_TMPDIR"))
        .join("corpus_snapshots")
        .join("sketch_plans");
    let _ = std::fs::remove_dir_all(&regenerated);
    let scratch =
        std::env::temp_dir().join(format!("sketch_plan_snapshots_{}", std::process::id()));
    std::fs::create_dir_all(&scratch).unwrap();
    let dir = snapshots_dir();
    let manifest: Value =
        serde_json::from_str(&std::fs::read_to_string(dir.join("manifest.json")).unwrap()).unwrap();
    let entries = manifest["workloads"].as_array().unwrap();
    let ids: Vec<&str> = entries.iter().map(|e| e["id"].as_str().unwrap()).collect();
    let mut failures = Vec::new();

    // A snapshot whose workload left the manifest would otherwise go stale.
    for entry in std::fs::read_dir(&dir).unwrap() {
        let file = entry.unwrap().file_name().to_string_lossy().into_owned();
        if let Some(id) = file.strip_suffix(".json") {
            if id != "manifest" && !ids.contains(&id) {
                failures.push(format!("{file}: no manifest workload with id {id}"));
            }
        }
    }

    for entry in entries {
        let id = entry["id"].as_str().unwrap();
        let document = stage_pipeline(&arguments(&manifest, entry), &scratch);
        if hit_time_limit(&document) {
            failures.push(format!(
                "{id}: the {} s time limit ended the search, so its plan depends on \
                 machine speed",
                super::TIME_LIMIT_S
            ));
            continue;
        }
        let actual_text = serde_json::to_string_pretty(&snapshot_of(&document)).unwrap() + "\n";
        let path = dir.join(format!("{id}.json"));
        if update {
            std::fs::write(&path, &actual_text).unwrap();
            continue;
        }
        // Both sides go through the parser: it does not round-trip every
        // float exactly, so the snapshot is compared with the reparsed text.
        let difference = match std::fs::read_to_string(&path) {
            Err(_) => Some("no snapshot".to_owned()),
            Ok(expected) if expected == actual_text => None,
            Ok(expected) => Some(
                first_differing_path(
                    &serde_json::from_str(&expected).unwrap(),
                    &serde_json::from_str(&actual_text).unwrap(),
                )
                .unwrap_or_else(|| first_difference(&expected, &actual_text)),
            ),
        };
        if let Some(difference) = difference {
            std::fs::create_dir_all(&regenerated).unwrap();
            std::fs::write(regenerated.join(format!("{id}.json")), &actual_text).unwrap();
            std::fs::write(
                regenerated.join(format!("{id}.document.json")),
                serde_json::to_string_pretty(&document).unwrap(),
            )
            .unwrap();
            failures.push(format!("{id}: plan changed at {difference}"));
        }
    }
    std::fs::remove_dir_all(&scratch).unwrap();
    assert!(
        failures.is_empty(),
        "{} sketch plan snapshot(s) differ; regenerated ones are in {}.\n\
         If the change is intended, run with UPDATE_SNAPSHOTS=1 and explain it \
         under \"Snapshot changes\" in the PR.\n\n{}",
        failures.len(),
        regenerated.display(),
        failures.join("\n\n")
    );
}

/// Each manifest entry names a known deployment and an existing input, and
/// builds its arguments.
#[test]
fn manifest_entries_build_their_arguments() {
    let manifest: Value = serde_json::from_str(
        &std::fs::read_to_string(snapshots_dir().join("manifest.json")).unwrap(),
    )
    .unwrap();
    for entry in manifest["workloads"].as_array().unwrap() {
        for key in ["workload", "tables"] {
            if let Some(path) = entry[key].as_str() {
                assert!(repository_root().join(path).is_file(), "{entry}: no {path}");
            }
        }
        let args = arguments(&manifest, entry);
        assert_eq!(
            args.iter().any(|a| a == "--enforce-accuracy-guarantees"),
            entry["accuracy_guarantees"] != json!("ignored")
        );
    }
}

/// A snapshot keeps every query's text and the search status.
#[test]
fn snapshot_keeps_the_queries_and_search_status() {
    let document = json!({
        "accuracy_guarantees": "enforced",
        "workload": {"queries": [{"text": "q1"}, {"text": "q2"}]},
        "stage2_physical_asap_optimization": {"candidates": [
            {"id": "plan-1", "display_name": "plan A", "dag": {"nodes": [1]}},
        ]},
        "stage3_plan_selection": {
            "selected": "plan-1",
            "costs": {"plan-1": {"cost_per_second": 0.1 + 0.2}},
            "accepted": {"plan-1": []},
            "search": {"candidate_connections": {"status": "optimal", "solver_ms": 1.5}},
        },
    });
    let snapshot = snapshot_of(&document);
    assert_eq!(snapshot["queries"], json!(["q1", "q2"]));
    assert_eq!(snapshot["search_status"], json!("optimal"));
    assert_eq!(snapshot["proven_optimal"], json!(true));
    assert_eq!(
        snapshot["selected"]["cost"],
        json!({"cost_per_second": 0.3})
    );
}

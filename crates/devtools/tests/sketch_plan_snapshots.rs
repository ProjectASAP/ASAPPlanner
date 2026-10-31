//! Planner snapshots over workloads that select sketches.
//!
//! The corpus snapshots are single queries without an accuracy target, so
//! their plans are exact. These workloads carry accuracy targets and select
//! sketches (KLL, DDSketch, a CMS heap), share a sketch pane chain between
//! queries or nested windows, answer a ratio of two quantiles from one pane
//! chain, and materialize panes, under the MVP deployment (raw data not
//! retained) and raw data retained. A change to sketch kinds, parameters,
//! sizes, sharing, timing, materialization or cost shows up as a snapshot
//! diff.
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
//! The search's seed and pane-width rounds have wall-clock budgets (5 s
//! each), so a search that stops at a budget, of time or of evaluations, can
//! select a different plan on a slower machine. Only a search that proves its
//! plan optimal is independent of speed, so every workload must end with
//! status `optimal` and no pane width left unsearched; any other ending
//! fails instead of being recorded. A workload with a
//! `deadline_reached_reason` in the manifest may instead end with status
//! `deadline_reached` (no pane width left unsearched); its snapshot records
//! that status, so the plan, cost and status must still match. Each
//! kept workload gave the same plan in three runs and on one core.
//! Workloads whose search stops at a budget (the raw-retained memo examples,
//! full random-mixed workloads, three or more quantile queries over 10,000
//! series, the `or` and `COUNT(DISTINCT)` workloads, single quantiles under
//! the default and raw-retained deployments) are left out.

use std::path::PathBuf;

use serde_json::{json, Value};

use super::{
    first_difference, first_differing_path, hit_time_limit, plan_snapshot, proven_optimal, rounded,
    search_status, stage_pipeline,
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
    let status = search_status(document);
    if !status.is_null() {
        snapshot.insert("search_status".into(), status.clone());
    }
    rounded(&Value::Object(snapshot))
}

/// Every manifest workload's plan matches its committed snapshot.
#[test]
#[ignore = "runs the release planner over the sketch workloads: see the module docs"]
fn sketch_plans_match_snapshots() {
    let _serial = super::SERIAL
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
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
        // A `deadline_reached_reason` accepts a search that ended at its
        // deadline after every pane width; plan and cost are still compared.
        let deadline_accepted = entry.get("deadline_reached_reason").is_some()
            && search_status(&document) == "deadline_reached"
            && document["planning_report"]["pane_widths_pending"] != json!(true);
        if !proven_optimal(&document) && !deadline_accepted {
            failures.push(format!(
                "{id}: the plan is not proven optimal (status {}, pane widths \
                 pending {}), so a round's budget may have chosen it; drop the \
                 workload or make it smaller",
                search_status(&document),
                document["planning_report"]["pane_widths_pending"]
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
            {"id": "P1", "display_name": "plan A", "dag": {"nodes": [1]}},
        ]},
        "stage3_plan_selection": {
            "cost": {"cost_per_second": 0.1 + 0.2},
            "accepted_accuracy_per_query": [],
            "search_report": {"status": "optimal", "solver_ms": 1},
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

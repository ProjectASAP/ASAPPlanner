//! Planner snapshots over workloads that select sketches.
//!
//! The corpus snapshots are single queries without an accuracy target, so
//! their plans are exact. These workloads carry accuracy targets and select
//! sketches (KLL, DDSketch, a CMS heap), share a sketch tumbling-window chain between
//! queries or nested windows, answer a ratio of two quantiles from one tumbling window
//! chain, and materialize tumbling windows, under the MVP deployment (raw data not
//! retained) and raw data retained. A change to sketch kinds, parameters,
//! sizes, sharing, timing, lifetime or cost shows up as a snapshot
//! diff.
//!
//! `tests/sketch_plan_snapshots/manifest.json` lists each workload by id
//! (read by `asap_devtools::corpus_registry::sketch_plans`):
//! its `workload` file relative to the repository root, SQL `tables`,
//! `deployment` (the `asap_planner` arguments of its deployment
//! capabilities and cost inputs, files in `deployments/`), and whether
//! accuracy guarantees are `ignored` (enforced otherwise). `<id>.json` beside it is its snapshot,
//! the corpus snapshot of the selected plan plus every query's text. Workloads copied from elsewhere live in `workloads/`.
//! Regenerate with
//! `UPDATE_SNAPSHOTS=1 cargo test --release -p asap-devtools --test corpus_snapshots -- --ignored sketch_plans_match_snapshots`.
//!
//! Each workload is planned by `asap_planner --run-until stage3`, with no
//! time limit. A workload whose selection fails records its error, as a
//! corpus query does.

use std::path::PathBuf;

use serde_json::{json, Value};

use asap_cost::CostInputs;
use asap_devtools::corpus_registry;
use asap_types::deployment_capabilities::DeploymentCapabilities;

use super::{asap_planner, first_difference, first_differing_path, plan_snapshot, rounded};

fn snapshots_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/sketch_plan_snapshots")
}

fn repository_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")
}

/// The stable part of an `asap_planner` document: every query's text and
/// the selected plan (see [`plan_snapshot`]).
fn snapshot_of(document: &Value) -> Value {
    let mut snapshot = plan_snapshot(document);
    let queries: Vec<Value> = document["workload"]["queries"]
        .as_array()
        .unwrap()
        .iter()
        .map(|query| query["text"].clone())
        .collect();
    snapshot.insert("queries".into(), Value::Array(queries));
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
    // The registry's sketch plan workloads, read from `manifest.json`.
    let workloads = corpus_registry::sketch_plans().unwrap();
    let ids: Vec<&str> = workloads
        .iter()
        .map(|w| w.id.strip_prefix("sketch_plans/").unwrap())
        .collect();
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

    for (workload, id) in workloads.iter().zip(&ids) {
        let document = asap_planner(&workload.asap_planner_args(&scratch).unwrap(), &scratch);
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

/// Each manifest entry loads into the registry with the capabilities and
/// cost inputs of the files its deployment names, and its accuracy setting.
#[test]
fn manifest_entries_load_with_their_deployments() {
    let read = |path: &str| -> Value {
        serde_json::from_str(&std::fs::read_to_string(repository_root().join(path)).unwrap())
            .unwrap()
    };
    let manifest = read("crates/devtools/tests/sketch_plan_snapshots/manifest.json");
    let entries = manifest["workloads"].as_array().unwrap();
    let workloads = corpus_registry::sketch_plans().unwrap();
    assert_eq!(workloads.len(), entries.len());
    for (entry, workload) in entries.iter().zip(&workloads) {
        assert_eq!(
            workload.id,
            format!("sketch_plans/{}", entry["id"].as_str().unwrap())
        );
        assert_eq!(
            workload.ignore_accuracy_guarantees,
            entry["accuracy_guarantees"] == json!("ignored")
        );
        let deployment = manifest["deployments"][entry["deployment"].as_str().unwrap()]
            .as_array()
            .unwrap();
        let file_after = |flag: &str| -> &str {
            let at = deployment.iter().position(|arg| arg == flag).unwrap();
            deployment[at + 1].as_str().unwrap()
        };
        assert_eq!(
            serde_json::to_value(&workload.deployment_capabilities).unwrap(),
            serde_json::to_value(
                serde_json::from_value::<DeploymentCapabilities>(read(file_after(
                    "--deployment-capabilities"
                )))
                .unwrap()
            )
            .unwrap(),
            "{}",
            workload.id
        );
        assert_eq!(
            workload.cost_inputs,
            serde_json::from_value::<CostInputs>(read(file_after("--cost-inputs"))).unwrap(),
            "{}",
            workload.id
        );
    }
}

/// A snapshot keeps every query's text.
#[test]
fn snapshot_keeps_the_queries() {
    let document = json!({
        "accuracy_guarantees": "enforced",
        "workload": {"queries": [{"text": "q1"}, {"text": "q2"}]},
        "stage3_plan_selection": {
            "plan": {"nodes": [1]},
            "cost": {"cost_per_second": 0.1 + 0.2},
            "accepted_accuracy_per_query": [],
        },
    });
    let snapshot = snapshot_of(&document);
    assert_eq!(snapshot["queries"], json!(["q1", "q2"]));
    assert_eq!(
        snapshot["selected"]["cost"],
        json!({"cost_per_second": 0.3})
    );
}

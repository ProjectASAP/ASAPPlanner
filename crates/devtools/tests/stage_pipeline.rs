//! `stage_pipeline` writes a valid four-stage viewer document for #509 Example 1.
use std::collections::HashSet;
use std::path::PathBuf;
use std::process::Command;

use asap_devtools::planner_layering_examples::{pattern_a, PatternARecurrence};
use asap_devtools::planning_stages::{lower_promql_roots, run_planning_stages};
use asap_types::ir::flat::FlatDag;
use asap_types::ir::physical_export::{PhysicalASAPDAG, PhysicalASAPDAGDocument};
use asap_types::ir::QueryRoot;
use asap_types::workload::DataArrival;
use serde_json::Value;

const COMMITTED: &str = "../../tools/dag-viewer/examples/planner-layering-example1.json";

fn generate(extra: &[&str]) -> Value {
    generate_example("planner-layering-1", extra)
}

fn generate_example(example: &str, extra: &[&str]) -> Value {
    let out = std::env::temp_dir().join(format!(
        "stage_pipeline_{}_{example}_{}.json",
        std::process::id(),
        extra.len()
    ));
    let status = Command::new(env!("CARGO_BIN_EXE_stage_pipeline"))
        .args(["--example", example, "--out"])
        .arg(&out)
        .args(extra)
        .status()
        .unwrap();
    assert!(status.success());
    let document = serde_json::from_str(&std::fs::read_to_string(&out).unwrap()).unwrap();
    std::fs::remove_file(out).unwrap();
    document
}

fn validated_dag(value: &Value, queries: usize) {
    let dag: FlatDag = serde_json::from_value(value.clone()).unwrap();
    assert_eq!(dag.roots.len(), queries);
    // Children come before their parents.
    for (id, node) in dag.nodes.iter().enumerate() {
        assert!(node
            .operator
            .children()
            .into_iter()
            .all(|child| *child < id));
    }
    for root in &dag.roots {
        if let QueryRoot::Operator(id) = root {
            assert!(*id < dag.nodes.len());
        }
    }
}

/// Every logical DAG is a well-formed flat DAG and every physical DAG validates;
/// each has one root per query; ids are unique; Stage 2 implements or prunes
/// (with a reason) each composed logical candidate, with no cost; Stage 3
/// accounts for every Stage 2 candidate once and costs exactly the valid ones;
/// the committed fixture is current.
#[test]
fn example1_document_is_valid_and_committed_fixture_is_current() {
    let document = generate(&[]);
    let committed: Value = serde_json::from_str(
        &std::fs::read_to_string(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(COMMITTED))
            .unwrap(),
    )
    .unwrap();
    assert_eq!(document, committed, "regenerate the committed fixture");

    assert_eq!(document["format"], "asap-stage-pipeline/v1");
    let queries = document["workload"]["queries"].as_array().unwrap().len();
    assert_eq!(queries, 2);
    validated_dag(&document["language_specific_frontends"]["dag"], queries);

    let logical = &document["logical_asap_optimization"];
    let candidates = logical["candidates"].as_array().unwrap();
    assert_eq!(logical["capped"], false);
    assert_eq!(logical["combinations"], candidates.len());
    let mut ids = HashSet::new();
    let mut composed = HashSet::new();
    for candidate in candidates {
        let id = candidate["id"].as_str().unwrap();
        assert!(ids.insert(id));
        if candidate.get("dag").is_some() {
            composed.insert(id);
            validated_dag(&candidate["dag"], queries);
        }
    }

    let physical = document["physical_asap_optimization"]["candidates"]
        .as_array()
        .unwrap();
    let pruned = document["physical_asap_optimization"]["pruned"]
        .as_array()
        .unwrap();
    assert!(pruned
        .iter()
        .all(|p| !p["reason"].as_str().unwrap().is_empty()));
    let sources: HashSet<_> = physical
        .iter()
        .chain(pruned)
        .map(|p| p["from_logical"].as_str().unwrap())
        .collect();
    assert_eq!(physical.len() + pruned.len(), composed.len());
    assert_eq!(sources, composed);
    for candidate in physical {
        assert!(candidate.get("cost").is_none(), "Stage 2 has no cost");
        let dag: PhysicalASAPDAG = serde_json::from_value(candidate["dag"].clone()).unwrap();
        assert_eq!(dag.roots.len(), queries);
        PhysicalASAPDAGDocument::new(dag).validate().unwrap();
    }

    let selection = &document["plan_selection"];
    let costs = selection["costs"].as_object().unwrap();
    let selected = selection["selected"].as_str().unwrap();
    let mut accounted = HashSet::from([selected]);
    for rejection in selection["rejected"].as_array().unwrap() {
        let id = rejection["id"].as_str().unwrap();
        assert!(accounted.insert(id), "{id} accounted for once");
        assert!(!rejection["reason"].as_str().unwrap().is_empty());
        assert_eq!(
            rejection["valid"].as_bool().unwrap(),
            costs.contains_key(id)
        );
    }
    assert!(costs.contains_key(selected));
    let all: HashSet<_> = physical.iter().map(|p| p["id"].as_str().unwrap()).collect();
    assert_eq!(accounted, all);
}

/// `--max-displayed` cuts only what the document shows, says so, and keeps
/// the selection made over all candidates.
#[test]
fn display_cap_is_recorded_and_keeps_the_selection() {
    let full = generate(&[]);
    let document = generate(&["--max-displayed", "1"]);
    let logical = &document["logical_asap_optimization"];
    assert_eq!(logical["capped"], true);
    assert!(logical["candidates"].as_array().unwrap().len() < 3);
    assert_eq!(logical["combinations"], 3);
    assert_eq!(
        document["plan_selection"]["selected"],
        full["plan_selection"]["selected"]
    );
}

/// A choice composition cannot substitute is listed with its error, and the
/// other candidates are still written.
#[test]
fn uncomposable_candidate_is_recorded_with_its_error() {
    let out =
        std::env::temp_dir().join(format!("stage_pipeline_{}_error.json", std::process::id()));
    let status = Command::new(env!("CARGO_BIN_EXE_stage_pipeline"))
        .args([
            "--promql",
            // Top-K partitioned by every label of its input has no item.
            "topk by (job) (3, sum by (job) (rate(m[5m])))",
            "--epsilon",
            "0.01",
            "--delta",
            "0.001",
            "--out",
        ])
        .arg(&out)
        .status()
        .unwrap();
    assert!(status.success());
    let document: Value = serde_json::from_str(&std::fs::read_to_string(&out).unwrap()).unwrap();
    std::fs::remove_file(out).unwrap();
    let candidates = document["logical_asap_optimization"]["candidates"]
        .as_array()
        .unwrap();
    let (composed, failed): (Vec<_>, Vec<_>) = candidates
        .iter()
        .partition(|candidate| candidate.get("dag").is_some());
    assert!(!composed.is_empty() && !failed.is_empty());
    for candidate in failed {
        assert!(candidate["error"].as_str().unwrap().contains("Top-K"));
    }
    for candidate in composed {
        validated_dag(&candidate["dag"], 1);
    }
}

/// Example 3a (243 candidates) selects what the planning stages select over
/// every candidate, which the acceptance tests also run: the document's
/// display cap does not change the selection, and the selected plan is
/// displayed with its cost.
#[test]
fn example3a_selects_the_cheapest_of_all_candidates() {
    let workload = pattern_a(PatternARecurrence::OnceAdHoc, DataArrival::Mixed);
    let run = run_planning_stages(&workload, lower_promql_roots(&workload).unwrap()).unwrap();
    assert_eq!(run.logical.len(), 243);
    let expected = run.selection.unwrap().selected;

    let document = generate_example("planner-layering-3a", &[]);
    let selection = &document["plan_selection"];
    assert_eq!(selection["selected"], expected.as_str());
    assert!(selection["costs"].get(&expected).is_some());
    let physical = document["physical_asap_optimization"]["candidates"]
        .as_array()
        .unwrap();
    assert!(physical.iter().any(|p| p["id"] == expected.as_str()));
}

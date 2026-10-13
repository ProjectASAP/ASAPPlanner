//! `stage_pipeline` writes a valid viewer document for #509 Example 1: the
//! language-specific frontends' DAG and the logical ASAP-aware optimization
//! candidates.
use std::collections::HashSet;
use std::path::PathBuf;
use std::process::Command;

use asap_types::ir::flat::FlatDag;
use asap_types::ir::QueryRoot;
use serde_json::Value;

const COMMITTED: &str = "../../tools/dag-viewer/examples/planner-layering-example1.json";

fn generate(extra: &[&str]) -> Value {
    let out = std::env::temp_dir().join(format!(
        "stage_pipeline_{}_{}.json",
        std::process::id(),
        extra.len()
    ));
    let status = Command::new(env!("CARGO_BIN_EXE_stage_pipeline"))
        .args(["--example", "planner-layering-1", "--out"])
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

/// Every DAG is a well-formed flat DAG with one root per query; ids are unique;
/// only the frontends and logical ASAP-aware optimization are present; the
/// committed fixture is current.
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
    assert!(document.get("physical_asap_optimization").is_none());
    assert!(document.get("plan_selection").is_none());
    validated_dag(&document["language_specific_frontends"]["dag"], queries);

    let logical = &document["logical_asap_optimization"];
    let candidates = logical["candidates"].as_array().unwrap();
    assert_eq!(logical["capped"], false);
    assert_eq!(logical["combinations"], candidates.len());
    let mut ids = HashSet::new();
    for candidate in candidates {
        assert!(ids.insert(candidate["id"].as_str().unwrap()));
        validated_dag(&candidate["dag"], queries);
    }
}

/// The Cartesian product is cut at `--max-candidates` and says so.
#[test]
fn candidate_cap_is_recorded() {
    let document = generate(&["--max-candidates", "2"]);
    let logical = &document["logical_asap_optimization"];
    assert_eq!(logical["capped"], true);
    assert_eq!(logical["candidates"].as_array().unwrap().len(), 2);
    assert!(logical["combinations"].as_u64().unwrap() > 2);
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

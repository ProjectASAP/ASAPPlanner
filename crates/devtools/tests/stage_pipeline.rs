//! `stage_pipeline` writes a valid Stage 0/1 viewer document for #509 Example 1.
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
/// only Stages 0 and 1 are present; the committed fixture is current.
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
    assert!(document.get("stage2_physical_asap").is_none());
    assert!(document.get("stage3_selection").is_none());
    validated_dag(&document["stage0_logical"]["dag"], queries);

    let stage1 = &document["stage1_logical_asap"];
    let candidates = stage1["candidates"].as_array().unwrap();
    assert_eq!(stage1["capped"], false);
    assert_eq!(stage1["combinations"], candidates.len());
    let mut ids = HashSet::new();
    for candidate in candidates {
        assert!(ids.insert(candidate["id"].as_str().unwrap()));
        validated_dag(&candidate["dag"], queries);
    }
}

/// The Cartesian product is cut at `--max-candidates` and says so.
#[test]
fn candidate_cap_is_recorded() {
    let document = generate(&["--max-candidates", "5"]);
    let stage1 = &document["stage1_logical_asap"];
    assert_eq!(stage1["capped"], true);
    assert_eq!(stage1["candidates"].as_array().unwrap().len(), 5);
    assert!(stage1["combinations"].as_u64().unwrap() > 5);
}

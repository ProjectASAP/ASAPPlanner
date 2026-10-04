//! `stage_pipeline` writes a valid four-stage viewer document for #509 Example 1.
use std::collections::HashSet;
use std::path::PathBuf;
use std::process::Command;

use asap_types::ir::export::{
    LogicalASAPDAG, LogicalASAPDAGDocument, PhysicalASAPDAG, PhysicalASAPDAGDocument,
};
use serde_json::Value;

const COMMITTED: &str = "../../tools/dag-viewer/examples/planner-layering-example1.json";

/// Every Example 1 candidate (88) is displayed; the default cap is 64.
const EXAMPLE1: [&str; 4] = ["--example", "planner-layering-1", "--max-candidates", "128"];

fn generate(args: &[&str]) -> Value {
    let out = std::env::temp_dir().join(format!(
        "stage_pipeline_{}_{}.json",
        std::process::id(),
        args.join("_")
    ));
    let status = Command::new(env!("CARGO_BIN_EXE_stage_pipeline"))
        .args(args)
        .arg("--out")
        .arg(&out)
        .status()
        .unwrap();
    assert!(status.success());
    let document = serde_json::from_str(&std::fs::read_to_string(&out).unwrap()).unwrap();
    std::fs::remove_file(out).unwrap();
    document
}

fn validated_dag(value: &Value, queries: usize) {
    let dag: LogicalASAPDAG = serde_json::from_value(value.clone()).unwrap();
    assert_eq!(dag.roots.len(), queries);
    LogicalASAPDAGDocument::new(dag).validate().unwrap();
}

/// Every exported DAG validates and has one root per query; ids are unique;
/// Stage 2 maps Stage 1 one-to-one with no cost; Stage 3 accounts for every
/// Stage 2 candidate once and costs exactly the valid ones; the committed
/// fixture is current.
#[test]
fn example1_document_is_valid_and_committed_fixture_is_current() {
    let document = generate(&EXAMPLE1);
    let committed: Value = serde_json::from_str(
        &std::fs::read_to_string(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(COMMITTED))
            .unwrap(),
    )
    .unwrap();
    assert_eq!(document, committed, "regenerate the committed fixture");

    assert_eq!(document["format"], "asap-stage-pipeline/v1");
    let queries = document["workload"]["queries"].as_array().unwrap().len();
    assert_eq!(queries, 2);
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

    let physical = document["stage2_physical_asap"]["candidates"]
        .as_array()
        .unwrap();
    let sources: HashSet<_> = physical
        .iter()
        .map(|p| p["from_logical"].as_str().unwrap())
        .collect();
    assert_eq!(physical.len(), candidates.len());
    assert_eq!(sources, ids);
    for candidate in physical {
        assert!(candidate.get("cost").is_none(), "Stage 2 has no cost");
        let dag: PhysicalASAPDAG = serde_json::from_value(candidate["dag"].clone()).unwrap();
        assert_eq!(dag.roots.len(), queries);
        PhysicalASAPDAGDocument::new(dag).validate().unwrap();
    }

    let stage3 = &document["stage3_selection"];
    let costs = stage3["costs"].as_object().unwrap();
    let selected = stage3["selected"].as_str().unwrap();
    let mut accounted = HashSet::from([selected]);
    for rejection in stage3["rejected"].as_array().unwrap() {
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

/// The Cartesian product is cut at `--max-candidates` and says so.
#[test]
fn candidate_cap_is_recorded() {
    let document = generate(&["--example", "planner-layering-1", "--max-candidates", "5"]);
    let stage1 = &document["stage1_logical_asap"];
    assert_eq!(stage1["capped"], true);
    assert_eq!(stage1["candidates"].as_array().unwrap().len(), 5);
    assert!(stage1["combinations"].as_u64().unwrap() > 5);
}

/// Example 3, Pattern B: the 5-min p99 every minute offers KLL and DDSketch
/// over the whole window and in 1-min tumbling panes, each merged before its
/// estimate.
#[test]
fn example3b_lists_tumbling_candidates() {
    let document = generate(&["--example", "planner-layering-3b"]);
    let stage1 = &document["stage1_logical_asap"];
    let labels: Vec<_> = stage1["candidates"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["label"].as_str().unwrap())
        .collect();
    assert_eq!(
        labels,
        [
            "Q1 exact",
            "Q1 Kll",
            "Q1 DDSketch",
            "Q1 Kll · tumbling 1m panes",
            "Q1 DDSketch · tumbling 1m panes"
        ]
    );
    for candidate in &stage1["candidates"].as_array().unwrap()[3..] {
        let dag: LogicalASAPDAG = serde_json::from_value(candidate["dag"].clone()).unwrap();
        let merges = dag
            .nodes
            .iter()
            .filter(|n| {
                matches!(
                    n.payload,
                    asap_types::ir::export::LogicalASAPOperatorPayload::SummaryMerge
                )
            })
            .count();
        assert_eq!(merges, 1, "{}", candidate["label"]);
    }
}

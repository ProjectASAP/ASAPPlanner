//! `stage_pipeline` writes valid four-stage viewer documents for #509's examples.
use std::collections::HashSet;
use std::path::PathBuf;
use std::process::Command;

use asap_types::ir::export::{
    LogicalASAPDAG, LogicalASAPDAGDocument, PhysicalASAPDAG, PhysicalASAPDAGDocument,
};
use serde_json::Value;

const COMMITTED: &str = "../../tools/dag-viewer/examples/planner-layering-example1.json";

/// Every Example 1 logical candidate (88) is displayed; the default cap is 64.
const EXAMPLE1: [&str; 4] = ["--example", "planner-layering-1", "--max-candidates", "128"];

fn generate(args: &[&str]) -> Value {
    // Tests run in parallel and may generate the same document.
    static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    let out = std::env::temp_dir().join(format!(
        "stage_pipeline_{}_{}.json",
        std::process::id(),
        NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
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
/// Stage 2 covers every Stage 1 candidate, with no cost; Stage 3 accounts for every
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
    // One all-query-time candidate per logical one, plus one with Q2's sum
    // panes at ingestion time for each of the 24 with panes.
    assert_eq!(physical.len(), candidates.len() + 24);
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

/// `--max-candidates` limits the plans written, cheapest first, and says
/// so; Stage 3 still prices every plan, so the selection does not change.
#[test]
fn candidate_cap_is_recorded() {
    let document = generate(&["--example", "planner-layering-1", "--max-candidates", "5"]);
    let full = generate(&EXAMPLE1);
    let stage1 = &document["stage1_logical_asap"];
    assert_eq!(stage1["capped"], true);
    assert!(stage1["candidates"].as_array().unwrap().len() <= 5);
    assert!(stage1["combinations"].as_u64().unwrap() > 5);
    let physical = document["stage2_physical_asap"]["candidates"]
        .as_array()
        .unwrap();
    assert_eq!(physical.len(), 5);
    let selection = &document["stage3_selection"];
    assert_eq!(selection["selected"], full["stage3_selection"]["selected"]);
    let costs = selection["costs"].as_object().unwrap();
    assert_eq!(costs.len(), 5, "the five cheapest plans are written");
    let full_physical = full["stage2_physical_asap"]["candidates"]
        .as_array()
        .unwrap();
    assert_eq!(document["shown_of"]["physical"], full_physical.len());
    assert!(full.get("shown_of").is_none());
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

/// Example 4, Pattern A repeated monthly: the same 486 candidates as the ad
/// hoc batch (3a), and none maintained at ingestion time. The windows (1–5 y)
/// are longer than the month between runs and no pane width fits, so
/// nothing is maintainable. The selected plan is 3a's, now amortized over
/// monthly runs instead of one run per hour of horizon.
#[test]
fn example4a_repeats_monthly_with_nothing_maintainable() {
    let once = generate(&[
        "--example",
        "planner-layering-3a",
        "--max-candidates",
        "600",
    ]);
    let monthly = generate(&[
        "--example",
        "planner-layering-4a",
        "--max-candidates",
        "600",
    ]);
    let physical = monthly["stage2_physical_asap"]["candidates"]
        .as_array()
        .unwrap();
    assert_eq!(physical.len(), 486);
    assert!(physical
        .iter()
        .all(|p| !p["label"].as_str().unwrap().contains("ingestion time")));
    let selected = |d: &Value| {
        d["stage3_selection"]["selected"]
            .as_str()
            .unwrap()
            .to_string()
    };
    assert_eq!(selected(&monthly), selected(&once));
    let cost = |d: &Value| {
        d["stage3_selection"]["costs"][selected(d)]["total"]
            .as_f64()
            .unwrap()
    };
    assert!(cost(&monthly) < cost(&once));
}

/// Every exported DAG in `document` validates with one root per query, and
/// the selected candidate is a Stage 2 candidate with a cost.
fn assert_valid_document(document: &Value, queries: usize) {
    assert_eq!(document["format"], "asap-stage-pipeline/v1");
    assert_eq!(
        document["workload"]["queries"].as_array().unwrap().len(),
        queries
    );
    validated_dag(&document["stage0_logical"]["dag"], queries);
    for candidate in document["stage1_logical_asap"]["candidates"]
        .as_array()
        .unwrap()
    {
        validated_dag(&candidate["dag"], queries);
    }
    let physical = document["stage2_physical_asap"]["candidates"]
        .as_array()
        .unwrap();
    for candidate in physical {
        let dag: PhysicalASAPDAG = serde_json::from_value(candidate["dag"].clone()).unwrap();
        assert_eq!(dag.roots.len(), queries);
        PhysicalASAPDAGDocument::new(dag).validate().unwrap();
    }
    let stage3 = &document["stage3_selection"];
    let selected = stage3["selected"].as_str().unwrap();
    assert!(physical.iter().any(|p| p["id"] == selected));
    assert!(stage3["costs"][selected]["total"].as_f64().is_some());
}

/// The selected Stage 2 candidate's label.
fn selected_label(document: &Value) -> String {
    let selected = &document["stage3_selection"]["selected"];
    document["stage2_physical_asap"]["candidates"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| &p["id"] == selected)
        .unwrap()["label"]
        .as_str()
        .unwrap()
        .to_string()
}

/// Example 2: the three SQL statistics over `flows` lower through the SQL
/// frontend, are reported as SQL, and plan. The built-in models have no
/// UnivMon accuracy model, so the selected plan is the exact one with the
/// shared input (recorded, not required by the design).
#[test]
fn example2_plans_the_sql_workload() {
    let document = generate(&["--example", "planner-layering-2", "--max-candidates", "200"]);
    assert_valid_document(&document, 3);
    assert!(document["workload"]["queries"]
        .as_array()
        .unwrap()
        .iter()
        .all(|q| q["language"] == "sql"));
    assert_eq!(document["stage1_logical_asap"]["capped"], false);
    assert_eq!(
        selected_label(&document),
        "Q1 exact · Q2 exact (Count acc) · Q3 exact (Count acc) · shared summary"
    );
}

/// Example 4b (Q49): on a deployment that does not keep raw data, the hourly
/// p99 every 10 min over 1,000 series sampled every second selects the
/// tumbling KLL maintained at ingestion time (B1), and the document says the
/// deployment keeps no raw data.
#[test]
fn example4b_selects_ingestion_time_panes_without_raw_data() {
    let document = generate(&["--example", "planner-layering-4b"]);
    assert_valid_document(&document, 1);
    assert_eq!(
        document["deployment"]["capabilities"]["raw_data_retained"],
        false
    );
    let label = selected_label(&document);
    assert!(label.contains("ingestion time"), "{label}");
    assert!(label.contains("tumbling 10m panes"), "{label}");
}

/// The document records the deployment inputs Stage 3 used: the executor's
/// capabilities (Hydra summaries named by their kind), the cost model with
/// its calibration, including the memory weight and raw-retention settings,
/// and the accuracy model's name.
#[test]
fn document_records_the_deployment_inputs() {
    let document = generate(&EXAMPLE1);
    let deployment = &document["deployment"];
    let capabilities = &deployment["capabilities"];
    assert_eq!(capabilities["source"], "asap_executor::capabilities");
    assert_eq!(capabilities["ingestion_time"], true);
    assert_eq!(capabilities["raw_data_retained"], true);
    assert_eq!(capabilities["raw_bytes_per_sample"], 16);
    assert!(capabilities["memory_budget_bytes"].is_null());
    let summaries = capabilities["summaries"].as_array().unwrap();
    let hydra = summaries
        .iter()
        .find(|s| s["summary"] == "HydraCms")
        .expect("HydraCms is listed");
    assert_eq!(
        hydra["readouts"],
        serde_json::json!(["TotalCount", "ItemCount"])
    );
    let calibration = &deployment["cost_model"]["calibration"];
    assert_eq!(deployment["cost_model"]["name"], "analytical-cost-v2");
    assert_eq!(calibration["cost_per_retained_byte_second"], 1.25e-7);
    assert_eq!(calibration["version"], "illustrative-v2");
    assert_eq!(deployment["accuracy_model"]["name"], "DefaultAccuracyModel");
}

const FLOWS: &str = r#"{"name": "flows", "columns": [{"name": "ts", "type": "timestamp", "nullable": false}, {"name": "src_ip", "type": "utf8", "nullable": false}], "time_index": 0}"#;

/// A `--sql` query over a `--table` lowers through the SQL frontend, plans,
/// and is reported as SQL with its accuracy target.
#[test]
fn sql_over_declared_tables_plans() {
    let document = generate(&[
        "--table",
        FLOWS,
        "--sql",
        "SELECT COUNT(DISTINCT src_ip) FROM flows",
        "--epsilon",
        "0.02",
    ]);
    assert_valid_document(&document, 1);
    let query = &document["workload"]["queries"][0];
    assert_eq!(query["language"], "sql");
    assert_eq!(query["requirements"]["accuracy"]["epsilon"], 0.02);
}

/// The tool's error message for `args`, which must fail.
fn failure(args: &[&str]) -> String {
    let output = Command::new(env!("CARGO_BIN_EXE_stage_pipeline"))
        .args(args)
        .args(["--out", "unused.json"])
        .output()
        .unwrap();
    assert!(!output.status.success(), "{args:?} should fail");
    String::from_utf8(output.stderr).unwrap()
}

/// A run is one language, `--table` needs `--sql`, and a malformed table
/// or a query over an undeclared table is an error, not a panic.
#[test]
fn bad_sql_arguments_are_rejected() {
    let query = "SELECT COUNT(*) FROM flows";
    let error = failure(&["--promql", "up", "--table", FLOWS, "--sql", query]);
    assert!(error.contains("one language"), "{error}");
    let error = failure(&["--table", FLOWS, "--promql", "up"]);
    assert!(error.contains("--table needs --sql"), "{error}");
    for table in [
        "{not json",
        r#"{"columns": [{"name": "ts", "type": "timestamp"}]}"#,
        r#"{"name": "flows", "columns": [{"name": "ts", "type": "decimal"}]}"#,
        r#"{"name": "flows", "columns": [{"name": "ts", "type": "timestamp"}], "time_index": 1}"#,
    ] {
        let error = failure(&["--table", table, "--sql", query]);
        assert!(error.contains("--table"), "{table}: {error}");
    }
    let error = failure(&["--table", FLOWS, "--table", FLOWS, "--sql", query]);
    assert!(error.contains("declared twice"), "{error}");
    let error = failure(&["--sql", query]);
    assert!(error.contains("lowering"), "{error}");
}

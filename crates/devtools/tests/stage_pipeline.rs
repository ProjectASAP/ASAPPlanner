//! `stage_pipeline` writes valid four-stage viewer documents for #509's examples.
use std::collections::HashSet;
use std::path::PathBuf;
use std::process::Command;

use asap_devtools::planner_layering_examples::{
    example1, pattern_a, PatternARecurrence, SHARED_ROWS_PER_SECOND,
};
use asap_devtools::planning_stages::{
    lower_promql_roots, run_planning_stages, PlanningStagesError,
};
use asap_plan_selection::RejectionCause;
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
    // Tests run as threads of one process, so the name needs a per-call counter.
    static CALLS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    let call = CALLS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let out = std::env::temp_dir().join(format!(
        "stage_pipeline_{}_{call}_{example}.json",
        std::process::id(),
    ));
    let status = Command::new(env!("CARGO_BIN_EXE_stage_pipeline"))
        .args(["--example", example, "--exhaustive", "--out"])
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
    validated_dag(&document["stage0_query_language_frontends"]["dag"], queries);

    let logical = &document["stage1_logical_asap_optimization"];
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

    let physical = document["stage2_physical_asap_optimization"]["candidates"]
        .as_array()
        .unwrap();
    let pruned = document["stage2_physical_asap_optimization"]["pruned"]
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
    // One physical candidate with nothing materialized per composed
    // logical candidate, `P<n>`, plus `P<n>-m<k>` for each other
    // materialization assignment.
    let unmaterialized = physical
        .iter()
        .filter(|p| !p["id"].as_str().unwrap().contains("-m"))
        .count();
    assert_eq!(unmaterialized + pruned.len(), composed.len());
    assert_eq!(sources, composed);
    for candidate in physical {
        assert!(candidate.get("cost").is_none(), "Stage 2 has no cost");
        let dag: PhysicalASAPDAG = serde_json::from_value(candidate["dag"].clone()).unwrap();
        assert_eq!(dag.roots.len(), queries);
        PhysicalASAPDAGDocument::new(dag).validate().unwrap();
    }

    let selection = &document["stage3_plan_selection"];
    let costs = selection["costs"].as_object().unwrap();
    let selected = selection["selected"].as_str().unwrap();
    let mut accounted = HashSet::from([selected]);
    for rejection in selection["rejected"].as_array().unwrap() {
        let id = rejection["id"].as_str().unwrap();
        assert!(accounted.insert(id), "{id} accounted for once");
        assert!(!rejection["reason"].as_str().unwrap().is_empty());
        // A candidate that failed a check is not priced; one that lost
        // under the cost model is.
        let cause: RejectionCause = serde_json::from_value(rejection["cause"].clone()).unwrap();
        let failed_a_check = cause.failed_a_check();
        assert_eq!(failed_a_check, !costs.contains_key(id), "{rejection}");
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
    let logical = &document["stage1_logical_asap_optimization"];
    assert_eq!(logical["capped"], true);
    assert!(logical["candidates"].as_array().unwrap().len() < 27);
    // 3 Pass 1 candidates, each kept as it is and with 8 Pass 2 candidates
    // (shared inputs, tumbling panes, and both): 3 × 9.
    assert_eq!(logical["combinations"], 27);
    assert_eq!(
        document["stage3_plan_selection"]["selected"],
        full["stage3_plan_selection"]["selected"]
    );
}

/// A top-k partitioned by every label of its input has no item, so a heap
/// over it could not be composed; such a heap reads one row per item and
/// is pruned in Pass 1 (`TopKHeapOverOneRowPerItem`), listed under
/// `pass1_pruned`, and every candidate left composes.
#[test]
fn a_topk_heap_over_one_row_per_item_is_pruned_with_its_reason() {
    let document = document_of(&[
        "--promql",
        "topk by (job) (3, sum by (job) (rate(m[5m])))",
        "--max-error",
        "0.01",
        "--failure-probability",
        "0.001",
    ]);
    let stage1 = &document["stage1_logical_asap_optimization"];
    assert_eq!(
        stage1["pass1_pruned"],
        serde_json::json!([{
            "target": 0,
            "query": "Q1",
            "pruned": [
                { "candidate": "CMS+heap", "reason": "TopKHeapOverOneRowPerItem" },
                { "candidate": "CountSketch+heap", "reason": "TopKHeapOverOneRowPerItem" },
            ],
            "fused_pruned": [],
        }])
    );
    for candidate in stage1["candidates"].as_array().unwrap() {
        validated_dag(&candidate["dag"], 1);
    }
}

/// Example 3a (243 Pass 1 candidates, each with 3 Pass 2 candidates)
/// selects what the planning stages select over
/// every candidate, which the acceptance tests also run: the document's
/// display cap does not change the selection, and the selected plan is
/// displayed with its cost.
#[test]
fn example3a_selects_the_cheapest_of_all_candidates() {
    let workload = pattern_a(
        PatternARecurrence::OnceAdHoc,
        DataArrival::Mixed {
            rows_per_second: SHARED_ROWS_PER_SECOND,
        },
    );
    let run = run_planning_stages(&workload, lower_promql_roots(&workload).unwrap(), None).unwrap();
    assert_eq!(run.stage1_logical_asap_optimization.len(), 972);
    let expected = run.stage3_plan_selection.unwrap().selected;

    let document = generate_example("planner-layering-3a", &[]);
    let selection = &document["stage3_plan_selection"];
    assert_eq!(selection["selected"], expected.as_str());
    assert!(selection["costs"].get(&expected).is_some());
    let physical = document["stage2_physical_asap_optimization"]["candidates"]
        .as_array()
        .unwrap();
    assert!(physical.iter().any(|p| p["id"] == expected.as_str()));
}

/// Example 2 plans its three SQL queries: Stage 1 lists a candidate in
/// which one UnivMon build serves all three, and plan selection selects a
/// plan.
#[test]
fn example2_plans_the_sql_workload_with_one_univmon_for_all_three() {
    let document = generate_example("planner-layering-2", &["--max-displayed", "100000"]);
    let queries = document["workload"]["queries"].as_array().unwrap();
    assert_eq!(queries.len(), 3);
    assert!(queries.iter().all(|q| q["language"] == "sql"));
    validated_dag(&document["stage0_query_language_frontends"]["dag"], 3);
    let univmon_builds = |candidate: &Value| {
        candidate["dag"]["nodes"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|n| {
                n["operator"]["ASAP"]["SummaryAgg"]["family"]["Sketch"][0]["algorithm"] == "UnivMon"
            })
            .count()
    };
    let candidates = document["stage1_logical_asap_optimization"]["candidates"]
        .as_array()
        .unwrap();
    assert!(
        candidates.iter().any(|c| {
            c["display_name"]
                .as_str()
                .unwrap()
                .starts_with("Q1 UnivMon · Q2 UnivMon (Count acc) · Q3 UnivMon (Count acc)")
                && univmon_builds(c) == 1
        }),
        "no candidate reads one UnivMon for all three"
    );
    assert!(document["stage3_plan_selection"]["selected"].is_string());
}

/// SQL queries over tables declared with `--table` are planned through the
/// same stages, and the document records each query's language.
#[test]
fn sql_queries_over_declared_tables_are_planned() {
    let out = std::env::temp_dir().join(format!("stage_pipeline_{}_sql.json", std::process::id()));
    let status = Command::new(env!("CARGO_BIN_EXE_stage_pipeline"))
        .args([
            "--table",
            r#"{"name": "metrics", "columns": [{"name": "ts", "type": "timestamp", "nullable": false},
                {"name": "service", "type": "utf8", "nullable": false},
                {"name": "latency", "type": "float64", "nullable": false}], "time_index": 0}"#,
            "--sql",
            "SELECT service, COUNT(*) FROM metrics GROUP BY service",
            "--sql",
            "SELECT service, AVG(latency) FROM metrics GROUP BY service",
            "--out",
        ])
        .arg(&out)
        .status()
        .unwrap();
    assert!(status.success());
    let document: Value = serde_json::from_str(&std::fs::read_to_string(&out).unwrap()).unwrap();
    std::fs::remove_file(out).unwrap();
    let queries = document["workload"]["queries"].as_array().unwrap();
    assert_eq!(queries.len(), 2);
    assert!(queries.iter().all(|q| q["language"] == "sql"));
    validated_dag(&document["stage0_query_language_frontends"]["dag"], 2);
    assert!(document["stage3_plan_selection"]["selected"].is_string());
}

/// One run mixes SQL (over declared tables) and PromQL queries, in the
/// order given, each named with `--name`.
#[test]
fn mixed_sql_and_promql_queries_are_planned_with_their_names() {
    let out =
        std::env::temp_dir().join(format!("stage_pipeline_{}_mixed.json", std::process::id()));
    let status = Command::new(env!("CARGO_BIN_EXE_stage_pipeline"))
        .args([
            "--table",
            r#"{"name": "metrics", "columns": [{"name": "ts", "type": "timestamp", "nullable": false},
                {"name": "service", "type": "utf8", "nullable": false}], "time_index": 0}"#,
            "--sql",
            "SELECT service, COUNT(*) FROM metrics GROUP BY service",
            "--name",
            "count_by_service",
            "--promql",
            "topk(5, rate(http_requests_total[5m]))",
            "--max-error",
            "0.01",
            "--out",
        ])
        .arg(&out)
        .status()
        .unwrap();
    assert!(status.success());
    let document: Value = serde_json::from_str(&std::fs::read_to_string(&out).unwrap()).unwrap();
    std::fs::remove_file(out).unwrap();
    let queries = document["workload"]["queries"].as_array().unwrap();
    let described: Vec<_> = queries
        .iter()
        .map(|q| (q["id"].as_str().unwrap(), q["language"].as_str().unwrap()))
        .collect();
    assert_eq!(described, [("count_by_service", "sql"), ("q2", "promql")]);
    validated_dag(&document["stage0_query_language_frontends"]["dag"], 2);
    assert!(document["stage3_plan_selection"]["selected"].is_string());
}

/// Runs `stage_pipeline` with `args` (and a throwaway `--out`), expecting it
/// to fail: its exit status and stderr.
fn failure(args: &[&str]) -> (Option<i32>, String) {
    let out = std::env::temp_dir().join(format!(
        "stage_pipeline_{}_failure.json",
        std::process::id()
    ));
    let output = Command::new(env!("CARGO_BIN_EXE_stage_pipeline"))
        .args(args)
        .arg("--out")
        .arg(&out)
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(!out.exists(), "a rejected run writes no document");
    (
        output.status.code(),
        String::from_utf8(output.stderr).unwrap(),
    )
}

/// `--max-combinations` limits the Stage 1 candidates, Pass 2's included:
/// Example 1 has 9 (3 Pass 1 candidates, each with 2 Pass 2 candidates).
/// Above the limit the run stops with an error as soon as the count passes
/// it, rather than truncating.
#[test]
fn workload_above_the_candidate_limit_is_rejected() {
    for limit in ["3", "8"] {
        let (status, stderr) = failure(&[
            "--exhaustive",
            "--example",
            "planner-layering-1",
            "--max-combinations",
            limit,
        ]);
        assert_eq!(status, Some(3), "{limit}");
        assert!(
            stderr.contains(&format!(
                "workload has more than {limit} Stage 1 candidates, Pass 2's included; \
                 the exhaustive search refuses it; --exhaustive composes every candidate"
            )),
            "{stderr}"
        );
    }
    // At Stage 1's 27 candidates, Stage 2's 57 physical candidates (30 maintain
    // pane chains at ingestion time) pass the limit; they are counted, not built.
    let (status, stderr) = failure(&[
        "--exhaustive",
        "--example",
        "planner-layering-1",
        "--max-combinations",
        "27",
    ]);
    assert_eq!(status, Some(3));
    assert!(
        stderr.contains(
            "workload has more than 27 Stage 2 physical candidates (materialization options \
             included); the exhaustive search refuses it"
        ),
        "{stderr}"
    );
    // At the limit, it plans every candidate.
    let document = generate(&["--max-combinations", "57"]);
    assert_eq!(
        document["stage1_logical_asap_optimization"]["combinations"],
        27
    );
    assert_eq!(
        document["stage2_physical_asap_optimization"]["candidates"]
            .as_array()
            .unwrap()
            .len(),
        57
    );
}

/// Nine approximate quantile queries over nine metrics, three candidates
/// each: 3^9 = 19683 Pass 1 combinations.
fn nine_quantile_queries() -> Vec<String> {
    let mut args = Vec::new();
    for i in 1..=9 {
        args.extend([
            "--promql".to_string(),
            format!("quantile_over_time(0.99, m{i}[5m])"),
        ]);
    }
    args.extend(["--max-error", "0.01", "--failure-probability", "0.001"].map(String::from));
    args
}

/// The exhaustive search's default limit is 10^4: the nine quantile
/// queries' 19683 combinations are rejected once the count passes it, and
/// the message names the exhaustive search.
#[test]
fn exhaustive_default_candidate_limit_is_ten_thousand() {
    let mut args = nine_quantile_queries();
    args.push("--exhaustive".into());
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    let (status, stderr) = failure(&args);
    assert_eq!(status, Some(3));
    assert!(
        stderr.contains("workload has more than 10000 Stage 1 candidates"),
        "{stderr}"
    );
    assert!(stderr.contains("--exhaustive"), "{stderr}");
}

/// Without `--exhaustive`, the same nine queries plan with the default
/// search, `plan_installable`'s: each query, reading its own metric, is an
/// independent component searched on its own, so no limit is reached. The
/// document holds the selected plan and the search's report.
#[test]
fn default_search_plans_the_nine_queries_the_exhaustive_search_refuses() {
    let args = nine_quantile_queries();
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    let document = document_of(&args);
    assert_eq!(document["search"], "default");
    let selection = &document["stage3_plan_selection"];
    let selected = selection["selected"].as_str().unwrap();
    let physical = document["stage2_physical_asap_optimization"]["candidates"]
        .as_array()
        .unwrap();
    assert_eq!(physical.len(), 1);
    assert_eq!(physical[0]["id"], selected);
    assert_eq!(
        selection["search"]["components"].as_array().unwrap().len(),
        9
    );
    assert_eq!(selection["search"]["queries"].as_array().unwrap().len(), 9);
}

/// `--max-combinations` limits only the exhaustive search.
#[test]
fn max_combinations_needs_exhaustive() {
    let (status, stderr) = failure(&["--promql", "sum(m)", "--max-combinations", "10"]);
    assert_eq!(status, Some(2));
    assert!(stderr.contains("needs --exhaustive"), "{stderr}");
}

/// A 9-query PromQL workload converted from ASAPQuery's random workloads
/// (seed 862, workload-01), planned as a deployment that keeps no raw data:
/// the default search plans it (the exhaustive search would enumerate more
/// than 10^5 Stage 1 candidates) and selects what `plan_installable`
/// selects with the same settings.
#[test]
fn default_search_plans_a_nine_query_workload_as_plan_installable_does() {
    const WORKLOAD: &str =
        "../../tools/dag-viewer/workloads/random-mixed-seed-862/workload-01.json";
    const NO_RAW: &str = r#"{"raw_data_retained": false, "query_time_results_retained": false}"#;
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(WORKLOAD);
    let document = document_of(&[
        "--workload",
        path.to_str().unwrap(),
        "--deployment-capabilities",
        NO_RAW,
        "--cost-model",
        "aws-fargate-us-east-1",
    ]);
    assert_eq!(document["search"], "default");
    let queries = document["workload"]["queries"].as_array().unwrap().len();
    assert_eq!(queries, 9);
    validated_dag(&document["stage0_query_language_frontends"]["dag"], queries);
    let selection = &document["stage3_plan_selection"];
    let selected = selection["selected"].as_str().unwrap();
    let physical = document["stage2_physical_asap_optimization"]["candidates"]
        .as_array()
        .unwrap();
    assert_eq!(physical.len(), 1);
    assert_eq!(physical[0]["id"], selected);
    let dag: PhysicalASAPDAG = serde_json::from_value(physical[0]["dag"].clone()).unwrap();
    assert_eq!(dag.roots.len(), queries);
    assert!(selection["search"]["candidates_checked"].as_u64().unwrap() > 0);

    let text = std::fs::read_to_string(&path).unwrap();
    let workload: asap_types::workload::PlanningWorkload = serde_json::from_str(&text).unwrap();
    let defaults = asap_plan_selection::PlanSelectionSettings::default();
    let settings = asap_plan_selection::PlanSelectionSettings {
        cost_model: asap_plan_selection::CostModel::UsageOverTime(
            asap_plan_selection::ResourceUnitCost::aws_fargate_us_east_1(defaults.resource_speed),
        ),
        ..defaults
    };
    let capabilities: asap_planner::DeploymentCapabilities = serde_json::from_str(NO_RAW).unwrap();
    let models = asap_plan_selection::PlanningModels::builtin()
        .with_plan_selection(&settings)
        .with_deployment_capabilities(&capabilities);
    let plan = tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap()
        .block_on(asap_planner::plan_installable(
            asap_planner::UserInput::new(
                &workload,
                asap_planner::FrontendInput::Promql { histograms: None },
                models,
            ),
        ))
        .unwrap();
    assert_eq!(plan.selection.unwrap().selected_candidate_id, selected);
}

/// The library never caps: without a limit every candidate is planned.
#[test]
fn planning_stages_without_a_limit_plan_every_candidate() {
    let workload = example1();
    let run = run_planning_stages(&workload, lower_promql_roots(&workload).unwrap(), None).unwrap();
    assert_eq!(run.stage1_logical_asap_optimization.len(), 27);
    assert_eq!(run.stage2_physical_asap_optimization.len(), 57);
    let limited = run_planning_stages(&workload, lower_promql_roots(&workload).unwrap(), Some(26));
    assert!(matches!(
        limited,
        Err(PlanningStagesError::TooManyCandidates { limit: 26 })
    ));
}

/// `--sample-interval-ms` is the sample interval of every metric the
/// queries read, as in the other lowering tools: `sum(up)`'s bare selector
/// looks back 30 s. The former `--interval-ms` is unknown.
#[test]
fn sample_interval_ms_sets_the_instant_selector_lookback() {
    let document = document_of(&["--promql", "sum(up)", "--sample-interval-ms", "30000"]);
    let ranges: Vec<&Value> = document["stage0_query_language_frontends"]["dag"]["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|node| node["operator"]["NonASAP"]["TimeRange"]["range"].as_object())
        .map(|range| &range["secs"])
        .collect();
    assert_eq!(ranges, [&Value::from(30)]);
    let (status, stderr) = failure(&["--promql", "sum(up)", "--interval-ms", "30000"]);
    assert_eq!(status, Some(2));
    assert!(
        stderr.contains("unknown argument --interval-ms"),
        "{stderr}"
    );
}

#[test]
fn table_declared_twice_is_rejected() {
    let table =
        r#"{"name": "metrics", "columns": [{"name": "ts", "type": "timestamp"}], "time_index": 0}"#;
    let (status, stderr) = failure(&[
        "--table",
        table,
        "--table",
        table,
        "--sql",
        "SELECT COUNT(*) FROM metrics",
    ]);
    assert_eq!(status, Some(2));
    assert!(
        stderr.contains("--table: table metrics is declared twice"),
        "{stderr}"
    );
}

/// A defaulted name that collides says it is the default.
#[test]
fn duplicate_default_name_says_it_is_the_default() {
    let (_, stderr) = failure(&["--promql", "sum(a)", "--name", "q2", "--promql", "sum(b)"]);
    assert!(
        stderr.contains("duplicate query name q2 (the default name of query 2)"),
        "{stderr}"
    );
    let (_, stderr) = failure(&["--promql", "sum(a)", "--promql", "sum(b)", "--name", "q1"]);
    assert!(stderr.contains("duplicate query name q1\n"), "{stderr}");
}

/// Runs `stage_pipeline` with `args`, expecting a document.
fn document_of(args: &[&str]) -> Value {
    static CALLS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    let call = CALLS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let out = std::env::temp_dir().join(format!(
        "stage_pipeline_{}_{call}_document.json",
        std::process::id()
    ));
    let output = Command::new(env!("CARGO_BIN_EXE_stage_pipeline"))
        .args(args)
        .arg("--out")
        .arg(&out)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let document = serde_json::from_str(&std::fs::read_to_string(&out).unwrap()).unwrap();
    std::fs::remove_file(out).unwrap();
    document
}

/// When Pass 1 fails, the document still carries the Stage 0 DAG, with the
/// error under Stage 1 and no selection, so the viewer can show both.
#[test]
fn pass1_failure_still_writes_stage0_and_the_error() {
    let document = document_of(&[
        "--promql",
        "topk(5, rate(m[5m]))",
        "--max-error",
        "1.5",
        "--failure-probability",
        "0.1",
    ]);
    validated_dag(&document["stage0_query_language_frontends"]["dag"], 1);
    let logical = &document["stage1_logical_asap_optimization"];
    assert!(
        logical["error"]
            .as_str()
            .unwrap()
            .starts_with("Pass 1: approximate accuracy"),
        "{logical}"
    );
    assert_eq!(logical["combinations"], 0);
    assert_eq!(logical["candidates"], serde_json::json!([]));
    assert_eq!(
        document["stage2_physical_asap_optimization"],
        serde_json::json!({ "candidates": [], "pruned": [] })
    );
    assert!(document["stage3_plan_selection"]["selected"].is_null());
}

/// Standalone scalar queries first, in the middle and last, which the
/// physical DAG keeps in `scalar_roots` by query position (the viewer's
/// test_viewer.py draws this workload).
const SCALAR_QUERIES_ARGS: &[&str] = &[
    "--promql",
    "scalar(sum(m))",
    "--promql",
    "sum(n)",
    "--promql",
    "time()",
    "--promql",
    "sum by (job) (k)",
    "--promql",
    "1 + 2",
];

#[test]
fn scalar_queries_keep_their_positions_in_scalar_roots() {
    let document = document_of(SCALAR_QUERIES_ARGS);
    let selected = document["stage3_plan_selection"]["selected"]
        .as_str()
        .unwrap();
    let physical = document["stage2_physical_asap_optimization"]["candidates"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["id"] == selected)
        .unwrap();
    let positions: Vec<_> = physical["dag"]["scalar_roots"]
        .as_array()
        .unwrap()
        .iter()
        .map(|entry| entry[0].as_u64().unwrap())
        .collect();
    assert_eq!(positions, [0, 2, 4]);
}

/// `time_index` must be a column index; anything else is an error rather
/// than a table without a time column.
#[test]
fn table_time_index_must_be_a_column_index() {
    for index in [r#""0""#, "-1", "1.5", "3"] {
        let table = format!(
            r#"{{"name": "metrics", "columns": [{{"name": "ts", "type": "timestamp"}}], "time_index": {index}}}"#
        );
        let (status, stderr) =
            failure(&["--table", &table, "--sql", "SELECT COUNT(*) FROM metrics"]);
        assert_eq!(status, Some(2), "{index}");
        assert!(
            stderr
                .contains("--table metrics: time_index must be the index of one of its 1 columns"),
            "{index}: {stderr}"
        );
    }
}

/// A query whose target sets no metric shows the metric its result is
/// naturally stated in (the default-metric table, display only): a top-k's
/// is the L1-scaled absolute error; an exact query shows none.
#[test]
fn a_target_without_a_metric_shows_the_query_default() {
    let document = generate(&[]);
    let queries = &document["workload"]["queries"];
    assert!(queries[0]["requirements"].get("default_metric").is_none());
    assert_eq!(
        queries[1]["requirements"]["default_metric"],
        "absolute_stream_l1"
    );
}

/// A quantile query's target may leave its metric unset: then KLL (rank)
/// and DDSketch (relative) candidates both meet it and compete on cost, and
/// the document reports, per valid candidate, the metric each was accepted
/// in and that the target's metric was not set. With `--accuracy-metric rank` only
/// rank guarantees (and exact plans) are accepted, and the report says so. The workload's
/// accuracy is the target's own JSON form.
#[test]
fn a_quantile_target_without_a_metric_accepts_rank_or_relative() {
    let query = [
        "--exhaustive",
        "--promql",
        "quantile_over_time(0.99, m[1m])",
        "--max-error",
        "0.01",
    ];
    let accepted = |document: &Value| -> Vec<String> {
        let selection = &document["stage3_plan_selection"];
        assert!(selection["selected"].is_string(), "{selection}");
        selection["accepted"]
            .as_object()
            .unwrap()
            .values()
            .map(|queries| queries[0]["summary"].as_str().unwrap().to_owned())
            .collect()
    };
    let unset = document_of(&query);
    assert_eq!(
        unset["workload"]["queries"][0]["requirements"]["accuracy"],
        serde_json::json!({"max_error": 0.01, "accuracy_scope": "whole_result"})
    );
    // A quantile has no single default metric to show.
    assert!(unset["workload"]["queries"][0]["requirements"]
        .get("default_metric")
        .is_some_and(Value::is_null));
    let summaries = accepted(&unset);
    assert!(
        summaries
            .iter()
            .all(|s| s.starts_with("metric not set; accepted ")),
        "{summaries:?}"
    );
    for wanted in ["accepted rank (Kll)", "accepted relative (DDSketch)"] {
        assert!(
            summaries.iter().any(|s| s.contains(wanted)),
            "{wanted}: {summaries:?}"
        );
    }
    let selected = unset["stage3_plan_selection"]["selected"].as_str().unwrap();
    assert!(unset["stage3_plan_selection"]["accepted"][selected][0]["accepted_metric"].is_string());

    let rank = document_of(&[&query[..], &["--accuracy-metric", "rank"]].concat());
    let summaries = accepted(&rank);
    assert!(
        summaries
            .iter()
            .all(|s| s == "metric rank; accepted rank (Kll)" || s == "metric rank; accepted exact"),
        "{summaries:?}"
    );
}

/// `--cost-model` chooses how plan selection compares Example 4's
/// candidates: cost model 1 (the default) selects the whole-window KLL (P2),
/// cost model 2 the exact plan (P1), and cost model 3 with a 256 GB cluster
/// the panes maintained at ingestion time (P3-m1), with no prices and, for
/// `pareto-front`, the front. Each candidate reports its usage.
#[test]
fn cost_model_flag_chooses_how_candidates_are_compared() {
    let selected = |extra: &[&str]| {
        let document = generate_example("planner-layering-3b", extra);
        document["stage3_plan_selection"].clone()
    };
    assert_eq!(selected(&[])["selected"], "P2");
    let usage = &selected(&[])["costs"]["P3-m1"]["usage"];
    assert_eq!(usage["average_memory_bytes"], 106_608_000_000.0);
    assert_eq!(
        usage["latency_per_query"],
        serde_json::json!([{ "compaction_ms": 0.0, "query_ms": 7.0 }])
    );
    assert_eq!(
        selected(&["--cost-model", "provisioned-capacity"])["selected"],
        "P1"
    );
    let cluster = selected(&[
        "--cost-model",
        "cluster-capacity",
        "--cluster-memory-bytes",
        "256000000000",
        "--cluster-disk-bytes",
        "1000000000000",
        "--cluster-objective",
        "pareto-front",
    ]);
    assert_eq!(cluster["selected"], "P3-m1");
    assert_eq!(cluster["front"], serde_json::json!(["P1", "P2", "P3-m1"]));
    assert!(cluster["costs"]["P3-m1"]["cost_per_second"].is_null());
}

/// `--cost-model aws-fargate-us-east-1` prices Example 4 under cost model 1
/// at AWS Fargate's us-east-1 prices, in US dollars per second: the
/// whole-window KLL (P2) is still selected, at $2.36124e-8/s (its 2.1
/// CPU-ms/s at $1.1244e-8 per CPU-ms; it keeps no memory), and the panes
/// maintained at ingestion time (P3-m1) cost about $1.2262e-4/s, almost all
/// for their 106.6 GB kept in memory. A costlier candidate's reason shows
/// such small costs in scientific notation.
#[test]
fn aws_fargate_preset_prices_example_4_in_dollars() {
    let document = generate_example(
        "planner-layering-3b",
        &["--cost-model", "aws-fargate-us-east-1"],
    );
    let selection = &document["stage3_plan_selection"];
    assert_eq!(selection["selected"], "P2");
    let cost = |id: &str| selection["costs"][id]["cost_per_second"].as_f64().unwrap();
    let close = |actual: f64, expected: f64, tolerance: f64| {
        assert!(
            ((actual - expected) / expected).abs() < tolerance,
            "{actual} != {expected}"
        )
    };
    close(cost("P2"), 2.36124e-8, 1e-9);
    close(cost("P3-m1"), 1.2262e-4, 1e-4);
    // A dollar cost below 0.001 is shown in scientific notation, not as 0.000.
    let p1 = selection["rejected"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["id"] == "P1")
        .unwrap();
    assert_eq!(
        p1["reason"],
        "costlier: 2.699e-8 vs 2.361e-8 cost_per_second"
    );
}

/// `--deployment-capabilities` declares the executor's capabilities, which
/// the document's `deployment_capabilities` shows (defaults filled in;
/// without the flag, every summary supported, raw data retained, no
/// query-time result retained). For Pattern B with KLL only and query-time
/// results retained: no DDSketch candidate is offered, and Stage 1's
/// `pass1_pruned` says why; the KLL panes gain B3, built at query time and
/// kept (`P3-m2`, 8,884.53 cost/s); the whole-window KLL (`P2`) is still
/// selected. Without raw data, every query-time scan is invalid and B1
/// (`P3-m1`) is selected. A deployment with an unknown field is refused.
#[test]
fn deployment_capabilities_flag_sets_the_capabilities_and_shows_them() {
    let default = generate_example("planner-layering-3b", &[]);
    assert_eq!(
        default["deployment_capabilities"],
        serde_json::json!({
            "supported_summaries": null,
            "raw_data_retained": true,
            "query_time_results_retained": false,
        })
    );
    let kll_kept = generate_example(
        "planner-layering-3b",
        &[
            "--deployment-capabilities",
            r#"{"supported_summaries": [{"family": "Kll", "readouts": ["Quantile"]}],
                "query_time_results_retained": true}"#,
        ],
    );
    assert_eq!(
        kll_kept["deployment_capabilities"],
        serde_json::json!({
            "supported_summaries": [{ "family": "Kll", "readouts": ["Quantile"] }],
            "raw_data_retained": true,
            "query_time_results_retained": true,
        })
    );
    let physical: Vec<&str> = kll_kept["stage2_physical_asap_optimization"]["candidates"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["id"].as_str().unwrap())
        .collect();
    assert_eq!(physical, ["P1", "P2", "P3", "P3-m1", "P3-m2"]);
    // Pass 1 lists why DDSketch is gone.
    assert_eq!(
        kll_kept["stage1_logical_asap_optimization"]["pass1_pruned"],
        serde_json::json!([{
            "target": 0,
            "query": "Q1",
            "pruned": [{ "candidate": "DDSketch", "reason": "UnsupportedByDeployment" }],
            "fused_pruned": [],
        }])
    );
    assert_eq!(
        default["stage1_logical_asap_optimization"]["pass1_pruned"],
        serde_json::json!([])
    );
    let selection = &kll_kept["stage3_plan_selection"];
    assert_eq!(selection["selected"], "P2");
    assert_eq!(selection["costs"]["P3-m2"]["cost_per_second"], 8884.53);

    let no_raw = generate_example(
        "planner-layering-3b",
        &[
            "--deployment-capabilities",
            r#"{"raw_data_retained": false}"#,
        ],
    );
    let selection = &no_raw["stage3_plan_selection"];
    assert_eq!(selection["selected"], "P3-m1");
    let p1 = selection["rejected"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["id"] == "P1")
        .unwrap();
    assert_eq!(
        p1["reason"],
        "scans 300000 ms of raw latency_ms at query time, but the deployment does not retain raw data"
    );

    let (status, stderr) = failure(&[
        "--exhaustive",
        "--example",
        "planner-layering-3b",
        "--deployment-capabilities",
        r#"{"raw_data": false}"#,
    ]);
    assert_eq!(status, Some(2));
    assert!(
        stderr.contains("--deployment-capabilities: unknown field `raw_data`"),
        "{stderr}"
    );
}

/// Cluster limits need cost model 3, and cost model 3 needs them.
#[test]
fn cluster_flags_and_cost_model_3_go_together() {
    let (code, stderr) = failure(&[
        "--exhaustive",
        "--example",
        "planner-layering-3b",
        "--cluster-memory-bytes",
        "1",
    ]);
    assert_eq!(code, Some(2));
    assert!(
        stderr.contains("need --cost-model cluster-capacity"),
        "{stderr}"
    );
    let (code, stderr) = failure(&[
        "--exhaustive",
        "--example",
        "planner-layering-3b",
        "--cost-model",
        "cluster-capacity",
    ]);
    assert_eq!(code, Some(2));
    assert!(stderr.contains("needs --cluster-memory-bytes"), "{stderr}");
}

/// Two sums and two maxima over two metrics share each metric's scan and
/// range between its own pair of queries: every logical candidate's label
/// names which queries share each node, so no two labels are the same.
#[test]
fn logical_candidate_labels_are_distinct() {
    let document = document_of(&[
        "--exhaustive",
        "--promql",
        "sum(a)",
        "--promql",
        "max(a)",
        "--promql",
        "sum(b)",
        "--promql",
        "max(b)",
    ]);
    let labels: Vec<&str> = document["stage1_logical_asap_optimization"]["candidates"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["display_name"].as_str().unwrap())
        .collect();
    let distinct: std::collections::HashSet<&str> = labels.iter().copied().collect();
    assert_eq!(labels.len(), 9, "{labels:#?}");
    assert_eq!(distinct.len(), 9, "{labels:#?}");
}

/// `--workload` plans a `PlanningWorkload` JSON file. A dashboard panel
/// (`max_over_time(m[5m])` every minute over 1M series) answered as the
/// last hour at 1-minute steps (`range_evaluation`) is priced as 61
/// evaluations per request: the kept panes (`P2-m2`, 70.51 cost/s) beat
/// rebuilding the window at every step (`P1`, 127.08), and the document
/// reports their retention, 1 h + 5 min. The workload section shows the
/// range. An invalid range is refused.
#[test]
fn workload_file_with_a_range_evaluation_is_planned() {
    let workload = serde_json::json!({
        "query_workload": {
            "language": "promql",
            "query_batch": null,
            "repeating_queries": [{
                "query": "max_over_time(m[5m])",
                "demand": {"fixed_interval": 60000},
                "requirements": {},
                "predictability": {"predictable": {"known_at": null}},
                "range_evaluation": {"step_ms": 60000, "span_ms": 3600000},
            }],
        },
        "data_workload": {
            "arrival": {"ingesting": {"rows_per_second": 66666.67}},
            "sources": {"m": {"series_count": 1000000, "sample_interval": 15000}},
        },
    });
    let path = std::env::temp_dir().join(format!(
        "stage_pipeline_{}_range_workload.json",
        std::process::id()
    ));
    std::fs::write(&path, workload.to_string()).unwrap();
    let document = document_of(&[
        "--exhaustive",
        "--workload",
        path.to_str().unwrap(),
        "--deployment-capabilities",
        r#"{"query_time_results_retained": true}"#,
    ]);
    assert_eq!(
        document["workload"]["queries"][0]["requirements"]["range_evaluation"],
        serde_json::json!({"step_ms": 60000, "span_ms": 3600000})
    );
    let selection = &document["stage3_plan_selection"];
    assert_eq!(selection["selected"], "P2-m2");
    let cost = |id: &str| selection["costs"][id]["cost_per_second"].as_f64().unwrap();
    assert!((cost("P2-m2") - 70.5133).abs() < 1e-3, "{}", cost("P2-m2"));
    assert!((cost("P1") - 127.0833).abs() < 1e-3, "{}", cost("P1"));
    assert_eq!(
        selection["pane_retention_ms"],
        serde_json::json!({"1": 3_900_000})
    );

    let mut invalid = workload.clone();
    invalid["query_workload"]["repeating_queries"][0]["range_evaluation"]["step_ms"] =
        serde_json::json!(0);
    std::fs::write(&path, invalid.to_string()).unwrap();
    let (status, stderr) = failure(&["--workload", path.to_str().unwrap()]);
    assert_ne!(status, Some(0));
    assert!(
        stderr.contains("step_ms must be greater than zero"),
        "{stderr}"
    );
    std::fs::remove_file(path).unwrap();
}

/// `--plan-selection-settings <file>` loads Stage 3's settings from JSON:
/// cost model 2 with the illustrative prices selects what
/// `--cost-model provisioned-capacity` selects for Example 4 (P1). The file
/// replaces the cost-model flags, so combining them is refused.
#[test]
fn plan_selection_settings_flag_loads_the_settings_from_json() {
    let path = std::env::temp_dir().join(format!(
        "stage_pipeline_{}_plan_selection_settings.json",
        std::process::id()
    ));
    let settings = asap_plan_selection::PlanSelectionSettings {
        cost_model: asap_plan_selection::CostModel::ProvisionedCapacity(
            asap_plan_selection::ResourceUnitCost::illustrative(),
        ),
        ..Default::default()
    };
    std::fs::write(&path, serde_json::to_string(&settings).unwrap()).unwrap();
    let path_arg = path.to_str().unwrap();
    let document = generate_example(
        "planner-layering-3b",
        &["--plan-selection-settings", path_arg],
    );
    assert_eq!(document["stage3_plan_selection"]["selected"], "P1");
    let (code, stderr) = failure(&[
        "--exhaustive",
        "--example",
        "planner-layering-3b",
        "--plan-selection-settings",
        path_arg,
        "--cost-model",
        "usage-over-time",
    ]);
    assert_eq!(code, Some(2));
    assert!(
        stderr.contains("--plan-selection-settings replaces --cost-model"),
        "{stderr}"
    );
    std::fs::remove_file(path).unwrap();
}

/// `--ignore-accuracy-guarantees` (issue #774): the built-in model rejects
/// every sketch candidate of an average of p99s as having no guarantee;
/// with the flag each is valid, accepted regardless of its guarantee, and
/// records why the guarantee check would have rejected it.
#[test]
fn ignore_accuracy_guarantees_flag_accepts_outputs_without_a_guarantee() {
    let args = [
        "--exhaustive",
        "--promql",
        "avg by (g) (quantile_over_time(0.99, x[100s]))",
        "--max-error",
        "0.01",
        "--failure-probability",
        "0.01",
        "--accuracy-metric",
        "rank",
    ];
    let builtin = document_of(&args);
    let unguaranteed: Vec<String> = builtin["stage3_plan_selection"]["rejected"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|r| r["cause"] == "no_accuracy_guarantee")
        .map(|r| r["id"].as_str().unwrap().to_string())
        .collect();
    assert!(!unguaranteed.is_empty(), "{builtin}");
    let mut ignoring = args.to_vec();
    ignoring.push("--ignore-accuracy-guarantees");
    let document = document_of(&ignoring);
    let accepted = &document["stage3_plan_selection"]["accepted"];
    for id in &unguaranteed {
        let query = &accepted[id][0];
        assert_eq!(query["accepted_regardless_of_guarantee"], true, "{id}");
        assert_eq!(
            query["guarantee_check_rejection"], "no_accuracy_guarantee",
            "{id}: {query}"
        );
    }
}

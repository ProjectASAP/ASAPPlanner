//! `stage_pipeline` writes valid four-stage viewer documents for #509's examples.
use std::collections::HashSet;
use std::path::PathBuf;
use std::process::Command;

use asap_devtools::planner_layering_examples::{example1, pattern_a, PatternARecurrence};
use asap_devtools::planning_stages::{
    lower_promql_roots, run_planning_stages, PlanningStagesError,
};
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
    let logical = &document["stage1_logical_asap_optimization"];
    assert_eq!(logical["capped"], true);
    assert!(logical["candidates"].as_array().unwrap().len() < 27);
    // 3 Pass 1 candidates, each with 8 Pass 2 candidates: shared inputs,
    // tumbling panes, and both.
    assert_eq!(logical["combinations"], 27);
    assert_eq!(
        document["stage3_plan_selection"]["selected"],
        full["stage3_plan_selection"]["selected"]
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
    let candidates = document["stage1_logical_asap_optimization"]["candidates"]
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

/// Example 3a (243 Pass 1 candidates, each with 3 Pass 2 candidates)
/// selects what the planning stages select over
/// every candidate, which the acceptance tests also run: the document's
/// display cap does not change the selection, and the selected plan is
/// displayed with its cost.
#[test]
fn example3a_selects_the_cheapest_of_all_candidates() {
    let workload = pattern_a(PatternARecurrence::OnceAdHoc, DataArrival::Mixed);
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
            "--epsilon",
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
            "--example",
            "planner-layering-1",
            "--max-combinations",
            limit,
        ]);
        assert_eq!(status, Some(3), "{limit}");
        assert!(
            stderr.contains(&format!(
                "workload has more than {limit} Stage 1 candidates, Pass 2's included; \
                 planning it needs early pruning (#673)"
            )),
            "{stderr}"
        );
    }
    // At Stage 1's 27 candidates, Stage 2's 57 physical candidates (30 maintain
    // pane chains at ingestion time) pass the limit; they are counted, not built.
    let (status, stderr) = failure(&[
        "--example",
        "planner-layering-1",
        "--max-combinations",
        "27",
    ]);
    assert_eq!(status, Some(3));
    assert!(
        stderr.contains(
            "workload has more than 27 Stage 2 physical candidates (materialization options \
             included); planning it needs early pruning (#673)"
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

/// The default limit is 10^4: nine approximate Top-K queries (3^9 = 19683
/// Pass 1 combinations) are rejected once the count passes it.
#[test]
fn default_candidate_limit_is_ten_thousand() {
    let queries: Vec<String> = (1..=9)
        .map(|i| format!("topk(5, rate(m{i}[5m]))"))
        .collect();
    let mut args = Vec::new();
    for query in &queries {
        args.extend(["--promql", query.as_str()]);
    }
    args.extend(["--epsilon", "0.01", "--delta", "0.001"]);
    let (status, stderr) = failure(&args);
    assert_eq!(status, Some(3));
    assert!(
        stderr.contains("workload has more than 10000 Stage 1 candidates"),
        "{stderr}"
    );
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
        "--epsilon",
        "1.5",
        "--delta",
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
/// in and that the target's metric was not set. With `--metric rank` only
/// rank guarantees (and exact plans) are accepted, and the report says so. The workload's
/// accuracy is the target's own JSON form.
#[test]
fn a_quantile_target_without_a_metric_accepts_rank_or_relative() {
    let query = [
        "--promql",
        "quantile_over_time(0.99, m[1m])",
        "--epsilon",
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
        serde_json::json!({"error": 0.01, "scope": "whole_result"})
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

    let rank = document_of(&[&query[..], &["--metric", "rank"]].concat());
    let summaries = accepted(&rank);
    assert!(
        summaries
            .iter()
            .all(|s| s == "metric rank; accepted rank (Kll)" || s == "metric rank; accepted exact"),
        "{summaries:?}"
    );
}

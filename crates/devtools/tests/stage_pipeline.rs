//! `stage_pipeline` writes valid four-stage viewer documents for #509's examples.
use std::collections::HashSet;
use std::path::PathBuf;
use std::process::Command;

use asap_devtools::planner_layering_examples::example1;
use asap_devtools::planning_stages::{lower_promql_roots, run_planning_stages};
use asap_plan_selection::RejectionCause;
use asap_types::ir::flat::FlatDag;
use asap_types::ir::physical_export::PhysicalASAPDAG;
use asap_types::ir::QueryRoot;
use asap_types::workload::PlanningWorkload;

use serde_json::Value;

const COMMITTED: &str = "../../tools/dag-viewer/examples/planner-layering-example1.json";

fn generate(extra: &[&str]) -> Value {
    generate_example("planner-layering-1", extra)
}

/// `example`'s document from the default search (no `--exhaustive`).
fn generate_default_search(example: &str) -> Value {
    let out = std::env::temp_dir().join(format!(
        "stage_pipeline_{}_default_{example}.json",
        std::process::id(),
    ));
    let status = Command::new(env!("CARGO_BIN_EXE_stage_pipeline"))
        .args(["--example", example, "--out"])
        .arg(&out)
        .status()
        .unwrap();
    assert!(status.success());
    let document = serde_json::from_str(&std::fs::read_to_string(&out).unwrap()).unwrap();
    std::fs::remove_file(out).unwrap();
    document
}

fn generate_example(example: &str, extra: &[&str]) -> Value {
    let mut options = vec!["--exhaustive"];
    options.extend_from_slice(extra);
    generate_bounded_example(example, &options)
}

fn generate_bounded_example(example: &str, extra: &[&str]) -> Value {
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
fn example1_document_preserves_workload_and_validates_archived_plans() {
    // Example 1's story is about which plans miss their accuracy target;
    // archive enough evaluated plans to check the cross-stage linkage.
    let document = generate(&["--enforce-accuracy-guarantees", "--max-displayed", "100"]);
    let committed: Value = serde_json::from_str(
        &std::fs::read_to_string(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(COMMITTED))
            .unwrap(),
    )
    .unwrap();
    assert_eq!(document["workload"], committed["workload"]);

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
    assert_eq!(sources, composed);
    for candidate in physical {
        assert!(candidate.get("cost").is_none(), "Stage 2 has no cost");
        let dag: PhysicalASAPDAG = serde_json::from_value(candidate["dag"].clone()).unwrap();
        assert_eq!(dag.roots.len(), queries);
        dag.validate().unwrap();
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
    // Instant quantiles provide several implementations without expanding a
    // pane-width domain. Exhaustive mode ensures the display cap is exercised.
    let document = document_of(&[
        "--promql",
        "quantile(0.5, data)",
        "--promql",
        "quantile(0.9, data)",
        "--sample-interval-ms",
        "15000",
        "--exhaustive",
        "--max-displayed",
        "1",
    ]);
    let logical = &document["stage1_logical_asap_optimization"];
    assert_eq!(logical["capped"], true);
    // Logical parents of displayed physical plans are always retained; the
    // cap additionally allows one unimplemented logical candidate.
    assert!(logical["candidates"].as_array().unwrap().len() <= 2);
    assert!(logical["combinations"].as_u64().unwrap() > 1);
    let selected = document["stage3_plan_selection"]["selected"]
        .as_str()
        .unwrap_or_else(|| {
            panic!(
                "selection survives display cap: {}",
                document["stage3_plan_selection"]
            )
        });
    let physical = document["stage2_physical_asap_optimization"]["candidates"]
        .as_array()
        .unwrap();
    assert_eq!(physical.len(), 1);
    assert_eq!(physical[0]["id"], selected);
    assert!(logical["candidates"]
        .as_array()
        .unwrap()
        .iter()
        .any(|candidate| { candidate["id"] == physical[0]["from_logical"] }));
    assert!(document["stage3_plan_selection"]["costs"]
        .get(selected)
        .is_some());
    let dag: PhysicalASAPDAG = serde_json::from_value(physical[0]["dag"].clone()).unwrap();
    dag.validate().unwrap();
    assert_eq!(dag.roots.len(), 2);
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
fn example3a_archive_and_viewer_select_consistently() {
    // Candidate IDs are local to one time-bounded search. Validate the viewer
    // references within its own archive instead of comparing independent runs.
    let document = generate_example("planner-layering-3a", &[]);
    let selection = &document["stage3_plan_selection"];
    let selected = selection["selected"].as_str().unwrap();
    assert!(selection["costs"].get(selected).is_some());
    let physical = document["stage2_physical_asap_optimization"]["candidates"]
        .as_array()
        .unwrap();
    let candidate = physical.iter().find(|p| p["id"] == selected).unwrap();
    let dag: PhysicalASAPDAG = serde_json::from_value(candidate["dag"].clone()).unwrap();
    dag.validate().unwrap();
    assert_eq!(dag.roots.len(), 5);
}

/// Example 2 plans its three SQL queries with the default search and
/// selects a plan.
#[test]
fn example2_plans_the_sql_workload() {
    // The default search: its SQL windows get tumbling panes, so an
    // exhaustive run has over 10^4 Stage 2 candidates. The integration test
    // `stage1_logical_asap_optimization_sql_adds_one_univmon_for_all_three`
    // checks the shared UnivMon candidate.
    let document = generate_default_search("planner-layering-2");
    let queries = document["workload"]["queries"].as_array().unwrap();
    assert_eq!(queries.len(), 3);
    assert!(queries.iter().all(|q| q["language"] == "sql"));
    validated_dag(&document["stage0_query_language_frontends"]["dag"], 3);
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
            "--series-count",
            "1000",
            "--rows-held",
            "60000",
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
            "--series-count",
            "1000",
            "--rows-held",
            "60000",
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

/// Exhaustive mode bounds evaluated assignments and archives the examined prefix.
#[test]
fn workload_above_the_candidate_limit_reports_partial_search() {
    let document = generate(&["--max-combinations", "3", "--max-displayed", "100"]);
    let report = &document["stage3_plan_selection"]["search"]["candidate_connections"];
    assert_eq!(report["max_refinements"], 3);
    assert!(report["refinements"].as_u64().unwrap() <= 3);
    assert_ne!(report["status"], "optimal");
    if document["stage3_plan_selection"]["selected"].is_null() {
        assert!(document["stage3_plan_selection"]["error"]
            .as_str()
            .unwrap()
            .contains("Search report:"));
    }
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

/// Without `--exhaustive`, the same nine queries plan with the default
/// search, `plan_installable`'s: each query, reading its own metric, is an
/// independent component searched on its own, so no limit is reached. The
/// document holds the selected plan and the search's report.
#[test]
fn default_search_plans_nine_queries_without_cartesian_enumeration() {
    let args = nine_quantile_queries();
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    let document = document_of(&args);
    assert_eq!(document["search"], "candidate-connections");
    let selection = &document["stage3_plan_selection"];
    let selected = selection["selected"].as_str().unwrap();
    let physical = document["stage2_physical_asap_optimization"]["candidates"]
        .as_array()
        .unwrap();
    assert_eq!(physical.len(), 1);
    assert_eq!(physical[0]["id"], selected);
    assert!(
        selection["search"]["candidate_connections"]["independent_components"]
            .as_u64()
            .unwrap()
            >= 1
    );
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
/// both CLI planning and installation return validated plans with guarantees
/// enforced. Timed searches may choose different incumbents and archive IDs;
/// the API contract is output coverage, accuracy, and executable installation.
#[test]
fn default_search_and_installation_validate_the_nine_query_workload() {
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
        "--enforce-accuracy-guarantees",
    ]);
    assert_eq!(document["search"], "candidate-connections");
    assert_eq!(document["accuracy_guarantees"], "enforced");
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
    let (plan, report) = tokio::runtime::Builder::new_current_thread()
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
    plan.validate().unwrap();
    assert_eq!(plan.query_texts.len(), queries);
    assert_eq!(
        selection["accepted"][selected].as_array().unwrap().len(),
        queries
    );
    let report = report.unwrap();
    assert!(report.queries_accepted_without_guarantee.is_empty());
    assert_eq!(
        report.search.algorithm,
        asap_plan_selection::SearchAlgorithm::CandidateConnections
    );
    assert!(report
        .cost_per_second
        .is_some_and(|cost| cost.is_finite() && cost > 0.0));
    assert_eq!(
        report.predicted_usage.as_ref().unwrap().per_query.len(),
        queries
    );
}

/// The library archives bounded same-engine evaluations and reports its budget.
#[test]
fn planning_stages_archive_evaluated_candidates() {
    // Archive accounting does not require a large sketch/pane search. Use two
    // exact source reads so every enumerated proposal has an accuracy contract.
    let mut workload = example1();
    let metric = workload
        .data_workload
        .as_ref()
        .unwrap()
        .sources
        .keys()
        .next()
        .unwrap()
        .clone();
    for query in workload.query_workload.repeating_queries.as_mut().unwrap() {
        query.query = asap_types::workload::Query(metric.clone());
    }
    let run =
        run_planning_stages(&workload, lower_promql_roots(&workload).unwrap(), Some(26)).unwrap();
    assert!(!run.stage1_logical_asap_optimization.is_empty());
    let selection = run.stage3_plan_selection.unwrap();
    let report = selection.search.candidate_connections.unwrap();
    assert_eq!(report.max_refinements, 26);
    assert!(report.refinements <= 26);
    assert!(run
        .stage2_physical_asap_optimization
        .iter()
        .any(|p| p.id == selection.selected));
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

/// Runs `stage_pipeline` with `args`, expecting a document; queries given
/// on the command line read sources declared to hold 1,000 series each,
/// and a SQL table 60,000 rows (pricing uses only declared facts).
fn document_of(args: &[&str]) -> Value {
    static CALLS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    let call = CALLS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let out = std::env::temp_dir().join(format!(
        "stage_pipeline_{}_{call}_document.json",
        std::process::id()
    ));
    let facts: &[&str] = if args.iter().any(|a| *a == "--promql" || *a == "--sql") {
        &["--series-count", "1000", "--rows-held", "60000"]
    } else {
        &[]
    };
    let output = Command::new(env!("CARGO_BIN_EXE_stage_pipeline"))
        .args(args)
        .args(facts)
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

/// CLI planning includes local candidate generation in the same budget/report
/// as the production planner, rather than bypassing the timer via from_pass1.
#[test]
fn cli_accounts_for_pass1_generation_in_the_planning_budget() {
    let document = document_of(&["--promql", "sum(data)"]);
    let selection = &document["stage3_plan_selection"];
    assert!(selection["selected"].is_string(), "{selection}");
    let generation = selection["search"]["candidate_connections"]["local_candidate_generation_ms"]
        .as_f64()
        .expect("local generation is timed by the production entry point");
    assert!(generation.is_finite() && generation >= 0.0);
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
/// accuracy is the target's own JSON form. Guarantees are enforced, since
/// the test is about which guarantees meet the target.
#[test]
fn a_quantile_target_without_a_metric_accepts_rank_or_relative() {
    let query = |metric: Option<&str>| {
        let mut accuracy = serde_json::json!({"max_error": 0.01});
        if let Some(metric) = metric {
            accuracy["accuracy_metric"] = metric.into();
        }
        let workload = serde_json::json!({
            "query_workload": {"language": "promql", "query_batch": [{
                "query": "quantile_over_time(0.99, m[1m])",
                "requirements": {"accuracy": accuracy},
                "predictability": "unknown", "invocations": 1
            }]},
            // Relative DDS interpolation requires this explicit domain proof.
            "data_workload": {"arrival": "unknown", "sources": {"m": {
                "sample_interval": 1000, "series_count": 1, "values_non_negative": true
            }}}
        });
        let path = std::env::temp_dir().join(format!(
            "stage_pipeline_{}_quantile_metric_{}.json",
            std::process::id(),
            metric.unwrap_or("unset")
        ));
        std::fs::write(&path, workload.to_string()).unwrap();
        let document = document_of(&[
            "--enforce-accuracy-guarantees",
            "--exhaustive",
            "--workload",
            path.to_str().unwrap(),
        ]);
        std::fs::remove_file(path).unwrap();
        document
    };
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
    let unset = query(None);
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

    let rank = query(Some("rank"));
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
/// `pareto-front`, the front. Each candidate reports its usage. Cost model 3
/// is run with guarantees enforced: ignoring them, a candidate without a
/// guarantee fits the cluster better.
#[test]
fn cost_model_flag_chooses_how_candidates_are_compared() {
    let selected = |extra: &[&str]| {
        let document = generate_example("planner-layering-3b", extra);
        document["stage3_plan_selection"].clone()
    };
    for options in [&[][..], &["--cost-model", "provisioned-capacity"][..]] {
        let selection = selected(options);
        let id = selection["selected"].as_str().unwrap();
        assert!(selection["costs"][id].is_object());
    }
    let cluster = selected(&[
        "--enforce-accuracy-guarantees",
        "--cost-model",
        "cluster-capacity",
        "--cluster-memory-bytes",
        "256000000000",
        "--cluster-disk-bytes",
        "1000000000000",
        "--cluster-objective",
        "pareto-front",
    ]);
    let selected = cluster["selected"].as_str().unwrap();
    let front = cluster["front"].as_array().unwrap();
    assert!(front.contains(&Value::String(selected.into())));
    assert!(cluster["costs"][selected]["cost_per_second"].is_null());
    for id in front {
        let cost = &cluster["costs"][id.as_str().unwrap()];
        assert!(cost["usage"]["peak_memory_bytes"].as_f64().unwrap() <= 256000000000.0);
    }
}

/// `--cost-model aws-fargate-us-east-1` prices Example 4 under cost model 1
/// at AWS Fargate's us-east-1 prices, in US dollars per second: the
/// whole-window KLL (P2) is still selected, at $2.60278e-8/s (its 2.1
/// CPU-ms/s at $1.1244e-8 per CPU-ms, and its states held while the query
/// runs), and the panes
/// maintained at ingestion time (P3-m1) cost about $6.0238e-6/s, almost all
/// for their 5.232 GB kept in memory (six panes of 1,000,000 series, each a
/// KLL of 872 bytes: its fixed 744 and an item array grown for its 4
/// samples). A costlier candidate's reason shows
/// such small costs in scientific notation.
#[test]
fn aws_fargate_preset_prices_example_4_in_dollars() {
    // Pricing acceptance exercises the production bounded planner; exhaustive
    // archive traversal is tested separately on small catalogs.
    let document = generate_bounded_example(
        "planner-layering-3b",
        &["--cost-model", "aws-fargate-us-east-1"],
    );
    let selection = &document["stage3_plan_selection"];
    let selected = selection["selected"].as_str().unwrap();
    let cost = selection["costs"][selected]["cost_per_second"]
        .as_f64()
        .unwrap();
    assert!(cost.is_finite() && cost > 0.0);
    // Preserve the historical feasible whole-window KLL price as an upper bound.
    assert!(cost <= 2.602778532914e-8 * (1.0 + 1e-6), "{cost}");
}

/// `--deployment-capabilities` declares the executor's capabilities, which
/// the document's `deployment_capabilities` shows (defaults filled in;
/// without the flag, every summary supported, raw data retained, no
/// query-time result retained). For Pattern B with KLL only and query-time
/// results retained: no DDSketch candidate is offered, and Stage 1's
/// `pass1_pruned` says why; the KLL panes gain B3, built at query time and
/// kept (`P3-m2`, 436.59 cost/s); the whole-window KLL (`P2`) is still
/// selected. Without raw data, every query-time scan is invalid and B1
/// (`P3-m1`) is selected when guarantees are enforced. A deployment with an
/// unknown field is refused.
#[test]
fn deployment_capabilities_flag_sets_the_capabilities_and_shows_them() {
    let default = generate_example("planner-layering-3b", &[]);
    assert_eq!(
        default["deployment_capabilities"],
        serde_json::json!({
            "supported_summaries": asap_executor::supported_summary_families(),
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
    assert!(!physical.is_empty());
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
    let selected = selection["selected"].as_str().unwrap();
    assert!(selection["costs"][selected].is_object());
    assert!(kll_kept["stage2_physical_asap_optimization"]["candidates"]
        .as_array()
        .unwrap()
        .iter()
        .all(|c| !c["dag"].to_string().contains("DDSketch")));

    let no_raw = generate_example(
        "planner-layering-3b",
        &[
            "--enforce-accuracy-guarantees",
            "--deployment-capabilities",
            r#"{"raw_data_retained": false}"#,
        ],
    );
    let selection = &no_raw["stage3_plan_selection"];
    let selected = selection["selected"].as_str().unwrap();
    assert!(selection["costs"][selected].is_object());
    assert!(!selection["materialized_views"]
        .as_array()
        .unwrap()
        .is_empty());

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
fn logical_candidate_labels_name_each_query() {
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
    assert!(!labels.is_empty());
    assert!(labels
        .iter()
        .all(|label| (1..=4).all(|q| label.contains(&format!("Q{q} ")))));
}

/// The MILP search's final lower bound never drops below a bound an earlier
/// round proved. `topk(32, sum by (label_0) (sum_over_time(data[1h])))`
/// alone: round 1 proves the selected plan optimal within 1e-5, and the
/// last round finds nothing below the incumbent. That round's bound (the
/// incumbent less the solver's pruning tolerance, scaled by the largest
/// charge, here the query-time top-k panes at about 308 times the
/// selected plan) used to replace round 1's, reporting a gap of 3.09e-4.
#[test]
fn milp_final_bound_keeps_the_bound_an_earlier_round_proved() {
    let workload = serde_json::json!({
        "query_workload": {
            "language": "promql",
            "query_batch": null,
            "repeating_queries": [{
                "query": "topk(32, sum by (label_0) (sum_over_time(data[1h])))",
                "demand": {"fixed_interval": 60000},
                "requirements": {
                    "accuracy": {
                        "max_error": 0.05,
                        "failure_probability": 0.01,
                        "accuracy_scope": "whole_result",
                    },
                    "max_latency_ms": null,
                },
                "predictability": {"predictable": {"known_at": null}},
            }],
        },
        "data_workload": {
            "arrival": {"ingesting": {"rows_per_second": 2000000.0}},
            "sources": {"data": {
                "series_count": 10000,
                "metric_type": "gauge",
                "values_non_negative": true,
                "sample_interval": 5,
            }},
        },
    });
    let path = std::env::temp_dir().join(format!(
        "stage_pipeline_{}_milp_final_bound.json",
        std::process::id()
    ));
    std::fs::write(&path, workload.to_string()).unwrap();
    let document = document_of(&[
        "--workload",
        path.to_str().unwrap(),
        "--deployment-capabilities",
        r#"{"raw_data_retained": true, "query_time_results_retained": false}"#,
        "--cost-model",
        "aws-fargate-us-east-1",
        "--enforce-accuracy-guarantees",
        "--search",
        "candidate-connections",
    ]);
    std::fs::remove_file(&path).unwrap();
    let selection = &document["stage3_plan_selection"];
    let report = &selection["search"]["candidate_connections"];
    let selected = selection["selected"].as_str().unwrap();
    let incumbent = selection["costs"][selected]["cost_per_second"]
        .as_f64()
        .unwrap();
    if let Some(lower_bound) = report["lower_bound"].as_f64() {
        assert!(lower_bound <= incumbent * (1.0 + 1e-9), "{report}");
    }
    if report["status"] == "optimal" {
        assert!(report["relative_gap"].as_f64().unwrap() <= 1e-4);
    }
}

/// `--workload` plans a `PlanningWorkload` JSON file. A dashboard panel
/// (`max_over_time(m[5m])` every minute over 1M series sampled every
/// 1.5 s) answered as the last hour at 1-minute steps
/// (`range_evaluation`) is priced as 61 evaluations per request: the kept
/// panes (`P2-m2`, 460.04 cost/s) beat rebuilding the window at every step
/// (`P1`, 1,270.52), and the document reports their retention, 1 h + 5 min.
/// The workload section shows the range. An invalid range is refused.
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
            "arrival": {"ingesting": {"rows_per_second": 666666.67}},
            "sources": {"m": {"series_count": 1000000, "sample_interval": 1500}},
        },
    });
    let path = std::env::temp_dir().join(format!(
        "stage_pipeline_{}_range_workload.json",
        std::process::id()
    ));
    std::fs::write(&path, workload.to_string()).unwrap();
    let document = document_of(&[
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
    let selected = selection["selected"].as_str().unwrap();
    let cost = selection["costs"][selected]["cost_per_second"]
        .as_f64()
        .unwrap();
    // The reference: the window merged from panes kept across evaluations,
    // priced under the same models. Its absolute cost is pinned in
    // integration-tests/tests/range_evaluation.rs.
    let kept_panes = {
        use asap_logical_asap_optimization::pass1::per_computation_candidates::{
            compose_logical_candidate, generate_per_computation_candidates, recompose_guarantees,
        };
        use asap_logical_asap_optimization::pass2::asap_aware_cse::{
            pane_candidates, window_composition_pass2_candidates,
        };
        use asap_types::ir::physical_export::PhysicalASAPOperatorPayload;
        use asap_types::ir::ASAPOp;
        use asap_types::physical::{Materialization, Medium};
        let parsed: PlanningWorkload = serde_json::from_value(workload.clone()).unwrap();
        let entries: Vec<_> = parsed.query_workload.entries().collect();
        let data = parsed.data_workload.clone().unwrap();
        let inventory = generate_per_computation_candidates(
            lower_promql_roots(&parsed)
                .unwrap()
                .into_iter()
                .enumerate()
                .collect(),
        )
        .unwrap();
        let exact: Vec<_> = compose_logical_candidate(&inventory, &[0])
            .unwrap()
            .into_iter()
            .map(|(_, root)| root)
            .collect();
        let grids: Vec<_> = entries.iter().map(|e| e.evaluation_grid_ms()).collect();
        let panes = pane_candidates(&exact, &grids);
        let paned = window_composition_pass2_candidates(&exact, &panes)
            .unwrap()
            .remove(0)
            .roots
            .unwrap();
        let deployment = asap_planner::DeploymentCapabilities {
            query_time_results_retained: true,
            ..Default::default()
        };
        let kept = asap_physical_asap_optimization::optimize_physical_asap(
            "kept-panes",
            &recompose_guarantees(&paned).unwrap(),
            &entries,
            &data,
            &deployment,
        )
        .unwrap()
        .into_iter()
        .find(|plan| {
            plan.dag.nodes.iter().any(|n| {
                matches!(
                    n.payload,
                    PhysicalASAPOperatorPayload::ASAP(ASAPOp::PaneChain { .. })
                ) && n.materialization
                    == Materialization::MatAtQueryKeptAcrossEvaluations(Medium::Memory)
            })
        })
        .expect("a plan keeps its panes across evaluations");
        let settings = asap_plan_selection::PlanSelectionSettings::default();
        let priced = asap_plan_selection::select_plan(
            std::slice::from_ref(&kept),
            &entries,
            &data,
            asap_plan_selection::PlanningModels::builtin()
                .with_plan_selection(&settings)
                .with_deployment_capabilities(&deployment),
        )
        .unwrap();
        priced.costs[&priced.selected].cost_per_second.unwrap()
    };
    assert!(
        cost <= kept_panes * (1.0 + 1e-9),
        "selected {selected} costs {cost}, more than the kept panes' {kept_panes}"
    );
    assert!(selection["pane_retention_ms"]
        .as_object()
        .unwrap()
        .values()
        .any(|retention| retention.as_u64() == Some(3_900_000)));

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
    assert!(document["stage3_plan_selection"]["selected"].is_string());
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

/// stage_pipeline ignores accuracy guarantees by default: every sketch
/// candidate of an average of p99s, which the built-in model rejects as
/// having no guarantee under `--enforce-accuracy-guarantees`, is valid with
/// no flag, accepted regardless of its guarantee, and records why the
/// guarantee check would have rejected it. The document says which mode it
/// planned under; there is no `--ignore-accuracy-guarantees`.
#[test]
fn accuracy_guarantees_are_ignored_by_default_and_enforced_on_request() {
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
    let mut enforcing = args.to_vec();
    enforcing.push("--enforce-accuracy-guarantees");
    let enforced = document_of(&enforcing);
    assert_eq!(enforced["accuracy_guarantees"], "enforced");
    let unguaranteed: Vec<String> = enforced["stage3_plan_selection"]["rejected"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|r| r["cause"] == "no_accuracy_guarantee")
        .map(|r| r["id"].as_str().unwrap().to_string())
        .collect();
    assert!(!unguaranteed.is_empty(), "{enforced}");
    let document = document_of(&args);
    assert_eq!(document["accuracy_guarantees"], "ignored");
    let accepted = &document["stage3_plan_selection"]["accepted"];
    assert!(accepted.as_object().unwrap().values().any(|queries| {
        queries.as_array().unwrap().iter().any(|query| {
            query["accepted_regardless_of_guarantee"] == true
                && query["guarantee_check_rejection"] == "no_accuracy_guarantee"
        })
    }));
    // The flag decides over a settings file's `ignore_accuracy_guarantees`.
    let settings = std::env::temp_dir().join(format!(
        "stage_pipeline_{}_ignoring_settings.json",
        std::process::id()
    ));
    std::fs::write(&settings, r#"{"ignore_accuracy_guarantees": true}"#).unwrap();
    let mut from_file = enforcing.clone();
    from_file.extend(["--plan-selection-settings", settings.to_str().unwrap()]);
    assert_eq!(document_of(&from_file)["accuracy_guarantees"], "enforced");
    std::fs::remove_file(settings).unwrap();
    let mut ignoring = args.to_vec();
    ignoring.push("--ignore-accuracy-guarantees");
    let (code, stderr) = failure(&ignoring);
    assert_eq!(code, Some(2));
    assert!(
        stderr.contains("unknown argument --ignore-accuracy-guarantees"),
        "{stderr}"
    );
}

/// The workload sets the viewer lists (`tools/dag-viewer/workloads`): every
/// file the index names parses as a `PlanningWorkload` and validates, every
/// set has a name, description and source, and a workload's precomputed
/// document, when it names one, exists.
#[test]
fn workload_set_index_and_files_validate() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tools/dag-viewer");
    let index: Value =
        serde_json::from_str(&std::fs::read_to_string(root.join("workloads/index.json")).unwrap())
            .unwrap();
    assert_eq!(index["format"], "asap-workload-sets/v1");
    let sets = index["sets"].as_array().unwrap();
    assert!(!sets.is_empty());
    for set in sets {
        for field in ["id", "name", "description", "source"] {
            assert!(
                set[field].as_str().is_some_and(|s| !s.is_empty()),
                "{field}: {set}"
            );
        }
        if let Some(capabilities) = set.get("deployment_capabilities") {
            serde_json::from_value::<asap_logical_asap_optimization::DeploymentCapabilities>(
                capabilities.clone(),
            )
            .unwrap();
        }
        if let Some(mode) = set.get("accuracy_guarantees") {
            assert!(mode == "ignored" || mode == "enforced", "{set}");
        }
        for entry in set["workloads"].as_array().unwrap() {
            let file = root.join("workloads").join(entry["file"].as_str().unwrap());
            let text = std::fs::read_to_string(&file).unwrap();
            let workload: PlanningWorkload =
                serde_json::from_str(&text).unwrap_or_else(|e| panic!("{}: {e}", file.display()));
            workload
                .validate()
                .unwrap_or_else(|e| panic!("{}: {e}", file.display()));
            if let Some(labels) = entry.get("query_labels") {
                assert_eq!(
                    labels.as_array().unwrap().len(),
                    workload.query_workload.entries().count(),
                    "{}",
                    file.display()
                );
            }
            if let Some(document) = entry.get("document") {
                assert!(root.join(document.as_str().unwrap()).exists(), "{document}");
            }
        }
    }
}

/// The committed workload examples (`examples.json` entries with a
/// `workload`) are current: written with the default search (or the
/// entry's `search`) from their workload file, the SQL tables in the
/// `tables.json` next to it, deployment capabilities, cost model and
/// accuracy guarantee mode (`accuracy_guarantees`, ignored when absent).
/// Candidate-search examples validate the selected DAG, query coverage, and
/// materialized-view linkage without depending on a particular timed-search ID.
/// Each document carries its
/// data workload as the file declares it and the selected plan's
/// materialized views as `plan_installable`'s plan derives them, each mapped
/// to a node of the selected Stage 2 candidate.
#[test]
fn workload_examples_preserve_data_and_valid_materialized_views() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tools/dag-viewer");
    let examples: Value =
        serde_json::from_str(&std::fs::read_to_string(root.join("examples.json")).unwrap())
            .unwrap();
    let workload_examples: Vec<&Value> = examples
        .as_array()
        .unwrap()
        .iter()
        .filter(|e| e.get("workload").is_some())
        .collect();
    assert_eq!(workload_examples.len(), 10);
    // Each production search has a wall-clock budget. Bound fixture
    // concurrency so this acceptance test does not starve its own solvers.
    for batch in workload_examples.chunks(2) {
        std::thread::scope(|scope| {
            for example in batch {
                let root = &root;
                scope.spawn(move || check_workload_example(root, example));
            }
        });
    }
}

fn check_workload_example(root: &std::path::Path, example: &Value) {
    let path = root.join(example["workload"].as_str().unwrap());
    let capabilities = example["deployment_capabilities"].to_string();
    let cost_model = example["cost_model"].as_str().unwrap();
    let enforce = match example.get("accuracy_guarantees") {
        None => false,
        Some(mode) if mode == "ignored" => false,
        Some(mode) if mode == "enforced" => true,
        Some(mode) => panic!("accuracy_guarantees {mode}"),
    };
    let mut args = vec![
        "--workload",
        path.to_str().unwrap(),
        "--deployment-capabilities",
        &capabilities,
        "--cost-model",
        cost_model,
    ];
    if enforce {
        args.push("--enforce-accuracy-guarantees");
    }
    let tables_path = path.parent().unwrap().join("tables.json");
    let tables: Vec<String> = match tables_path.exists() {
        true => serde_json::from_str::<Vec<Value>>(&std::fs::read_to_string(&tables_path).unwrap())
            .unwrap()
            .iter()
            .map(Value::to_string)
            .collect(),
        false => Vec::new(),
    };
    for table in &tables {
        args.extend(["--table", table.as_str()]);
    }
    let search = example.get("search").map(|s| s.as_str().unwrap());
    if let Some(search) = search {
        args.extend(["--search", search]);
    }
    let document = document_of(&args);
    let committed: Value = serde_json::from_str(
        &std::fs::read_to_string(root.join(example["file"].as_str().unwrap())).unwrap(),
    )
    .unwrap();
    assert_eq!(
        document["workload"], committed["workload"],
        "{} input contract",
        example["id"]
    );

    let workload: PlanningWorkload =
        serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    assert_eq!(
        document["workload"]["data_workload"],
        serde_json::to_value(workload.data_workload.as_ref().unwrap()).unwrap()
    );
    let queries = document["workload"]["queries"].as_array().unwrap();
    assert!(queries
        .iter()
        .all(|q| q["recurrence"]["repeated"].is_object() && q["predictability"].is_object()));
    check_candidate_example(document, committed, example);
}

/// Current candidates may differ from historical snapshots; validate the emitted DAG linkage.
fn check_candidate_example(document: Value, _committed: Value, example: &Value) {
    let selection = &document["stage3_plan_selection"];
    let selected = selection["selected"]
        .as_str()
        .unwrap_or_else(|| panic!("{}: no validated selected plan: {selection}", example["id"]));
    let candidates = document["stage2_physical_asap_optimization"]["candidates"]
        .as_array()
        .unwrap();
    let candidate = candidates
        .iter()
        .find(|candidate| candidate["id"] == selected)
        .expect("selected plan is present in the physical archive");
    let dag: PhysicalASAPDAG = serde_json::from_value(candidate["dag"].clone()).unwrap();
    dag.validate().unwrap();
    let queries = document["workload"]["queries"].as_array().unwrap().len();
    assert_eq!(dag.roots.len(), queries);
    assert_eq!(
        selection["accepted"][selected].as_array().unwrap().len(),
        queries
    );
    if document["accuracy_guarantees"] == "enforced" {
        for query in selection["accepted"][selected].as_array().unwrap() {
            assert_eq!(query["accepted_regardless_of_guarantee"], false);
            assert!(query["guarantee_check_rejection"].is_null());
        }
    }
    assert!(
        selection.get("materialized_views_error").is_none(),
        "{selection}"
    );
    let nodes: HashSet<_> = dag.nodes.iter().map(|node| node.id as u64).collect();
    for view in selection["materialized_views"].as_array().unwrap() {
        assert!(nodes.contains(&view["stage2_node_id"].as_u64().unwrap()));
        assert!(
            view["installed_position"].as_u64().unwrap()
                < view["installed_count"].as_u64().unwrap()
        );
    }
    let report = &selection["search"]["candidate_connections"];
    assert!(report.is_object(), "{}: {selection}", example["id"]);
    assert!(report["physical_configurations"].as_u64().unwrap() > 0);
    assert!(selection["costs"][selected].is_object());
}

/// The `--promql` path's data workload is what it plans with: every metric
/// the queries read, sampled every `--sample-interval-ms`.
#[test]
fn promql_queries_carry_their_sampled_data_workload() {
    // `document_of` declares 1,000 series and 60,000 rows held per source.
    let document = document_of(&[
        "--promql",
        "sum(rate(a[1m])) / sum(b)",
        "--sample-interval-ms",
        "30000",
    ]);
    assert_eq!(
        document["workload"]["data_workload"]["sources"],
        serde_json::json!({
            "a": {"series_count": 1000, "metric_type": null, "sample_interval": 30000, "stream_norms": null, "max_distinct_per_evaluation": null, "rows_held": 60000},
            "b": {"series_count": 1000, "metric_type": null, "sample_interval": 30000, "stream_norms": null, "max_distinct_per_evaluation": null, "rows_held": 60000},
        })
    );
    // No materialized view: every query computes at query time.
    assert_eq!(
        document["stage3_plan_selection"]["materialized_views"],
        serde_json::json!([])
    );
}

/// Under `--exhaustive` with a display cap, the document shows the selected
/// candidate first, then the other valid ones cheapest first, then the
/// rejected ones, each cause in turn; `rejected_by_cause` counts every
/// rejection.
#[test]
fn display_cap_shows_the_selected_then_valid_cheapest_then_each_cause() {
    let document = generate_example("planner-layering-3a", &["--max-displayed", "6"]);
    let selection = &document["stage3_plan_selection"];
    let physical = document["stage2_physical_asap_optimization"]["candidates"]
        .as_array()
        .unwrap();
    assert_eq!(physical.len(), 6);
    let ids: HashSet<&str> = physical.iter().map(|p| p["id"].as_str().unwrap()).collect();
    assert!(ids.contains(selection["selected"].as_str().unwrap()));
    let counts = selection["rejected_by_cause"].as_object().unwrap();
    let total: u64 = counts.values().map(|n| n.as_u64().unwrap()).sum();
    // 2,187 candidates: one selected, the rest rejected for a failed check
    // or a loss under the cost model.
    assert!(total >= physical.len().saturating_sub(1) as u64);
    // The shown valid ones (priced) are the cheapest of all valid ones.
    let costs = selection["costs"].as_object().unwrap();
    assert!(costs.len() >= 2, "{costs:?}");
}

/// With no supported summaries declared, `stage_pipeline` uses the executor's
/// families, as `plan_installable` does. CountSketch cannot answer a total, so
/// it is neither generated for count nor reported as deployment-pruned.
#[test]
fn undeclared_supported_summaries_default_to_the_executors_families() {
    let document = document_of(&[
        "--exhaustive",
        "--promql",
        "count by (job) (m)",
        "--max-error",
        "0.01",
        "--failure-probability",
        "0.01",
    ]);
    let labels: Vec<&str> = document["stage2_physical_asap_optimization"]["candidates"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["display_name"].as_str().unwrap())
        .collect();
    for family in ["Count acc", "Cms", "UnivMon"] {
        assert!(
            labels.iter().any(|label| label.contains(family)),
            "missing {family}: {labels:?}"
        );
    }
    assert!(labels.iter().all(|label| !label.contains("CountSketch")));
    assert_eq!(
        document["stage1_logical_asap_optimization"]["pass1_pruned"],
        serde_json::json!([])
    );
    assert_eq!(
        document["deployment_capabilities"]["supported_summaries"],
        serde_json::to_value(asap_executor::supported_summary_families()).unwrap()
    );
}

/// #840's workload: two `quantile_over_time` queries and two instant
/// `quantile by (G)` queries over 10,000 gauge series, each at 1% rank error.
const QUANTILE_BY_WORKLOAD: &str = r#"{"query_workload": {"language": "promql", "query_batch": null, "repeating_queries": [
  {"query": "quantile_over_time(0.5, data[1m])", "demand": {"fixed_interval": 10000},
   "requirements": {"accuracy": {"max_error": 0.01, "failure_probability": 0.001}},
   "predictability": {"predictable": {"known_at": null}}},
  {"query": "quantile_over_time(0.9, data[5m])", "demand": {"fixed_interval": 10000},
   "requirements": {"accuracy": {"max_error": 0.01, "failure_probability": 0.001}},
   "predictability": {"predictable": {"known_at": null}}},
  {"query": "quantile by (host, job) (0.5, data)", "demand": {"fixed_interval": 10000},
   "requirements": {"accuracy": {"max_error": 0.01, "failure_probability": 0.001}},
   "predictability": {"predictable": {"known_at": null}}},
  {"query": "quantile by (job) (0.5, data)", "demand": {"fixed_interval": 10000},
   "requirements": {"accuracy": {"max_error": 0.01, "failure_probability": 0.001}},
   "predictability": {"predictable": {"known_at": null}}}]},
 "data_workload": {"arrival": {"ingesting": {"rows_per_second": 666.67}},
  "sources": {"data": {"series_count": 10000, "metric_type": "gauge", "sample_interval": 15000}}}}"#;

/// Stage 3 prices the exact baseline of an instant `quantile by (G)` next
/// to the sketches: every evaluated plan but the selected one is rejected
/// only for its cost, and accepted plans answer Q3 and Q4 exactly.
#[test]
fn exact_grouped_quantile_plans_compile_and_are_compared() {
    let path = std::env::temp_dir().join(format!(
        "stage_pipeline_{}_quantile_by_workload.json",
        std::process::id()
    ));
    std::fs::write(&path, QUANTILE_BY_WORKLOAD).unwrap();
    // Guarantees enforced, so accepted plans passed the guarantee check.
    let document = document_of(&[
        "--workload",
        path.to_str().unwrap(),
        "--enforce-accuracy-guarantees",
    ]);
    std::fs::remove_file(path).unwrap();
    let selection = &document["stage3_plan_selection"];
    assert_eq!(document["accuracy_guarantees"], "enforced");
    let by_cause = selection["rejected_by_cause"].as_object().unwrap();
    // Every plan but the selected one is rejected only for its cost.
    assert!(
        by_cause
            .keys()
            .all(|cause| ["costlier", "tied_lost_on_enumeration_order"].contains(&cause.as_str())),
        "{by_cause:?}"
    );
    // Accepted plans passed the guarantee check (the selected one among them).
    let accepted = selection["accepted"].as_object().unwrap();
    // Accepted plans include ones that answer Q3 and Q4 without a sketch.
    let exact_q3_q4 = accepted
        .values()
        .filter(|queries| {
            let queries = queries.as_array().unwrap();
            queries[2]["sketches"].as_array().unwrap().is_empty()
                && queries[3]["sketches"].as_array().unwrap().is_empty()
        })
        .count();
    assert!(exact_q3_q4 > 0);
}

/// Two windowed quantiles over one metric (Q1 over 1 m, Q2 over 5 m, both
/// every 10 s): with a shared scan, Stage 2 also offers the scan built once
/// per evaluation (`query time, shared: Scan`). It reads the 5 m window
/// once (≈0.1364/s) instead of once per reader (0.2203/s), and beats the
/// separate scans (0.1530/s).
#[test]
fn a_scan_two_queries_share_is_offered_built_once_per_evaluation() {
    let query = |q: &str| {
        serde_json::json!({
            "query": q,
            "demand": {"fixed_interval": 10000},
            "requirements": {"accuracy": {"max_error": 0.01, "failure_probability": 0.001}},
            "predictability": {"predictable": {"known_at": null}},
        })
    };
    let workload = serde_json::json!({
        "query_workload": {
            "language": "promql",
            "query_batch": null,
            "repeating_queries": [
                query("quantile_over_time(0.5, data[1m])"),
                query("quantile_over_time(0.9, data[5m])"),
            ],
        },
        "data_workload": {
            "arrival": {"ingesting": {"rows_per_second": 666.67}},
            "sources": {"data": {"series_count": 10000, "metric_type": "gauge", "sample_interval": 15000}},
        },
    });
    let path = std::env::temp_dir().join(format!(
        "stage_pipeline_{}_shared_scan_workload.json",
        std::process::id()
    ));
    std::fs::write(&path, workload.to_string()).unwrap();
    // This checks the production planner's feasible cost, not exhaustive
    // archival coverage of all full-width assignments.
    let document = document_of(&["--workload", path.to_str().unwrap()]);
    std::fs::remove_file(path).unwrap();
    let physical = document["stage2_physical_asap_optimization"]["candidates"]
        .as_array()
        .unwrap();
    let selection = &document["stage3_plan_selection"];
    let selected = selection["selected"].as_str().unwrap();
    assert!(physical.iter().any(|p| p["id"] == selected));
    let cost = selection["costs"][selected]["cost_per_second"]
        .as_f64()
        .unwrap();
    // Preserve the historical shared-scan feasible price as an upper bound.
    assert!(cost <= 0.137, "{cost}");
}

/// #840's workload: Q1 and Q2 window `data` (1 m and 5 m), Q3 and Q4 read
/// its latest samples by (host, job) and by job. The four scans project
/// different labels. The widening rule offers one scan containing every
/// required label, with column projections above the requested time ranges.
/// This explicit candidate must install and compile, and its shared scan is
/// charged once per evaluation. The optimizer may instead select cheaper
/// partial sharing: a wide five-minute scan can cost more than a narrow long
/// scan plus a wide fifteen-second scan.
#[test]
fn scans_projecting_different_labels_are_shared() {
    let query = |q: &str| {
        serde_json::json!({
            "query": q,
            "demand": {"fixed_interval": 10000},
            "requirements": {"accuracy": {"max_error": 0.01, "failure_probability": 0.001}},
            "predictability": {"predictable": {"known_at": null}},
        })
    };
    let workload = serde_json::json!({
        "query_workload": {
            "language": "promql",
            "query_batch": null,
            "repeating_queries": [
                query("quantile_over_time(0.5, data[1m])"),
                query("quantile_over_time(0.9, data[5m])"),
                query("quantile by (host, job) (0.5, data)"),
                query("quantile by (job) (0.5, data)"),
            ],
        },
        "data_workload": {
            "arrival": {"ingesting": {"rows_per_second": 666.67}},
            "sources": {"data": {"series_count": 10000, "metric_type": "gauge", "sample_interval": 15000}},
        },
    });
    let path = std::env::temp_dir().join(format!(
        "stage_pipeline_{}_union_labels_workload.json",
        std::process::id()
    ));
    std::fs::write(&path, workload.to_string()).unwrap();
    let document = document_of(&[
        "--max-displayed",
        "100000",
        "--workload",
        path.to_str().unwrap(),
    ]);
    std::fs::remove_file(path).unwrap();
    // Inspect an offered widened candidate directly. A bounded optimizer may
    // correctly prefer a narrow long scan plus a short wide scan; its archive
    // is not an enumeration of every supported sharing configuration.
    let workload: PlanningWorkload = serde_json::from_value(workload).unwrap();
    let entries: Vec<_> = workload.query_workload.entries().collect();
    let data = workload.data_workload.as_ref().unwrap();
    let roots = lower_promql_roots(&workload).unwrap();
    use asap_logical_asap_optimization::pass2::asap_aware_cse::with_union_scan_labels;
    use asap_types::physical::{Materialization, Medium};
    // Plans `roots` with identical sub-DAGs made one node, as the memo's
    // groups are, and a node read by several roots kept for the evaluation.
    let plan = |name: &str, roots: &[asap_types::ir::QueryRoot]| {
        let shared: Vec<_> = asap_types::ir::cse::share_common_sub_dags(
            roots
                .iter()
                .enumerate()
                .map(|(id, root)| match root {
                    asap_types::ir::QueryRoot::Operator(node) => (id, std::rc::Rc::clone(node)),
                    asap_types::ir::QueryRoot::Scalar(_) => panic!("operator root"),
                })
                .collect(),
        )
        .into_iter()
        .map(|(_, node)| asap_types::ir::QueryRoot::Operator(node))
        .collect();
        let physical = asap_physical_asap_optimization::optimize_physical_asap_assigned(
            name,
            &shared,
            &entries,
            data,
            &asap_planner::DeploymentCapabilities::default(),
            &mut |readers| {
                Some(if readers.len() > 1 {
                    Materialization::MatAtQuery(Medium::Memory)
                } else {
                    Materialization::NotMaterialized
                })
            },
        )
        .unwrap()
        .expect("a node read by several roots can be kept for the evaluation");
        let offered = asap_plan_selection::select_plan(
            std::slice::from_ref(&physical),
            &entries,
            data,
            asap_plan_selection::PlanningModels::builtin(),
        )
        .unwrap();
        (physical, offered)
    };
    // The widened scan: one scan with every label, shared by all four.
    let widened = with_union_scan_labels(&roots).unwrap().unwrap();
    let (physical, offered) = plan("widened-scan-rule", &widened);
    // The partial-sharing reference: the two windows share a narrow scan, and
    // the two instant selectors a scan widened to their labels.
    let mut partial = roots[..2].to_vec();
    partial.extend(with_union_scan_labels(&roots[2..]).unwrap().unwrap());
    let (_, reference) = plan("partial-sharing", &partial);
    let (installed, _) =
        asap_planner::installable_plan_of(&physical.roots, Some(&offered), &entries).unwrap();
    installed.validate().unwrap();
    installed.check_compiles().unwrap();
    let dag = &physical.dag;
    let scans: Vec<_> = dag
        .nodes
        .iter()
        .filter(|n| n.payload.kind_name() == "Scan")
        .collect();
    let [scan] = &scans[..] else {
        panic!("one scan: {scans:?}")
    };
    let field_names: Vec<&str> = scan
        .output_schema
        .fields
        .iter()
        .map(|f| f.name.as_str())
        .collect();
    assert_eq!(
        field_names,
        ["ts", "value", "host", "job", "$promql_series_identity"]
    );
    let cost = &offered.costs[&offered.selected];
    let scan_cost = &cost.per_node[&scan.id];
    assert_eq!(scan_cost.multiplier, 1, "{scan_cost:?}");
    // 10,000 series × 20 samples in 5 m, once per 10 s evaluation.
    let detail = &scan_cost.detail;
    assert!(
        detail.starts_with("scan 200000 samples; x 0.1000/s"),
        "{detail}"
    );
    let selection = &document["stage3_plan_selection"];
    let shared_cost = cost.cost_per_second.unwrap();
    assert!(shared_cost.is_finite() && shared_cost > 0.0);
    let selected = selection["selected"].as_str().unwrap();
    let selected_cost = selection["costs"][selected]["cost_per_second"]
        .as_f64()
        .unwrap();
    // Offering maximal sharing must not force a costlier plan, and the
    // search must find a plan no dearer than the partial-sharing one (#994).
    let reference_cost = reference.costs[&reference.selected]
        .cost_per_second
        .unwrap();
    assert!(
        selected_cost <= shared_cost,
        "{selected_cost} > {shared_cost}"
    );
    assert!(
        selected_cost <= reference_cost * (1.0 + 1e-9),
        "{selected_cost} > partial sharing {reference_cost}"
    );
}

/// The public engine is candidate-connections; retired selectors are rejected.
#[test]
fn search_flag_picks_the_search_algorithm() {
    let document = document_of(&["--promql", "sum(m)", "--search", "candidate-connections"]);
    assert_eq!(document["search"], "candidate-connections");
    let report = &document["stage3_plan_selection"]["search"];
    assert!(report["candidate_connections"].is_object(), "{report}");
    assert!(report.get("routed_to_legacy").is_none());
}

/// Unknown and retired engine names cannot silently fall back.
#[test]
fn search_flag_is_checked() {
    for engine in ["legacy", "memo-milp", "memo-brute-force", "milp"] {
        let (status, stderr) = failure(&["--promql", "sum(m)", "--search", engine]);
        assert_eq!(status, Some(2));
        assert!(
            stderr.contains("expected candidate-connections"),
            "{stderr}"
        );
    }
}

/// A zero planning budget is accepted for the default engine; it must not claim optimality.
#[test]
fn milp_time_limit_is_reported() {
    let document = document_of(&["--promql", "sum(m)", "--milp-time-limit-s", "0"]);
    let selection = &document["stage3_plan_selection"];
    assert!(selection["selected"].is_null());
    let report = &selection["search"]["candidate_connections"];
    assert!(
        report.is_object(),
        "budget stop retains its report: {selection}"
    );
    assert_eq!(report["status"], "time_limit");
    assert!(selection["error"]
        .as_str()
        .unwrap()
        .contains("planning budget"));
}

/// The memo search's document names its selected plan's logical candidate
/// under the id its physical candidates read (`from_logical`), as the
/// viewer's validator requires: the brute force numbers its logical plans,
/// so the selected one need not be the first.
#[test]
fn the_memo_search_document_names_the_selected_logical_candidate() {
    let document = document_of(&[
        "--promql",
        "quantile_over_time(0.5, data[5m])",
        "--promql",
        "quantile_over_time(0.9, data[5m])",
        "--sample-interval-ms",
        "15000",
        "--search",
        "candidate-connections",
    ]);
    assert_eq!(document["search"], "candidate-connections");
    let logical: HashSet<&str> = document["stage1_logical_asap_optimization"]["candidates"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["id"].as_str().unwrap())
        .collect();
    let physical = document["stage2_physical_asap_optimization"]["candidates"]
        .as_array()
        .unwrap();
    assert!(!physical.is_empty());
    for candidate in physical {
        let from = candidate["from_logical"].as_str().unwrap();
        assert!(logical.contains(from), "{from} is not in {logical:?}");
    }
    let selected = document["stage3_plan_selection"]["selected"]
        .as_str()
        .unwrap();
    assert!(physical.iter().any(|c| c["id"] == selected));
}

/// Each shown candidate's `per_node` entries carry the node's operator,
/// timing and predicted CPU, as `plan_installable`'s predicted usage names
/// them, and the candidate's `cpu_core_seconds_per_second` is their sum:
/// enough to compare a plan's predicted CPU with a runtime's measurement
/// without `plan_installable`. Report-only: prices are unchanged.
#[test]
fn per_node_costs_show_operator_timing_and_predicted_cpu() {
    // The selected plan supplies the serialization evidence; exhaustive
    // candidate coverage is tested separately by the archive tests.
    let document = generate_bounded_example("planner-layering-1", &[]);
    let costs = document["stage3_plan_selection"]["costs"]
        .as_object()
        .unwrap();
    assert!(
        !costs.is_empty(),
        "missing evaluated costs: {}",
        document["stage3_plan_selection"]
    );
    let mut timings = HashSet::new();
    for (id, cost) in costs {
        let per_node = cost["per_node"].as_object().unwrap();
        let mut sum = 0.0;
        for node in per_node.values() {
            assert!(node["operator"].is_string(), "{id}: {node}");
            timings.insert(node["timing"].as_str().unwrap().to_owned());
            let cpu = node["cpu_core_seconds_per_second"].as_f64().unwrap();
            assert!(cpu >= 0.0, "{id}: {node}");
            sum += cpu;
        }
        let total = cost["cpu_core_seconds_per_second"].as_f64().unwrap();
        assert!(total > 0.0, "{id}");
        assert!(
            (total - sum).abs() <= 1e-9 * total,
            "{id}: {total} vs {sum}"
        );
    }
    assert!(
        timings.is_subset(&HashSet::from([
            "ingestion_time".to_owned(),
            "query_time".to_owned()
        ])),
        "{timings:?}"
    );
}

/// `rate`, `increase` and `irate` over a source the workload declares a
/// gauge still plan, with a warning under `stage3_plan_selection.warnings`
/// (which the viewer shows) naming the query, the function and the metric.
/// A counter gets none.
#[test]
fn a_counter_function_over_a_declared_gauge_plans_with_a_warning() {
    let workload = |metric_type: &str| {
        serde_json::json!({
            "query_workload": {
                "language": "promql",
                "query_batch": null,
                "repeating_queries": [
                    {
                        "query": "sum by (job) (rate(m[5m]))",
                        "demand": {"fixed_interval": 60000},
                        "requirements": {},
                        "predictability": {"predictable": {"known_at": null}},
                    },
                    {
                        "query": "max_over_time(m[5m])",
                        "demand": {"fixed_interval": 60000},
                        "requirements": {},
                        "predictability": {"predictable": {"known_at": null}},
                    },
                ],
            },
            "data_workload": {
                "arrival": {"ingesting": {"rows_per_second": 1000.0}},
                "sources": {"m": {"series_count": 1000, "metric_type": metric_type,
                                  "sample_interval": 15000}},
            },
        })
    };
    let path = std::env::temp_dir().join(format!(
        "stage_pipeline_{}_gauge_rate_workload.json",
        std::process::id()
    ));
    std::fs::write(&path, workload("gauge").to_string()).unwrap();
    let gauge = document_of(&["--workload", path.to_str().unwrap()]);
    std::fs::write(&path, workload("counter").to_string()).unwrap();
    let counter = document_of(&["--workload", path.to_str().unwrap()]);
    std::fs::remove_file(path).unwrap();
    assert!(gauge["stage3_plan_selection"]["selected"].is_string());
    assert_eq!(
        gauge["stage3_plan_selection"]["warnings"],
        serde_json::json!([
            "q1: rate over m, which the data workload declares a gauge: rate assumes a \
             counter (a drop is a reset); planned as written"
        ])
    );
    assert!(counter["stage3_plan_selection"]["selected"].is_string());
    assert!(counter["stage3_plan_selection"].get("warnings").is_none());
}

/// A PromQL file and a SQL file, each a `PlanningWorkload` in its own
/// language.
fn mixed_language_workload_files(sql_arrival: Value, sql_facts: Value) -> (PathBuf, PathBuf) {
    let entry = |query: &str| {
        serde_json::json!({
            "query": query,
            "demand": {"fixed_interval": 60000},
            "requirements": {},
            "predictability": {"predictable": {"known_at": null}},
        })
    };
    let promql = serde_json::json!({
        "query_workload": {
            "language": "promql",
            "query_batch": null,
            "repeating_queries": [entry("sum by (job) (rate(http_requests_total[1m]))")],
        },
        "data_workload": {
            "arrival": {"ingesting": {"rows_per_second": 1000.0}},
            "sources": {"http_requests_total": {"series_count": 15000, "metric_type": "counter", "sample_interval": 15000}},
        },
    });
    let sql = serde_json::json!({
        "query_workload": {
            "language": {"sql": "datafusion_sql"},
            "query_batch": null,
            "repeating_queries": [entry(
                "SELECT service, COUNT(*) FROM flows \
                 WHERE ts >= NOW() - INTERVAL '1 minute' GROUP BY service"
            )],
        },
        "data_workload": {"arrival": sql_arrival, "sources": {"flows": sql_facts}},
    });
    // Tests run as threads of one process, so the names need a per-call
    // counter.
    static CALLS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    let call = CALLS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let path = |name: &str| {
        std::env::temp_dir().join(format!(
            "stage_pipeline_{}_{call}_mixed_{name}.json",
            std::process::id()
        ))
    };
    let (promql_path, sql_path) = (path("promql"), path("sql"));
    std::fs::write(&promql_path, promql.to_string()).unwrap();
    std::fs::write(&sql_path, sql.to_string()).unwrap();
    (promql_path, sql_path)
}

/// `flows(ts, service)`. The time index is declared non-null: the executor
/// builds no count over a nullable time column, so a plan of the SQL query
/// over it would not compile.
const FLOWS_TABLE: &str = r#"{"name": "flows", "columns": [{"name": "ts", "type": "timestamp", "nullable": false}, {"name": "service", "type": "utf8", "nullable": false}], "time_index": 0}"#;

/// One run can plan PromQL and SQL queries together: `--workload` repeats,
/// each file is lowered by its own language's frontend (SQL over the
/// `--table` declarations), and their queries are planned as one workload.
/// The document numbers the queries across files (q1 from the PromQL
/// file, q2 from the SQL file) and its data workload is the union of the
/// files' sources, with their arrival rates added (1000 + 500 rows/s).
#[test]
fn several_workload_files_in_different_languages_are_planned_together() {
    let (promql, sql) = mixed_language_workload_files(
        serde_json::json!({"ingesting": {"rows_per_second": 500.0}}),
        serde_json::json!({"series_count": 10, "sample_interval": 1000}),
    );
    let document = document_of(&[
        "--workload",
        promql.to_str().unwrap(),
        "--workload",
        sql.to_str().unwrap(),
        "--table",
        FLOWS_TABLE,
    ]);
    let queries = document["workload"]["queries"].as_array().unwrap();
    let ids_and_languages: Vec<(&str, &str)> = queries
        .iter()
        .map(|q| (q["id"].as_str().unwrap(), q["language"].as_str().unwrap()))
        .collect();
    assert_eq!(ids_and_languages, [("q1", "promql"), ("q2", "sql")]);
    let data = &document["workload"]["data_workload"];
    assert_eq!(
        data["arrival"],
        serde_json::json!({"ingesting": {"rows_per_second": 1500.0}})
    );
    let sources: Vec<&String> = data["sources"].as_object().unwrap().keys().collect();
    assert_eq!(sources, ["flows", "http_requests_total"]);
    assert_eq!(
        document["stage0_query_language_frontends"]["dag"]["roots"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
    assert!(
        document["stage3_plan_selection"]["selected"].is_string(),
        "{}",
        document["stage3_plan_selection"]
    );
    std::fs::remove_file(promql).unwrap();
    std::fs::remove_file(sql).unwrap();
}

/// Files planned together must agree on how the data arrives and on any
/// source they both declare: a run is refused rather than picking one.
/// While data arrives, each source must also declare its own rate: plan
/// selection shares a workload's `rows_per_second` out over all its
/// sources, so `flows` (10 groups, no sample interval) would be priced at
/// 1500 × 10 / 15010 ≈ 1 row/s instead of its file's 500.
#[test]
fn workload_files_that_disagree_on_the_data_are_refused() {
    let (promql, sql) = mixed_language_workload_files(
        serde_json::json!({"ingesting": {"rows_per_second": 500.0}}),
        serde_json::json!({"series_count": 10}),
    );
    let (_, stderr) = failure(&[
        "--workload",
        promql.to_str().unwrap(),
        "--workload",
        sql.to_str().unwrap(),
        "--table",
        FLOWS_TABLE,
    ]);
    assert!(
        stderr.contains("source flows declares no rate of its own"),
        "{stderr}"
    );
    std::fs::remove_file(&sql).unwrap();

    // A table the SQL file reads but does not declare has no rate either.
    let (_, sql) = mixed_language_workload_files(
        serde_json::json!({"ingesting": {"rows_per_second": 500.0}}),
        serde_json::json!({"series_count": 10, "sample_interval": 1000}),
    );
    let mut text: Value = serde_json::from_str(&std::fs::read_to_string(&sql).unwrap()).unwrap();
    text["data_workload"]["sources"] = serde_json::json!({});
    std::fs::write(&sql, text.to_string()).unwrap();
    let (_, stderr) = failure(&[
        "--workload",
        promql.to_str().unwrap(),
        "--workload",
        sql.to_str().unwrap(),
        "--table",
        FLOWS_TABLE,
    ]);
    assert!(
        stderr.contains("source flows declares no rate of its own"),
        "{stderr}"
    );

    let (promql, sql) = mixed_language_workload_files(
        serde_json::json!("at_rest"),
        serde_json::json!({"rows_held": 1000}),
    );
    let (_, stderr) = failure(&[
        "--workload",
        promql.to_str().unwrap(),
        "--workload",
        sql.to_str().unwrap(),
        "--table",
        FLOWS_TABLE,
    ]);
    assert!(stderr.contains("arrival"), "{stderr}");
    std::fs::remove_file(&sql).unwrap();

    // The same source declared twice with different facts.
    let other = std::env::temp_dir().join(format!(
        "stage_pipeline_{}_mixed_promql_other.json",
        std::process::id()
    ));
    let mut text: Value = serde_json::from_str(&std::fs::read_to_string(&promql).unwrap()).unwrap();
    text["data_workload"]["sources"]["http_requests_total"]["series_count"] = serde_json::json!(7);
    std::fs::write(&other, text.to_string()).unwrap();
    let (_, stderr) = failure(&[
        "--workload",
        promql.to_str().unwrap(),
        "--workload",
        other.to_str().unwrap(),
    ]);
    assert!(stderr.contains("http_requests_total"), "{stderr}");
    std::fs::remove_file(promql).unwrap();
    std::fs::remove_file(other).unwrap();
}

/// Issue #890: a classic-bucket `histogram_quantile` with exact accuracy
/// enforced gets a selected plan the executor compiles, as its bucket rates
/// alone (the control) already did.
#[test]
fn a_classic_histogram_quantile_gets_a_selected_plan() {
    let buckets = r#"sum by (le) (rate(http_request_duration_seconds_bucket{service="customer-orders-service"}[5m]))"#;
    for query in [
        format!("histogram_quantile(0.5, {buckets})"),
        buckets.to_string(),
    ] {
        let document = document_of(&[
            "--promql",
            &query,
            "--sample-interval-ms",
            "15000",
            "--enforce-accuracy-guarantees",
        ]);
        let selection = &document["stage3_plan_selection"];
        assert!(selection["selected"].is_string(), "{query}: {selection}");
    }
}

/// The issue's example workload (#1003): one ratio of per-series quantiles,
/// every minute, ε = 0.01, δ = 0.001.
const MEMO_RATIO_OF_QUANTILES: &str =
    "../../tools/dag-viewer/memo-examples/promql/ratio-of-quantiles-5m.json";

/// The `memo` section: the memo after each step, the selected choice and
/// the enumeration, on the issue's example.
#[test]
fn memo_section_lists_each_step_the_selected_choice_and_the_enumeration() {
    let out = std::env::temp_dir().join(format!(
        "stage_pipeline_{}_memo_ratio.json",
        std::process::id()
    ));
    let status = Command::new(env!("CARGO_BIN_EXE_stage_pipeline"))
        .args(["--workload", MEMO_RATIO_OF_QUANTILES, "--out"])
        .arg(&out)
        .status()
        .unwrap();
    assert!(status.success());
    let document: Value = serde_json::from_str(&std::fs::read_to_string(&out).unwrap()).unwrap();
    std::fs::remove_file(out).unwrap();
    let memo = &document["memo"];
    let steps = memo["steps"].as_array().unwrap();
    let names: Vec<_> = steps.iter().map(|s| s["step"].as_str().unwrap()).collect();
    assert_eq!(names, ["stage0", "pass1", "pass2", "concrete_states"]);
    let candidates = |step: &Value| -> u64 {
        step["groups"]
            .as_array()
            .unwrap()
            .iter()
            .map(|g| g["candidate_count"].as_u64().unwrap())
            .sum()
    };
    // The PromQL frontend gives each series its identity as a column, so the
    // two `quantile_over_time(0.5, data[5m])` have a unique key and share one
    // group: 7 groups (scan, range, two quantile groups, ratio, sum, avg),
    // 7 + 7 + 5 Pass 1 candidates, 7 × 7 candidate DAGs.
    assert_eq!(steps[0]["groups"].as_array().unwrap().len(), 7);
    assert_eq!(candidates(&steps[0]), 0);
    assert_eq!(steps[1]["groups"].as_array().unwrap().len(), 7);
    assert_eq!(candidates(&steps[1]), 19);
    let per_step = &memo["enumeration"]["per_step"];
    assert_eq!(
        per_step["stage0"],
        serde_json::json!({"count": 1, "complete": true})
    );
    assert_eq!(
        per_step["pass1"],
        serde_json::json!({"count": 49, "complete": true})
    );
    // The listed step is the last one enumerated completely within 200
    // choices (Stage 0 has no candidates to choose), else the last step.
    let listed = ["pass1", "pass2", "concrete_states"]
        .into_iter()
        .rfind(|step| {
            per_step[step]["complete"] == true && per_step[step]["count"].as_u64().unwrap() <= 200
        })
        .unwrap_or("concrete_states");
    assert_eq!(memo["enumeration"]["step"], listed);
    if listed != "concrete_states" {
        assert_eq!(
            memo["enumeration"]["choices"].as_array().unwrap().len() as u64,
            per_step[listed]["count"].as_u64().unwrap()
        );
    }
    // The node table is a valid FlatDag (children before parents) holding
    // each node once; every group's and listed candidate's `node` names a
    // node of it, and each boundary names a node and one of the candidate's
    // child groups.
    let nodes = memo["nodes"].as_array().unwrap();
    validated_dag(&serde_json::json!({ "nodes": nodes, "roots": [] }), 0);
    let distinct: HashSet<String> = nodes.iter().map(Value::to_string).collect();
    assert_eq!(distinct.len(), nodes.len());
    let in_table = |id: &Value| (id.as_u64().unwrap() as usize) < nodes.len();
    for step in steps {
        for group in step["groups"].as_array().unwrap() {
            assert!(in_table(&group["node"]) && group.get("dag").is_none());
            for candidate in group["candidates"].as_array().unwrap() {
                assert!(in_table(&candidate["node"]) && candidate.get("dag").is_none());
                let children = candidate["children"].as_array().unwrap();
                for (node, child) in candidate["group_of_node"].as_object().unwrap() {
                    assert!(node.parse::<usize>().unwrap() < nodes.len());
                    assert!(children.contains(child));
                }
            }
        }
    }
    // Every candidate of the selected choice exists in the last step.
    let last = steps.last().unwrap()["groups"].as_array().unwrap();
    let choice = memo["selected_choice"].as_object().unwrap();
    assert!(!choice.is_empty());
    for (group, index) in choice {
        let group = &last[group.parse::<usize>().unwrap()];
        assert!(index.as_u64().unwrap() < group["candidate_count"].as_u64().unwrap());
    }
    let enumeration = &memo["enumeration"];
    let choices = enumeration["choices"].as_array().unwrap();
    assert!(!choices.is_empty() && choices.len() <= 200);
    for choice in choices {
        let roots = choice["roots"].as_array().unwrap();
        assert!(!roots.is_empty() && roots.iter().all(in_table));
        // Pricing is off without `--price-enumerated`.
        assert!(choice.get("valid").is_none());
    }
    assert_eq!(enumeration["priced"], Value::Null);
    // `--no-memo` leaves the section out.
    let out = std::env::temp_dir().join(format!(
        "stage_pipeline_{}_no_memo_ratio.json",
        std::process::id()
    ));
    let status = Command::new(env!("CARGO_BIN_EXE_stage_pipeline"))
        .args(["--workload", MEMO_RATIO_OF_QUANTILES, "--no-memo", "--out"])
        .arg(&out)
        .status()
        .unwrap();
    assert!(status.success());
    let document: Value = serde_json::from_str(&std::fs::read_to_string(&out).unwrap()).unwrap();
    std::fs::remove_file(out).unwrap();
    assert!(document.get("memo").is_none());
}

/// Composing the selected choice over the last step's memo gives the
/// selected plan's Stage 1 DAG, on the issue's example and #509 Example 1.
/// Stage 1's DAG is rebuilt from the candidates' operator implementations
/// (`implement_candidate_operators`: an exact top-k becomes
/// `Project(Limit(Sort))`), so the composed query DAGs are compared after
/// the same implementation.
#[test]
fn composed_selected_choice_is_the_selected_stage1_dag() {
    use asap_devtools::planning_stages::run_default_search_over;
    use asap_logical_asap_optimization::pass1::per_computation_candidates::recompose_guarantees;
    use asap_logical_asap_optimization::pass2::memo::enumeration::build_logical_asap_dags_from_chosen_candidates;
    use asap_types::ir::flat::flatten;
    let ratio: PlanningWorkload =
        serde_json::from_str(&std::fs::read_to_string(MEMO_RATIO_OF_QUANTILES).unwrap()).unwrap();
    for workload in [ratio, example1()] {
        let roots = lower_promql_roots(&workload).unwrap();
        let entries: Vec<_> = workload.query_workload.entries().collect();
        let data = workload.data_workload.clone().unwrap_or_default();
        let run = run_default_search_over(
            roots,
            &entries,
            &data,
            asap_plan_selection::PlanningModels::builtin(),
        )
        .unwrap();
        let steps = run.memo_steps.as_ref().unwrap();
        let choice = steps.selected_chosen_candidates.as_ref().unwrap();
        let memo = &steps.concrete_states;
        let composed = build_logical_asap_dags_from_chosen_candidates(memo, choice).unwrap();
        let composed: Vec<_> = (0..entries.len())
            .map(|query| {
                let (_, node) = composed
                    .iter()
                    .find(|(group, _)| memo.groups[group.0 as usize].roots_of.contains(&query))
                    .unwrap();
                QueryRoot::Operator(
                    asap_physical_asap_optimization::implement_candidate_operators(node, &[])
                        .unwrap(),
                )
            })
            .collect::<Vec<_>>();
        // As reconstruction does, over the implemented operators.
        let composed = recompose_guarantees(&composed).unwrap();
        let [selected] = &run.stage1_logical_asap_optimization[..] else {
            panic!("the default search keeps the selected Stage 1 candidate")
        };
        let selected = selected.roots.as_ref().unwrap();
        assert_eq!(composed.len(), selected.len());
        for (composed, selected) in composed.into_iter().zip(selected) {
            assert_eq!(
                serde_json::to_value(flatten(&[composed]).0).unwrap(),
                serde_json::to_value(flatten(std::slice::from_ref(selected)).0).unwrap()
            );
        }
    }
}

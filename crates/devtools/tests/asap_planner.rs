//! `asap_planner` writes valid stage documents: each stage's output over
//! #509's examples and ad-hoc workloads, the deployment and cost inputs it
//! plans with, and the inputs it refuses.
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use asap_types::ir::flat::FlatDag;
use asap_types::ir::physical_export::PhysicalASAPDAG;
use asap_types::ir::QueryRoot;
use asap_types::workload::PlanningWorkload;

use serde_json::{json, Value};

/// A file under the system temp directory, removed when dropped.
struct TempFile(PathBuf);

impl TempFile {
    /// A new file holding `contents`. Tests run as threads of one process,
    /// so the name needs a per-call counter.
    fn new(contents: &str) -> Self {
        static CALLS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let call = CALLS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "asap_planner_test_{}_{call}.json",
            std::process::id()
        ));
        std::fs::write(&path, contents).unwrap();
        Self(path)
    }

    fn json(value: &Value) -> Self {
        Self::new(&value.to_string())
    }

    fn path(&self) -> &str {
        self.0.to_str().unwrap()
    }
}

impl Drop for TempFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// `asap_planner` with `args`.
fn planner(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_asap_planner"))
        .args(args)
        .output()
        .unwrap()
}

/// The JSON document `asap_planner` writes for `args`; the run exits 1
/// exactly when a stage section records an error, else 0.
fn document_of(args: &[&str]) -> Value {
    let mut args = args.to_vec();
    args.extend(["--format", "json"]);
    let output = planner(&args);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let document: Value = serde_json::from_str(&String::from_utf8(output.stdout).unwrap())
        .unwrap_or_else(|e| panic!("{e}: {stderr}"));
    let failed = [
        "stage1_logical_asap_optimization",
        "stage2_physical_asap_optimization",
        "stage3_plan_selection",
    ]
    .iter()
    .any(|stage| !document[stage]["error"].is_null());
    assert_eq!(output.status.code(), Some(i32::from(failed)), "{stderr}");
    document
}

/// The document for the workload `workload` with `args`.
fn document(workload: &Value, args: &[&str]) -> Value {
    let file = TempFile::json(workload);
    let mut all = vec!["--workload", file.path()];
    all.extend_from_slice(args);
    document_of(&all)
}

/// `asap_planner`'s standard error for `args`, whose input it must refuse
/// (status 1, without the usage line, which is for bad arguments) without
/// writing output.
fn refused(args: &[&str]) -> String {
    let output = planner(args);
    assert_eq!(output.status.code(), Some(1), "{args:?}");
    assert!(output.stdout.is_empty(), "a refused run writes nothing");
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(!stderr.contains("usage:"), "{stderr}");
    stderr
}

/// The committed workload file of #509's `example` (`example1`, `example2`,
/// `example3a`, `example3b`).
fn example(example: &str) -> String {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../tools/dag-viewer/workloads/planner-layering")
        .join(format!("{example}.json"))
        .to_str()
        .unwrap()
        .to_owned()
}

/// The document for `example` with `args`.
fn example_document(name: &str, args: &[&str]) -> Value {
    let path = example(name);
    let mut all = vec!["--workload", path.as_str()];
    all.extend_from_slice(args);
    document_of(&all)
}

/// `tables.json`'s tables, each as a `--sql-catalog-table` argument.
fn catalog_tables(tables_json: &Path) -> Vec<String> {
    serde_json::from_str::<Vec<Value>>(&std::fs::read_to_string(tables_json).unwrap())
        .unwrap()
        .iter()
        .map(Value::to_string)
        .collect()
}

/// An exact accuracy target.
fn exact() -> Value {
    json!({"max_error": 0.0, "failure_probability": 0.0})
}

/// Within `max_error` with probability `1 - failure_probability`.
fn within(max_error: f64, failure_probability: f64) -> Value {
    json!({"max_error": max_error, "failure_probability": failure_probability})
}

/// One-off PromQL `queries`, each run once with `accuracy` and unknown
/// predictability, reading `metrics`, each 1,000 series sampled every 15 s
/// and holding 60,000 rows, with an unknown arrival.
fn adhoc(queries: &[&str], accuracy: Value, metrics: &[&str]) -> Value {
    let facts = json!({"series_count": 1000, "sample_interval": 15000, "rows_held": 60000});
    json!({
        "query_workload": {"language": "promql", "query_batch": queries.iter().map(|query| json!({
            "query": query,
            "requirements": {"accuracy": accuracy},
            "predictability": "unknown",
            "invocations": 1,
        })).collect::<Vec<_>>()},
        "data_workload": {
            "arrival": "unknown",
            "sources": metrics.iter().map(|m| (m.to_string(), facts.clone())).collect::<serde_json::Map<_, _>>(),
        },
    })
}

/// A repeating PromQL entry every 10 s within 1% (failure probability 0.001).
fn every_10s_within_1_percent(query: &str) -> Value {
    json!({
        "query": query,
        "demand": {"fixed_interval_at": {"interval": 10000, "evaluation_phase": 0}},
        "requirements": {"accuracy": {"max_error": 0.01, "failure_probability": 0.001}},
        "predictability": {"predictable": {"known_at": null}},
    })
}

/// A `--cost-inputs` file of `inputs`.
fn cost_inputs(inputs: &asap_cost::CostInputs) -> TempFile {
    TempFile::new(&serde_json::to_string(inputs).unwrap())
}

/// Cost model 1 at AWS Fargate's us-east-1 costs.
fn aws_fargate() -> asap_cost::CostInputs {
    let defaults = asap_cost::CostInputs::default();
    asap_cost::CostInputs {
        cost_model: asap_cost::CostModel::UsageOverTime(
            asap_cost::ResourceUnitCost::aws_fargate_us_east_1(defaults.resource_speed),
        ),
        ..defaults
    }
}

/// Stage 3 selected a plan: `stage3_plan_selection` has its `plan` and the
/// plan's `cost`.
fn selects_a_plan(selection: &Value) -> bool {
    selection["plan"].is_object() && selection["cost"].is_object()
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

/// Stage 3's plan, validated, with one root per operator query of
/// `queries`.
fn selected_physical(document: &Value, queries: usize) -> PhysicalASAPDAG {
    let dag: PhysicalASAPDAG =
        serde_json::from_value(document["stage3_plan_selection"]["plan"].clone()).unwrap();
    dag.validate().unwrap();
    assert_eq!(dag.roots.len() + dag.scalar_roots.len(), queries);
    dag
}

/// Every candidate label of the memo step `step`.
fn memo_labels(document: &Value, step: &str) -> Vec<String> {
    document["memo"]["steps"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["step"] == step)
        .unwrap()["groups"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|g| g["candidates"].as_array().unwrap())
        .map(|c| c["label"].as_str().unwrap().to_owned())
        .collect()
}

/// Example 1's document: the frontends' DAG is a well-formed flat DAG with
/// one root per query; each physical candidate names a candidate of the
/// final memo, with unique ids and no cost; Stage 3's plan validates.
#[test]
fn example1_document_validates_its_stage_dags() {
    let document = example_document("example1", &["--run-until", "stage3"]);
    assert_eq!(document["format"], "asap-stage-pipeline/v2");
    let queries = document["workload"]["queries"].as_array().unwrap().len();
    assert_eq!(queries, 2);
    validated_dag(&document["stage0_query_language_frontends"]["dag"], queries);

    let pass2 = memo_step(&document, "pass2");
    let physical = document["stage2_physical_asap_optimization"]["physical_candidates"]
        .as_array()
        .unwrap();
    assert!(!physical.is_empty());
    for (id, candidate) in physical.iter().enumerate() {
        assert_eq!(candidate["id"], id);
        assert!(candidate.get("cost").is_none(), "Stage 2 has no cost");
        let group = &pass2["groups"][candidate["group"].as_u64().unwrap() as usize];
        assert!(
            group["candidates"][candidate["index_in_group"].as_u64().unwrap() as usize].is_object()
        );
    }
    assert!(selects_a_plan(&document["stage3_plan_selection"]));
    selected_physical(&document, queries);
}

/// The memo step `step` of `document`.
fn memo_step<'a>(document: &'a Value, step: &str) -> &'a Value {
    document["memo"]["steps"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["step"] == step)
        .unwrap_or_else(|| panic!("no memo step {step}"))
}

/// A top-k partitioned by every label of its input has no item, so a heap
/// over it could not be composed; Pass 1 offers no heap for it (a heap
/// would read one row per item), and a plan is selected.
#[test]
fn a_topk_heap_over_one_row_per_item_is_not_offered() {
    let document = document(
        &adhoc(
            &["topk by (job) (3, sum by (job) (rate(m[5m])))"],
            within(0.01, 0.001),
            &["m"],
        ),
        &["--run-until", "stage3", "--ignore-accuracy-guarantees"],
    );
    let labels = memo_labels(&document, "pass1");
    assert!(
        labels.iter().all(|label| !label.contains("WithHeap")),
        "{labels:?}"
    );
    selected_physical(&document, 1);
}

/// Example 3a (243 Pass 1 candidates) selects a plan whose physical DAG
/// validates with its five queries.
#[test]
fn example3a_selects_a_valid_plan() {
    let document = example_document(
        "example3a",
        &["--run-until", "stage3", "--ignore-accuracy-guarantees"],
    );
    assert!(selects_a_plan(&document["stage3_plan_selection"]));
    selected_physical(&document, 5);
}

/// Example 2's three SQL queries over `flows` lower to one well-formed DAG
/// with a root per query.
#[test]
fn example2_lowers_its_three_sql_queries() {
    let tables = catalog_tables(
        &PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../tools/dag-viewer/workloads/planner-layering/tables.json"),
    );
    let document = example_document(
        "example2",
        &["--run-until", "stage0", "--sql-catalog-table", &tables[0]],
    );
    let queries = document["workload"]["queries"].as_array().unwrap();
    assert_eq!(queries.len(), 3);
    assert!(queries.iter().all(|q| q["language"] == "sql"));
    validated_dag(&document["stage0_query_language_frontends"]["dag"], 3);
}

/// Example 2 plans its three SQL queries and selects a plan.
#[test]
fn example2_plans_the_sql_workload() {
    let tables = catalog_tables(
        &PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../tools/dag-viewer/workloads/planner-layering/tables.json"),
    );
    let document = example_document(
        "example2",
        &[
            "--run-until",
            "stage3",
            "--ignore-accuracy-guarantees",
            "--sql-catalog-table",
            &tables[0],
        ],
    );
    assert!(selects_a_plan(&document["stage3_plan_selection"]));
    selected_physical(&document, 3);
}

/// `metrics(ts, service, latency)`.
const METRICS_TABLE: &str = r#"{"name": "metrics", "columns": [{"name": "ts", "type": "timestamp", "nullable": false},
    {"name": "service", "type": "utf8", "nullable": false},
    {"name": "latency", "type": "float64", "nullable": false}], "time_index": 0}"#;

/// One-off exact SQL `queries`, each run once, over `metrics`, which holds
/// 1,000 series and 60,000 rows.
fn adhoc_sql(queries: &[&str]) -> Value {
    json!({
        "query_workload": {"language": {"sql": "datafusion_sql"}, "query_batch": queries.iter().map(|query| json!({
            "query": query,
            "requirements": {"accuracy": exact()},
            "predictability": "unknown",
            "invocations": 1,
        })).collect::<Vec<_>>()},
        "data_workload": {"arrival": "unknown", "sources": {"metrics": {"series_count": 1000, "rows_held": 60000}}},
    })
}

/// A count and an average by service over `metrics`.
fn two_sql_queries() -> Value {
    adhoc_sql(&[
        "SELECT service, COUNT(*) FROM metrics GROUP BY service",
        "SELECT service, AVG(latency) FROM metrics GROUP BY service",
    ])
}

/// SQL queries over tables declared with `--sql-catalog-table` are lowered
/// by the SQL frontend, and the document records each query's language.
#[test]
fn sql_queries_over_declared_tables_are_lowered() {
    let document = document(
        &two_sql_queries(),
        &[
            "--run-until",
            "stage0",
            "--sql-catalog-table",
            METRICS_TABLE,
        ],
    );
    let queries = document["workload"]["queries"].as_array().unwrap();
    assert_eq!(queries.len(), 2);
    assert!(queries.iter().all(|q| q["language"] == "sql"));
    validated_dag(&document["stage0_query_language_frontends"]["dag"], 2);
}

/// SQL queries over declared tables are planned through every stage to a
/// selected plan.
#[test]
fn sql_queries_over_declared_tables_are_planned() {
    let document = document(
        &two_sql_queries(),
        &[
            "--run-until",
            "stage3",
            "--ignore-accuracy-guarantees",
            "--sql-catalog-table",
            METRICS_TABLE,
        ],
    );
    assert!(selects_a_plan(&document["stage3_plan_selection"]));
    selected_physical(&document, 2);
}

/// Nine approximate quantile queries over nine metrics, three candidates
/// each: 3^9 = 19683 Pass 1 combinations.
fn nine_quantile_queries() -> Value {
    let queries: Vec<String> = (1..=9)
        .map(|i| format!("quantile_over_time(0.99, m{i}[5m])"))
        .collect();
    let metrics: Vec<String> = (1..=9).map(|i| format!("m{i}")).collect();
    adhoc(
        &queries.iter().map(String::as_str).collect::<Vec<_>>(),
        within(0.01, 0.001),
        &metrics.iter().map(String::as_str).collect::<Vec<_>>(),
    )
}

/// Nine quantile queries plan with the candidate-connection search, which
/// searches Stage 2's catalog without enumerating the combinations. The
/// document holds Stage 2's candidates, the plan and the selection MILP's
/// size.
#[test]
fn nine_queries_plan_without_cartesian_enumeration() {
    let document = document(
        &nine_quantile_queries(),
        &["--run-until", "stage3", "--ignore-accuracy-guarantees"],
    );
    assert_eq!(document["search"], "candidate-connections");
    let selection = &document["stage3_plan_selection"];
    assert!(selects_a_plan(selection), "{selection}");
    let physical = document["stage2_physical_asap_optimization"]["physical_candidates"]
        .as_array()
        .unwrap();
    assert!(!physical.is_empty());
    assert!(
        selection["milp_variables"].as_u64().unwrap() > 0,
        "{selection}"
    );
}

/// A deployment that keeps neither raw data nor query-time results.
const NO_RAW: &str = r#"{"raw_data_retained": false, "query_time_results_retained": false}"#;

/// ASAPQuery's random workload (seed 862, workload-01): nine PromQL queries.
const NINE_QUERY_WORKLOAD: &str =
    "../../tools/dag-viewer/workloads/random-mixed-seed-862/workload-01.json";

/// The nine-query workload's document up to `run_until`, planned as a
/// deployment that keeps no raw data at AWS Fargate costs.
fn nine_query_document(run_until: &str) -> Value {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(NINE_QUERY_WORKLOAD);
    let capabilities = TempFile::new(NO_RAW);
    let fargate = cost_inputs(&aws_fargate());
    document_of(&[
        "--workload",
        path.to_str().unwrap(),
        "--run-until",
        run_until,
        "--deployment-capabilities",
        capabilities.path(),
        "--cost-inputs",
        fargate.path(),
    ])
}

/// The nine-query workload, planned as a deployment that keeps no raw data
/// at AWS Fargate costs with guarantees enforced: `asap_planner` lowers it
/// to one root per query, and installation returns a validated plan that
/// accepts no query without its guarantee.
#[test]
fn the_nine_query_workload_lowers_and_installs() {
    let document = nine_query_document("stage2");
    assert_eq!(document["search"], "candidate-connections");
    assert_eq!(document["accuracy_guarantees"], "enforced");
    let queries = document["workload"]["queries"].as_array().unwrap().len();
    assert_eq!(queries, 9);
    validated_dag(&document["stage0_query_language_frontends"]["dag"], queries);

    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(NINE_QUERY_WORKLOAD);
    let workload: PlanningWorkload =
        serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    let settings = aws_fargate();
    let capabilities: asap_planner::DeploymentCapabilities = serde_json::from_str(NO_RAW).unwrap();
    let models = asap_planner::PlanningModels::builtin()
        .with_cost_inputs(&settings)
        .with_deployment_capabilities(&capabilities);
    let (plan, report) = tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap()
        .block_on(asap_planner::plan_installable(
            asap_planner::UserInput::new(&workload, asap_planner::FrontendInput::Promql, models),
        ))
        .unwrap();
    plan.validate().unwrap();
    assert_eq!(plan.query_texts.len(), queries);
    let report = report.unwrap();
    assert!(report
        .cost_per_second
        .is_some_and(|cost| cost.is_finite() && cost > 0.0));
    assert_eq!(report.predicted_usage.per_query.len(), queries);
}

/// `asap_planner` selects a validated plan for the nine-query workload
/// (no raw data, AWS Fargate costs, guarantees enforced), with an
/// accepted accuracy per query and the selection MILP's size.
#[test]
fn the_nine_query_workload_selects_a_valid_plan() {
    let document = nine_query_document("stage3");
    let queries = 9;
    let selection = &document["stage3_plan_selection"];
    assert!(selects_a_plan(selection), "{selection}");
    selected_physical(&document, queries);
    assert!(selection["milp_variables"].as_u64().unwrap() > 0);
    assert_eq!(
        selection["accepted_accuracy_per_query"]
            .as_array()
            .unwrap()
            .len(),
        queries
    );
}

/// A bare instant selector looks back its metric's sample interval, as
/// the data workload declares it: `sum(up)` over `up` sampled every 30 s
/// reads a 30 s range.
#[test]
fn the_sample_interval_sets_the_instant_selector_lookback() {
    let mut workload = adhoc(&["sum(up)"], exact(), &["up"]);
    workload["data_workload"]["sources"]["up"]["sample_interval"] = json!(30000);
    let document = document(&workload, &["--run-until", "stage0"]);
    let ranges: Vec<&Value> = document["stage0_query_language_frontends"]["dag"]["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|node| node["operator"]["NonASAP"]["TimeRange"]["range"].as_object())
        .map(|range| &range["secs"])
        .collect();
    assert_eq!(ranges, [&Value::from(30)]);
}

/// A table declared twice is refused.
#[test]
fn table_declared_twice_is_rejected() {
    let table =
        r#"{"name": "metrics", "columns": [{"name": "ts", "type": "timestamp"}], "time_index": 0}"#;
    let workload = TempFile::json(&adhoc_sql(&["SELECT COUNT(*) FROM metrics"]));
    let stderr = refused(&[
        "--workload",
        workload.path(),
        "--run-until",
        "stage0",
        "--sql-catalog-table",
        table,
        "--sql-catalog-table",
        table,
    ]);
    assert!(
        stderr.contains("--sql-catalog-table: table metrics is declared twice"),
        "{stderr}"
    );
}

/// Standalone scalar queries first, in the middle and last keep their
/// query positions in the selected physical DAG's `scalar_roots` (the
/// viewer's test_viewer.py draws this workload).
#[test]
fn scalar_queries_keep_their_positions_in_scalar_roots() {
    let document = document(
        &adhoc(
            &[
                "scalar(sum(m))",
                "sum(n)",
                "time()",
                "sum by (job) (k)",
                "1 + 2",
            ],
            exact(),
            &["k", "m", "n"],
        ),
        &["--run-until", "stage3", "--ignore-accuracy-guarantees"],
    );
    assert!(selects_a_plan(&document["stage3_plan_selection"]));
    let positions: Vec<_> = document["stage3_plan_selection"]["plan"]["scalar_roots"]
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
    let workload = TempFile::json(&adhoc_sql(&["SELECT COUNT(*) FROM metrics"]));
    for index in [r#""0""#, "-1", "1.5", "3"] {
        let table = format!(
            r#"{{"name": "metrics", "columns": [{{"name": "ts", "type": "timestamp"}}], "time_index": {index}}}"#
        );
        let stderr = refused(&[
            "--workload",
            workload.path(),
            "--run-until",
            "stage0",
            "--sql-catalog-table",
            &table,
        ]);
        assert!(
            stderr.contains(
                "--sql-catalog-table metrics: time_index must be the index of one of its 1 columns"
            ),
            "{index}: {stderr}"
        );
    }
}

/// A query whose target sets no metric shows the metric its result is
/// naturally stated in (the default-metric table, display only): a top-k's
/// is the L1-scaled absolute error; an exact query shows none.
#[test]
fn a_target_without_a_metric_shows_the_query_default() {
    let document = example_document("example1", &["--run-until", "stage0"]);
    let queries = &document["workload"]["queries"];
    assert!(queries[0]["requirements"].get("default_metric").is_none());
    assert_eq!(
        queries[1]["requirements"]["default_metric"],
        "absolute_stream_l1"
    );
}

/// A quantile query's target may leave its metric unset: then Pass 1 keeps
/// both KLL (rank) and DDSketch (relative) candidates, which compete on
/// cost, and the selection reports what it accepted. With
/// `accuracy_metric: rank`, the metric rule leaves DDSketch no edge and the
/// selection accepts rank (or exact). The workload's
/// accuracy is the target's own JSON form. Guarantees are enforced (the
/// default), since the test is about which guarantees meet the target.
#[test]
fn a_quantile_target_without_a_metric_accepts_rank_or_relative() {
    let query = |metric: Option<&str>| {
        let mut accuracy = json!({"max_error": 0.01});
        if let Some(metric) = metric {
            accuracy["accuracy_metric"] = metric.into();
        }
        let workload = json!({
            "query_workload": {"language": "promql", "query_batch": [{
                "query": "quantile_over_time(0.99, m[1m])",
                "requirements": {"accuracy": accuracy},
                "predictability": "unknown", "invocations": 1
            }]},
            // Relative DDS interpolation requires this explicit domain proof.
            "data_workload": {"arrival": "unknown", "sources": {"m": {
                "sample_interval": 1000, "series_count": 1, "column_signs": {"value": "non_negative"}
            }}}
        });
        document(&workload, &["--run-until", "stage3"])
    };
    let summary = |document: &Value| -> String {
        let selection = &document["stage3_plan_selection"];
        assert!(selects_a_plan(selection), "{selection}");
        selection["accepted_accuracy_per_query"][0]["summary"]
            .as_str()
            .unwrap()
            .to_owned()
    };
    let unset = query(None);
    assert_eq!(unset["accuracy_guarantees"], "enforced");
    assert_eq!(
        unset["workload"]["queries"][0]["requirements"]["accuracy"],
        json!({"max_error": 0.01, "accuracy_scope": "whole_result"})
    );
    // A quantile has no single default metric to show.
    assert!(unset["workload"]["queries"][0]["requirements"]
        .get("default_metric")
        .is_some_and(Value::is_null));
    let labels = memo_labels(&unset, "pass1");
    for family in ["Kll", "DDSketch"] {
        assert!(
            labels.iter().any(|l| l.contains(family)),
            "{family}: {labels:?}"
        );
    }
    let accepted = summary(&unset);
    assert!(accepted.starts_with("accepted "), "{accepted}");

    // A DDSketch states a relative error, which a rank target cannot
    // read (the metric rule): only KLL or an exact plan is accepted.
    let rank = query(Some("rank"));
    let accepted = summary(&rank);
    assert!(
        accepted == "accepted rank (Kll)" || accepted == "accepted exact",
        "{accepted}"
    );
}

/// The `--cost-inputs` cost model decides how Example 4's candidates are
/// compared: cost model 1 (the default) selects a plan with a cost per
/// second, and Stage 3's selection MILP refuses cost models 2 and 3 with
/// an error saying it implements cost model 1 only (#888).
#[test]
fn the_cost_model_chooses_how_candidates_are_compared() {
    use asap_cost::{ClusterLimits, ClusterObjective, CostInputs, CostModel, ResourceUnitCost};
    let selected = |cost_model: CostModel| {
        let file = cost_inputs(&CostInputs {
            cost_model,
            ..Default::default()
        });
        let args = ["--run-until", "stage3", "--cost-inputs", file.path()];
        example_document("example3b", &args)["stage3_plan_selection"].clone()
    };
    let usage = selected(CostModel::UsageOverTime(ResourceUnitCost::illustrative()));
    assert!(selects_a_plan(&usage), "{usage}");
    assert!(usage["cost"]["cost_per_second"].is_number());
    for cost_model in [
        CostModel::ProvisionedCapacity(ResourceUnitCost::illustrative()),
        CostModel::ClusterCapacity(ClusterLimits {
            memory_bytes: 256_000_000_000,
            disk_bytes: 1_000_000_000_000,
            cpu_cores: None,
            objective: ClusterObjective::LowestLatency,
        }),
    ] {
        let refused = selected(cost_model);
        assert!(!selects_a_plan(&refused), "{refused}");
        assert!(
            refused["error"]
                .as_str()
                .unwrap()
                .contains("implements cost model 1 only"),
            "{refused}"
        );
    }
}

/// AWS Fargate's us-east-1 costs Example 4 under cost model 1 in US
/// dollars per second: the selected plan costs no more than the
/// whole-window KLL, $2.60278e-8/s (its 2.1 CPU-ms/s at $1.1244e-8 per
/// CPU-ms, and its states held while the query runs).
#[test]
fn aws_fargate_prices_example_4_in_dollars() {
    let fargate = cost_inputs(&aws_fargate());
    let document = example_document(
        "example3b",
        &[
            "--run-until",
            "stage3",
            "--ignore-accuracy-guarantees",
            "--cost-inputs",
            fargate.path(),
        ],
    );
    let cost = document["stage3_plan_selection"]["cost"]["cost_per_second"]
        .as_f64()
        .unwrap();
    assert!(cost.is_finite() && cost > 0.0);
    assert!(cost <= 2.602778532914e-8 * (1.0 + 1e-6), "{cost}");
}

/// `--deployment-capabilities` declares the executor's capabilities, which
/// the document's `deployment_capabilities` shows (defaults filled in;
/// without the flag, every summary supported, raw data retained, no
/// query-time result retained). For Pattern B with KLL only and query-time
/// results retained: Pass 1 offers no DDSketch candidate. Without raw data, a plan is still selected
/// with guarantees enforced, from materialized views. A deployment with an
/// unknown field is refused.
#[test]
fn deployment_capabilities_flag_sets_the_capabilities_and_shows_them() {
    let with_capabilities = |capabilities: &str, extra: &[&str]| {
        let file = TempFile::new(capabilities);
        let mut args = vec![
            "--run-until",
            "stage3",
            "--deployment-capabilities",
            file.path(),
        ];
        args.extend_from_slice(extra);
        example_document("example3b", &args)
    };
    let default = example_document(
        "example3b",
        &["--run-until", "stage3", "--ignore-accuracy-guarantees"],
    );
    assert_eq!(
        default["deployment_capabilities"],
        json!({
            "supported_summaries": asap_executor::supported_summary_families(),
            "raw_data_retained": true,
            "query_time_results_retained": false,
        })
    );
    let kll_kept = with_capabilities(
        r#"{"supported_summaries": [{"summary_type": "Kll", "readouts": ["Quantile"]}],
            "query_time_results_retained": true}"#,
        &["--ignore-accuracy-guarantees"],
    );
    assert_eq!(
        kll_kept["deployment_capabilities"],
        json!({
            "supported_summaries": [{ "summary_type": "Kll", "readouts": ["Quantile"] }],
            "raw_data_retained": true,
            "query_time_results_retained": true,
        })
    );
    let has_ddsketch = |document: &Value| {
        memo_labels(document, "pass1")
            .iter()
            .any(|l| l.contains("DDSketch"))
    };
    assert!(!has_ddsketch(&kll_kept));
    assert!(has_ddsketch(&default));
    assert!(selects_a_plan(&kll_kept["stage3_plan_selection"]));
    assert!(!kll_kept["stage3_plan_selection"]["plan"]
        .to_string()
        .contains("DDSketch"));

    let no_raw = with_capabilities(r#"{"raw_data_retained": false}"#, &[]);
    let selection = &no_raw["stage3_plan_selection"];
    assert!(selects_a_plan(selection), "{selection}");
    assert!(!selection["materialized_views"]
        .as_array()
        .unwrap()
        .is_empty());

    let unknown = TempFile::new(r#"{"raw_data": false}"#);
    let stderr = refused(&[
        "--workload",
        &example("example3b"),
        "--run-until",
        "stage0",
        "--deployment-capabilities",
        unknown.path(),
    ]);
    assert!(
        stderr.contains("--deployment-capabilities") && stderr.contains("unknown field `raw_data`"),
        "{stderr}"
    );
}

/// The default is to enforce accuracy guarantees: an average of p99s has
/// no guarantee from any sketch, so constraint (K) leaves only an exact
/// plan (or none). With `--ignore-accuracy-guarantees`, no query has a
/// target and a plan is selected on cost alone. The document says which
/// mode it planned under, and a `--cost-inputs` file cannot carry the
/// mode.
#[test]
fn accuracy_guarantees_are_enforced_by_default_and_ignored_on_request() {
    let workload = adhoc(
        &["avg by (g) (quantile_over_time(0.99, x[100s]))"],
        json!({"max_error": 0.01, "failure_probability": 0.01, "accuracy_metric": "rank"}),
        &["x"],
    );
    let enforced = document(&workload, &["--run-until", "stage3"]);
    assert_eq!(enforced["accuracy_guarantees"], "enforced");
    let selection = &enforced["stage3_plan_selection"];
    if selects_a_plan(selection) {
        let accepted = &selection["accepted_accuracy_per_query"][0];
        assert_eq!(accepted["sketches"], json!([]), "{accepted}");
    }

    let ignored = document(
        &workload,
        &["--run-until", "stage3", "--ignore-accuracy-guarantees"],
    );
    assert_eq!(ignored["accuracy_guarantees"], "ignored");
    assert!(selects_a_plan(&ignored["stage3_plan_selection"]));

    let settings = TempFile::new(r#"{"ignore_accuracy_guarantees": true}"#);
    let file = TempFile::json(&workload);
    let stderr = refused(&[
        "--workload",
        file.path(),
        "--run-until",
        "stage0",
        "--cost-inputs",
        settings.path(),
    ]);
    assert!(stderr.contains("ignore_accuracy_guarantees"), "{stderr}");
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
            serde_json::from_value::<asap_types::deployment_capabilities::DeploymentCapabilities>(
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

/// Each workload example (`examples.json` entries with a `workload`), run
/// from its workload file with the SQL tables in the `tables.json` next to
/// it, its deployment capabilities and cost inputs (`cost-inputs/<cost_model>.json`)
/// when it names them, and its accuracy guarantee mode,
/// carries its data workload as the file declares it and selects a valid
/// plan, whose materialized views, as `plan_installable`'s plan derives
/// them, each map to a node of the selected Stage 2 candidate.
#[test]
fn workload_examples_select_valid_plans_with_linked_materialized_views() {
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
    assert!(!workload_examples.is_empty());
    for example in workload_examples {
        check_workload_example(&root, example);
    }
}

fn check_workload_example(root: &Path, example: &Value) {
    let path = root.join(example["workload"].as_str().unwrap());
    let mut args = vec![
        "--workload",
        path.to_str().unwrap(),
        "--run-until",
        "stage3",
    ];
    let capabilities = example.get("deployment_capabilities").map(TempFile::json);
    if let Some(capabilities) = &capabilities {
        args.extend(["--deployment-capabilities", capabilities.path()]);
    }
    let inputs = example.get("cost_model").map(|name| {
        root.join("cost-inputs")
            .join(format!("{}.json", name.as_str().unwrap()))
    });
    if let Some(inputs) = &inputs {
        args.extend(["--cost-inputs", inputs.to_str().unwrap()]);
    }
    match example.get("accuracy_guarantees") {
        None => args.push("--ignore-accuracy-guarantees"),
        Some(mode) if mode == "ignored" => args.push("--ignore-accuracy-guarantees"),
        Some(mode) if mode == "enforced" => {}
        Some(mode) => panic!("accuracy_guarantees {mode}"),
    }
    let tables_path = path.parent().unwrap().join("tables.json");
    let tables = match tables_path.exists() {
        true => catalog_tables(&tables_path),
        false => Vec::new(),
    };
    for table in &tables {
        args.extend(["--sql-catalog-table", table.as_str()]);
    }
    let document = document_of(&args);

    let workload: PlanningWorkload =
        serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    assert_eq!(
        document["workload"]["data_workload"],
        serde_json::to_value(workload.data_workload.as_ref().unwrap()).unwrap()
    );
    let queries = document["workload"]["queries"].as_array().unwrap();
    // Each query carries its recurrence and predictability as declared.
    assert!(queries
        .iter()
        .all(|q| !q["recurrence"].is_null() && !q["predictability"].is_null()));

    let selection = &document["stage3_plan_selection"];
    assert!(
        selects_a_plan(selection),
        "{}: no validated selected plan: {selection}",
        example["id"]
    );
    let dag = selected_physical(&document, queries.len());
    let accepted = selection["accepted_accuracy_per_query"].as_array().unwrap();
    assert_eq!(accepted.len(), queries.len());
    assert!(
        selection.get("materialized_views_error").is_none(),
        "{selection}"
    );
    let nodes: HashSet<_> = dag.nodes.iter().map(|node| node.id as u64).collect();
    for view in selection["materialized_views"].as_array().unwrap() {
        assert!(nodes.contains(&view["node_id"].as_u64().unwrap()));
    }
}

/// With no supported summaries declared, `asap_planner` uses the executor's
/// families, as `plan_installable` does. CountSketch cannot answer a total, so
/// Pass 1 does not offer it for count.
#[test]
fn undeclared_supported_summaries_default_to_the_executors_families() {
    let document = document(
        &adhoc(&["count by (job) (m)"], within(0.01, 0.01), &["m"]),
        &[
            "--run-until",
            "stage1_pass1",
            "--ignore-accuracy-guarantees",
        ],
    );
    let labels = memo_labels(&document, "pass1");
    for family in ["FinalizeExactAccumulator", "Cms", "UnivMon"] {
        assert!(
            labels.iter().any(|label| label.contains(family)),
            "missing {family}: {labels:?}"
        );
    }
    assert!(labels.iter().all(|label| !label.contains("CountSketch")));
    assert_eq!(
        document["deployment_capabilities"]["supported_summaries"],
        serde_json::to_value(asap_executor::supported_summary_families()).unwrap()
    );
}

/// Two windowed quantiles over one metric (Q1 over 1 m, Q2 over 5 m, both
/// every 10 s): with a shared scan, Stage 2 also offers the scan built once
/// per evaluation, which reads the 5 m window once (≈0.1364/s) instead of
/// once per query (0.2203/s); the selected plan reads the scan built once
/// per evaluation and costs at most 0.144/s.
#[test]
fn a_scan_two_queries_share_is_offered_built_once_per_evaluation() {
    let workload = json!({
        "query_workload": {
            "language": "promql",
            "query_batch": null,
            "repeating_queries": [
                every_10s_within_1_percent("quantile_over_time(0.5, data[1m])"),
                every_10s_within_1_percent("quantile_over_time(0.9, data[5m])"),
            ],
        },
        "data_workload": {
            "arrival": {"ingesting": {"rows_per_second": 666.67}},
            "sources": {"data": {"series_count": 10000, "metric_type": "gauge", "sample_interval": 15000}},
        },
    });
    let document = document(
        &workload,
        &["--run-until", "stage3", "--ignore-accuracy-guarantees"],
    );
    let selection = &document["stage3_plan_selection"];
    let cost = selection["cost"]["cost_per_second"].as_f64().unwrap();
    assert!(cost <= 0.144, "{cost}");
    // The plan's one scan, built once per evaluation.
    let selected_scans: Vec<&Value> = selection["plan"]["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|node| node["payload"]["NonASAP"]["Scan"].is_object())
        .collect();
    assert_eq!(selected_scans.len(), 1, "{selected_scans:?}");
    assert_eq!(
        selected_scans[0]["lifetime"],
        json!({"OneEvaluation": "Memory"})
    );
}

/// The search is the candidate-connection search, whose selection MILP's
/// size and times Stage 3's section reports.
#[test]
fn the_search_reports_candidate_connections() {
    let document = document(
        &adhoc(&["sum(m)"], exact(), &["m"]),
        &["--run-until", "stage3"],
    );
    assert_eq!(document["search"], "candidate-connections");
    let selection = &document["stage3_plan_selection"];
    for field in [
        "milp_variables",
        "milp_constraints",
        "milp_construction_ms",
        "milp_solve_ms",
    ] {
        assert!(selection[field].is_u64(), "{field}: {selection}");
    }
}

/// The selected plan's `per_node` entries carry the node's operator,
/// timing and predicted CPU, as `plan_installable`'s predicted usage names
/// them, and its `cpu_core_seconds_per_second` is their sum: enough to
/// compare a plan's predicted CPU with a runtime's measurement without
/// `plan_installable`.
#[test]
fn per_node_costs_show_operator_timing_and_predicted_cpu() {
    let document = example_document(
        "example1",
        &["--run-until", "stage3", "--ignore-accuracy-guarantees"],
    );
    let selection = &document["stage3_plan_selection"];
    assert!(
        selects_a_plan(selection),
        "missing evaluated costs: {selection}"
    );
    let cost = &selection["cost"];
    let mut timings = HashSet::new();
    let mut sum = 0.0;
    for node in cost["per_node"].as_object().unwrap().values() {
        assert!(node["operator"].is_string(), "{node}");
        timings.insert(node["timing"].as_str().unwrap().to_owned());
        let cpu = node["cpu_core_seconds_per_second"].as_f64().unwrap();
        assert!(cpu >= 0.0, "{node}");
        sum += cpu;
    }
    let total = cost["cpu_core_seconds_per_second"].as_f64().unwrap();
    assert!(total > 0.0);
    assert!((total - sum).abs() <= 1e-9 * total, "{total} vs {sum}");
    assert!(
        timings.is_subset(&HashSet::from([
            "ingestion_time".to_owned(),
            "query_time".to_owned()
        ])),
        "{timings:?}"
    );
}

/// A rate by job and a 5 m maximum, every minute, over `m`, declared a
/// `metric_type` with 1,000 series sampled every 15 s.
fn rate_and_max_over(metric_type: &str) -> Value {
    let entry = |query: &str| {
        json!({
            "query": query,
            "demand": {"fixed_interval_at": {"interval": 60000, "evaluation_phase": 0}},
            "requirements": {},
            "predictability": {"predictable": {"known_at": null}},
        })
    };
    json!({
        "query_workload": {
            "language": "promql",
            "query_batch": null,
            "repeating_queries": [
                entry("sum by (job) (rate(m[5m]))"),
                entry("max_over_time(m[5m])"),
            ],
        },
        "data_workload": {
            "arrival": {"ingesting": {"rows_per_second": 1000.0}},
            "sources": {"m": {"series_count": 1000, "metric_type": metric_type,
                              "sample_interval": 15000}},
        },
    })
}

/// The warning for `rate_and_max_over("gauge")`'s first query.
const RATE_OVER_GAUGE_WARNING: &str = "q1: rate over m, which the data workload declares a \
     gauge: rate assumes a counter (a drop is a reset); planned as written";

/// `rate`, `increase` and `irate` over a source the workload declares a
/// gauge get a warning, whichever stage the run stops at, naming the query,
/// the function and the metric; the text output prints it. A counter gets
/// none.
#[test]
fn a_counter_function_over_a_declared_gauge_is_warned_about_in_text() {
    let text = |metric_type: &str| {
        let file = TempFile::json(&rate_and_max_over(metric_type));
        let output = planner(&["--workload", file.path(), "--run-until", "stage0"]);
        assert!(output.status.success());
        String::from_utf8(output.stdout).unwrap()
    };
    let warnings = |text: &str| -> Vec<String> {
        text.lines()
            .filter_map(|line| line.strip_prefix("warning: "))
            .map(str::to_owned)
            .collect()
    };
    assert_eq!(warnings(&text("gauge")), [RATE_OVER_GAUGE_WARNING]);
    assert!(warnings(&text("counter")).is_empty());
}

/// `rate`, `increase` and `irate` over a source the workload declares a
/// gauge still plan, with a warning under `stage3_plan_selection.warnings`
/// (which the viewer shows) naming the query, the function and the metric.
/// A counter gets none.
#[test]
fn a_counter_function_over_a_declared_gauge_plans_with_a_warning() {
    let args = ["--run-until", "stage3", "--ignore-accuracy-guarantees"];
    let gauge = document(&rate_and_max_over("gauge"), &args);
    let counter = document(&rate_and_max_over("counter"), &args);
    assert!(selects_a_plan(&gauge["stage3_plan_selection"]));
    assert_eq!(
        gauge["stage3_plan_selection"]["warnings"],
        json!([RATE_OVER_GAUGE_WARNING])
    );
    assert!(selects_a_plan(&counter["stage3_plan_selection"]));
    assert!(counter["stage3_plan_selection"].get("warnings").is_none());
}

/// A PromQL file and a SQL file, each a `PlanningWorkload` in its own
/// language.
fn mixed_language_workload_files(sql_arrival: Value, sql_facts: Value) -> (TempFile, TempFile) {
    let entry = |query: &str| {
        json!({
            "query": query,
            "demand": {"fixed_interval_at": {"interval": 60000, "evaluation_phase": 0}},
            "requirements": {},
            "predictability": {"predictable": {"known_at": null}},
        })
    };
    let promql = json!({
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
    let sql = json!({
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
    (TempFile::json(&promql), TempFile::json(&sql))
}

/// `flows(ts, service)`. The time index is declared non-null: the executor
/// builds no count over a nullable time column, so a plan of the SQL query
/// over it would not compile.
const FLOWS_TABLE: &str = r#"{"name": "flows", "columns": [{"name": "ts", "type": "timestamp", "nullable": false}, {"name": "service", "type": "utf8", "nullable": false}], "time_index": 0}"#;

/// The document for the mixed-language files (SQL at 500 rows/s over
/// `flows`, 10 series sampled every second), run until `run_until`.
fn mixed_language_document(run_until: &str) -> Value {
    let (promql, sql) = mixed_language_workload_files(
        json!({"ingesting": {"rows_per_second": 500.0}}),
        json!({"series_count": 10, "sample_interval": 1000}),
    );
    document_of(&[
        "--workload",
        promql.path(),
        "--workload",
        sql.path(),
        "--sql-catalog-table",
        FLOWS_TABLE,
        "--run-until",
        run_until,
        "--ignore-accuracy-guarantees",
    ])
}

/// One run can lower PromQL and SQL queries together: `--workload`
/// repeats, each file is lowered by its own language's frontend (SQL over
/// the `--sql-catalog-table` declarations), and their queries are one
/// workload. The document numbers the queries across files (q1 from the
/// PromQL file, q2 from the SQL file) and its data workload is the union
/// of the files' sources, with their arrival rates added (1000 + 500
/// rows/s).
#[test]
fn several_workload_files_in_different_languages_are_lowered_together() {
    let document = mixed_language_document("stage0");
    let queries = document["workload"]["queries"].as_array().unwrap();
    let ids_and_languages: Vec<(&str, &str)> = queries
        .iter()
        .map(|q| (q["id"].as_str().unwrap(), q["language"].as_str().unwrap()))
        .collect();
    assert_eq!(ids_and_languages, [("q1", "promql"), ("q2", "sql")]);
    let data = &document["workload"]["data_workload"];
    assert_eq!(
        data["arrival"],
        json!({"ingesting": {"rows_per_second": 1500.0}})
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
}

/// PromQL and SQL queries from several workload files are planned together
/// to one selected plan.
#[test]
fn several_workload_files_in_different_languages_are_planned_together() {
    let document = mixed_language_document("stage3");
    assert!(
        selects_a_plan(&document["stage3_plan_selection"]),
        "{}",
        document["stage3_plan_selection"]
    );
    selected_physical(&document, 2);
}

/// Files planned together must agree on how the data arrives and on any
/// source they both declare: a run is refused rather than picking one.
/// While data arrives, each source must also declare its own rate: plan
/// selection shares a workload's `rows_per_second` out over all its
/// sources, so `flows` (10 groups, no sample interval) would be costed at
/// 1500 × 10 / 15010 ≈ 1 row/s instead of its file's 500.
#[test]
fn workload_files_that_disagree_on_the_data_are_refused() {
    let refused_together = |first: &TempFile, second: &TempFile| {
        refused(&[
            "--workload",
            first.path(),
            "--workload",
            second.path(),
            "--sql-catalog-table",
            FLOWS_TABLE,
            "--run-until",
            "stage0",
        ])
    };
    let (promql, sql) = mixed_language_workload_files(
        json!({"ingesting": {"rows_per_second": 500.0}}),
        json!({"series_count": 10}),
    );
    let stderr = refused_together(&promql, &sql);
    assert!(
        stderr.contains("source flows declares no rate of its own"),
        "{stderr}"
    );

    // A table the SQL file reads but does not declare has no rate either.
    let (_, sql) = mixed_language_workload_files(
        json!({"ingesting": {"rows_per_second": 500.0}}),
        json!({"series_count": 10, "sample_interval": 1000}),
    );
    let mut text: Value = serde_json::from_str(&std::fs::read_to_string(&sql.0).unwrap()).unwrap();
    text["data_workload"]["sources"] = json!({});
    let sql = TempFile::json(&text);
    let stderr = refused_together(&promql, &sql);
    assert!(
        stderr.contains("source flows declares no rate of its own"),
        "{stderr}"
    );

    let (promql, sql) = mixed_language_workload_files(json!("at_rest"), json!({"rows_held": 1000}));
    let stderr = refused_together(&promql, &sql);
    assert!(stderr.contains("arrival"), "{stderr}");

    // The same source declared twice with different facts.
    let mut text: Value =
        serde_json::from_str(&std::fs::read_to_string(&promql.0).unwrap()).unwrap();
    text["data_workload"]["sources"]["http_requests_total"]["series_count"] = json!(7);
    let other = TempFile::json(&text);
    let stderr = refused_together(&promql, &other);
    assert!(stderr.contains("http_requests_total"), "{stderr}");
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
        let document = document(
            &adhoc(
                &[&query],
                exact(),
                &["http_request_duration_seconds_bucket"],
            ),
            &["--run-until", "stage3"],
        );
        let selection = &document["stage3_plan_selection"];
        assert!(selects_a_plan(selection), "{query}: {selection}");
    }
}

/// The issue's example workload (#1003): one ratio of per-series quantiles,
/// every minute, ε = 0.01, δ = 0.001.
const MEMO_RATIO_OF_QUANTILES: &str =
    "../../tools/dag-viewer/memo-examples/promql/ratio-of-quantiles-5m.json";

/// The `memo` section: the memo after each step, each candidate with its
/// operator over its input groups, θ and one required accuracy per input,
/// on the issue's example with guarantees ignored, and the selected
/// candidates.
#[test]
fn memo_section_lists_each_step_and_the_selected_candidates() {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(MEMO_RATIO_OF_QUANTILES);
    let document = document_of(&[
        "--workload",
        path.to_str().unwrap(),
        "--run-until",
        "stage2",
        "--ignore-accuracy-guarantees",
    ]);
    let memo = &document["memo"];
    let steps = memo["steps"].as_array().unwrap();
    let names: Vec<_> = steps.iter().map(|s| s["step"].as_str().unwrap()).collect();
    assert_eq!(names, ["stage0", "pass1", "pass2"]);
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
    // group: 7 groups at Stage 0 (scan, range, two quantile groups, ratio,
    // sum, avg). Pass 1 adds the summary-state and readout groups (19) and
    // one candidate per θ of the lists.
    assert_eq!(steps[0]["groups"].as_array().unwrap().len(), 7);
    assert_eq!(candidates(&steps[0]), 0);
    assert_eq!(steps[1]["groups"].as_array().unwrap().len(), 19);
    assert_eq!(candidates(&steps[1]), 36);
    // Every candidate is one operator over its input groups, with one
    // requirement per input, and every group it reads is in the step.
    for step in steps {
        let groups = step["groups"].as_array().unwrap();
        for group in groups {
            assert!(group.get("node").is_none() && group["operator"].is_object());
            for candidate in group["candidates"].as_array().unwrap() {
                let children = candidate["children"].as_array().unwrap();
                assert_eq!(
                    candidate["required_accuracy"].as_array().unwrap().len(),
                    children.len()
                );
                assert!(children
                    .iter()
                    .all(|child| (child.as_u64().unwrap() as usize) < groups.len()));
                assert!(candidate.get("parameters").is_some());
            }
        }
    }
    assert!(memo.get("selected_candidates").is_none());
    // Every selected candidate exists in the last step.
    let selected = document_of(&[
        "--workload",
        path.to_str().unwrap(),
        "--run-until",
        "stage3",
        "--ignore-accuracy-guarantees",
    ]);
    let memo = &selected["memo"];
    let last = memo["steps"].as_array().unwrap().last().unwrap()["groups"]
        .as_array()
        .unwrap();
    let selected = memo["selected_candidates"].as_object().unwrap();
    assert!(!selected.is_empty());
    for (group, indices) in selected {
        let group = &last[group.parse::<usize>().unwrap()];
        for index in indices.as_array().unwrap() {
            assert!(index.as_u64().unwrap() < group["candidate_count"].as_u64().unwrap());
        }
    }
}

/// Edges are listed by layer, on a p90 over 5 m within rank 0.01 every
/// minute at phase 0 over arriving data: Pass 2's memo step lists, per
/// input of each candidate, every producer of the input group with
/// $\alpha^{\log}$ and the reason when it is 0 (KLL sizes too small for
/// the readout's required accuracy), and Stage 1's `pruned_candidates` the
/// candidates Pass 2 eliminated with their causes; Stage 2's
/// `physical_candidates` list, per input, the physical edges whose logical
/// edge holds, with $\alpha^{\mathrm{phys}}$ and the reason when it is 0
/// (a query-time consumer of an ingestion-time result that is not
/// materialized), the plan's edges marked; and Stage 2's pruned candidates
/// carry their properties and reason.
#[test]
fn edges_are_listed_by_layer_with_their_alpha_and_rejection() {
    let workload = json!({
        "query_workload": {"language": "promql", "query_batch": null, "repeating_queries": [{
            "query": "quantile_over_time(0.9, data[5m])",
            "demand": {"fixed_interval_at": {"interval": 60000, "evaluation_phase": 0}},
            "requirements": {"accuracy": {"max_error": 0.01, "failure_probability": 0.01, "accuracy_metric": "rank"}},
            "predictability": {"predictable": {"known_at": null}},
        }]},
        "data_workload": {
            "arrival": {"ingesting": {"rows_per_second": 1000.0}},
            "sources": {"data": {"series_count": 1000, "sample_interval": 1000, "metric_type": "gauge"}},
        },
    });
    let document = document(&workload, &["--run-until", "stage3"]);
    let pruned = document["stage1_logical_asap_optimization"]["pruned_candidates"]
        .as_array()
        .unwrap();
    assert!(!pruned.is_empty());
    for candidate in pruned {
        for cause in candidate["causes"].as_array().unwrap() {
            assert!(
                cause["root"].is_object() || cause["root"].is_string(),
                "{cause}"
            );
            assert!(cause["text"].is_string(), "{cause}");
        }
    }
    let pass2 = memo_step(&document, "pass2")["groups"].as_array().unwrap();
    let (mut logical_holds, mut logical_fails) = (0, 0);
    for group in pass2 {
        for candidate in group["candidates"].as_array().unwrap() {
            for input in candidate["inputs"].as_array().unwrap() {
                let producers = pass2[input["group"].as_u64().unwrap() as usize]["candidate_count"]
                    .as_u64()
                    .unwrap();
                for edge in input["edges"].as_array().unwrap() {
                    assert!(edge["producer"].as_u64().unwrap() < producers, "{edge}");
                    let holds = edge["satisfies_logical_requirement"] == true;
                    assert_eq!(edge["rejection"].is_null(), holds, "{edge}");
                    if holds {
                        logical_holds += 1;
                    } else {
                        logical_fails += 1;
                        // α^log fails, or the consumer reads only producers
                        // of its own θ.
                        let rejection = edge["rejection"].as_str().unwrap();
                        assert!(
                            rejection.starts_with("α^log = 0: ")
                                || rejection == "no edge: the producer's θ differs",
                            "{rejection}"
                        );
                    }
                }
            }
        }
    }
    assert!(logical_holds > 0 && logical_fails > 0);
    let stage2 = &document["stage2_physical_asap_optimization"];
    let candidates = stage2["physical_candidates"].as_array().unwrap();
    assert!(!candidates.is_empty());
    let (mut physical_holds, mut physical_fails, mut selected) = (0, 0, 0);
    for candidate in candidates {
        assert!(candidate["properties"]["timing"].is_string());
        for input in candidate["inputs"].as_array().unwrap() {
            for edge in input["edges"].as_array().unwrap() {
                assert!((edge["producer"].as_u64().unwrap() as usize) < candidates.len());
                let holds = edge["satisfies_physical_requirement"] == true;
                assert_eq!(edge["rejection"].is_null(), holds, "{edge}");
                if holds {
                    physical_holds += 1;
                } else {
                    physical_fails += 1;
                    assert!(edge["rejection"]
                        .as_str()
                        .unwrap()
                        .starts_with("α^phys = 0: "));
                }
                if edge["selected"] == true {
                    assert!(holds, "a selected edge is admissible: {edge}");
                    selected += 1;
                }
            }
        }
    }
    assert!(physical_holds > 0 && physical_fails > 0);
    let plan_edges = document["stage3_plan_selection"]["plan"]["edges"]
        .as_array()
        .unwrap()
        .len();
    assert!(selected > 0 && selected <= plan_edges);
    for pruned in stage2["pruned_candidates"].as_array().unwrap() {
        assert!(
            pruned["properties"]["lifetime"].is_string()
                || pruned["properties"]["lifetime"].is_object()
        );
        assert!(pruned["reason"].is_string() || pruned["reason"].is_object());
    }
}

/// The text output ends with Stage 3's selection and the selected plan's
/// cost per second.
#[test]
fn text_output_shows_the_selected_plans_cost() {
    let output = planner(&["--workload", &example("example1"), "--run-until", "stage3"]);
    assert!(output.status.success());
    let text = String::from_utf8(output.stdout).unwrap();
    assert!(
        text.contains("Stage 3 output: PhysicalASAPDAG and Selection"),
        "{text}"
    );
    assert!(text.contains("\ncost_per_second: "), "{text}");
}

/// A memo example under `tools/dag-viewer/memo-examples`.
fn memo_example(path: &str) -> String {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../tools/dag-viewer/memo-examples")
        .join(path)
        .to_str()
        .unwrap()
        .to_owned()
}

/// The arguments for raw data retained, query-time results not retained,
/// AWS Fargate costs and enforced guarantees, with `capabilities` the
/// temporary deployment capabilities file they name.
fn milp_args(capabilities: &TempFile) -> Vec<String> {
    let aws = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../tools/dag-viewer/cost-inputs/aws-fargate-us-east-1.json");
    vec![
        "--run-until".into(),
        "stage3".into(),
        "--deployment-capabilities".into(),
        capabilities.path().into(),
        "--cost-inputs".into(),
        aws.to_str().unwrap().into(),
    ]
}

fn raw_data_retained() -> TempFile {
    TempFile::json(&json!({"raw_data_retained": true, "query_time_results_retained": false}))
}

/// #1095 C5: example 1 and the RangeHeavy T5 q0.5 + q0.9 workload select a
/// plan through the selection MILP, with an accepted accuracy per query.
#[test]
fn milp_search_selects_a_plan_on_example1_and_rangeheavy() {
    let capabilities = raw_data_retained();
    let rangeheavy = memo_example("promql/rangeheavy-t5-q05-q09-1h.json");
    let example1 = example("example1");
    let milp = milp_args(&capabilities);
    let runs: [Vec<&str>; 2] = [
        vec!["--workload", example1.as_str(), "--run-until", "stage3"],
        std::iter::once("--workload")
            .chain([rangeheavy.as_str()])
            .chain(milp.iter().map(String::as_str))
            .collect(),
    ];
    for args in runs {
        let document = document_of(&args);
        let selection = &document["stage3_plan_selection"];
        assert!(selects_a_plan(selection), "{args:?}: {selection}");
        assert!(selection["milp_variables"].as_u64().unwrap() > 0);
        let queries = document["workload"]["queries"].as_array().unwrap().len();
        let accepted = selection["accepted_accuracy_per_query"].as_array().unwrap();
        assert_eq!(accepted.len(), queries);
    }
}

/// #1127 regressions: with guarantees enforced, ratio-of-quantiles (KLL
/// rank errors compose no guarantee through the quotient) and
/// rollupheavy-t11 (HLL's failure probability over 10^6 distinct users is
/// above its contract's) select a plan that passes every query's accuracy
/// check and uses no summary of the family Pass 1 pruned.
#[test]
fn milp_search_selects_checked_plans_on_ratio_and_rollupheavy() {
    let capabilities = raw_data_retained();
    let milp = milp_args(&capabilities);
    let tables = catalog_tables(Path::new(&memo_example("sql/tables.json")));
    for (workload, pruned_family) in [
        ("promql/ratio-of-quantiles-5m.json", "Kll"),
        ("sql/rollupheavy-t11-service-endpoint-5m.json", "Hll"),
    ] {
        let path = memo_example(workload);
        let mut args = vec!["--workload", path.as_str()];
        args.extend(milp.iter().map(String::as_str));
        if workload.starts_with("sql/") {
            for table in &tables {
                args.extend(["--sql-catalog-table", table.as_str()]);
            }
        }
        let document = document_of(&args);
        let selection = &document["stage3_plan_selection"];
        assert!(selects_a_plan(selection), "{workload}: {selection}");
        let queries = document["workload"]["queries"].as_array().unwrap().len();
        let accepted = selection["accepted_accuracy_per_query"].as_array().unwrap();
        assert_eq!(accepted.len(), queries);
        for query in accepted {
            assert!(
                !query["sketches"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|sketch| sketch == pruned_family),
                "{workload}: {query}"
            );
        }
    }
}

/// #1095 C5: on RangeHeavy T8+T5 (`q0.9 / q0.5` with both operands also
/// queried alone), Stage 3 selects a DDSketch plan that passes every
/// query's accuracy check (KLL fails the check there).
#[test]
#[ignore = "Stage 3 over RangeHeavy T8+T5 takes about 4.5 minutes in debug; run with --ignored"]
fn milp_search_selects_a_checked_ddsketch_plan_for_a_ratio_and_its_operands() {
    let capabilities = raw_data_retained();
    let path = memo_example("promql/rangeheavy-t8-t5-1h.json");
    let mut args = vec!["--workload", path.as_str()];
    let milp = milp_args(&capabilities);
    args.extend(milp.iter().map(String::as_str));
    let document = document_of(&args);
    let selection = &document["stage3_plan_selection"];
    assert!(selects_a_plan(selection), "{selection}");
    for query in selection["accepted_accuracy_per_query"].as_array().unwrap() {
        assert_eq!(query["sketches"], json!(["DDSketch"]), "{query}");
    }
}

/// The viewer's cost-input files carry the costs of the presets they are
/// named after, so a change to `crates/cost`'s constants fails here instead
/// of leaving the viewer and the snapshots on the old numbers.
#[test]
fn cost_input_files_match_their_presets() {
    use asap_cost::{CostInputs, CostModel, ResourceUnitCost};
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tools/dag-viewer/cost-inputs");
    let presets = [
        ("usage-over-time.json", ResourceUnitCost::illustrative()),
        (
            "aws-fargate-us-east-1.json",
            ResourceUnitCost::aws_fargate_us_east_1(CostInputs::default().resource_speed),
        ),
    ];
    for (file, unit_cost) in presets {
        let loaded: CostInputs =
            serde_json::from_str(&std::fs::read_to_string(dir.join(file)).unwrap()).unwrap();
        let expected = CostInputs {
            cost_model: CostModel::UsageOverTime(unit_cost),
            ..CostInputs::default()
        };
        // Through JSON, as the file was read: serde_json's float parsing
        // may differ from the computed value in the last bit.
        let expected: CostInputs =
            serde_json::from_str(&serde_json::to_string(&expected).unwrap()).unwrap();
        assert_eq!(loaded, expected, "{file}");
    }
}

/// The text output's memo after Pass 2 is complete: every group has its
/// operator and a properties line (each column's lineage and sign, whether
/// it reads only time series, its evaluation grid); a sketch build shows
/// its θ and a readout its required accuracy; Pass 2's and Stage 2's
/// pruned candidates are listed with their causes; every `G<n>` named
/// resolves to a printed group. Workload: two windowed quantiles every
/// 10 s within 1 % (the shared-scan workload).
#[test]
fn the_text_memo_after_pass_2_prints_every_group_with_its_properties() {
    let workload = json!({
        "query_workload": {
            "language": "promql",
            "query_batch": null,
            "repeating_queries": [
                every_10s_within_1_percent("quantile_over_time(0.5, data[1m])"),
                every_10s_within_1_percent("quantile_over_time(0.9, data[5m])"),
            ],
        },
        "data_workload": {
            "arrival": {"ingesting": {"rows_per_second": 666.67}},
            "sources": {"data": {"series_count": 10000, "metric_type": "gauge", "sample_interval": 15000}},
        },
    });
    let file = TempFile::json(&workload);
    let output = planner(&[
        "--workload",
        file.path(),
        "--run-until",
        "stage3",
        "--format",
        "text",
    ]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let text = String::from_utf8(output.stdout).unwrap();
    let pass2 = text
        .split("==================== Stage 1 Pass 2 output")
        .nth(1)
        .and_then(|rest| rest.split("==================== Stage 2 output").next())
        .expect("a Pass 2 section");
    let lines: Vec<&str> = pass2.lines().collect();
    let mut groups = Vec::new();
    for (index, line) in lines.iter().enumerate() {
        let Some(id) = line
            .strip_prefix('G')
            .and_then(|rest| rest.split(':').next())
        else {
            continue;
        };
        let Ok(id) = id.parse::<usize>() else {
            continue;
        };
        groups.push(id);
        let properties = lines[index + 1];
        assert!(
            properties.contains('=') && properties.contains(':'),
            "G{id}'s columns with lineage and sign: {properties}"
        );
        assert!(
            properties.contains("reads only time series") || properties.contains("reads a table"),
            "{properties}"
        );
        assert!(
            properties.contains("evaluated every")
                || properties.contains("no known evaluation grid"),
            "{properties}"
        );
    }
    assert_eq!(
        groups,
        (0..groups.len()).collect::<Vec<_>>(),
        "groups in order"
    );
    for line in &lines {
        let candidate = line.trim_start();
        if candidate.contains("SummaryAgg") && candidate.starts_with('[') {
            assert!(candidate.contains("θ="), "a build shows its θ: {candidate}");
        }
        if candidate.contains("SummaryEstimate")
            && candidate.starts_with('[')
            && !candidate.contains("pruned")
        {
            assert!(
                candidate.contains("requires="),
                "a readout shows its requirement: {candidate}"
            );
        }
        for word in line.split(|c: char| !c.is_ascii_alphanumeric()) {
            if let Some(id) = word
                .strip_prefix('G')
                .and_then(|id| id.parse::<usize>().ok())
            {
                assert!(id < groups.len(), "G{id} resolves: {line}");
            }
        }
    }
    assert!(
        pass2.contains("pruned by Pass 2"),
        "Pass 2's pruned candidates"
    );
    assert!(
        text.contains("pruned by Stage 2"),
        "Stage 2's pruned candidates"
    );
    assert!(text.contains("==================== Stage 3 output"));
}

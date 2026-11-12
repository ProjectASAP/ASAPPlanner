//! Helpers shared by the planning tests that load a workload fixture, plan it
//! through the production stages, and compare costs, or that run PromQL or
//! SQL workloads through Pass 2.
use asap_cost::CostInputs;
use asap_devtools::planner_fuzz::{read_seed_file, FuzzSettings, FuzzWorkload, Planner};
use asap_devtools::planning_stages::{
    lower_promql_roots, lower_sql_roots, run_search_over, run_stages_until, sql_catalog, RunUntil,
};
use asap_planner::{PlanSearchSettings, PlanningModels};
use asap_types::stages::LogicalASAPMemoCandidates;
use asap_types::workload::PlanningWorkload;
use serde_json::{json, Value};
use std::path::Path;

/// The workload in `tests/plan_sharing_fixtures/<fixture>.json` over the
/// `metrics(ts, city, host, value)` table, whose columns after `ts` are
/// nullable when `nullable_columns` is set.
pub fn fuzz_workload(fixture: &str, nullable_columns: bool) -> FuzzWorkload {
    let mut w = read_seed_file(
        &Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/plan_sharing_fixtures")
            .join(format!("{fixture}.json")),
    )
    .unwrap();
    w.tables = vec![json!({"name":"metrics", "time_index":0, "columns":[
        {"name":"ts","type":"timestamp","nullable":false},
        {"name":"city","type":"utf8","nullable":nullable_columns},
        {"name":"host","type":"utf8","nullable":nullable_columns},
        {"name":"value","type":"float64","nullable":nullable_columns}
    ]})];
    w
}

/// `w` with its queries replaced by `queries`, each keeping the demand and
/// requirements of the workload's first query.
pub fn with_queries(mut w: FuzzWorkload, queries: &[String]) -> FuzzWorkload {
    let template = w.queries[0].clone();
    w.queries = queries
        .iter()
        .map(|query| {
            let mut entry = template.clone();
            entry.entry.query.0 = query.clone();
            entry
        })
        .collect();
    w
}

/// The selected plan's cost per second for `w`.
pub fn cost(w: &FuzzWorkload) -> f64 {
    let roots = Planner::new(FuzzSettings::default()).lower(w).unwrap();
    let planning = w.planning_workload();
    let entries: Vec<_> = planning.query_workload.entries().collect();
    let settings = CostInputs::default();
    let deployment = w.deployment();
    let models = PlanningModels::builtin()
        .with_cost_inputs(&settings)
        .with_deployment_capabilities(&deployment);
    let run = run_search_over(
        roots,
        &entries,
        &w.data,
        models,
        PlanSearchSettings::default(),
    )
    .unwrap();
    let (plan, _) = run.run.selection.as_ref().unwrap();
    let value =
        asap_integration_tests::planning::plan_cost_per_second(plan, &entries, &w.data, &settings);
    println!("cost={value}");
    value
}

/// Pass 2's output for PromQL `queries`, each with its accuracy target,
/// repeating every minute over the gauge `m` and the counter `c`, 1000
/// series each.
pub fn promql_pass2(queries: &[(&str, Value)]) -> LogicalASAPMemoCandidates {
    pass2_of(
        json!("promql"),
        queries,
        json!({
            "m": {"series_count": 1000, "metric_type": "gauge", "sample_interval": 1000},
            "c": {"series_count": 1000, "metric_type": "counter", "sample_interval": 1000},
        }),
        None,
    )
}

/// Pass 2's output for SQL `queries` over the table
/// `flows(ts, src, dst, bytes)` of 1000 groups of rows, each with its
/// accuracy target, repeating every minute.
pub fn sql_pass2(queries: &[(&str, Value)]) -> LogicalASAPMemoCandidates {
    let flows = json!({"name": "flows", "time_index": 0, "columns": [
        {"name": "ts", "type": "timestamp"},
        {"name": "src", "type": "utf8"},
        {"name": "dst", "type": "utf8"},
        {"name": "bytes", "type": "float64"},
    ]});
    pass2_of(
        json!({"sql": "datafusion_sql"}),
        queries,
        json!({"flows": {"series_count": 1000}}),
        Some(&[flows.to_string()]),
    )
}

/// Pass 2's output for `queries` in `language` over `sources`, through
/// production lowering (SQL over the `tables` catalog).
fn pass2_of(
    language: Value,
    queries: &[(&str, Value)],
    sources: Value,
    tables: Option<&[String]>,
) -> LogicalASAPMemoCandidates {
    let workload: PlanningWorkload = serde_json::from_value(json!({
        "query_workload": {
            "language": language,
            "repeating_queries": queries.iter().map(|(query, accuracy)| json!({
                "query": query,
                "demand": {"fixed_interval_at": {"interval": 60000, "evaluation_phase": 0}},
                "requirements": {"accuracy": accuracy},
                "predictability": {"predictable": {"known_at": null}},
            })).collect::<Vec<_>>(),
        },
        "data_workload": {
            "arrival": {"ingesting": {"rows_per_second": 1000.0}},
            "sources": sources,
        },
    }))
    .unwrap();
    workload.validate().unwrap();
    let roots = match tables {
        Some(tables) => lower_sql_roots(&workload, &sql_catalog(tables).unwrap().0).unwrap(),
        None => lower_promql_roots(&workload).unwrap(),
    };
    let entries: Vec<_> = workload.query_workload.entries().collect();
    let data = workload.data_workload.clone().unwrap_or_default();
    let run = run_stages_until(
        roots,
        &entries,
        &data,
        PlanningModels::builtin(),
        RunUntil::Pass2,
    )
    .unwrap();
    run.run.logical_asap_candidates.unwrap()
}

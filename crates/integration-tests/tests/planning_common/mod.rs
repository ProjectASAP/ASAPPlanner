//! Helpers shared by the planning tests that load a workload fixture, plan it
//! through the production stages, and compare costs, or that run PromQL or
//! SQL workloads through Pass 2.
use asap_cost::{CostInputs, CostModel, ResourceUnitCost};
use asap_devtools::planner_fuzz::{read_seed_file, FuzzSettings, FuzzWorkload, Planner};
use asap_planner::{PlanSearchSettings, PlanningModels};
use asap_planner_cli::planning_stages::{
    lower_promql_roots, lower_sql_roots, run_search_over, run_stages_until, sql_catalog, RunUntil,
};
use asap_types::plan::physical_asap_dag::PhysicalASAPDAG;
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
    cost_with(w, &CostInputs::default())
}

/// [`cost`] under `settings`.
pub fn cost_with(w: &FuzzWorkload, settings: &CostInputs) -> f64 {
    let plan = selected_plan(w, settings);
    let planning = w.planning_workload();
    let entries: Vec<_> = planning.query_workload.entries().collect();
    let value =
        asap_integration_tests::planning::plan_cost_per_second(&plan, &entries, &w.data, settings);
    println!("cost={value}");
    value
}

/// The plan the production stages select for `w` under `settings`.
pub fn selected_plan(w: &FuzzWorkload, settings: &CostInputs) -> PhysicalASAPDAG {
    let roots = Planner::new(FuzzSettings::default()).lower(w).unwrap();
    let planning = w.planning_workload();
    let entries: Vec<_> = planning.query_workload.entries().collect();
    let deployment = w.deployment();
    let models = PlanningModels::builtin()
        .with_cost_inputs(settings)
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
    plan.clone()
}

/// Pass 2's output for PromQL `queries`, each with its accuracy target,
/// repeating every minute over the gauge `m`, the counter `c` and the
/// gauge `latency` whose values are declared non-negative (so DDSketch's
/// quantile guarantee holds over it), 1000 series each.
pub fn promql_pass2(queries: &[(&str, Value)]) -> LogicalASAPMemoCandidates {
    promql_pass2_over(
        queries,
        json!({
            "m": {"series_count": 1000, "metric_type": "gauge", "sample_interval": 1000},
            "c": {"series_count": 1000, "metric_type": "counter", "sample_interval": 1000},
            "latency": {"series_count": 1000, "metric_type": "gauge", "sample_interval": 1000,
                "column_signs": {"value": "non_negative"}},
        }),
    )
}

/// [`promql_pass2`] with the facts `sources` declares.
pub fn promql_pass2_over(queries: &[(&str, Value)], sources: Value) -> LogicalASAPMemoCandidates {
    pass2_of(json!("promql"), queries, sources, None)
}

/// Pass 2's output for SQL `queries` over the table
/// `flows(ts, src, dst, bytes)` of 1000 groups of rows, each with its
/// accuracy target, repeating every minute.
pub fn sql_pass2(queries: &[(&str, Value)]) -> LogicalASAPMemoCandidates {
    sql_pass2_over(queries, json!({"flows": {"series_count": 1000}}))
}

/// [`sql_pass2`] with the facts `sources` declares for `flows` and for
/// `hosts(ts, host, region)`, which a query may join with `flows`.
pub fn sql_pass2_over(queries: &[(&str, Value)], sources: Value) -> LogicalASAPMemoCandidates {
    let flows = json!({"name": "flows", "time_index": 0, "columns": [
        {"name": "ts", "type": "timestamp"},
        {"name": "src", "type": "utf8"},
        {"name": "dst", "type": "utf8"},
        {"name": "bytes", "type": "float64"},
    ]});
    let hosts = json!({"name": "hosts", "time_index": 0, "columns": [
        {"name": "ts", "type": "timestamp"},
        {"name": "host", "type": "utf8"},
        {"name": "region", "type": "utf8"},
    ]});
    pass2_of(
        json!({"sql": "datafusion_sql"}),
        queries,
        sources,
        Some(&[flows.to_string(), hosts.to_string()]),
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
        Default::default(),
        RunUntil::Pass2,
    )
    .unwrap();
    run.run.logical_asap_candidates.unwrap()
}

/// The default cost inputs with `cost_per_disk_byte_second` 0: for tests
/// whose plans differ in CPU, not in the raw data a plan keeps on disk
/// (which would make a query-time scan's twins near-equal in cost, a tie
/// the solver may break either way).
pub fn cost_inputs_without_disk_cost() -> CostInputs {
    CostInputs {
        cost_model: CostModel::UsageOverTime(ResourceUnitCost {
            cost_per_disk_byte_second: 0.0,
            ..ResourceUnitCost::illustrative()
        }),
        ..CostInputs::default()
    }
}

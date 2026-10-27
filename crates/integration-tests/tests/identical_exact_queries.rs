//! Original issue workloads against the production candidate-connection planner.
use asap_devtools::planner_fuzz::{read_seed_file, FuzzSettings, FuzzWorkload, Planner};
use asap_devtools::planning_stages::run_search_over;
use asap_plan_selection::{PlanSelectionSettings, PlanningModels, Stage1ToStage3Search};
use asap_planner::PlannerExecutorCompileCheck;
use serde_json::json;
use std::path::Path;

fn workload() -> FuzzWorkload {
    let mut w = read_seed_file(
        &Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/sharing_issue_fixtures/922.json"),
    )
    .unwrap();
    w.tables = vec![json!({"name":"metrics", "time_index":0, "columns":[
        {"name":"ts","type":"timestamp","nullable":false},
        {"name":"city","type":"utf8","nullable":false},
        {"name":"host","type":"utf8","nullable":false},
        {"name":"value","type":"float64","nullable":false}
    ]})];
    w
}

fn cost(w: &FuzzWorkload) -> f64 {
    let roots = Planner::new(FuzzSettings::default()).lower(w).unwrap();
    let planning = w.planning_workload();
    let entries: Vec<_> = planning.query_workload.entries().collect();
    let settings = PlanSelectionSettings::default();
    let deployment = w.deployment();
    let models = PlanningModels::builtin()
        .with_plan_selection(&settings)
        .with_deployment_capabilities(&deployment)
        .with_executor_compile_check(&PlannerExecutorCompileCheck);
    let run = run_search_over(
        roots,
        &entries,
        &w.data,
        models,
        Stage1ToStage3Search {
            milp_time_limit_ms: Some(30_000),
            ..Default::default()
        },
    )
    .unwrap();
    let selection = run.stage3_plan_selection.as_ref().unwrap();
    let value = selection.costs[&selection.selected]
        .cost_per_second
        .unwrap();
    println!("cost={value}; selected={}", selection.selected);
    value
}

/// #922: two identical exact percentile queries pay for their result once.
#[test]
fn identical_exact_percentiles_share_work() {
    let related = workload();
    let mut base = related.clone();
    base.queries.truncate(1);
    let base_cost = cost(&base);
    let related_cost = cost(&related);
    assert!(
        related_cost <= base_cost * 1.001,
        "{related_cost} vs {base_cost}"
    );
}

/// Different ranks share the source window but still compute their own values.
#[test]
fn different_exact_percentiles_share_the_input_window() {
    let mut related = workload();
    related.queries[1].entry.query.0 = related.queries[1].entry.query.0.replace("0.99", "0.5");
    let related_cost = cost(&related);
    assert!(related_cost <= 0.0883, "{related_cost}");
}

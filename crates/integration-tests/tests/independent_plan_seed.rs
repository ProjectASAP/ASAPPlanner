//! The joint search never returns a plan costlier than planning each query
//! on its own: each query's own plan, combined, is evaluated first.
use asap_devtools::planner_fuzz::{read_seed_file, FuzzSettings, FuzzWorkload, Planner};
use serde_json::json;
use std::path::Path;

const P90: &str = "percentile_cont(0.9) WITHIN GROUP (ORDER BY value)";
const W5: &str = "ts > now() - INTERVAL '5' MINUTE";

/// `queries` in the #902 SQL workload (`metrics`, every 10 s).
fn sql(queries: &[String]) -> FuzzWorkload {
    let mut w = read_seed_file(
        &Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/sharing_issue_fixtures/902.json"),
    )
    .unwrap();
    w.tables = vec![json!({"name":"metrics", "time_index":0, "columns":[
        {"name":"ts","type":"timestamp","nullable":false},
        {"name":"city","type":"utf8","nullable":true},
        {"name":"host","type":"utf8","nullable":true},
        {"name":"value","type":"float64","nullable":true}
    ]})];
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

/// The selected plan's cost per second, with the default 30 s budget.
fn cost(queries: &[String]) -> f64 {
    use asap_devtools::planning_stages::run_search_over;
    use asap_plan_selection::{PlanSelectionSettings, PlanningModels, Stage1ToStage3Search};
    use asap_planner::PlannerExecutorCompileCheck;
    let w = sql(queries);
    let roots = Planner::new(FuzzSettings::default()).lower(&w).unwrap();
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
    selection.costs[&selection.selected]
        .cost_per_second
        .unwrap()
}

/// Asserts the joint plan of `queries` costs no more than their separate
/// plans, and returns (joint, separate sum).
fn assert_no_costlier_than_separate(queries: &[String]) -> (f64, f64) {
    let separate: f64 = queries
        .iter()
        .map(|query| cost(std::slice::from_ref(query)))
        .sum();
    let together = cost(queries);
    println!("together {together}, separate sum {separate}");
    assert!(
        together <= separate * 1.001,
        "joint {together} above separate plans {separate}"
    );
    (together, separate)
}

/// Two per-city p90 queries: each alone maintains an ingestion-time KLL pane
/// chain; together the search used to stop at a query-time plan about twice
/// as costly.
#[test]
fn per_city_percentiles_cost_no_more_than_separate_plans() {
    assert_no_costlier_than_separate(&[
        format!("SELECT {P90} FROM metrics WHERE city = 'NYC' AND {W5}"),
        format!("SELECT {P90} FROM metrics WHERE city = 'SF' AND {W5}"),
    ]);
}

/// A grouped and a filtered percentile over one window: already at the
/// separate plans' cost, which seeding must keep.
#[test]
fn grouped_and_filtered_percentiles_cost_no_more_than_separate_plans() {
    let (together, _) = assert_no_costlier_than_separate(&[
        format!("SELECT city, {P90} FROM metrics WHERE {W5} GROUP BY city"),
        format!("SELECT {P90} FROM metrics WHERE {W5} AND city IN ('NYC', 'SF')"),
    ]);
    assert!(together <= 0.2284651 * 1.001, "{together}");
}

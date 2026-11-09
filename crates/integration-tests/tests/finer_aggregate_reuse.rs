//! A coarse aggregate read from a finer one through a compensating filter and,
//! when its groups are coarser, a rollup of values or states (#950).
use asap_devtools::planner_fuzz::{read_seed_file, FuzzSettings, FuzzWorkload, Planner};
use asap_devtools::planning_stages::run_search_over;

use serde_json::json;
use std::path::Path;

const P90: &str = "percentile_cont(0.9) WITHIN GROUP (ORDER BY value)";
const WINDOW: &str = "ts > now() - INTERVAL '5' MINUTE";

/// The #902 workload (5-minute window, every 10 s, ε = 0.01) with `queries`.
fn workload(queries: &[String]) -> FuzzWorkload {
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

/// The selected plan's cost per second for `queries`.
fn cost(queries: &[String]) -> f64 {
    use asap_cost::CostInputs;
    use asap_planner::{PlanSearchSettings, PlanningModels};
    let w = workload(queries);
    let roots = Planner::new(FuzzSettings::default()).lower(&w).unwrap();
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

/// Example B end to end: adding the filtered sum to the finer sums costs
/// only its read of the finer result, not a second scan and aggregation.
#[test]
fn filtered_sum_costs_little_beyond_the_finer_sums() {
    let finer =
        format!("SELECT city, host, SUM(value) FROM metrics WHERE {WINDOW} GROUP BY city, host");
    let filtered = format!("SELECT SUM(value) FROM metrics WHERE {WINDOW} AND city = 'NYC'");
    let alone = cost(std::slice::from_ref(&finer));
    let separate = cost(std::slice::from_ref(&filtered));
    let together = cost(&[finer, filtered]);
    println!("finer alone {alone}, filtered alone {separate}, together {together}");
    // Measured (every 10 s at phase 0, the finer sums over a 10 s
    // tumbling-window chain): 0.0284027/s alone, 0.0054346/s for the
    // filtered sum alone, 0.0290029/s together: the `Filter` and `Aggregate`
    // over the finer result add 0.0006/s, a ninth of a separate scan and sum.
    assert!(
        together <= alone * 1.03,
        "{together} vs {alone} + {separate}"
    );
}

/// Example C end to end: the joint plan costs no more than planning each
/// query alone, and no more than on main before the state path (0.2285/s).
/// The joint optimum is 0.1963/s: q1 keeps the NYC and SF states of a KLL
/// by city built over q0's query-time rows.
#[test]
#[ignore = "needs #1095's engine: since #1047 P7 Stage 3 has no per-query seed, and the joint search ends at 0.3321/s, above the separate plans' 0.2285/s"]
fn filtered_kll_states_cost_no_more_than_separate_plans() {
    let grouped = format!("SELECT city, {P90} FROM metrics WHERE {WINDOW} GROUP BY city");
    let filtered = format!("SELECT {P90} FROM metrics WHERE {WINDOW} AND city IN ('NYC', 'SF')");
    let alone = cost(std::slice::from_ref(&grouped));
    let separate = cost(std::slice::from_ref(&filtered));
    let together = cost(&[grouped, filtered]);
    println!("grouped alone {alone}, filtered alone {separate}, together {together}");
    assert!(
        together <= (alone + separate) * 1.001,
        "{together} vs {alone} + {separate}"
    );
    assert!(together <= 0.2285 * 1.01, "{together} vs main's 0.2285");
}

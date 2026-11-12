//! Original issue workloads against the production candidate-connection planner.
use asap_cost::CostInputs;
use asap_devtools::planner_fuzz::{read_seed_file, FuzzSettings, FuzzWorkload, Planner};
use asap_devtools::planning_stages::run_search_over;

use asap_planner::{PlanSearchSettings, PlanningModels};

use serde_json::json;
use std::path::Path;

fn workload(issue: u32) -> FuzzWorkload {
    let mut w = read_seed_file(
        &Path::new(env!("CARGO_MANIFEST_DIR"))
            .join(format!("tests/sharing_issue_fixtures/{issue}.json")),
    )
    .unwrap();
    w.tables = vec![json!({"name":"metrics", "time_index":0, "columns":[
        {"name":"ts","type":"timestamp","nullable":false},
        {"name":"city","type":"utf8","nullable":false},
        {"name":"host","type":"utf8","nullable":false},
        {"name":"value","type":"float64","nullable":false}
    ]})];
    if issue == 902 || issue == 886 {
        for field in w.tables[0]["columns"]
            .as_array_mut()
            .unwrap()
            .iter_mut()
            .skip(1)
        {
            field["nullable"] = json!(true);
        }
    }
    w
}

fn cost(w: &FuzzWorkload) -> f64 {
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

/// #923: deriving the coarse sum costs only a sum over the 150 fine groups.
/// Stage 2 offers that plan; Stage 3 selects a `OneEvaluation` build whose
/// not-materialized sibling would avoid a second scan (#1142).
#[test]
#[ignore = "Stage 3 misses the cheaper plan Stage 2 offers (#1142)"]
fn exact_coarse_sum_reuses_fine_sum() {
    let related = workload(923);
    let mut base = related.clone();
    base.queries.truncate(1);
    let base_cost = cost(&base);
    let related_cost = cost(&related);
    assert!(
        related_cost <= base_cost + 0.00004,
        "{related_cost} vs {base_cost}"
    );
}

/// #924: the 1m sum reads the newest tumbling window of the existing 2m chain.
#[test]
fn nested_exact_sum_reuses_newest_tumbling_windows() {
    let related = workload(924);
    let mut base = related.clone();
    base.queries.truncate(1);
    let base_cost = cost(&base);
    let related_cost = cost(&related);
    assert!(
        related_cost <= base_cost * 1.001,
        "{related_cost} vs {base_cost}"
    );
}

/// #925: the slower copy reads the same maintained sum.
#[test]
fn slower_repeat_reuses_faster_sum() {
    let related = workload(925);
    let mut base = related.clone();
    base.queries.truncate(1);
    let base_cost = cost(&base);
    let related_cost = cost(&related);
    assert!(
        related_cost <= base_cost * 1.001,
        "{related_cost} vs {base_cost}"
    );
}

/// #902: the filtered query avoids its independent scan and aggregation.
#[test]
fn grouped_filtered_quantile_plans() {
    assert!(
        cost(&workload(902)) < 0.25,
        "filtered query must reuse the grouped result"
    );
}

/// #922: duplicate exact TopK queries share their result and inner window work.
#[test]
fn duplicate_exact_topk_shares_work() {
    let mut w = workload(924);
    for query in &mut w.queries {
        query.entry.query.0 = "topk(3, sum_over_time(m_gauge[2m]))".into();
    }
    let mut base = w.clone();
    base.queries.truncate(1);
    let single = cost(&base);
    let duplicate = cost(&w);
    assert!(duplicate <= single * 1.001, "{duplicate} vs {single}");
}

/// #924/#925 SQL variants: nested windows and slower queries reuse one sum.
#[test]
fn sql_sum_shares_nested_windows_and_slower_readers() {
    let mut w = workload(922);
    w.queries.truncate(1);
    w.queries[0].entry.query.0 =
        "SELECT SUM(value) FROM metrics WHERE ts > now() - INTERVAL '5' MINUTE".into();
    w.queries[0].entry.demand = serde_json::from_value(
        json!({"fixed_interval_at": {"interval": 60000, "evaluation_phase": 0}}),
    )
    .unwrap();
    let single = cost(&w);
    let mut nested = w.clone();
    let mut shorter = w.queries[0].clone();
    shorter.entry.query.0 = shorter.entry.query.0.replace("'5'", "'1'");
    nested.queries.push(shorter);
    let nested_cost = cost(&nested);
    assert!(nested_cost <= single * 1.001, "{nested_cost} vs {single}");
    let mut slower = w.queries[0].clone();
    slower.entry.demand = serde_json::from_value(
        json!({"fixed_interval_at": {"interval": 120000, "evaluation_phase": 0}}),
    )
    .unwrap();
    w.queries.push(slower);
    let repeated_cost = cost(&w);
    assert!(
        repeated_cost <= single * 1.001,
        "{repeated_cost} vs {single}"
    );
}

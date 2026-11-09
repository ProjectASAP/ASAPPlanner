//! The selection MILP models the selected plan's cost exactly: its optimal
//! objective is `asap_cost::plan_cost` of the plan it builds, under cost
//! model 1 (concepts doc, section 5, "Cost and retention").
use asap_devtools::planning_stages::{lower_promql_roots, run_planning_stages_over};
use asap_planner::PlanningModels;
use asap_types::workload::PlanningWorkload;
use serde_json::json;

/// A query `query` every 10 s at phase 0, within 1 % at δ = 0.01.
fn every_10s(query: &str) -> serde_json::Value {
    json!({
        "query": query,
        "demand": {"fixed_interval_at": {"interval": 10000, "evaluation_phase": 0}},
        "requirements": {"accuracy": {"max_error": 0.01, "failure_probability": 0.01}},
        "predictability": {"predictable": {"known_at": null}},
    })
}

/// Two windowed quantiles over one metric (1 m and 5 m, every 10 s; 10,000
/// series every 15 s), accuracy not enforced: the selected plan shares
/// one query-time scan, whose execution reads the 5 m window once and
/// retains it (h_v, constraint (R)). The MILP's objective equals the plan's
/// cost to 1e-6 of it.
#[test]
fn the_milp_objective_is_the_plan_cost_on_a_shared_scan() {
    let workload: PlanningWorkload = serde_json::from_value(json!({
        "query_workload": {
            "language": "promql",
            "query_batch": null,
            "repeating_queries": [
                every_10s("quantile_over_time(0.5, data[1m])"),
                every_10s("quantile_over_time(0.9, data[5m])"),
            ],
        },
        "data_workload": {
            "arrival": {"ingesting": {"rows_per_second": 666.67}},
            "sources": {"data": {"series_count": 10000, "metric_type": "gauge", "sample_interval": 15000}},
        },
    }))
    .unwrap();
    let plan_cost = milp_objective_equals_plan_cost(
        &workload,
        PlanningModels::builtin().with_ignore_accuracy_guarantees(true),
    );
    assert!(plan_cost <= 0.144, "{plan_cost}");
}

/// The o11y benchmark's q21: `sum(rate(..[5m] offset 1h))`, a 1 h range
/// query every 60 s. Raw rows it reads may be materialized at ingestion
/// time behind the offset (a `TimeShift` over the `Scan`); that store
/// retains 1 h 5 m of rows, which its `TimeRange` reads 5 m of, in the
/// MILP and in `plan_cost` alike (the MILP's objective was 0.326/s for a
/// plan `plan_cost` charged 0.0219/s, one second of rows retained).
#[test]
fn the_milp_objective_is_the_plan_cost_on_an_offset_range_query() {
    let source = json!({"sample_interval": 1000, "series_count": 1000, "metric_type": "counter"});
    let workload: PlanningWorkload = serde_json::from_value(json!({
        "query_workload": {
            "language": "promql",
            "query_batch": null,
            "repeating_queries": [{
                "query": "sum(rate(http_requests_total{job=\"order-service\"}[5m] offset 1h))",
                "demand": {"fixed_interval_at": {"interval": 60000, "evaluation_phase": 0}},
                "requirements": {"accuracy": {"max_error": 0.01, "failure_probability": 0.001}},
                "predictability": {"predictable": {"known_at": null}},
                "range_evaluation": {"step_ms": 60000, "span_ms": 3600000},
            }],
        },
        "data_workload": {
            "arrival": {"ingesting": {"rows_per_second": 1000.0}},
            "sources": {"http_requests_total": source},
        },
    }))
    .unwrap();
    milp_objective_equals_plan_cost(&workload, PlanningModels::builtin());
}

/// Plans `workload` and checks the MILP's objective equals `plan_cost` of
/// the selected plan to 1e-6 of it; returns that cost per second.
fn milp_objective_equals_plan_cost(workload: &PlanningWorkload, models: PlanningModels) -> f64 {
    let roots = lower_promql_roots(workload).unwrap();
    let entries: Vec<_> = workload.query_workload.entries().collect();
    let data = workload.data_workload.clone().unwrap();
    let run = run_planning_stages_over(roots, &entries, &data, models).unwrap();
    let (plan, selection) = run.run.selection.as_ref().expect("a plan is selected");
    let plan_cost = asap_integration_tests::planning::plan_cost_per_second(
        plan,
        &entries,
        &data,
        &asap_cost::CostInputs::default(),
    );
    assert!(
        (selection.milp_objective - plan_cost).abs() <= 1e-6 * plan_cost,
        "MILP objective {} vs plan cost {plan_cost}",
        selection.milp_objective
    );
    plan_cost
}

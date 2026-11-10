//! The selection MILP models the selected plan's cost exactly: its optimal
//! objective is `asap_cost::plan_cost` of the plan it builds, under cost
//! model 1 (concepts doc, section 5, "Cost and retention").
use asap_cost::CostInputs;
use asap_devtools::corpus_registry::{plan_workload, repeating_promql_workload};
use asap_devtools::planning_stages::RunUntil;
use asap_types::deployment_capabilities::DeploymentCapabilities;
use asap_types::ir::properties::AccuracyTarget;
use asap_types::workload::{DurationMs, PlanningWorkload, PrometheusMetricType, SourceFacts};
use serde_json::json;

/// Two windowed quantiles over one metric (1 m and 5 m, every 10 s; 10,000
/// series every 15 s), accuracy not enforced: the selected plan shares
/// one query-time scan, whose execution reads the 5 m window once and
/// retains it (h_v, constraint (R)). The MILP's objective equals the plan's
/// cost to 1e-6 of it.
#[test]
fn the_milp_objective_is_the_plan_cost_on_a_shared_scan() {
    let workload = repeating_promql_workload(
        &[
            "quantile_over_time(0.5, data[1m])",
            "quantile_over_time(0.9, data[5m])",
        ],
        10_000,
        AccuracyTarget::max_error(0.01).failure_probability(0.01),
        &[(
            "data",
            SourceFacts {
                series_count: Some(10_000),
                metric_type: Some(PrometheusMetricType::Gauge),
                sample_interval: Some(DurationMs(15_000)),
                ..SourceFacts::default()
            },
        )],
    );
    let plan_cost = milp_objective_equals_plan_cost(&workload, true);
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
    milp_objective_equals_plan_cost(&workload, false);
}

/// Plans `workload` (accuracy not enforced when `ignore_accuracy`) on the
/// default deployment and checks the MILP's objective equals `plan_cost`
/// of the selected plan to 1e-6 of it; returns that cost per second.
fn milp_objective_equals_plan_cost(workload: &PlanningWorkload, ignore_accuracy: bool) -> f64 {
    let cost_inputs = CostInputs::default();
    let planned = plan_workload(
        workload,
        &[],
        &DeploymentCapabilities::default(),
        &cost_inputs,
        ignore_accuracy,
        RunUntil::Stage3,
    )
    .unwrap();
    let (plan, selection) = planned
        .stages
        .run
        .selection
        .as_ref()
        .expect("a plan is selected");
    let plan_cost = asap_integration_tests::planning::plan_cost_per_second(
        plan,
        &planned.entries,
        &planned.data,
        &cost_inputs,
    );
    assert!(
        (selection.milp_objective - plan_cost).abs() <= 1e-6 * plan_cost,
        "MILP objective {} vs plan cost {plan_cost}",
        selection.milp_objective
    );
    plan_cost
}

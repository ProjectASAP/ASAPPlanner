//! Stage 3's selection objective is the cost of the plan it builds: the
//! MILP's optimum (`Selection::selected_plan_cost_per_second`) equals
//! `asap_cost::plan_cost` of the selected plan, under the default unit
//! costs and under unit costs that charge per-request overhead and disk.

use asap_cost::{CostInputs, CostModel, ResourceUnitCost};
use asap_devtools::planning_stages::{lower_promql_roots, run_planning_stages_over};
use asap_planner::{DeploymentCapabilities, PlanningModels};
use asap_types::workload::PlanningWorkload;

/// PromQL `queries`, each `(query, interval_ms, accuracy JSON)`, over the
/// gauge `m` (1,000 series, one sample per second, 1,000 rows/s).
fn workload(queries: &[(&str, u32, &str)]) -> PlanningWorkload {
    let entries: Vec<String> = queries
        .iter()
        .map(|(query, interval_ms, accuracy)| {
            format!(
                r#"{{"query": {query:?}, "demand": {{"fixed_interval_at": {{"interval": {interval_ms}, "evaluation_phase": 0}}}},
                  "requirements": {{{accuracy}}}, "predictability": {{"predictable": {{"known_at": null}}}}}}"#
            )
        })
        .collect();
    serde_json::from_str(&format!(
        r#"{{"query_workload": {{"language": "promql", "repeating_queries": [{}]}},
            "data_workload": {{"arrival": {{"ingesting": {{"rows_per_second": 1000.0}}}},
              "sources": {{"m": {{"series_count": 1000, "metric_type": "gauge", "sample_interval": 1000}}}}}}}}"#,
        entries.join(",")
    ))
    .unwrap()
}

/// The MILP's optimum and `plan_cost` of the plan Stage 3 selects for
/// `workload` under `settings`, with raw data kept or not.
fn objective_and_plan_cost(
    workload: &PlanningWorkload,
    settings: &CostInputs,
    raw_data_retained: bool,
) -> (f64, f64) {
    let capabilities = DeploymentCapabilities {
        raw_data_retained,
        ..Default::default()
    };
    let models = PlanningModels::builtin()
        .with_cost_inputs(settings)
        .with_deployment_capabilities(&capabilities);
    objective_and_plan_cost_with(workload, models, settings)
}

/// The MILP's optimum and `plan_cost` of the plan Stage 3 selects for
/// `workload` with `models`, whose cost inputs are `settings`.
fn objective_and_plan_cost_with(
    workload: &PlanningWorkload,
    models: PlanningModels<'_>,
    settings: &CostInputs,
) -> (f64, f64) {
    let entries: Vec<_> = workload.query_workload.entries().collect();
    let data = workload.data_workload.clone().unwrap();
    let roots = lower_promql_roots(workload).unwrap();
    let run = run_planning_stages_over(roots, &entries, &data, models).unwrap();
    let (plan, selection) = run.run.selection.as_ref().expect("a plan is selected");
    let (cost, _) = asap_cost::plan_cost(plan, &entries, &data, settings).unwrap();
    (
        selection.selected_plan_cost_per_second,
        cost.cost_per_second.expect("cost model 1"),
    )
}

fn assert_equal(name: &str, (objective, plan_cost): (f64, f64)) {
    assert!(
        (objective - plan_cost).abs() <= 1e-9 * plan_cost.abs().max(1e-12),
        "{name}: objective {objective} != plan_cost {plan_cost}"
    );
}

/// Unit costs that charge what the default ones leave at 0: 50 µs of
/// runtime overhead per query request and 1e-9 per byte-second on disk.
fn overhead_and_disk_costs() -> CostInputs {
    let mut settings = CostInputs::default();
    settings.measured_operation_costs.ns_per_evaluation = 50_000.0;
    settings.cost_model = CostModel::UsageOverTime(ResourceUnitCost {
        cost_per_disk_byte_second: 1e-9,
        ..ResourceUnitCost::illustrative()
    });
    settings
}

/// A 5-minute sum every minute and a 14-minute max every 7 minutes, no raw
/// data kept: one ingestion-time `Scan` feeds a 60 s chain and a 420 s
/// chain. Each row is scanned once at ingestion, so the scan runs once
/// per second for both (not once per refresh window, which made
/// `plan_cost` 5.6% higher than the objective).
#[test]
fn an_ingestion_time_scan_feeding_chains_of_two_widths_is_charged_once_per_second() {
    let w = workload(&[
        ("sum_over_time(m[5m])", 60_000, ""),
        ("max_over_time(m[14m])", 420_000, ""),
    ]);
    assert_equal(
        "two widths",
        objective_and_plan_cost(&w, &CostInputs::default(), false),
    );
}

/// The same objective as `plan_cost` with per-request overhead and disk
/// charged: a raw-data source two query-time scans read is kept on disk
/// once, at the larger read, and each query request pays its overhead.
#[test]
fn the_objective_includes_request_overhead_and_charges_raw_data_on_disk_once_per_source() {
    let settings = overhead_and_disk_costs();
    let workloads = [
        (
            "two series filters on m",
            workload(&[
                (r#"sum_over_time(m{job="a"}[5m])"#, 60_000, ""),
                (r#"sum_over_time(m{job="b"}[5m])"#, 60_000, ""),
            ]),
        ),
        (
            "one query",
            workload(&[("count_over_time(m[1m])", 60_000, "")]),
        ),
    ];
    for (name, w) in &workloads {
        assert_equal(name, objective_and_plan_cost(w, &settings, true));
    }
}

/// Two windowed quantiles over one metric (1 m and 5 m, every 10 s; 10,000
/// series every 15 s), accuracy not enforced: the selected plan shares one
/// query-time scan, whose execution reads the 5 m window once and retains
/// it ($h_v$, constraint (R)), at most 0.144/s.
#[test]
fn the_objective_is_the_plan_cost_on_a_shared_scan() {
    let w: PlanningWorkload = serde_json::from_value(serde_json::json!({
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
    let models = PlanningModels::builtin().with_ignore_accuracy_guarantees(true);
    let (objective, plan_cost) = objective_and_plan_cost_with(&w, models, &CostInputs::default());
    assert_equal("shared scan", (objective, plan_cost));
    assert!(plan_cost <= 0.144, "{plan_cost}");
}

/// The o11y benchmark's q21: `sum(rate(..[5m] offset 1h))`, a 1 h range
/// query every 60 s. Raw rows it reads may be materialized at ingestion
/// time behind the offset (a `TimeShift` over the `Scan`); that store
/// retains 1 h 5 m of rows, which its `TimeRange` reads 5 m of, in the
/// MILP and in `plan_cost` alike (the MILP's objective was 0.326/s for a
/// plan `plan_cost` charged 0.0219/s, one second of rows retained).
#[test]
fn the_objective_is_the_plan_cost_on_an_offset_range_query() {
    let source = serde_json::json!({"sample_interval": 1000, "series_count": 1000, "metric_type": "counter"});
    let w: PlanningWorkload = serde_json::from_value(serde_json::json!({
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
    assert_equal(
        "offset range query",
        objective_and_plan_cost_with(&w, PlanningModels::builtin(), &CostInputs::default()),
    );
}

/// A query `query` every 10 s at phase 0, within 1 % at δ = 0.01.
fn every_10s(query: &str) -> serde_json::Value {
    serde_json::json!({
        "query": query,
        "demand": {"fixed_interval_at": {"interval": 10000, "evaluation_phase": 0}},
        "requirements": {"accuracy": {"max_error": 0.01, "failure_probability": 0.01}},
        "predictability": {"predictable": {"known_at": null}},
    })
}

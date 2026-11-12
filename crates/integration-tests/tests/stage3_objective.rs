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

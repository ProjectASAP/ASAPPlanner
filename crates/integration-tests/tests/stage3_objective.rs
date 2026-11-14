//! Stage 3's selection objective is the cost of the plan it builds: the
//! MILP's optimum (`Selection::selected_plan_cost_per_second`) equals
//! `asap_cost::plan_cost` of the selected plan, under the default unit
//! costs and under unit costs that charge per-request overhead and disk.

use asap_cost::{CostInputs, CostModel, ResourceUnitCost};
use asap_devtools::planning_stages::{lower_promql_roots, run_planning_stages_over};
use asap_planner::{DeploymentCapabilities, PlanningModels};
use asap_types::ir::properties::ExecutionTiming;
use asap_types::ir::NonASAPOp;
use asap_types::plan::physical_asap_dag::{PhysicalASAPDAG, PhysicalASAPOperatorPayload};
use asap_types::stages::Lifetime;
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
    selected_plan_objective_and_plan_cost(workload, settings, raw_data_retained).1
}

/// The plan Stage 3 selects for `workload` under `settings`, with raw data
/// kept or not, with the MILP's optimum and its `plan_cost`.
fn selected_plan_objective_and_plan_cost(
    workload: &PlanningWorkload,
    settings: &CostInputs,
    raw_data_retained: bool,
) -> (PhysicalASAPDAG, (f64, f64)) {
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
        plan.clone(),
        (
            selection.selected_plan_cost_per_second,
            cost.cost_per_second.expect("cost model 1"),
        ),
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

/// Offset windows, no raw data kept: `sum_over_time(m[5m] offset 90s)`
/// every minute, and `sum_over_time(m[1m] offset 1h)` every hour with
/// `sum_over_time(m[1m])` every 10 s. An ingestion-time `TimeShift` view
/// over the arriving rows keeps the rows its retention and its offset
/// cover (390 s and 61 min of rows), not one second of them, so it is
/// dearer than the chain plans and is not selected; the selected plan
/// installs, and the objective is its `plan_cost`.
#[test]
fn offset_windows_without_raw_data_select_installable_plans_costed_for_their_retention() {
    let workloads = [
        (
            "offset 90s",
            workload(&[("sum_over_time(m[5m] offset 90s)", 60_000, "")]),
        ),
        (
            "offset 1h every hour",
            workload(&[
                ("sum_over_time(m[1m] offset 1h)", 3_600_000, ""),
                ("sum_over_time(m[1m])", 10_000, ""),
            ]),
        ),
    ];
    for (name, w) in &workloads {
        let (plan, costs) = selected_plan_objective_and_plan_cost(w, &CostInputs::default(), false);
        assert_equal(name, costs);
        let ingestion_time_shift_views: Vec<_> = plan
            .nodes
            .iter()
            .filter(|node| {
                node.output_state.timing == ExecutionTiming::IngestionTime
                    && matches!(node.lifetime, Lifetime::AcrossEvaluations(_))
                    && matches!(
                        node.payload,
                        PhysicalASAPOperatorPayload::NonASAP(NonASAPOp::TimeShift { .. })
                    )
            })
            .map(|node| node.id)
            .collect();
        assert!(
            ingestion_time_shift_views.is_empty(),
            "{name}: an ingestion-time TimeShift view is selected: {ingestion_time_shift_views:?}"
        );
        let entries: Vec<_> = w.query_workload.entries().collect();
        asap_planner::installable_plan_of(plan, &entries).unwrap_or_else(|error| {
            panic!("{name}: the selected plan does not install: {error:?}")
        });
    }
}

/// Unit costs with a disk price like a cloud block volume's (3.1e-17 per
/// byte-second): what keeping raw data on disk costs per second is below
/// 1e-9, where the solver drops a constraint coefficient as zero.
fn small_disk_costs() -> CostInputs {
    CostInputs {
        cost_model: CostModel::UsageOverTime(ResourceUnitCost {
            cost_per_cpu_op: 1.1244e-14,
            cost_per_scanned_byte: 1.1244e-15,
            cost_per_memory_byte_second: 1.150183379650116e-15,
            cost_per_disk_byte_second: 3.1e-17,
            ..ResourceUnitCost::illustrative()
        }),
        ..CostInputs::default()
    }
}

/// `sum_over_time(m[5m] offset 30m)` and `sum_over_time(m[5m])` every
/// minute and `count_over_time(m[2m])` every hour, raw data kept, disk
/// charged at 3.1e-17 per byte-second: raw data on disk costs about 2e-9
/// per second. Its row carries the objective's scale, so the solver keeps
/// it, and the plan selected costs less than the one selected when disk
/// is free, costed at the disk price. With the scale on the raw-data
/// variable's objective coefficient instead, the solver dropped the
/// unscaled coefficients and selected that plan, 57% dearer.
#[test]
fn raw_data_on_disk_costing_less_than_the_solver_tolerance_still_steers_selection() {
    let w = workload(&[
        ("sum_over_time(m[5m] offset 30m)", 60_000, ""),
        ("sum_over_time(m[5m])", 60_000, ""),
        ("count_over_time(m[2m])", 3_600_000, ""),
    ]);
    let settings = small_disk_costs();
    let (_, (objective, plan_cost)) = selected_plan_objective_and_plan_cost(&w, &settings, true);
    assert_equal("small disk price", (objective, plan_cost));
    let mut free_disk = settings.clone();
    if let CostModel::UsageOverTime(unit_costs) = &mut free_disk.cost_model {
        unit_costs.cost_per_disk_byte_second = 0.0;
    }
    let (ignoring_disk, _) = selected_plan_objective_and_plan_cost(&w, &free_disk, true);
    let entries: Vec<_> = w.query_workload.entries().collect();
    let data = w.data_workload.clone().unwrap();
    let (ignoring_disk_cost, _) =
        asap_cost::plan_cost(&ignoring_disk, &entries, &data, &settings).unwrap();
    let ignoring_disk_cost = ignoring_disk_cost.cost_per_second.unwrap();
    assert!(
        plan_cost < ignoring_disk_cost * (1.0 - 1e-6),
        "selected {plan_cost} is not cheaper than {ignoring_disk_cost}, the plan selected ignoring disk"
    );
}

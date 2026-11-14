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
    selected_plan_objective_and_plan_cost_with(workload, models, settings)
}

/// The plan Stage 3 selects for `workload` with `models`, whose cost
/// inputs are `settings`, with the MILP's optimum and its `plan_cost`.
fn selected_plan_objective_and_plan_cost_with(
    workload: &PlanningWorkload,
    models: PlanningModels<'_>,
    settings: &CostInputs,
) -> (PhysicalASAPDAG, (f64, f64)) {
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
    let (_, (objective, plan_cost)) =
        selected_plan_objective_and_plan_cost_with(&w, models, &CostInputs::default());
    assert_equal("shared scan", (objective, plan_cost));
    assert!(plan_cost <= 0.144, "{plan_cost}");
}

/// The o11y benchmark's q21: `sum(rate(..[5m] offset 1h))`, a 1 h range
/// query every 60 s. Raw rows it reads may be materialized at ingestion
/// time behind the offset (a `TimeShift` over the `Scan`); that store
/// keeps 2 h 5 m of rows (its retention: the 1 h span, the 5 m range and
/// the 1 h offset), which its `TimeRange` reads 5 m of, in the MILP and in
/// `plan_cost` alike (the MILP's objective was 0.326/s for a plan
/// `plan_cost` charged 0.0219/s, one second of rows retained).
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
        selected_plan_objective_and_plan_cost_with(
            &w,
            PlanningModels::builtin(),
            &CostInputs::default(),
        )
        .1,
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

/// `query` requested every `interval_ms` at phase 0 as a range query with
/// steps of `step_ms` over `span_ms`, or as an instant query without one.
fn requested(query: &str, interval_ms: u32, range: Option<(u64, u64)>) -> serde_json::Value {
    let mut entry = serde_json::json!({
        "query": query,
        "demand": {"fixed_interval_at": {"interval": interval_ms, "evaluation_phase": 0}},
        "requirements": {"max_latency_ms": null},
        "predictability": {"predictable": {"known_at": null}},
    });
    if let Some((step_ms, span_ms)) = range {
        entry["range_evaluation"] = serde_json::json!({"step_ms": step_ms, "span_ms": span_ms});
    }
    entry
}

/// `entries` over the gauge `m` (1,000 series, one sample per second).
fn over_m(entries: Vec<serde_json::Value>) -> PlanningWorkload {
    serde_json::from_value(serde_json::json!({
        "query_workload": {"language": "promql", "repeating_queries": entries},
        "data_workload": {
            "arrival": {"ingesting": {"rows_per_second": 1000.0}},
            "sources": {"m": {"series_count": 1000, "metric_type": "gauge", "sample_interval": 1000}},
        },
    }))
    .unwrap()
}

/// Two range queries every 300 s at 30 s steps over 600 s that read one
/// `@ end()` subtree: they share its shift, `OneEvaluation`, and its input
/// runs once per request, in the MILP and in `plan_cost` alike
/// (`plan_cost` charged it at every step: 0.1513/s for an objective of
/// 0.0473/s).
#[test]
fn the_objective_is_the_plan_cost_on_a_shared_at_end_subtree() {
    let w = over_m(vec![
        requested(
            "sum_over_time(m[5m] @ end())",
            300_000,
            Some((30_000, 600_000)),
        ),
        requested(
            "max_over_time(m[5m] @ end())",
            300_000,
            Some((30_000, 600_000)),
        ),
    ]);
    assert_equal(
        "shared @ end() subtree",
        objective_and_plan_cost(&w, &CostInputs::default(), true),
    );
}

/// Two range queries every 60 s at 30 s steps, over 1 h and over 90 s,
/// below `@ start()`: one context, whose requests start at two times each
/// minute, `k · 60 s` and `k · 60 s + 30 s`; Stage 3 and `plan_cost` count
/// both.
#[test]
fn the_objective_is_the_plan_cost_on_at_start_subtrees_of_two_spans() {
    let w = over_m(vec![
        requested(
            "max_over_time(m[5m] @ start())",
            60_000,
            Some((30_000, 3_600_000)),
        ),
        requested(
            "max_over_time(m[5m] @ start())",
            60_000,
            Some((30_000, 90_000)),
        ),
    ]);
    assert_equal(
        "@ start() subtrees of two spans",
        objective_and_plan_cost(&w, &CostInputs::default(), true),
    );
}

/// An instant `max_over_time(m[5m])` every 300 s and the range query
/// `max_over_time(m[5m] @ start())` every 300 s at 30 s steps over 600 s:
/// the range query reads `m[5m]` at its requests' start, 600 s before the
/// instant query's evaluation, so no result computed for one evaluation is
/// read by both (a `OneEvaluation` `m[5m]` served both).
#[test]
fn an_at_start_subtree_shares_no_evaluation_result_with_an_instant_query() {
    let w = over_m(vec![
        requested("max_over_time(m[5m])", 300_000, None),
        requested(
            "max_over_time(m[5m] @ start())",
            300_000,
            Some((30_000, 600_000)),
        ),
    ]);
    let (plan, objective_and_plan_cost) =
        selected_plan_objective_and_plan_cost(&w, &CostInputs::default(), true);
    assert_equal("@ start() beside an instant query", objective_and_plan_cost);
    let producers = |id| {
        plan.edges
            .iter()
            .filter(move |edge| edge.consumer == id)
            .map(|edge| edge.producer)
    };
    // The nodes each query's walk reaches below its root, stopping at
    // results kept across evaluations or computed at ingestion time.
    let reached = |root: usize| {
        let mut reached = std::collections::BTreeSet::new();
        let mut pending = vec![root];
        while let Some(id) = pending.pop() {
            let node = &plan.nodes[id];
            if matches!(node.lifetime, Lifetime::AcrossEvaluations(_))
                || node.output_state.timing == ExecutionTiming::IngestionTime
                || !reached.insert(id)
            {
                continue;
            }
            pending.extend(producers(id));
        }
        reached
    };
    let (instant, at_start) = (reached(plan.roots[0]), reached(plan.roots[1]));
    assert!(
        instant.is_disjoint(&at_start),
        "{instant:?} and {at_start:?} share a node: {plan:#?}"
    );
}

/// `@ end()` and `@ start()` range queries every 300 s at 30 s steps next
/// to the plain `sum_over_time(m[5m])` every 30 s, on the same 30 s grid:
/// an anchored input runs once per request of the queries that reach its
/// shift, not of every query on the grid reaching `m[5m]` (the objective
/// was 0.2404/s for a plan `plan_cost` charged 0.1364/s).
#[test]
fn the_objective_is_the_plan_cost_on_anchored_and_plain_queries_on_one_grid() {
    let w = over_m(vec![
        requested(
            "sum_over_time(m[5m] @ end())",
            300_000,
            Some((30_000, 600_000)),
        ),
        requested(
            "max_over_time(m[5m] @ start())",
            300_000,
            Some((30_000, 600_000)),
        ),
        requested("sum_over_time(m[5m])", 30_000, None),
    ]);
    assert_equal(
        "anchored and plain queries on one grid",
        objective_and_plan_cost(&w, &CostInputs::default(), true),
    );
}

/// Example 3a of the planner-layering design doc: five one-off p99s over
/// 1,000,000 series sampled every 15 s, over the last 5 y, 1 y, 1 y offset
/// 1 y and 2 y, and 3 y offset 2 y, accuracy guarantees ignored. An
/// ingestion-time view behind an offset retains one output per window it
/// maintains, so its $\pi_v$ is one output's cost over that window: costed
/// per second instead, $\pi_v h_v$ of a view kept for years was 6.6e12/s,
/// its $W^{\mathrm{mat}}_v$ as negative, and the objective's scale left the
/// solver at a plan costing 4.68e7/s instead of 2.17e4/s.
#[test]
fn the_objective_is_the_plan_cost_on_one_off_queries_over_years() {
    let query = |query: &str| {
        serde_json::json!({
            "query": query,
            "requirements": {"accuracy": {"accuracy_metric": "rank", "max_error": 0.005, "failure_probability": 0.01, "accuracy_scope": "whole_result"}},
            "predictability": "ad_hoc", "invocations": 1, "execute_at": 1767225600000_u64,
        })
    };
    let w: PlanningWorkload = serde_json::from_value(serde_json::json!({
        "query_workload": {"language": "promql", "query_batch": [
            query("quantile_over_time(0.99, latency_ms[5y])"),
            query("quantile_over_time(0.99, latency_ms[1y])"),
            query("quantile_over_time(0.99, latency_ms[1y] offset 1y)"),
            query("quantile_over_time(0.99, latency_ms[1y] offset 2y)"),
            query("quantile_over_time(0.99, latency_ms[3y] offset 2y)"),
        ]},
        "data_workload": {
            "arrival": {"mixed": {"rows_per_second": 1_000_000.0 / 15.0}},
            "sources": {"latency_ms": {"series_count": 1_000_000, "sample_interval": 15_000}},
        },
    }))
    .unwrap();
    let models = PlanningModels::builtin().with_ignore_accuracy_guarantees(true);
    let (objective, plan_cost) =
        selected_plan_objective_and_plan_cost_with(&w, models, &CostInputs::default()).1;
    assert_equal("one-off queries over years", (objective, plan_cost));
    assert!(objective < 30_000.0, "objective {objective}");
}

/// `sum_over_time(m[5m] @ end())` and `sum_over_time(m[5m])`, both instant
/// every 60 s: an instant query's `@ end()` is its evaluation time, so the
/// anchored input runs in the evaluation context and shares `m[5m]` with
/// the plain query, in the MILP and in `plan_cost` alike (0.0360/s; in a
/// request context of its own it cost 0.0620/s).
#[test]
fn an_instant_at_end_input_shares_the_evaluation_context() {
    let w = over_m(vec![
        requested("sum_over_time(m[5m] @ end())", 60_000, None),
        requested("sum_over_time(m[5m])", 60_000, None),
    ]);
    let (objective, plan_cost) = objective_and_plan_cost(&w, &CostInputs::default(), true);
    assert_equal(
        "instant @ end() beside a plain query",
        (objective, plan_cost),
    );
    assert!(objective < 0.04, "objective {objective}");
}

/// `sum_over_time(m[1m])` every 20 s at phase 10 s and
/// `max_over_time(m[1m])` every 10 s: one 10 s grid, two phases. Stage 3
/// and `plan_cost` pick the queries of each context with one rule
/// (`queries_of_context`), and the optimum is the plan's cost.
#[test]
fn the_objective_is_the_plan_cost_on_two_phases_of_one_grid() {
    let mut late = requested("sum_over_time(m[1m])", 20_000, None);
    late["demand"]["fixed_interval_at"]["evaluation_phase"] = serde_json::json!(10_000);
    let w = over_m(vec![late, requested("max_over_time(m[1m])", 10_000, None)]);
    assert_equal(
        "two phases of one grid",
        objective_and_plan_cost(&w, &CostInputs::default(), true),
    );
}

/// An instant `sum_over_time(m[5m] @ end())` and a range
/// `sum_over_time(m[5m] @ end() offset 10m)` over the last 10 minutes,
/// both every 60 s: `m[5m]` is read by the instant query in its
/// evaluation context and by the range query once per request below its
/// shift, so a run in the evaluation context is rated by the instant query
/// only, in the MILP and in `plan_cost` alike (the objective was 0.3642/s
/// for a plan `plan_cost` charged 0.1542/s).
#[test]
fn the_objective_is_the_plan_cost_on_an_instant_and_a_shifted_range_at_end() {
    let w: PlanningWorkload = serde_json::from_value(serde_json::json!({
        "query_workload": {"language": "promql", "repeating_queries": [
            requested("sum_over_time(m[5m] @ end())", 60_000, None),
            requested(
                "sum_over_time(m[5m] @ end() offset 600s)",
                60_000,
                Some((60_000, 600_000)),
            ),
        ]},
        "data_workload": {
            "arrival": {"ingesting": {"rows_per_second": 2000.0}},
            "sources": {
                "m": {"series_count": 1000, "metric_type": "gauge", "sample_interval": 1000},
                "c": {"series_count": 1000, "metric_type": "counter", "sample_interval": 1000},
            },
        },
    }))
    .unwrap();
    let capabilities = DeploymentCapabilities {
        supported_summaries: Some(asap_executor::supported_summary_families()),
        ..Default::default()
    };
    let models = PlanningModels::builtin().with_deployment_capabilities(&capabilities);
    assert_equal(
        "instant and shifted range @ end()",
        selected_plan_objective_and_plan_cost_with(&w, models, &CostInputs::default()).1,
    );
}

/// Two range queries every 300 s on one grid reach one `@ end()` group,
/// `m[5m] @ end()`, and may read it through different candidates: a sum
/// over 10 minutes at 30 s steps and a median over an hour at 60 s steps,
/// accuracy guarantees ignored (so the median may read a sketch). Stage 3
/// rates the anchored context from the memo, `plan_cost` from the plan's
/// walk; the optimum is the plan's cost.
#[test]
fn the_objective_is_the_plan_cost_when_two_queries_read_one_anchored_group() {
    let w = over_m(vec![
        requested(
            "sum_over_time(m[5m] @ end())",
            300_000,
            Some((30_000, 600_000)),
        ),
        requested(
            "quantile_over_time(0.5, m[5m] @ end())",
            300_000,
            Some((60_000, 3_600_000)),
        ),
    ]);
    let models = PlanningModels::builtin().with_ignore_accuracy_guarantees(true);
    assert_equal(
        "two queries reading one anchored group",
        selected_plan_objective_and_plan_cost_with(&w, models, &CostInputs::default()).1,
    );
}

//! Stage 3's selection objective is the cost of the plan it builds: the
//! MILP's optimum (`Selection::selected_plan_cost_per_second`) equals
//! `asap_cost::plan_cost` of the selected plan, under the default unit
//! costs and under unit costs that charge per-request overhead and disk.

use asap_cost::{
    AnswerCache, CostInputs, CostModel, KeptQueryTimeResults, ResourceUnitCost, ViewWindowCache,
};
use asap_devtools::planning_stages::{lower_promql_roots, run_planning_stages_over};
use asap_planner::{DeploymentCapabilities, PlanningModels};
use asap_types::ir::properties::ExecutionTiming;
use asap_types::ir::NonASAPOp;
use asap_types::plan::physical_asap_dag::{
    PhysicalASAPDAG, PhysicalASAPNodeId, PhysicalASAPOperatorPayload,
};
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
            selection
                .selected_plan_cost_per_second
                .expect("cost model 1"),
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

/// The ids of the plan's `Scan` nodes, each with how many of the plan's
/// roots (queries) read it, directly or through other nodes: a `Scan` read
/// by 2 of 2 queries is one scan they share.
fn scans_and_their_reading_query_counts(
    plan: &PhysicalASAPDAG,
) -> Vec<(PhysicalASAPNodeId, usize)> {
    let reads = |root: PhysicalASAPNodeId, target: PhysicalASAPNodeId| {
        let mut pending = vec![root];
        let mut seen = vec![root];
        while let Some(node) = pending.pop() {
            if node == target {
                return true;
            }
            for edge in plan.edges.iter().filter(|edge| edge.consumer == node) {
                if !seen.contains(&edge.producer) {
                    seen.push(edge.producer);
                    pending.push(edge.producer);
                }
            }
        }
        false
    };
    plan.nodes
        .iter()
        .filter(|node| {
            matches!(
                node.payload,
                PhysicalASAPOperatorPayload::NonASAP(NonASAPOp::Scan { .. })
            )
        })
        .map(|node| {
            let readers = plan
                .roots
                .iter()
                .filter(|&&root| reads(root, node.id))
                .count();
            (node.id, readers)
        })
        .collect()
}

/// Asserts that `plan` has one `Scan`, read by all `query_count` queries.
fn assert_one_scan_read_by_every_query(plan: &PhysicalASAPDAG, query_count: usize) {
    assert_eq!(
        scans_and_their_reading_query_counts(plan)
            .iter()
            .map(|&(_, readers)| readers)
            .collect::<Vec<_>>(),
        vec![query_count],
        "expected one Scan read by all {query_count} queries"
    );
}

/// `plan_cost` per second of the plan Stage 3 selects for each of the
/// `queries` planned alone.
fn costs_of_queries_planned_alone(
    queries: &[serde_json::Value],
    workload_of: impl Fn(&[serde_json::Value]) -> PlanningWorkload,
    models: impl Fn() -> PlanningModels<'static>,
) -> Vec<f64> {
    queries
        .iter()
        .map(|query| {
            let alone = workload_of(std::slice::from_ref(query));
            selected_plan_objective_and_plan_cost_with(&alone, models(), &CostInputs::default())
                .1
                 .1
        })
        .collect()
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

/// Unit costs that charge disk alone, at a price like a cloud block
/// volume's (3.1e-17 per byte-second): what keeping raw data on disk costs
/// per second is below 1e-9, where the solver drops a constraint
/// coefficient as zero, and every other resource is free.
fn disk_only_costs() -> CostInputs {
    CostInputs {
        cost_model: CostModel::UsageOverTime(ResourceUnitCost {
            cost_per_cpu_op: 0.0,
            cost_per_scanned_byte: 0.0,
            cost_per_memory_byte_second: 0.0,
            cost_per_provisioned_memory_byte_second: 0.0,
            cost_per_provisioned_cpu_core_second: 0.0,
            cost_per_disk_byte_second: 3.1e-17,
        }),
        ..CostInputs::default()
    }
}

/// `sum_over_time(m[5m] offset 30m)` and `sum_over_time(m[5m])` every
/// minute and `count_over_time(m[2m])` every hour, raw data kept, with disk
/// the only resource charged (3.1e-17 per byte-second): a plan that scans
/// raw data keeps it on disk for about 2e-9 per second, one that reads
/// only stored windows costs nothing. The raw-data row carries the
/// objective's scale, so the solver keeps that 2e-9 and selects a plan
/// keeping no raw data, whose objective is its `plan_cost`, 0. With the
/// scale on the raw-data variable's objective coefficient instead, the
/// solver dropped the unscaled coefficients and could select either.
#[test]
fn raw_data_on_disk_costing_less_than_the_solver_tolerance_still_steers_selection() {
    let w = workload(&[
        ("sum_over_time(m[5m] offset 30m)", 60_000, ""),
        ("sum_over_time(m[5m])", 60_000, ""),
        ("count_over_time(m[2m])", 3_600_000, ""),
    ]);
    let settings = disk_only_costs();
    let (plan, (objective, plan_cost)) = selected_plan_objective_and_plan_cost(&w, &settings, true);
    assert_equal("disk only", (objective, plan_cost));
    assert_eq!(
        plan_cost, 0.0,
        "the selected plan keeps raw data: {plan:#?}"
    );
    // A plan scanning raw data, the one selected under the default costs,
    // pays for it on disk.
    let (scanning, _) = selected_plan_objective_and_plan_cost(&w, &CostInputs::default(), true);
    let entries: Vec<_> = w.query_workload.entries().collect();
    let data = w.data_workload.clone().unwrap();
    let (scanning_cost, _) = asap_cost::plan_cost(&scanning, &entries, &data, &settings).unwrap();
    let scanning_cost = scanning_cost.cost_per_second.unwrap();
    assert!(
        (1e-10..1e-8).contains(&scanning_cost),
        "a plan scanning raw data costs {scanning_cost} on disk"
    );
}

/// Two windowed quantiles over one metric (1 m and 5 m, every 10 s; 10,000
/// series every 15 s), accuracy not enforced: the selected plan shares one
/// query-time scan, whose execution reads the 5 m window once and retains
/// it ($h_v$, constraint (R)). It costs less than the two queries planned
/// alone, which each read the metric once.
#[test]
fn the_objective_is_the_plan_cost_on_a_shared_scan() {
    let queries = [
        every_10s("quantile_over_time(0.5, data[1m])"),
        every_10s("quantile_over_time(0.9, data[5m])"),
    ];
    let workload_of = |queries: &[serde_json::Value]| -> PlanningWorkload {
        serde_json::from_value(serde_json::json!({
            "query_workload": {
                "language": "promql",
                "query_batch": null,
                "repeating_queries": queries,
            },
            "data_workload": {
                "arrival": {"ingesting": {"rows_per_second": 666.67}},
                "sources": {"data": {"series_count": 10000, "metric_type": "gauge", "sample_interval": 15000}},
            },
        }))
        .unwrap()
    };
    let models = || PlanningModels::builtin().with_ignore_accuracy_guarantees(true);
    let (plan, (objective, plan_cost)) = selected_plan_objective_and_plan_cost_with(
        &workload_of(&queries),
        models(),
        &CostInputs::default(),
    );
    assert_equal("shared scan", (objective, plan_cost));
    assert_one_scan_read_by_every_query(&plan, 2);
    let alone: f64 = costs_of_queries_planned_alone(&queries, workload_of, models)
        .iter()
        .sum();
    assert!(
        plan_cost < alone,
        "shared {plan_cost} is not below the two queries planned alone, {alone}"
    );
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
///
/// The data store here keeps raw rows in batches of 30 min
/// (`raw_batch_width_ms`), which the second check depends on: a per-series
/// grouping visits each series once per batch, and a 15 s series has at
/// most one row in a batch narrower than 15 s, so at the 10 s default every
/// row is a visit, the raw scan costs more than the views and the two
/// plans are the same. With 30 min batches, a series has 120 rows in a
/// batch. Stored DDSketch windows priced at their view layout, the views
/// are the cheapest plan with raw data kept or not. The same deployment
/// asked one p99 over the last hour reads the raw rows at query time,
/// which costs less than maintaining views for it.
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
    let mut settings = CostInputs::default();
    settings.resource_speed.raw_batch_width_ms = 1_800_000;
    let models = PlanningModels::builtin()
        .with_ignore_accuracy_guarantees(true)
        .with_cost_inputs(&settings);
    let (objective, plan_cost) =
        selected_plan_objective_and_plan_cost_with(&w, models, &settings).1;
    assert_equal("one-off queries over years", (objective, plan_cost));
    // The same queries over arriving rows only, with no raw data kept (a
    // deployment that stores nothing, as `mixed` arrival does): only
    // ingestion-time views over the arriving rows can answer them, the
    // kind of plan the objective's scale once led the solver to at 2,000x
    // the cost of the best. With raw data kept the selection can only cost
    // less or the same; here the views are the cheapest either way.
    let capabilities = DeploymentCapabilities {
        raw_data_retained: false,
        ..Default::default()
    };
    let views_only = PlanningModels::builtin()
        .with_ignore_accuracy_guarantees(true)
        .with_cost_inputs(&settings)
        .with_deployment_capabilities(&capabilities);
    let mut ingesting = serde_json::to_value(&w).unwrap();
    ingesting["data_workload"]["arrival"] =
        serde_json::json!({"ingesting": {"rows_per_second": 1_000_000.0 / 15.0}});
    let ingesting: PlanningWorkload = serde_json::from_value(ingesting).unwrap();
    let (_, (_, views_only_plan_cost)) =
        selected_plan_objective_and_plan_cost_with(&ingesting, views_only, &settings);
    assert!(
        plan_cost <= views_only_plan_cost,
        "selected {plan_cost} is above the ingestion-time views plan, {views_only_plan_cost}"
    );
    // Stored DDSketch windows are priced at their view layout, lighter
    // than the raw rows they summarize, so no memory price makes reading
    // years of raw rows cheaper. One p99 over the last hour, once, is the
    // case where raw rows win: the selection reads them at query time and
    // costs less than maintaining the views for it.
    let mut last_hour = serde_json::to_value(&w).unwrap();
    last_hour["query_workload"]["query_batch"] =
        serde_json::json!([query("quantile_over_time(0.99, latency_ms[1h])")]);
    let last_hour: PlanningWorkload = serde_json::from_value(last_hour).unwrap();
    let raw_kept = PlanningModels::builtin()
        .with_ignore_accuracy_guarantees(true)
        .with_cost_inputs(&settings);
    let (raw_kept_plan, (_, raw_kept_plan_cost)) =
        selected_plan_objective_and_plan_cost_with(&last_hour, raw_kept, &settings);
    assert!(
        raw_kept_plan.nodes.iter().any(|node| {
            matches!(
                node.payload,
                PhysicalASAPOperatorPayload::NonASAP(NonASAPOp::Scan { .. })
            ) && node.output_state.timing == ExecutionTiming::QueryTime
        }),
        "one-off last hour: the selection scans no raw rows at query time"
    );
    let mut last_hour_ingesting = serde_json::to_value(&last_hour).unwrap();
    last_hour_ingesting["data_workload"]["arrival"] =
        serde_json::json!({"ingesting": {"rows_per_second": 1_000_000.0 / 15.0}});
    let last_hour_ingesting: PlanningWorkload =
        serde_json::from_value(last_hour_ingesting).unwrap();
    let views_only = PlanningModels::builtin()
        .with_ignore_accuracy_guarantees(true)
        .with_cost_inputs(&settings)
        .with_deployment_capabilities(&capabilities);
    let (_, (_, views_only_plan_cost)) =
        selected_plan_objective_and_plan_cost_with(&last_hour_ingesting, views_only, &settings);
    assert!(
        raw_kept_plan_cost < views_only_plan_cost,
        "one-off last hour: selected {raw_kept_plan_cost}, views {views_only_plan_cost}"
    );
}

/// `sum_over_time(m[5m] @ end())` and `sum_over_time(m[5m])`, both instant
/// every 60 s: an instant query's `@ end()` is its evaluation time, so the
/// anchored input runs in the evaluation context and shares `m[5m]` with
/// the plain query, in the MILP and in `plan_cost` alike. The two cost less
/// than planned alone, which is what any plan that does not share `m[5m]`
/// (such as one in a request context of its own) costs.
#[test]
fn an_instant_at_end_input_shares_the_evaluation_context() {
    let queries = [
        requested("sum_over_time(m[5m] @ end())", 60_000, None),
        requested("sum_over_time(m[5m])", 60_000, None),
    ];
    let (plan, (objective, plan_cost)) = selected_plan_objective_and_plan_cost_with(
        &over_m(queries.to_vec()),
        PlanningModels::builtin(),
        &CostInputs::default(),
    );
    assert_equal(
        "instant @ end() beside a plain query",
        (objective, plan_cost),
    );
    assert_one_scan_read_by_every_query(&plan, 2);
    let alone: f64 = costs_of_queries_planned_alone(
        &queries,
        |queries| over_m(queries.to_vec()),
        PlanningModels::builtin,
    )
    .iter()
    .sum();
    assert!(
        plan_cost < alone,
        "shared {plan_cost} is not below the two queries planned alone, {alone}"
    );
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

/// `q + scalar(count(q))` with `q` = `quantile_over_time(0.99, m[5m])`,
/// exact, raw data kept: the `+`'s `Project` reads `q` as its child and
/// the count through `scalar()`. The plan's `Project` keeps `q` as its
/// child (building it handed the count in as the child, as
/// `Operator::map_children` mapped the scalar reference first), and
/// `plan_cost` sizes it by its child, `q`'s 1,000 rows, as the MILP does,
/// not by the count's one row, whatever order the plan lists its edges in.
#[test]
fn the_objective_is_the_plan_cost_on_a_scalar_operand() {
    let w = workload(&[(
        "quantile_over_time(0.99, m[5m]) + scalar(count(quantile_over_time(0.99, m[5m])))",
        60_000,
        "",
    )]);
    let settings = CostInputs::default();
    let (plan, costs) = selected_plan_objective_and_plan_cost(&w, &settings, true);
    assert_equal("scalar operand", costs);
    let entries: Vec<_> = w.query_workload.entries().collect();
    let data = w.data_workload.clone().unwrap();
    let mut edges_reversed = plan.clone();
    edges_reversed.edges.reverse();
    let cost_of = |plan| {
        asap_cost::plan_cost(plan, &entries, &data, &settings)
            .unwrap()
            .0
            .cost_per_second
    };
    assert_eq!(cost_of(&edges_reversed), cost_of(&plan));
}

/// A dashboard range panel, `max_over_time(m[1m])` over the last hour at
/// 15 s steps, refreshed every 15 s. Without a cache of final query
/// results, every request re-evaluates all 241 steps, so the views it
/// reads keep the 1 h span beyond their look-back. When the data plane
/// caches final query results, a request evaluates only the step since
/// the previous one, so they keep 15 s instead; the plan is cheaper and
/// the objective is still its `plan_cost`.
#[test]
fn cached_final_query_results_retain_a_range_panel_for_its_refresh_interval() {
    let panel = over_m(vec![requested(
        "max_over_time(m[1m])",
        15_000,
        Some((15_000, 3_600_000)),
    )]);
    let settings = CostInputs::default();
    let plan_with = |final_query_results_cached| {
        let capabilities = DeploymentCapabilities {
            raw_data_retained: false,
            final_query_results_cached,
            ..Default::default()
        };
        let models = PlanningModels::builtin()
            .with_cost_inputs(&settings)
            .with_deployment_capabilities(&capabilities);
        selected_plan_objective_and_plan_cost_with(&panel, models, &settings)
    };
    let longest_retention_ms = |plan: &PhysicalASAPDAG| {
        plan.materialized_views()
            .unwrap()
            .iter()
            .map(|view| view.retention_ms())
            .max()
            .expect("the panel reads a view")
    };
    let (uncached, uncached_cost) = plan_with(false);
    let (cached, cached_cost) = plan_with(true);
    assert_equal("uncached range panel", uncached_cost);
    assert_equal("cached range panel", cached_cost);
    let (uncached_ms, cached_ms) = (
        longest_retention_ms(&uncached),
        longest_retention_ms(&cached),
    );
    println!(
        "range panel: longest view retention {uncached_ms} ms uncached, {cached_ms} ms cached; \
         cost {}/s uncached, {}/s cached",
        uncached_cost.1, cached_cost.1
    );
    assert!(
        uncached_ms >= 3_600_000 + 60_000,
        "uncached: {uncached_ms} ms"
    );
    assert!(
        cached_ms <= 60_000 + 15_000 + 60_000,
        "cached: {cached_ms} ms"
    );
    assert!(
        cached_cost.1 < uncached_cost.1,
        "{cached_cost:?} vs {uncached_cost:?}"
    );
}

/// The default cost inputs with the data plane's window cache, idle 60 s:
/// a view's windows on disk, the recently written or read ones also in
/// memory.
fn with_window_cache() -> CostInputs {
    let mut settings = CostInputs::default();
    settings.data_plane_execution.view_window_cache = Some(ViewWindowCache { idle_ms: 60_000 });
    settings
}

/// Under a window cache of 60 s idle time, without raw data: a 15 s chain
/// read by `max_over_time(m[5m] offset 1h)` and by `max_over_time(m[5m])`
/// every 15 s keeps resident the windows of the reader that keeps the most
/// (24 of 261), in the MILP ($r_v$, (W)) and in `plan_cost` alike, and
/// costs less than with every retained window in memory; so do a 1 h
/// quantile (wider levels, a cover, where cheaper) and a 1 h range at 15 s
/// steps of a 5 min max.
#[test]
fn the_objective_is_the_plan_cost_with_a_window_cache() {
    let offset_and_recent = over_m(vec![
        requested("max_over_time(m[5m] offset 1h)", 15_000, None),
        requested("max_over_time(m[5m])", 15_000, None),
    ]);
    let (objective, plan_cost) =
        objective_and_plan_cost(&offset_and_recent, &with_window_cache(), false);
    assert_equal("offset and recent readers", (objective, plan_cost));
    let (_, without_cache) =
        objective_and_plan_cost(&offset_and_recent, &CostInputs::default(), false);
    assert!(
        plan_cost < without_cache,
        "with the cache {plan_cost} is not below {without_cache}"
    );
    for (name, entries) in [
        (
            "1 h quantile within 1 %",
            vec![{
                let mut quantile = requested("quantile_over_time(0.99, m[1h])", 15_000, None);
                quantile["requirements"] = serde_json::json!({"accuracy": {"max_error": 0.01, "failure_probability": 0.01}});
                quantile
            }],
        ),
        (
            "1 h range of a 5 min max",
            vec![requested(
                "max_over_time(m[5m])",
                15_000,
                Some((15_000, 3_600_000)),
            )],
        ),
    ] {
        assert_equal(
            name,
            objective_and_plan_cost(&over_m(entries), &with_window_cache(), false),
        );
    }
}

/// The default cost inputs with the data plane's window cache (idle 60 s),
/// its query-time results kept 60 s and its answers kept 60 s beyond the
/// plan's longest retention, each cache bounded by `max_bytes`.
fn with_result_caches(max_bytes: Option<u64>) -> CostInputs {
    let mut settings = with_window_cache();
    settings.data_plane_execution.kept_query_time_results = Some(KeptQueryTimeResults {
        retention_ms: 60_000,
        max_bytes,
    });
    settings.data_plane_execution.answer_cache = Some(AnswerCache {
        retention_margin_ms: 60_000,
        max_bytes,
    });
    settings
}

/// With the result and answer caches, unbounded and bounded at 64 KiB
/// (below what they keep): the MILP charges each `OneEvaluation`
/// execution's kept results, each query's answers over the plan's longest
/// retention ($H$, $w_{q,e}$), each bound as the least of the pool and the
/// bound ($t$, $b$), and a range request's new steps only, as `plan_cost`
/// does. Workloads: two windowed quantiles sharing a query-time scan (raw
/// data retained), a 1 h range at 15 s steps every 15 s, and a 1 h range of
/// `rate(..[5m] offset 1h)` every 60 s. Under the bound the shared-scan
/// plan costs less than without it.
#[test]
fn the_objective_is_the_plan_cost_with_result_and_answer_caches() {
    let quantiles = over_m(vec![
        every_10s("quantile_over_time(0.5, m[1m])"),
        every_10s("quantile_over_time(0.9, m[5m])"),
    ]);
    let range = over_m(vec![requested(
        "max_over_time(m[5m])",
        15_000,
        Some((15_000, 3_600_000)),
    )]);
    let offset_range = over_m(vec![requested(
        "max_over_time(m[5m] offset 1h)",
        60_000,
        Some((60_000, 3_600_000)),
    )]);
    for (name, workload, raw_data_retained) in [
        ("shared scan", &quantiles, true),
        ("1 h range every 15 s", &range, false),
        ("offset 1 h range every 60 s", &offset_range, false),
    ] {
        for max_bytes in [None, Some(64 * 1024)] {
            assert_equal(
                &format!("{name}, caches bounded at {max_bytes:?}"),
                objective_and_plan_cost(
                    workload,
                    &with_result_caches(max_bytes),
                    raw_data_retained,
                ),
            );
        }
    }
    let (_, unbounded) = objective_and_plan_cost(&quantiles, &with_result_caches(None), true);
    let (_, bounded) =
        objective_and_plan_cost(&quantiles, &with_result_caches(Some(64 * 1024)), true);
    assert!(
        bounded < unbounded,
        "bounded {bounded} is not below unbounded {unbounded}"
    );
}

/// With an answer cache, a 1 h range at 15 s steps requested every 20 s
/// computes 4/3 steps per request on average, in the MILP and in
/// `plan_cost` alike (the debug check in `select_plan` compares the MILP's
/// optimum with `plan_cost`). So does one every 10 s, computing 1 step per
/// request on its 5 s evaluation grid (the query's, not its requests'), and
/// one below `@ end()` every 15 s (raw data retained).
#[test]
fn the_objective_is_the_plan_cost_when_a_range_computes_fractional_steps() {
    for (name, query, interval_ms, raw_data_retained) in [
        ("every 20 s", "max_over_time(m[5m])", 20_000, false),
        ("every 10 s", "max_over_time(m[5m])", 10_000, false),
        (
            "@ end() every 15 s",
            "sum_over_time(m[5m] @ end())",
            15_000,
            true,
        ),
    ] {
        let range = over_m(vec![requested(
            query,
            interval_ms,
            Some((15_000, 3_600_000)),
        )]);
        assert_equal(
            &format!("1 h range at 15 s steps {name}"),
            objective_and_plan_cost(&range, &with_result_caches(None), raw_data_retained),
        );
    }
}

/// The two equality gaps the selection side's construction leaves open,
/// checked with the result and answer caches and the window cache on:
/// - Stage 3 takes a query-time reader's read times from the queries
///   reaching its memo group on its grid, `plan_cost` from the queries
///   reaching its node in the plan: an instant and a 1 h range of one
///   5 min max on one grid, reading one chain;
/// - kept results of a `OneEvaluation` node in a context anchored by `@`,
///   whose grid is the requests', not the queries' evaluation grid: two
///   range queries sharing a `@ end()` subtree, and two of different spans
///   below `@ start()`.
#[test]
fn the_objective_is_the_plan_cost_on_shared_readers_and_anchored_results_with_caches() {
    let instant_and_range = over_m(vec![
        requested("max_over_time(m[5m])", 15_000, None),
        requested("max_over_time(m[5m])", 15_000, Some((15_000, 3_600_000))),
    ]);
    let shared_at_end = over_m(vec![
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
    let at_start_of_two_spans = over_m(vec![
        requested(
            "sum_over_time(m[1m] @ start())",
            60_000,
            Some((30_000, 3_600_000)),
        ),
        requested(
            "max_over_time(m[1m] @ start())",
            60_000,
            Some((30_000, 90_000)),
        ),
    ]);
    for (name, workload, raw_data_retained) in [
        (
            "an instant and a range on one grid",
            &instant_and_range,
            false,
        ),
        ("a shared @ end() subtree", &shared_at_end, true),
        (
            "@ start() subtrees of two spans",
            &at_start_of_two_spans,
            true,
        ),
    ] {
        for max_bytes in [None, Some(64 * 1024)] {
            assert_equal(
                &format!("{name}, caches bounded at {max_bytes:?}"),
                objective_and_plan_cost(
                    workload,
                    &with_result_caches(max_bytes),
                    raw_data_retained,
                ),
            );
        }
    }
}

/// A query that reaches a memo group the selected plan answers it another
/// way. `quantile_over_time(0.9, m[5m])` within 1 % every 15 s reads a KLL
/// chain through a cover and a merge. The median of the same window exactly,
/// as a 1 h range on the same grid, is answered from raw rows (an exact
/// quantile priced high, so the approximate one does not share them; 1,000
/// series every 100 ms). Both quantiles reach the merge's memo group, so
/// Stage 3 runs the merge at both queries' evaluations (241 steps every
/// 15 s and one), while in the plan only the approximate query reads it,
/// once every 15 s: the MILP charges 1,256/s more than `plan_cost`
/// (485,519 against 484,263). Ignored: a known gap, which the debug check
/// in `select_plan` reports; closing it needs a query's own reads in the
/// selection's rates (`cost-terms-from-calibration.md`, section 1).
#[test]
#[ignore = "known gap: a query reaching a group it is answered another way counts in its rates"]
fn the_objective_is_the_plan_cost_when_a_query_reaches_a_group_it_reads_another_way() {
    let mut approximate = requested("quantile_over_time(0.9, m[5m])", 15_000, None);
    approximate["requirements"] =
        serde_json::json!({"accuracy": {"max_error": 0.01, "failure_probability": 0.01}});
    let mut exact = requested(
        "quantile_over_time(0.5, m[5m])",
        15_000,
        Some((15_000, 3_600_000)),
    );
    exact["requirements"] =
        serde_json::json!({"accuracy": {"max_error": 0.0, "failure_probability": 0.0}});
    let workload: PlanningWorkload = serde_json::from_value(serde_json::json!({
        "query_workload": {"language": "promql", "repeating_queries": [approximate, exact]},
        "data_workload": {
            "arrival": {"ingesting": {"rows_per_second": 10000.0}},
            "sources": {"m": {"series_count": 1000, "metric_type": "gauge", "sample_interval": 100}},
        },
    }))
    .unwrap();
    let mut settings = CostInputs::default();
    settings.measured_operation_costs = serde_json::from_value(serde_json::json!({
        "operators": [
            {"operator": "Aggregate", "phase": "query_time", "measure": "quantile", "ns_per_unit": 1e4},
            {"operator": "SummaryAgg", "phase": "query_time", "sketch": {"Kll": {"k": 200}}, "ns_per_unit": 1e4},
            {"operator": "SummaryAgg", "phase": "query_time", "sketch": {"DDSketch": {"alpha": 0.01}}, "ns_per_unit": 1e4},
        ],
    }))
    .unwrap();
    assert_equal(
        "an approximate and an exact quantile on one grid",
        objective_and_plan_cost(&workload, &settings, true),
    );
}

//! Acceptance and regression tests of cost model 3's `lowest_mean_latency`
//! (`docs/design_docs/proposals/stage3-lowest-mean-latency.md`, section
//! 6.1 and 6.3): Stage 3 selects the plan with the smallest
//! frequency-weighted mean query latency (freshness wait plus query
//! execution) among the plans within the cluster's limits.
//!
//! The workloads read the gauge `m` (1,000 series, one sample per second).
//! Times come from a measured table ([`max_min_table`]), so the expected
//! plan does not depend on formula constants: `max_over_time(m[5m])` is
//! answered either by a query-time scan of 300,000 samples, or by an
//! ingestion-time chain of 10 s windows whose refresh emits 1,000 states
//! per window.

use asap_devtools::planning_stages::{lower_promql_roots, run_planning_stages_over};
use asap_planner::{CostInputs, DeploymentCapabilities, PlanningModels};
use asap_types::ir::NonASAPOp;
use asap_types::plan::physical_asap_dag::PhysicalASAPOperatorPayload as Payload;
use serde_json::{json, Value};

use crate::lowest_mean_latency_common::{
    cluster, cost_inputs, data_plane, deployment_capabilities, every, max_min_table, over_m, plan,
    planned, planned_on, usage_over_time, with_compressed_samples,
};

/// `max_over_time(m[5m])` every 10 s.
fn max_every_10s(max_latency_ms: Option<f64>) -> Value {
    every("max_over_time(m[5m])", 10_000, max_latency_ms)
}

/// Cost model 3 with `lowest_mean_latency` and no binding limit.
fn lowest_mean_latency() -> Value {
    cluster("lowest_mean_latency", None, None, None)
}

/// Assert `actual` is within 1% of `expected` (or 1e-9 absolute).
fn assert_close(name: &str, actual: f64, expected: f64) {
    assert!(
        (actual - expected).abs() <= 0.01 * expected.abs().max(1e-7),
        "{name}: {actual} is not close to {expected}"
    );
}

/// A1. A scan of 30 ms per run, or a chain refreshing at 10 ms of CPU and
/// 30 ms of I/O wait per 10 s window: cost model 1 selects the chain (its
/// CPU per second is lower), `lowest_mean_latency` the scan (30.3 ms
/// against about 40 ms, the chain's wait for the newest window counted).
#[tokio::test]
async fn a1_lowest_mean_latency_picks_a_lower_latency_plan_than_the_cheapest() {
    let workload = over_m(vec![max_every_10s(None)], 1000);
    let table = max_min_table(100.0, 10_000.0, 0.0);
    let io_wait = json!({"refresh_io_wait": {"ns_per_refresh": 30_000_000.0}});
    let cheapest = plan(
        &workload,
        &cost_inputs(usage_over_time(), table.clone(), io_wait.clone(), json!({})),
        true,
    )
    .await;
    assert!(cheapest.has_ingestion_views(), "cost model 1: the chain");
    let fastest = plan(
        &workload,
        &cost_inputs(lowest_mean_latency(), table, io_wait, json!({})),
        true,
    )
    .await;
    assert!(
        !fastest.has_ingestion_views(),
        "lowest mean latency: the scan"
    );
    let mean = fastest.mean_latency_ms();
    assert!((29.0..32.0).contains(&mean), "mean latency {mean} ms");
}

/// The A2 and A7 workload: a chain refreshing at 9 s of CPU per 10 s window
/// (load 0.9 on one precompute thread; latency about 9 s) or a raw read of
/// 20 s per run (2 query cores).
fn heavy_refresh(precompute_threads: u32) -> Value {
    cost_inputs(
        lowest_mean_latency(),
        max_min_table(66_666.0, 9_000_000.0, 0.0),
        json!({}),
        data_plane(json!({"precompute_threads": precompute_threads})),
    )
}

/// A2. The lowest-latency plan's precompute load is 0.9 on one thread, over
/// `max_thread_utilization` 0.7: it is invalid, and the scan is selected.
#[tokio::test]
async fn a2_a_plan_over_the_precompute_load_bound_is_rejected() {
    let workload = over_m(vec![max_every_10s(None)], 1000);
    let selected = plan(&workload, &heavy_refresh(1), true).await;
    assert!(!selected.has_ingestion_views(), "the chain's load is 0.9");
    assert!(selected.usage("precompute_load") <= 0.7);
}

/// A3. The A6 settings select the chain (13 ms against the raw read's
/// 30 ms), whose windows of states are its peak memory; a memory limit one
/// byte below that peak selects the raw read, whose state is smaller,
/// within the limit.
#[tokio::test]
async fn a3_a_tighter_memory_limit_switches_to_a_smaller_state_plan() {
    let workload = over_m(vec![max_every_10s(None)], 1000);
    let settings = |memory| {
        cost_inputs(
            cluster("lowest_mean_latency", memory, None, None),
            max_min_table(100.0, 10_000.0, 0.0),
            json!({}),
            json!({}),
        )
    };
    let unlimited = plan(&workload, &settings(None), true).await;
    assert!(unlimited.has_ingestion_views());
    let peak = unlimited.usage("peak_memory_bytes") as u64;
    let selected = plan(&workload, &settings(Some(peak - 1)), true).await;
    assert!(!selected.has_ingestion_views(), "the raw read");
    assert!(selected.usage("peak_memory_bytes") <= (peak - 1) as f64);
}

/// A3 under a window cache of 60 s idle time, for
/// `max_over_time(m[5m] offset 1h)`: the chain's peak memory is the most
/// windows its reader keeps resident and the one being built, in the
/// MILP's memory row as in `plan_cost`: a memory limit at that peak keeps
/// the chain, one byte below it selects the raw read.
#[tokio::test]
async fn a3_under_a_window_cache_the_memory_limit_counts_resident_windows() {
    let workload = over_m(
        vec![every("max_over_time(m[5m] offset 1h)", 10_000, None)],
        1000,
    );
    let settings = |memory| {
        cost_inputs(
            cluster("lowest_mean_latency", memory, None, None),
            max_min_table(100.0, 10_000.0, 0.0),
            json!({}),
            data_plane(json!({"view_window_cache": {"idle_ms": 60_000}})),
        )
    };
    let unlimited = plan(&workload, &settings(None), true).await;
    assert!(unlimited.has_ingestion_views());
    let peak = unlimited.usage("peak_memory_bytes") as u64;
    let at_peak = plan(&workload, &settings(Some(peak)), true).await;
    assert!(at_peak.has_ingestion_views(), "the chain fits at its peak");
    let selected = plan(&workload, &settings(Some(peak - 1)), true).await;
    assert!(!selected.has_ingestion_views(), "the raw read");
    assert!(selected.usage("peak_memory_bytes") <= (peak - 1) as f64);
}

/// The A4 and A5 table: each chain refreshes at 2 s per window; each scan
/// takes 3 s, a `min` 1 s more (`min_ns_per_row`).
fn conflicting(min_ns_per_row: f64) -> Value {
    cost_inputs(
        lowest_mean_latency(),
        max_min_table(10_000.0, 2_000.0 * 1_000.0, min_ns_per_row),
        json!({}),
        json!({}),
    )
}

/// A4. `max` (q1) and `min` (q2) every 10 s. Chains of one refresh group
/// wait for each other (4 s each when both are selected). The mean is
/// smallest with q1 scanned (3 s) and q2 on its chain (2 s); q1's bound of
/// 2.5 s forces q1 onto its chain and q2 onto the scan, though the mean
/// rises.
#[tokio::test]
async fn a4_max_latency_ms_is_enforced() {
    let min_every_10s = |bound| every("min_over_time(m[5m])", 10_000, bound);
    let unbounded = plan(
        &over_m(vec![max_every_10s(None), min_every_10s(None)], 1000),
        &conflicting(3_333.0),
        true,
    )
    .await;
    assert!(!unbounded.query_reads_a_view(0) && unbounded.query_reads_a_view(1));
    let bounded = plan(
        &over_m(
            vec![max_every_10s(Some(2_500.0)), min_every_10s(None)],
            1000,
        ),
        &conflicting(3_333.0),
        true,
    )
    .await;
    assert!(bounded.query_reads_a_view(0) && !bounded.query_reads_a_view(1));
    assert!(bounded.query_ms(0, "latency_ms") <= 2_500.0);
    assert!(bounded.mean_latency_ms() > unbounded.mean_latency_ms());
}

/// A5. `max` every 10 s and `min` every 20 s, each faster on its chain
/// alone (2 s) than scanned (3 s), slower when both chains wait for each
/// other: the chain goes to the more frequent query, and reversing the
/// intervals reverses the choice.
#[tokio::test]
async fn a5_the_more_frequent_query_gets_the_lower_latency() {
    for (max_interval, min_interval) in [(10_000, 20_000), (20_000, 10_000)] {
        let workload = over_m(
            vec![
                every("max_over_time(m[5m])", max_interval, None),
                every("min_over_time(m[5m])", min_interval, None),
            ],
            1000,
        );
        let selected = plan(&workload, &conflicting(0.0), true).await;
        let max_is_more_frequent = max_interval < min_interval;
        assert_eq!(
            (
                selected.query_reads_a_view(0),
                selected.query_reads_a_view(1)
            ),
            (max_is_more_frequent, !max_is_more_frequent),
            "max every {max_interval} ms, min every {min_interval} ms"
        );
    }
}

/// A6. The chain's refresh (10 ms) is in the query's latency, which is
/// below the scan's 30 ms; at 40 ms of refresh the scan is selected.
#[tokio::test]
async fn a6_the_freshness_wait_counts() {
    let workload = over_m(vec![max_every_10s(None)], 1000);
    let with_refresh = |state_ns| {
        cost_inputs(
            lowest_mean_latency(),
            max_min_table(100.0, state_ns, 0.0),
            json!({}),
            json!({}),
        )
    };
    let light = plan(&workload, &with_refresh(10_000.0), true).await;
    assert!(light.query_reads_a_view(0));
    assert!(light.query_ms(0, "precompute_execution_ms") >= 10.0);
    assert!(light.query_ms(0, "latency_ms") >= 10.0);
    let heavy = plan(&workload, &with_refresh(40_000.0), true).await;
    assert!(!heavy.query_reads_a_view(0), "40 ms of refresh: the scan");
}

/// A7. The A2 workload with two precompute threads: the chain's load is
/// 0.45, within the bound, and its lower latency wins.
#[tokio::test]
async fn a7_the_precompute_thread_count_changes_the_choice() {
    let workload = over_m(vec![max_every_10s(None)], 1000);
    let one = plan(&workload, &heavy_refresh(1), true).await;
    assert!(!one.has_ingestion_views());
    let two = plan(&workload, &heavy_refresh(2), true).await;
    assert!(two.has_ingestion_views());
    assert_close("precompute load", two.usage("precompute_load"), 0.45);
}

/// A8. With every time 0, every plan's mean latency ties at 0: the plan
/// selected has the least peak memory (no plan fits one byte less), then
/// the least disk among those (no plan fits one byte less of disk at that
/// memory).
#[tokio::test]
async fn a8_ties_go_to_the_least_memory_then_the_least_disk() {
    let workload = over_m(vec![max_every_10s(None)], 1000);
    let at = |memory, disk| {
        let free: Vec<Value> = max_min_table(0.0, 0.0, 0.0)
            .into_iter()
            .map(|mut entry| {
                entry["ns_per_unit"] = json!(0.0);
                entry
            })
            .collect();
        let mut settings = cost_inputs(
            cluster("lowest_mean_latency", memory, disk, None),
            free,
            json!({}),
            json!({}),
        );
        // The nodes without an entry are free too.
        settings["frozen_calibration_factor"] = json!(1e-12);
        settings
    };
    let selected = plan(&workload, &at(None, None), true).await;
    assert_eq!(selected.mean_latency_ms(), 0.0);
    let peak = selected.usage("peak_memory_bytes") as u64;
    let disk = selected.usage("disk_bytes") as u64;
    let less_memory = planned(&workload, &at(Some(peak - 1), None), true).await;
    assert!(
        less_memory.is_err(),
        "a plan with less peak memory exists, so the tie went the wrong way"
    );
    if disk > 0 {
        let less_disk = planned(&workload, &at(Some(peak), Some(disk - 1)), true).await;
        assert!(
            less_disk.is_err(),
            "a plan with less disk at that memory exists"
        );
    }
}

/// A9. Each query's latency is its four parts' sum, its weight its
/// evaluations per second (0.1 every 10 s), and the selection MILP's mean
/// latency is the mean `plan_cost` reports for the selected plan.
#[tokio::test]
async fn a9_the_report_breaks_latency_down() {
    let workload = over_m(vec![max_every_10s(None)], 1000);
    let settings = cost_inputs(
        lowest_mean_latency(),
        max_min_table(100.0, 10_000.0, 0.0),
        json!({}),
        json!({}),
    );
    let selected = plan(&workload, &settings, true).await;
    let parts: f64 = [
        "allowed_lateness_ms",
        "precompute_queueing_ms",
        "precompute_execution_ms",
        "query_execution_ms",
    ]
    .iter()
    .map(|part| selected.query_ms(0, part))
    .sum();
    assert_close("latency", selected.query_ms(0, "latency_ms"), parts);
    assert_close(
        "weight",
        selected.query_ms(0, "evaluations_per_second"),
        0.1,
    );
    // The MILP's mean latency, from the stages run directly.
    let settings: CostInputs = serde_json::from_value(settings).unwrap();
    let capabilities = deployment_capabilities(true);
    let models = PlanningModels::builtin()
        .with_cost_inputs(&settings)
        .with_deployment_capabilities(&capabilities);
    let entries: Vec<_> = workload.query_workload.entries().collect();
    let data = workload.data_workload.clone().unwrap();
    let run = run_planning_stages_over(
        lower_promql_roots(&workload).unwrap(),
        &entries,
        &data,
        models,
    )
    .unwrap();
    let (_, selection) = run.run.selection.as_ref().expect("a plan is selected");
    let milp_mean = selection
        .selected_plan_mean_latency_ms
        .expect("lowest_mean_latency reports the selected plan's mean latency");
    assert_close("MILP mean", milp_mean, selected.mean_latency_ms());
}

/// A10. A memory limit below every plan's peak, and a latency bound no plan
/// meets: the error names the limit, with the least any plan needs.
#[tokio::test]
async fn a10_no_plan_within_the_limits_is_an_error_naming_the_limit() {
    let table = max_min_table(100.0, 10_000.0, 0.0);
    let workload = over_m(vec![max_every_10s(None)], 1000);
    let tiny_memory = cost_inputs(
        cluster("lowest_mean_latency", Some(1), None, None),
        table.clone(),
        json!({}),
        json!({}),
    );
    let error = planned(&workload, &tiny_memory, true)
        .await
        .err()
        .expect("no plan fits 1 byte");
    assert!(
        error.contains("no plan fits the cluster's limits") && error.contains("Memory"),
        "{error}"
    );
    let bounded = over_m(vec![max_every_10s(Some(0.001))], 1000);
    let error = planned(
        &bounded,
        &cost_inputs(lowest_mean_latency(), table, json!({}), json!({})),
        true,
    )
    .await
    .err()
    .expect("no plan answers within 0.001 ms");
    assert!(
        error.contains("no plan fits the cluster's limits") && error.contains("QueryLatency"),
        "{error}"
    );
}

/// A11. A scan of 16 s per 10 s run (1.6 cores) or a chain of 7 s of CPU
/// per window and 1.6 s per run (0.86 cores) and 10 s of I/O wait per
/// window (latency 18.6 s): with
/// `cpu_cores` 2 the faster scan; with 1, the chain, the only plan within
/// it on average.
#[tokio::test]
async fn a11_the_cpu_limit_is_average_cores() {
    let workload = over_m(vec![max_every_10s(None)], 1000);
    let with_cores = |cores| {
        cost_inputs(
            cluster("lowest_mean_latency", None, None, Some(cores)),
            max_min_table(53_333.0, 7_000_000.0, 0.0),
            json!({"refresh_io_wait": {"ns_per_refresh": 10_000_000_000.0}}),
            data_plane(json!({"precompute_threads": 3})),
        )
    };
    let two = plan(&workload, &with_cores(2), true).await;
    assert!(!two.has_ingestion_views(), "two cores: the faster scan");
    let one = plan(&workload, &with_cores(1), true).await;
    assert!(one.has_ingestion_views(), "one core: the chain");
    assert!(one.usage("cpu_core_seconds_per_second") <= 1.0);
}

/// A12. A range request of 61 steps every 5 min weighs 61/300 per second;
/// an instant query every minute 1/60.
#[tokio::test]
async fn a12_a_range_requests_weight_counts_its_steps() {
    let mut range = every("max_over_time(m[5m])", 300_000, None);
    range["range_evaluation"] = json!({"step_ms": 60_000, "span_ms": 3_600_000});
    let workload = over_m(
        vec![range, every("min_over_time(m[5m])", 60_000, None)],
        1000,
    );
    let settings = cost_inputs(
        lowest_mean_latency(),
        max_min_table(100.0, 10_000.0, 0.0),
        json!({}),
        json!({}),
    );
    let selected = plan(&workload, &settings, true).await;
    assert_close(
        "range weight",
        selected.query_ms(0, "evaluations_per_second"),
        61.0 / 300.0,
    );
    assert_close(
        "instant weight",
        selected.query_ms(1, "evaluations_per_second"),
        1.0 / 60.0,
    );
}

/// A13. A one-off query of 10 invocations weighs 10 over the default
/// horizon of 3,600 s.
#[tokio::test]
async fn a13_a_non_repeating_querys_weight_is_spread_over_the_horizon() {
    let workload: asap_planner::PlanningWorkload = serde_json::from_value(json!({
        "query_workload": {"language": "promql", "query_batch": [{
            "query": "max_over_time(m[5m])",
            "requirements": {"max_latency_ms": null},
            "predictability": "ad_hoc", "invocations": 10, "execute_at": 1767225600000_u64,
        }]},
        "data_workload": {
            "arrival": {"ingesting": {"rows_per_second": 1000.0}},
            "sources": {"m": {"series_count": 1000, "metric_type": "gauge", "sample_interval": 1000}},
        },
    }))
    .unwrap();
    let settings = cost_inputs(
        lowest_mean_latency(),
        max_min_table(100.0, 10_000.0, 0.0),
        json!({}),
        json!({}),
    );
    let selected = plan(&workload, &settings, true).await;
    assert_close(
        "weight",
        selected.query_ms(0, "evaluations_per_second"),
        10.0 / 3_600.0,
    );
}

/// A14. An allowed lateness of 5 s adds 5 s to every plan's latency: the
/// A1 choice (the scan) stands, at about 5,030 ms.
#[tokio::test]
async fn a14_the_allowed_lateness_is_in_every_querys_latency() {
    let workload = over_m(vec![max_every_10s(None)], 1000);
    let selected = plan(
        &workload,
        &cost_inputs(
            lowest_mean_latency(),
            max_min_table(100.0, 10_000.0, 0.0),
            json!({"refresh_io_wait": {"ns_per_refresh": 30_000_000.0}}),
            data_plane(json!({"allowed_lateness_ms": 5_000})),
        ),
        true,
    )
    .await;
    assert!(!selected.has_ingestion_views());
    assert_eq!(selected.query_ms(0, "allowed_lateness_ms"), 5_000.0);
    let latency = selected.query_ms(0, "latency_ms");
    assert!(
        (5_029.0..5_032.0).contains(&latency),
        "latency {latency} ms"
    );
}

/// A15. `max` and `min` over the same 5 minutes read one shared query-time
/// scan (30 ms): its time is on both queries' paths.
#[tokio::test]
async fn a15_a_shared_node_counts_on_every_readers_path() {
    let workload = over_m(
        vec![
            max_every_10s(None),
            every("min_over_time(m[5m])", 10_000, None),
        ],
        1000,
    );
    // Chains refreshing at 1 s per window lose to the scans.
    let selected = plan(
        &workload,
        &cost_inputs(
            lowest_mean_latency(),
            max_min_table(100.0, 1_000_000.0, 0.0),
            json!({}),
            json!({}),
        ),
        true,
    )
    .await;
    assert!(!selected.has_ingestion_views());
    let scans = selected
        .plan
        .dag
        .nodes
        .iter()
        .filter(|node| matches!(node.payload, Payload::NonASAP(NonASAPOp::Scan { .. })))
        .count();
    assert_eq!(scans, 1, "one scan shared by both queries");
    for q in 0..2 {
        assert!(selected.query_ms(q, "query_execution_ms") >= 30.0, "q{q}");
    }
}

/// A16. The A7 plan's refresh work is 0.9 core-seconds per second: at the
/// target utilisation 0.7 it needs 2 precompute threads; ingest and query
/// need 1 each. Planning again with 2 precompute threads recommends 2.
#[tokio::test]
async fn a16_the_report_recommends_thread_counts() {
    let workload = over_m(vec![max_every_10s(None)], 1000);
    let selected = plan(&workload, &heavy_refresh(2), true).await;
    let recommended = &selected.report["predicted_usage"]["recommended_configuration"];
    let threads = |role: &str| recommended[role]["recommended_threads"].as_u64();
    assert_eq!(threads("precompute"), Some(2), "{recommended}");
    assert_eq!(threads("ingest"), Some(1), "{recommended}");
    assert_eq!(threads("query"), Some(1), "{recommended}");
    let refresh_cores = recommended["precompute"]["predicted_busy_thread_seconds_per_second"]
        .as_f64()
        .unwrap();
    assert_close("refresh cores", refresh_cores, 0.9);
    let again = plan(&workload, &heavy_refresh(2), true).await;
    assert_eq!(
        again.report["predicted_usage"]["recommended_configuration"]["precompute"]
            ["recommended_threads"]
            .as_u64(),
        Some(2)
    );
}

/// A17. The A2 raw read uses about 2.0 query cores on one query thread: it
/// is still selected, and the report recommends 3 query threads
/// (2.0 / 0.7 = 2.86).
#[tokio::test]
async fn a17_query_cpu_is_not_limited_by_the_thread_count() {
    let workload = over_m(vec![max_every_10s(None)], 1000);
    let selected = plan(&workload, &heavy_refresh(1), true).await;
    assert!(!selected.has_ingestion_views());
    let query = &selected.report["predicted_usage"]["recommended_configuration"]["query"];
    assert_eq!(query["recommended_threads"].as_u64(), Some(3), "{query}");
}

/// The A1 table without the field this work adds (`ns_per_state_emitted`).
fn old_table() -> Vec<Value> {
    max_min_table(100.0, 0.0, 0.0)
        .into_iter()
        .map(|mut entry| {
            entry
                .as_object_mut()
                .unwrap()
                .remove("ns_per_state_emitted");
            entry
        })
        .collect()
}

/// R1. Under cost model 1 a latency bound changes nothing: the plan with
/// a bound tighter than any plan's latency is the plan without one.
#[tokio::test]
async fn r1_cost_model_1_enforces_no_latency_bound() {
    // Cost inputs as written before this work: no new field.
    let settings = cost_inputs(usage_over_time(), old_table(), json!({}), json!({}));
    let without = plan(&over_m(vec![max_every_10s(None)], 1000), &settings, true).await;
    let with = plan(
        &over_m(vec![max_every_10s(Some(0.001))], 1000),
        &settings,
        true,
    )
    .await;
    assert_eq!(without.plan.dag, with.plan.dag);
    assert_eq!(
        without.report["cost_per_second"],
        with.report["cost_per_second"]
    );
}

/// R2. Cost model 2, and cost model 3's other objectives, are still not
/// supported by the selection MILP.
#[tokio::test]
async fn r2_other_objectives_are_unsupported_as_before() {
    let workload = over_m(vec![max_every_10s(None)], 1000);
    let provisioned = json!({"provisioned_capacity": {
        "cost_per_cpu_op": 0.0, "cost_per_scanned_byte": 0.0,
        "cost_per_memory_byte_second": 0.0,
        "cost_per_provisioned_memory_byte_second": 1e-9,
        "cost_per_provisioned_cpu_core_second": 1.0,
        "cost_per_disk_byte_second": 0.0}});
    for cost_model in [
        provisioned,
        cluster("lowest_latency", None, None, None),
        cluster("least_memory", None, None, None),
    ] {
        let error = planned(&workload, &json!({"cost_model": cost_model}), true)
            .await
            .err()
            .expect("unsupported");
        assert!(error.contains("does not support"), "{error}");
    }
}

/// R3. Cost inputs written before this work (no `data_plane_execution`, no
/// new calibration field) load with the defaults; zero threads and a
/// utilisation outside (0, 1] are refused.
#[tokio::test]
async fn r3_old_cost_inputs_load_and_new_ones_are_validated() {
    let old: CostInputs = serde_json::from_value(json!({
        "cost_model": usage_over_time(),
        "measured_operation_costs": {"operators": old_table()},
    }))
    .unwrap();
    assert_eq!(old.validate(), Ok(()));
    let workload = over_m(vec![max_every_10s(None)], 1000);
    for (field, value) in [
        ("precompute_threads", json!(0)),
        ("query_threads", json!(0)),
        ("ingest_threads", json!(0)),
        ("max_thread_utilization", json!(0.0)),
        ("max_thread_utilization", json!(1.5)),
    ] {
        let settings = cost_inputs(
            lowest_mean_latency(),
            vec![],
            json!({}),
            data_plane(json!({ field: value })),
        );
        let error = planned(&workload, &settings, true)
            .await
            .err()
            .unwrap_or_else(|| panic!("{field} {value} is refused"));
        assert!(
            error.contains(field) && !error.contains("unknown field"),
            "{field} {value}: {error}"
        );
    }
}

/// A18. Latency bounds as exclusions: with a 40 ms refresh the chain's
/// least latency (over 40 ms) is over the query's 35 ms bound, so its
/// executions are left out before the MILP, and the raw read (30.3 ms) is
/// selected.
#[tokio::test]
async fn a18_a_candidate_over_every_readers_bound_is_pruned_before_the_milp() {
    let workload = over_m(vec![max_every_10s(Some(35.0))], 1000);
    let settings: CostInputs = serde_json::from_value(cost_inputs(
        lowest_mean_latency(),
        max_min_table(100.0, 40_000.0, 0.0),
        json!({}),
        json!({}),
    ))
    .unwrap();
    let capabilities = deployment_capabilities(true);
    let models = PlanningModels::builtin()
        .with_cost_inputs(&settings)
        .with_deployment_capabilities(&capabilities);
    let entries: Vec<_> = workload.query_workload.entries().collect();
    let data = workload.data_workload.clone().unwrap();
    let run = run_planning_stages_over(
        lower_promql_roots(&workload).unwrap(),
        &entries,
        &data,
        models,
    )
    .unwrap();
    let (plan, selection) = run.run.selection.as_ref().expect("a plan is selected");
    assert!(
        selection.executions_pruned_by_latency_bound > 0,
        "the chain's executions are left out"
    );
    assert!(plan.nodes.iter().all(|node| node.output_state.timing
        == asap_planner::ExecutionTiming::QueryTime
        || !node.is_view()));
}

/// A19 (#1460). Memory and disk rows enter the selection MILP only when a
/// solve without them selects a plan over a limit: a bound of 10^15 bytes
/// (the unlimited cluster) beside coefficients of 5e-6 to 1e7 can keep
/// HiGHS at the root node, where neither a node limit nor a time limit
/// stops it. `max_over_time(m[5m])` every 10 s: unlimited, the selection
/// solves once and its plan stands; with memory one byte below that plan's
/// peak (A3), the plan without the rows is over the limit, so the
/// selection is solved once more with them and keeps within the limit.
/// On main every solve holds both rows, so both selections solve as often.
/// A large finite limit no plan reaches (10^12 bytes of memory and of disk)
/// builds the same model as the unlimited cluster: same rows, same solves.
#[tokio::test]
async fn a19_memory_and_disk_rows_enter_only_when_a_limit_binds() {
    let workload = over_m(vec![max_every_10s(None)], 1000);
    let settings_with = |memory: Option<u64>, disk: Option<u64>| {
        cost_inputs(
            cluster("lowest_mean_latency", memory, disk, None),
            max_min_table(100.0, 10_000.0, 0.0),
            json!({}),
            json!({}),
        )
    };
    let settings = |memory: Option<u64>| settings_with(memory, None);
    // How many times the selection MILP was solved under `settings`, and its
    // final model's rows, from the stages run directly (`Selection`).
    let solved = |settings: &Value| {
        let settings: CostInputs = serde_json::from_value(settings.clone()).unwrap();
        let capabilities = DeploymentCapabilities {
            raw_data_retained: true,
            ..Default::default()
        };
        let models = PlanningModels::builtin()
            .with_cost_inputs(&settings)
            .with_deployment_capabilities(&capabilities);
        let entries: Vec<_> = workload.query_workload.entries().collect();
        let data = workload.data_workload.clone().unwrap();
        let run = run_planning_stages_over(
            lower_promql_roots(&workload).unwrap(),
            &entries,
            &data,
            models,
        )
        .unwrap();
        let (_, selection) = run.run.selection.as_ref().expect("a plan is selected");
        (selection.milp_solve_count, selection.milp_constraint_count)
    };
    let solve_count = |settings: &Value| solved(settings).0;
    assert_eq!(
        solved(&settings_with(
            Some(1_000_000_000_000),
            Some(1_000_000_000_000)
        )),
        solved(&settings(None)),
        "a finite limit no plan reaches builds the unlimited model"
    );
    let peak = plan(&workload, &settings(None), true)
        .await
        .usage("peak_memory_bytes") as u64;
    let limited = plan(&workload, &settings(Some(peak - 1)), true).await;
    assert!(limited.usage("peak_memory_bytes") <= (peak - 1) as f64);
    assert_eq!(
        solve_count(&settings(Some(peak - 1))),
        solve_count(&settings(None)) + 1,
        "the memory row is added by one more solve, only where the limit binds"
    );
}

/// A20 (#1460). Under a disk limit, the plan and its tie-breaks keep within
/// it. `max_over_time(m[5m])` and `max_over_time(n[5m])` every 10 s, 1,000
/// series each, raw data retained, every price 0 except a query-time scan
/// of the raw store, a hair (1e-9 ns per row) slower than raw rows kept in
/// memory at ingestion: the memory tie-break trades each in-memory copy
/// (9.6 MB of memory) for a scan of the store (9.6 MB of disk) when it may.
/// Unlimited it trades both; with disk limited to 14.4 MB (either scan fits
/// alone, both do not) the selected plan is within the limit. Which
/// near-tied plan the first solve returns is the solver's choice, so the
/// tie-break's own limit check is unit-tested
/// (`a_tie_break_is_kept_only_within_the_memory_and_disk_limits`).
#[tokio::test]
async fn a20_a_tie_break_keeps_within_the_disk_limit() {
    let workload: asap_planner::PlanningWorkload = serde_json::from_value(json!({
        "query_workload": {"language": "promql", "repeating_queries": [
            every("max_over_time(m[5m])", 10_000, None),
            every("max_over_time(n[5m])", 10_000, None),
        ]},
        "data_workload": {
            "arrival": {"ingesting": {"rows_per_second": 2000.0}},
            "sources": {
                "m": {"series_count": 1000, "metric_type": "gauge", "sample_interval": 1000},
                "n": {"series_count": 1000, "metric_type": "gauge", "sample_interval": 1000},
            },
        },
    }))
    .unwrap();
    let prices: Vec<Value> = max_min_table(0.0, 0.0, 0.0)
        .into_iter()
        .map(|mut entry| {
            let scan_at_query_time = entry["operator"] == "Scan" && entry["phase"] == "query_time";
            entry["ns_per_unit"] = json!(if scan_at_query_time { 1e-9 } else { 0.0 });
            if entry.get("ns_per_state_emitted").is_some() {
                entry["ns_per_state_emitted"] = json!(0.0);
            }
            entry
        })
        .collect();
    let settings = |disk: Option<u64>| {
        let mut settings = cost_inputs(
            cluster("lowest_mean_latency", None, disk, None),
            prices.clone(),
            json!({}),
            json!({}),
        );
        // Solved exactly, so the first solve takes the strictly faster
        // copies in memory.
        settings["latency_selection"] = json!({"exact_solve": true});
        settings
    };
    // Unlimited, the memory tie-break trades both copies for scans.
    let unlimited = plan(&workload, &settings(None), true).await;
    assert!(unlimited.usage("disk_bytes") > 14_400_000.0);
    let selected = planned(&workload, &settings(Some(14_400_000)), true)
        .await
        .unwrap_or_else(|error| panic!("a plan within the limits: {error}"));
    assert!(selected.usage("disk_bytes") <= 14_400_000.0);
}

/// The MILP's latency per query and mean for `settings` on `workload`, from
/// the stages run directly (`Selection`), on a deployment with
/// `capabilities`.
fn milp_latencies(
    workload: &asap_planner::PlanningWorkload,
    settings: &Value,
    capabilities: &DeploymentCapabilities,
) -> (Vec<f64>, f64) {
    let settings: CostInputs = serde_json::from_value(settings.clone()).unwrap();
    let models = PlanningModels::builtin()
        .with_cost_inputs(&settings)
        .with_deployment_capabilities(capabilities);
    let entries: Vec<_> = workload.query_workload.entries().collect();
    let data = workload.data_workload.clone().unwrap();
    let run = run_planning_stages_over(
        lower_promql_roots(workload).unwrap(),
        &entries,
        &data,
        models,
    )
    .unwrap();
    let (_, selection) = run.run.selection.as_ref().expect("a plan is selected");
    (
        selection
            .selected_plan_latency_ms_per_query
            .clone()
            .unwrap(),
        selection.selected_plan_mean_latency_ms.unwrap(),
    )
}

/// Each query's latency in the MILP equals the report's (`plan_cost`'s):
/// the MILP reads, per query, the sub-DAG it reaches in the plan.
fn assert_milp_latencies_are_the_reports(
    workload: &asap_planner::PlanningWorkload,
    settings: &Value,
    planned: &crate::lowest_mean_latency_common::Planned,
) {
    assert_milp_latencies_are_the_reports_on(
        workload,
        settings,
        &deployment_capabilities(true),
        planned,
    );
}

/// [`assert_milp_latencies_are_the_reports`] on a deployment with
/// `capabilities`.
fn assert_milp_latencies_are_the_reports_on(
    workload: &asap_planner::PlanningWorkload,
    settings: &Value,
    capabilities: &DeploymentCapabilities,
    planned: &crate::lowest_mean_latency_common::Planned,
) {
    let (per_query, mean) = milp_latencies(workload, settings, capabilities);
    for (q, milp) in per_query.iter().enumerate() {
        assert_close(
            &format!("q{q} latency"),
            *milp,
            planned.query_ms(q, "latency_ms"),
        );
    }
    assert_close("mean latency", mean, planned.mean_latency_ms());
}

/// A9 with the ingest threads' partial copies: two ingest threads each
/// build a copy of the chain's newest window, merged at its refresh. The
/// merge is in the query's precompute execution and the copy in the peak
/// memory, in the MILP and in `plan_cost` alike.
#[tokio::test]
async fn a9_partial_copies_are_counted_in_the_milp_as_in_the_report() {
    let workload = over_m(vec![max_every_10s(None)], 1000);
    let with_threads = |ingest_threads: u32| {
        cost_inputs(
            lowest_mean_latency(),
            max_min_table(100.0, 10_000.0, 0.0),
            json!({}),
            data_plane(json!({ "ingest_threads": ingest_threads })),
        )
    };
    let one = plan(&workload, &with_threads(1), true).await;
    let two = plan(&workload, &with_threads(2), true).await;
    assert!(one.has_ingestion_views() && two.has_ingestion_views());
    assert!(
        two.query_ms(0, "precompute_execution_ms") > one.query_ms(0, "precompute_execution_ms"),
        "the merge of the copies is counted"
    );
    assert!(two.usage("peak_memory_bytes") > one.usage("peak_memory_bytes"));
    assert_milp_latencies_are_the_reports(&workload, &with_threads(2), &two);
}

/// A range request evaluates each step: its query-time work is one step's
/// (the 30.3 ms raw read) times its 61 steps, in the report and the MILP.
#[tokio::test]
async fn a9_a_range_requests_work_counts_each_step() {
    let mut range = every("max_over_time(m[5m])", 300_000, None);
    range["range_evaluation"] = json!({"step_ms": 60_000, "span_ms": 3_600_000});
    let workload = over_m(vec![range], 1000);
    // Chains refreshing at 10 s per window lose to the raw read.
    let settings = cost_inputs(
        lowest_mean_latency(),
        max_min_table(100.0, 10_000_000.0, 0.0),
        json!({}),
        json!({}),
    );
    let selected = plan(&workload, &settings, true).await;
    assert!(!selected.has_ingestion_views());
    assert_close(
        "query execution",
        selected.query_ms(0, "query_execution_ms"),
        61.0 * 30.3,
    );
    assert_milp_latencies_are_the_reports(&workload, &settings, &selected);
}

/// A range request's steps split over the query threads: on 4 threads its
/// 61 steps take 16 rounds, so its query-time work is 16 × 30.3 ms in the
/// report and the MILP, and its CPU is the one-thread plan's.
#[tokio::test]
async fn a9b_a_range_requests_steps_split_over_the_query_threads() {
    let mut range = every("max_over_time(m[5m])", 300_000, None);
    range["range_evaluation"] = json!({"step_ms": 60_000, "span_ms": 3_600_000});
    let workload = over_m(vec![range], 1000);
    let settings = |threads: u32| {
        cost_inputs(
            lowest_mean_latency(),
            max_min_table(100.0, 10_000_000.0, 0.0),
            json!({}),
            data_plane(json!({ "query_threads": threads })),
        )
    };
    let one = plan(&workload, &settings(1), true).await;
    let four = plan(&workload, &settings(4), true).await;
    assert!(!four.has_ingestion_views());
    assert_close(
        "query execution",
        four.query_ms(0, "query_execution_ms"),
        16.0 * 30.3,
    );
    assert_close(
        "CPU",
        four.usage("cpu_core_seconds_per_second"),
        one.usage("cpu_core_seconds_per_second"),
    );
    assert_milp_latencies_are_the_reports(&workload, &settings(4), &four);
}

/// A9 with compressed-sample chains among the candidates: the 61-step
/// range request's selection returns (it once stalled the solver at its
/// root node for over 20 minutes, issue #1460), its plan decodes compressed
/// samples instead of reading raw rows per step, and the MILP's latency is
/// the report's.
#[tokio::test]
async fn a9c_a_range_request_plans_with_compressed_samples() {
    let mut range = every("max_over_time(m[5m])", 300_000, None);
    range["range_evaluation"] = json!({"step_ms": 60_000, "span_ms": 3_600_000});
    let workload = over_m(vec![range], 1000);
    let settings = cost_inputs(
        lowest_mean_latency(),
        max_min_table(100.0, 10_000_000.0, 0.0),
        json!({}),
        json!({}),
    );
    let capabilities = with_compressed_samples();
    let started = std::time::Instant::now();
    let selected = planned_on(&workload, &settings, &capabilities)
        .await
        .unwrap_or_else(|error| panic!("{error}"));
    assert!(
        started.elapsed() < std::time::Duration::from_secs(60),
        "planning took {:?}",
        started.elapsed()
    );
    assert!(selected.plan.dag.nodes.iter().any(|node| matches!(
        node.payload,
        Payload::ASAP(asap_types::ir::ASAPOp::DecodeSamplesInRange { .. })
    )));
    assert_milp_latencies_are_the_reports_on(&workload, &settings, &capabilities, &selected);
}

/// §6.4: the MILP's reads of each query are the sub-DAG it reaches in the
/// plan, so each query's latency is the report's: two queries sharing a
/// raw read (A15), and two conflicting queries (A4).
#[tokio::test]
async fn milp_reads_are_the_reachable_sub_dag() {
    let max_and_min = || {
        over_m(
            vec![
                max_every_10s(None),
                every("min_over_time(m[5m])", 10_000, None),
            ],
            1000,
        )
    };
    for settings in [
        cost_inputs(
            lowest_mean_latency(),
            max_min_table(100.0, 1_000_000.0, 0.0),
            json!({}),
            json!({}),
        ),
        conflicting(3_333.0),
    ] {
        let workload = max_and_min();
        let selected = plan(&workload, &settings, true).await;
        assert_milp_latencies_are_the_reports(&workload, &settings, &selected);
    }
}

/// Latency-bound exclusions are sound: with the bound exactly the selected
/// plan's latency (the raw read; then, with a heavier refresh, the chain
/// over its raw rows), that plan is still selected.
#[tokio::test]
async fn a18_a_plan_exactly_at_its_bound_is_kept() {
    for state_ns in [10_000.0, 40_000.0] {
        let settings = cost_inputs(
            lowest_mean_latency(),
            max_min_table(100.0, state_ns, 0.0),
            json!({}),
            json!({}),
        );
        let unbounded = plan(&over_m(vec![max_every_10s(None)], 1000), &settings, true).await;
        let latency = unbounded.query_ms(0, "latency_ms");
        let bounded = plan(
            &over_m(vec![max_every_10s(Some(latency))], 1000),
            &settings,
            true,
        )
        .await;
        assert_eq!(bounded.plan.dag, unbounded.plan.dag, "state {state_ns} ns");
    }
}

/// R4. `lowest_mean_latency` refuses cost inputs whose times did not pass
/// the calibration gate, unless they opt in for testing.
#[tokio::test]
async fn r4_lowest_mean_latency_needs_a_passed_calibration_or_the_opt_in() {
    let workload = over_m(vec![max_every_10s(None)], 1000);
    let mut settings = cost_inputs(
        lowest_mean_latency(),
        max_min_table(100.0, 10_000.0, 0.0),
        json!({}),
        json!({}),
    );
    settings
        .as_object_mut()
        .unwrap()
        .remove("latency_calibration");
    let error = planned(&workload, &settings, true)
        .await
        .err()
        .expect("refused");
    assert!(error.contains("calibration gate"), "{error}");
    settings["latency_calibration"] = json!("passed_gate");
    planned(&workload, &settings, true)
        .await
        .expect("a passed gate is accepted");
}

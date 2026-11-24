//! Stored views' memory, disk and disk traffic, as the plan report shows
//! them per node and per plan: what a deployment compares with its
//! measurements (issues #1469 and #1472).
//!
//! The plans read the gauge `m` without raw data retained, on a deployment
//! that runs neither wider tumbling-window levels nor covers, so the only
//! plan is one ingestion-time chain, one state per series per window,
//! merged at query time. Every input is JSON, so the tests compile against
//! today's types: a field the design adds is refused when the JSON is
//! loaded, or is missing from the report.

use asap_planner::{
    plan_installable, CostInputs, DeploymentCapabilities, FrontendInput, PlanningModels,
    PlanningWorkload, UserInput,
};
use serde_json::{json, Value};

use crate::lowest_mean_latency_common::{
    cost_inputs, entry, every, number, over_m, usage_over_time,
};

/// Assert `actual` is within 1% of `expected`.
fn assert_close(name: &str, actual: f64, expected: f64) {
    assert!(
        (actual - expected).abs() <= 0.01 * expected.abs().max(1e-9),
        "{name}: {actual} is not close to {expected}"
    );
}

/// The plan (as JSON) and its report for `workload` under `cost_inputs`,
/// one chain and no wider levels or covers; Err when the cost inputs do
/// not load or planning fails, as text.
pub(crate) async fn single_chain_plan(
    workload: &PlanningWorkload,
    cost_inputs: &Value,
) -> Result<(Value, Value), String> {
    let cost_inputs: CostInputs = serde_json::from_value(cost_inputs.clone())
        .map_err(|error| format!("cost inputs do not load: {error}"))?;
    let capabilities = DeploymentCapabilities {
        raw_data_retained: false,
        excluded_candidates: serde_json::from_value(json!([
            {"operator": "SummaryMergeWindows"},
            {"operator": "TimeRangeCover"}
        ]))
        .unwrap(),
        ..Default::default()
    };
    let models = PlanningModels::builtin()
        .with_cost_inputs(&cost_inputs)
        .with_deployment_capabilities(&capabilities);
    let (plan, report) = plan_installable(UserInput::new(workload, FrontendInput::Promql, models))
        .await
        .map_err(|error| format!("planning failed: {error}"))?;
    Ok((
        serde_json::to_value(plan).unwrap(),
        serde_json::to_value(report).unwrap(),
    ))
}

/// The measured table: every node at 1 ns per unit, the chain and the
/// merge of exact Max states.
pub(crate) fn max_table() -> Vec<Value> {
    vec![
        entry("Scan", "ingestion_time", json!({"ns_per_unit": 1.0})),
        entry(
            "SummaryTumblingWindowChain",
            "ingestion_time",
            json!({"exact": "Max", "ns_per_unit": 1.0}),
        ),
        entry("TimeRange", "query_time", json!({"ns_per_unit": 1.0})),
        entry(
            "SummaryMerge",
            "query_time",
            json!({"exact": "Max", "ns_per_unit": 1.0}),
        ),
        entry(
            "FinalizeExactAccumulator",
            "query_time",
            json!({"exact": "Max", "ns_per_unit": 1.0}),
        ),
    ]
}

/// The plan's one chain node: its id, width and retention in ms.
pub(crate) fn chain(plan: &Value) -> (u64, u64, u64) {
    let node = plan["dag"]["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|node| node["payload"]["ASAP"]["SummaryTumblingWindowChain"].is_object())
        .unwrap_or_else(|| panic!("no chain in {plan}"));
    let width = node["payload"]["ASAP"]["SummaryTumblingWindowChain"]["tumbling_window_width_ms"]
        .as_u64()
        .unwrap();
    (
        node["id"].as_u64().unwrap(),
        width,
        node["retention_ms"].as_u64().expect("a chain has a retention"),
    )
}

/// The report's entry for node `id`.
pub(crate) fn report_node(report: &Value, id: u64) -> &Value {
    report["predicted_usage"]["per_node"]
        .as_array()
        .unwrap()
        .iter()
        .find(|node| node["node_id"].as_u64() == Some(id))
        .unwrap_or_else(|| panic!("no node {id} in {report}"))
}

/// Σ over the report's nodes of field `name`; panics when a node lacks it.
pub(crate) fn sum_over_nodes(report: &Value, name: &str) -> f64 {
    report["predicted_usage"]["per_node"]
        .as_array()
        .unwrap()
        .iter()
        .map(|node| number(node, name))
        .sum()
}

/// K1. The report lists each node's memory, disk and disk traffic, and
/// they add up to the plan's totals. A 15 s chain of `max_over_time(m[5m]
/// offset 1h)` every 15 s, under a window cache of 60 s idle time, keeps
/// its retained windows on disk at `bytes_per_window` each (its peak disk
/// is that times ⌈retention ÷ width⌉) and writes one window per width.
#[tokio::test]
async fn k1_the_report_lists_each_nodes_storage() {
    let workload = over_m(
        vec![every("max_over_time(m[5m] offset 1h)", 15_000, None)],
        1000,
    );
    let inputs = cost_inputs(
        usage_over_time(),
        max_table(),
        json!({}),
        json!({"view_window_cache": {"idle_ms": 60_000}}),
    );
    let (plan, report) = single_chain_plan(&workload, &inputs).await.unwrap();
    let (id, width_ms, retention_ms) = chain(&plan);
    let node = report_node(&report, id);
    let bytes_per_window = number(node, "bytes_per_window");
    assert!(bytes_per_window > 0.0, "{node}");
    assert_close(
        "chain peak disk",
        number(node, "peak_disk_bytes"),
        bytes_per_window * retention_ms.div_ceil(width_ms) as f64,
    );
    assert_close(
        "chain writes",
        number(node, "disk_written_bytes_per_second"),
        bytes_per_window * 1000.0 / width_ms as f64,
    );
    assert!(number(node, "disk_read_bytes_per_second") >= 0.0);
    assert!(number(node, "peak_memory_bytes") > 0.0, "{node}");
    let usage = &report["predicted_usage"];
    assert_close(
        "disk",
        sum_over_nodes(&report, "peak_disk_bytes"),
        number(usage, "disk_bytes"),
    );
    assert_close(
        "average memory",
        sum_over_nodes(&report, "average_memory_bytes"),
        number(usage, "average_memory_bytes"),
    );
    assert_close(
        "writes",
        sum_over_nodes(&report, "disk_written_bytes_per_second"),
        number(usage, "disk_written_bytes_per_second"),
    );
    assert_close(
        "reads",
        sum_over_nodes(&report, "disk_read_bytes_per_second"),
        number(usage, "disk_read_bytes_per_second"),
    );
}

/// K2 (guard). A report written before the per-node storage fields loads,
/// as the plan report type: its nodes have none of them.
#[tokio::test]
async fn k2_a_report_without_storage_fields_still_loads() {
    let workload = over_m(vec![every("max_over_time(m[5m])", 10_000, None)], 100);
    let inputs = cost_inputs(usage_over_time(), max_table(), json!({}), json!({}));
    let (_, mut report) = single_chain_plan(&workload, &inputs).await.unwrap();
    let storage_fields = [
        "average_memory_bytes",
        "peak_memory_bytes",
        "bytes_per_window",
        "peak_disk_bytes",
        "disk_written_bytes_per_second",
        "disk_read_bytes_per_second",
    ];
    for node in report["predicted_usage"]["per_node"]
        .as_array_mut()
        .unwrap()
    {
        for field in storage_fields {
            node.as_object_mut().unwrap().remove(field);
        }
    }
    for field in ["disk_written_bytes_per_second", "disk_read_bytes_per_second"] {
        report["predicted_usage"]
            .as_object_mut()
            .unwrap()
            .remove(field);
    }
    let loaded: Result<asap_planner::SelectedPlanReport, _> = serde_json::from_value(report);
    assert!(loaded.is_ok(), "{loaded:?}");
}

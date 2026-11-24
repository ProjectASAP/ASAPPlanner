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

use serde_json::{json, Value};

use crate::lowest_mean_latency_common::{
    assert_close, chain_table, cluster, cost_inputs, every, number, over_m, single_chain_plan,
    usage_over_time,
};

/// The measured table: every node at 1 ns per unit, the chain and the
/// merge of exact Max states.
fn max_table() -> Vec<Value> {
    chain_table(json!({}), vec![])
}

/// The plan's one chain node: its id, width and retention in ms.
fn chain(plan: &Value) -> (u64, u64, u64) {
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
        node["retention_ms"]
            .as_u64()
            .expect("a chain has a retention"),
    )
}

/// The report's entry for node `id`.
fn report_node(report: &Value, id: u64) -> &Value {
    report["predicted_usage"]["per_node"]
        .as_array()
        .unwrap()
        .iter()
        .find(|node| node["node_id"].as_u64() == Some(id))
        .unwrap_or_else(|| panic!("no node {id} in {report}"))
}

/// Σ over the report's nodes of field `name`; panics when a node lacks it.
fn sum_over_nodes(report: &Value, name: &str) -> f64 {
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
        number(node, "disk_bytes"),
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
        sum_over_nodes(&report, "disk_bytes"),
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
        "disk_bytes",
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
    for field in [
        "disk_written_bytes_per_second",
        "disk_read_bytes_per_second",
    ] {
        report["predicted_usage"]
            .as_object_mut()
            .unwrap()
            .remove(field);
    }
    let loaded: Result<asap_planner::SelectedPlanReport, _> = serde_json::from_value(report);
    assert!(loaded.is_ok(), "{loaded:?}");
}

/// P1. The demo deployment's inputs (#1469) plan a workload: cost model 3's
/// lowest mean latency within 96 GiB, 101,456,000,000 bytes of disk and 20
/// cores, view windows in files of 15 min kept 60 s past retention under a
/// 2 GiB cache. The selected plan's disk is within the limit.
#[tokio::test]
async fn p1_the_demo_inputs_plan_under_lowest_mean_latency() {
    let workload = over_m(vec![every("max_over_time(m[5m])", 10_000, None)], 1000);
    let inputs = cost_inputs(
        crate::lowest_mean_latency_common::cluster(
            "lowest_mean_latency",
            Some(103_079_215_104),
            Some(101_456_000_000),
            Some(20),
        ),
        max_table(),
        json!({}),
        json!({"view_window_cache": {
            "idle_ms": 60_000, "max_bytes": 2_147_483_648u64,
            "file_span_ms": 900_000, "retention_margin_ms": 60_000}}),
    );
    let (_, report) = single_chain_plan(&workload, &inputs)
        .await
        .unwrap_or_else(|error| panic!("{error}"));
    assert!(number(&report["predicted_usage"], "disk_bytes") <= 101_456_000_000.0);
}

/// The plan's disk bytes and its chain (id, width, retention) for
/// `query` every 10 s over 1,000 series of `m`, one chain under a window
/// cache (so its retained windows are its disk), with
/// `data_plane_execution` fields beside the cache.
async fn chain_disk(
    query: &str,
    extra_data_plane: Value,
) -> Result<(f64, (u64, u64, u64)), String> {
    let workload = over_m(vec![every(query, 10_000, None)], 1000);
    let mut data_plane = json!({"view_window_cache": {"idle_ms": 60_000}});
    for (key, value) in extra_data_plane.as_object().unwrap() {
        data_plane[key] = value.clone();
    }
    let inputs = cost_inputs(usage_over_time(), max_table(), json!({}), data_plane);
    let (plan, report) = single_chain_plan(&workload, &inputs).await?;
    Ok((
        number(&report["predicted_usage"], "disk_bytes"),
        chain(&plan),
    ))
}

/// R1. A stored per-series row holds its series key and its state, not the
/// text of the labels its readers read: the labels are derived from the
/// series identity. `topk by (job) (5, max_over_time(m[5m]))` keeps a
/// per-series chain carrying `job`; `topk(5, max_over_time(m[5m]))` one
/// carrying no label. Both keep the same bytes on disk.
#[tokio::test]
async fn r1_a_stored_per_series_row_holds_no_label_text() {
    let (without_label, chain_without) = chain_disk("topk(5, max_over_time(m[5m]))", json!({}))
        .await
        .unwrap();
    let (with_label, chain_with) = chain_disk("topk by (job) (5, max_over_time(m[5m]))", json!({}))
        .await
        .unwrap();
    assert_eq!(
        (chain_without.1, chain_without.2),
        (chain_with.1, chain_with.2),
        "the same width and retention"
    );
    assert_close("disk with a carried label", with_label, without_label);
}

/// R2. The bytes a stored row holds beside its state are a deployment
/// input, `stored_row_bytes_beside_state` (default 72: an 8-byte window
/// time and a 64-byte series key). At 4 bytes, each of the chain's 1,000
/// rows per retained window holds 68 bytes less.
#[tokio::test]
async fn r2_stored_row_bytes_beside_state_is_a_deployment_input() {
    let query = "topk(5, max_over_time(m[5m]))";
    let (default_disk, (_, width_ms, retention_ms)) = chain_disk(query, json!({})).await.unwrap();
    let (four_bytes_disk, _) = chain_disk(query, json!({"stored_row_bytes_beside_state": 4}))
        .await
        .unwrap_or_else(|error| panic!("{error}"));
    let retained_windows = retention_ms.div_ceil(width_ms) as f64;
    assert_close(
        "disk at 4 bytes beside each state",
        four_bytes_disk,
        default_disk - 68.0 * 1000.0 * retained_windows,
    );
}

/// Cost model 1 at the illustrative prices with disk costed at a fortieth of
/// memory per byte-second.
fn usage_over_time_with_disk() -> Value {
    let mut cost_model = usage_over_time();
    cost_model["usage_over_time"]["cost_per_disk_byte_second"] = json!(1.25e-7 / 40.0);
    cost_model
}

/// The window cache of the measured deployment: windows in files of
/// `file_span_ms`, kept `retention_margin_ms` past their retention.
fn cache_with_files(file_span_ms: u64, retention_margin_ms: u64) -> Value {
    json!({"view_window_cache": {
        "idle_ms": 60_000, "file_span_ms": file_span_ms, "retention_margin_ms": retention_margin_ms}})
}

/// F1. A view's windows are in files of 15 min, deleted whole once their
/// newest window is 60 s past retention: at the peak a chain keeps
/// ⌈(retention + 60 s + 15 min) ÷ width⌉ windows on disk, and cost model 1
/// charges the time-averaged (retention + 60 s + 7.5 min) ÷ width windows.
/// `max_over_time(m[5m])` every 10 s over 1,000 series: a 10 s chain.
#[tokio::test]
async fn f1_file_slack_is_on_disk_and_costed_at_its_average() {
    let workload = over_m(vec![every("max_over_time(m[5m])", 10_000, None)], 1000);
    let planned = |data_plane: Value| {
        let workload = workload.clone();
        async move {
            let inputs = cost_inputs(usage_over_time_with_disk(), max_table(), json!({}), data_plane);
            single_chain_plan(&workload, &inputs).await
        }
    };
    let (plan, without) = planned(json!({"view_window_cache": {"idle_ms": 60_000}}))
        .await
        .unwrap();
    let (_, width_ms, retention_ms) = chain(&plan);
    let retained = retention_ms.div_ceil(width_ms) as f64;
    let bytes_per_window = number(&without["predicted_usage"], "disk_bytes") / retained;
    let (_, with) = planned(cache_with_files(900_000, 60_000))
        .await
        .unwrap_or_else(|error| panic!("{error}"));
    assert_close(
        "peak disk with file slack",
        number(&with["predicted_usage"], "disk_bytes"),
        bytes_per_window * (retention_ms + 60_000 + 900_000).div_ceil(width_ms) as f64,
    );
    let average_windows = (retention_ms as f64 + 60_000.0 + 450_000.0) / width_ms as f64;
    assert_close(
        "cost of the slack",
        number(&with, "cost_per_second") - number(&without, "cost_per_second"),
        1.25e-7 / 40.0 * bytes_per_window * (average_windows - retained),
    );
}

/// F2. Under cost model 3's `lowest_mean_latency`, the disk limit counts
/// the file slack: with a limit of 1 byte, the least disk any plan needs
/// (the only plan is the chain) is its peak with slack,
/// ⌈(retention + 60 s + 15 min) ÷ width⌉ windows, as the refusal reports.
#[tokio::test]
async fn f2_lowest_mean_latency_counts_file_slack_in_the_disk_limit() {
    let workload = over_m(vec![every("max_over_time(m[5m])", 10_000, None)], 1000);
    let reference = cost_inputs(
        usage_over_time(),
        max_table(),
        json!({}),
        json!({"view_window_cache": {"idle_ms": 60_000}}),
    );
    let (plan, report) = single_chain_plan(&workload, &reference).await.unwrap();
    let (_, width_ms, retention_ms) = chain(&plan);
    let bytes_per_window =
        number(&report["predicted_usage"], "disk_bytes") / retention_ms.div_ceil(width_ms) as f64;
    let limited = cost_inputs(
        cluster("lowest_mean_latency", None, Some(1), None),
        max_table(),
        json!({}),
        cache_with_files(900_000, 60_000),
    );
    let error = single_chain_plan(&workload, &limited)
        .await
        .expect_err("no plan fits 1 byte of disk");
    let needed: f64 = error
        .split("disk: the least any plan needs is ")
        .nth(1)
        .and_then(|rest| rest.split(' ').next())
        .and_then(|number| number.parse().ok())
        .unwrap_or_else(|| panic!("no disk shortfall in: {error}"));
    assert_close(
        "least disk",
        needed,
        bytes_per_window * (retention_ms + 60_000 + 900_000).div_ceil(width_ms) as f64,
    );
}

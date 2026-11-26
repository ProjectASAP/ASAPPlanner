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

use asap_cost::ResourceUnitCost;
use serde_json::{json, Value};

use crate::lowest_mean_latency_common::{
    assert_close, chain_table, cluster, cost_inputs, entry, every, number, over_m,
    single_chain_plan, usage_over_time,
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
        cluster(
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

/// The report's chain node (from `plan`'s one chain) and the chain's
/// (id, width, retention) for `query` every `interval_ms` over 1,000
/// series of `m`, under a 60 s window cache, the chain's measured entry
/// carrying `chain_fields` (e.g. `memory_bytes_per_state`) and
/// `data_plane_execution` carrying `extra_data_plane` beside the cache.
async fn chain_node(
    query: &str,
    interval_ms: u32,
    chain_fields: Value,
    extra_data_plane: Value,
) -> Result<(Value, (u64, u64, u64)), String> {
    chain_node_with_table(
        query,
        interval_ms,
        chain_table(chain_fields, vec![]),
        extra_data_plane,
    )
    .await
}

/// [`chain_node`] with the measured table `table`.
async fn chain_node_with_table(
    query: &str,
    interval_ms: u32,
    table: Vec<Value>,
    extra_data_plane: Value,
) -> Result<(Value, (u64, u64, u64)), String> {
    let workload = over_m(vec![every(query, interval_ms, None)], 1000);
    let mut data_plane = json!({"view_window_cache": {"idle_ms": 60_000}});
    for (key, value) in extra_data_plane.as_object().unwrap() {
        data_plane[key] = value.clone();
    }
    let inputs = cost_inputs(usage_over_time(), table, json!({}), data_plane);
    let (plan, report) = single_chain_plan(&workload, &inputs).await?;
    let chain = chain(&plan);
    Ok((report_node(&report, chain.0).clone(), chain))
}

/// R1 (#1486). A stored per-series row holds, beside its state, a 4-byte
/// offset of the state in its column and a 4-byte dictionary key per
/// label column (the series identity is one); the label text is kept once
/// per series per file, not per row, and the window time once per
/// window. Measured: a per-series exact `Sum` carrying `job`, `region`
/// and `service` stores 24 + 4 × 4 + 4 = 44 bytes per row.
/// `topk(5, max_over_time(m[5m]))` keeps a chain carrying only the
/// identity: 4 + 4 = 8 bytes beside each 16-byte state (measured),
/// 1,000 × 24 bytes per window (a flat 72 beside it gave 88,000).
#[tokio::test]
async fn r1_an_identity_only_row_holds_an_offset_and_one_key() {
    let (node, _) = chain_node(
        "topk(5, max_over_time(m[5m]))",
        10_000,
        json!({"memory_bytes_per_state": 16.0}),
        json!({}),
    )
    .await
    .unwrap_or_else(|error| panic!("{error}"));
    assert_close(
        "bytes per window",
        number(&node, "bytes_per_window"),
        1000.0 * (16.0 + 4.0 + 4.0),
    );
}

/// R2 (#1486). Each label column a per-series row carries is one more
/// 4-byte key: `topk by (job) (5, max_over_time(m[5m]))` carries `job`
/// beside the identity, 4 + 2 × 4 = 12 bytes beside each 16-byte state,
/// 4,000 bytes per window more than the chain carrying no label (R1).
#[tokio::test]
async fn r2_each_carried_label_column_is_one_more_key() {
    let state = json!({"memory_bytes_per_state": 16.0});
    let (without_label, chain_without) = chain_node(
        "topk(5, max_over_time(m[5m]))",
        10_000,
        state.clone(),
        json!({}),
    )
    .await
    .unwrap_or_else(|error| panic!("{error}"));
    let (with_label, chain_with) = chain_node(
        "topk by (job) (5, max_over_time(m[5m]))",
        10_000,
        state,
        json!({}),
    )
    .await
    .unwrap_or_else(|error| panic!("{error}"));
    assert_eq!(
        (chain_without.1, chain_without.2),
        (chain_with.1, chain_with.2),
        "the same width and retention"
    );
    assert_close(
        "bytes per window with job",
        number(&with_label, "bytes_per_window"),
        1000.0 * (16.0 + 4.0 + 2.0 * 4.0),
    );
    assert_close(
        "one more key per row",
        number(&with_label, "bytes_per_window") - number(&without_label, "bytes_per_window"),
        1000.0 * 4.0,
    );
}

/// R3 (#1486). The row layout's sizes are deployment inputs replacing
/// `stored_row_bytes_beside_state`: `state_offset_bytes_per_row`
/// (default 4) and `key_bytes_per_label_column` (default 4). At 8 and 2,
/// the identity-only chain of R1 holds 8 + 2 = 10 bytes beside each
/// 16-byte state; the old field is refused.
#[tokio::test]
async fn r3_the_row_layout_is_a_deployment_input() {
    let query = "topk(5, max_over_time(m[5m]))";
    let state = json!({"memory_bytes_per_state": 16.0});
    let (node, _) = chain_node(
        query,
        10_000,
        state.clone(),
        json!({"state_offset_bytes_per_row": 8, "key_bytes_per_label_column": 2}),
    )
    .await
    .unwrap_or_else(|error| panic!("{error}"));
    assert_close(
        "bytes per window",
        number(&node, "bytes_per_window"),
        1000.0 * (16.0 + 8.0 + 2.0),
    );
    let error = chain_node(
        query,
        10_000,
        state,
        json!({"stored_row_bytes_beside_state": 72}),
    )
    .await
    .expect_err("the replaced field is refused");
    assert!(error.contains("stored_row_bytes_beside_state"), "{error}");
}

/// S1 (#1486). A chain's measured entry gives the bytes one state's row
/// takes in the view's files, `stored_bytes_per_state` (the view's file
/// bytes ÷ states, the row's keys and offset included, as
/// compressed on disk), beside `memory_bytes_per_state`, the decoded
/// state in memory. A window is `stored_bytes_per_window` = 1,000 × 3.2 in
/// its files (disk peak, disk writes) and `bytes_per_window` = 1,000 ×
/// (16 + 8) decoded; memory is charged as without the entry.
/// `max_over_time(m[5m])` every 10 s: a 10 s chain.
#[tokio::test]
async fn s1_stored_bytes_are_disk_and_writes_not_memory() {
    let query = "max_over_time(m[5m])";
    let (decoded_only, _) = chain_node(
        query,
        10_000,
        json!({"memory_bytes_per_state": 16.0}),
        json!({}),
    )
    .await
    .unwrap_or_else(|error| panic!("{error}"));
    let (node, (_, width_ms, retention_ms)) = chain_node(
        query,
        10_000,
        json!({"memory_bytes_per_state": 16.0, "stored_bytes_per_state": 3.2}),
        json!({}),
    )
    .await
    .unwrap_or_else(|error| panic!("{error}"));
    let stored_per_window = 1000.0 * 3.2;
    assert_close(
        "stored bytes per window",
        number(&node, "stored_bytes_per_window"),
        stored_per_window,
    );
    assert_close(
        "decoded bytes per window",
        number(&node, "bytes_per_window"),
        number(&decoded_only, "bytes_per_window"),
    );
    assert_close(
        "disk at the peak",
        number(&node, "disk_bytes"),
        stored_per_window * retention_ms.div_ceil(width_ms) as f64,
    );
    assert_close(
        "disk writes",
        number(&node, "disk_written_bytes_per_second"),
        stored_per_window * 1000.0 / width_ms as f64,
    );
    for memory in ["average_memory_bytes", "peak_memory_bytes"] {
        assert_close(memory, number(&node, memory), number(&decoded_only, memory));
    }
}

/// S2 (#1486, guard). Without `stored_bytes_per_state`, a window's stored
/// bytes are its decoded bytes: the report's `stored_bytes_per_window`
/// equals `bytes_per_window`, and disk is the decoded bytes per retained
/// window.
#[tokio::test]
async fn s2_without_a_measured_stored_size_stored_is_decoded() {
    let (node, (_, width_ms, retention_ms)) =
        chain_node("max_over_time(m[5m])", 10_000, json!({}), json!({}))
            .await
            .unwrap_or_else(|error| panic!("{error}"));
    let decoded = number(&node, "bytes_per_window");
    assert_close(
        "stored bytes per window",
        number(&node, "stored_bytes_per_window"),
        decoded,
    );
    assert_close(
        "disk at the peak",
        number(&node, "disk_bytes"),
        decoded * retention_ms.div_ceil(width_ms) as f64,
    );
}

/// S3 (#1486). `stored_bytes_per_state` is interpolated between measured
/// series counts exactly as `memory_bytes_per_state` is (linearly in log
/// bytes over log series): entries at 100 and 10,000 series storing 2 and
/// 8 bytes per state give √(2 × 8) = 4 at 1,000 series. An entry pair
/// where one lacks it leaves stored = decoded.
#[tokio::test]
async fn s3_stored_bytes_interpolate_like_memory_bytes() {
    let at = |series: u64, stored: Option<f64>| {
        let mut chain = json!({"exact": "Max", "ns_per_unit": 1.0,
            "memory_bytes_per_state": 16.0, "measured_series_count": series});
        if let Some(stored) = stored {
            chain["stored_bytes_per_state"] = json!(stored);
        }
        chain
    };
    let planned = |entries: [Value; 2]| async move {
        let mut table = chain_table(json!({}), vec![]);
        table.retain(|entry| entry["operator"] != "SummaryTumblingWindowChain");
        for chain in entries {
            table.push(entry("SummaryTumblingWindowChain", "ingestion_time", chain));
        }
        chain_node_with_table("max_over_time(m[5m])", 10_000, table, json!({}))
            .await
            .unwrap_or_else(|error| panic!("{error}"))
            .0
    };
    let node = planned([at(100, Some(2.0)), at(10_000, Some(8.0))]).await;
    assert_close(
        "interpolated stored bytes per window",
        number(&node, "stored_bytes_per_window"),
        1000.0 * 4.0,
    );
    let node = planned([at(100, Some(2.0)), at(10_000, None)]).await;
    assert_close(
        "one entry without it: stored is decoded",
        number(&node, "stored_bytes_per_window"),
        number(&node, "bytes_per_window"),
    );
}

/// S4 (#1486). `stored_bytes_per_state` sizes a state: an entry giving it
/// without a sketch or an exact accumulator (here the `TimeRange`'s, which
/// reads plain rows) is refused when the cost inputs load.
#[tokio::test]
async fn s4_stored_bytes_need_a_state_family() {
    let mut table = chain_table(json!({}), vec![]);
    for entry in &mut table {
        if entry["operator"] == "TimeRange" {
            entry["stored_bytes_per_state"] = json!(2.0);
        }
    }
    let error = chain_node_with_table("max_over_time(m[5m])", 10_000, table, json!({}))
        .await
        .expect_err("a plain-row entry cannot size a state");
    assert!(
        error.contains("stored_bytes_per_state but no sketch or exact state"),
        "{error}"
    );
}

/// Cost model 1 at the illustrative prices with disk at the illustrative
/// preset's cost (a fortieth of memory per byte-second).
fn usage_over_time_with_disk() -> Value {
    let mut cost_model = usage_over_time();
    cost_model["usage_over_time"]["cost_per_disk_byte_second"] =
        json!(ResourceUnitCost::illustrative().cost_per_disk_byte_second);
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
            let inputs = cost_inputs(
                usage_over_time_with_disk(),
                max_table(),
                json!({}),
                data_plane,
            );
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

/// The plan (as JSON) and report of `query` every `interval_ms` over
/// `series` series of `m`, one chain, cost model 1, with
/// `data_plane_execution`.
async fn one_chain(
    query: &str,
    interval_ms: u32,
    series: u64,
    data_plane: Value,
) -> (Value, Value) {
    let workload = over_m(vec![every(query, interval_ms, None)], series);
    let inputs = cost_inputs(usage_over_time(), max_table(), json!({}), data_plane);
    single_chain_plan(&workload, &inputs)
        .await
        .unwrap_or_else(|error| panic!("{error}"))
}

/// D1. A window read while not in the cache is read from disk: its bytes
/// are scanned bytes, charged at the scanned-byte cost, and they add their
/// read time (`ms_per_scanned_byte`, 1e-7 by default) to the query.
/// `max_over_time(m[1h])` every 5 min reads its 12 windows of 5 min; with
/// a 60 s idle time only the newest, written as the request runs, is still
/// in the cache, so every read takes the other 11 from disk: 11 × bytes per
/// window every 300 s.
#[tokio::test]
async fn d1_cold_reads_are_scanned_bytes_and_query_time() {
    let query = "max_over_time(m[1h])";
    let (_, uncached) = one_chain(query, 300_000, 1000, json!({})).await;
    let (plan, cached) = one_chain(
        query,
        300_000,
        1000,
        json!({"view_window_cache": {"idle_ms": 60_000}}),
    )
    .await;
    let (_, width_ms, retention_ms) = chain(&plan);
    assert_eq!(width_ms, 300_000);
    let bytes_per_window =
        number(&cached["predicted_usage"], "disk_bytes") / retention_ms.div_ceil(width_ms) as f64;
    let cold_bytes_per_read = 11.0 * bytes_per_window;
    assert_close(
        "scanned bytes per second",
        number(&cached["predicted_usage"], "scanned_bytes_per_second")
            - number(&uncached["predicted_usage"], "scanned_bytes_per_second"),
        cold_bytes_per_read / 300.0,
    );
    let query_ms = |report: &Value| {
        number(
            &report["predicted_usage"]["per_query"][0],
            "query_execution_ms",
        )
    };
    assert_close(
        "query time",
        query_ms(&cached) - query_ms(&uncached),
        cold_bytes_per_read * 1e-7,
    );
}

/// D2 (guard). A range read every width it slides by reads only windows it
/// read or wrote within the idle time: `max_over_time(m[3m])` every 10 s
/// under a 60 s idle time reads nothing from disk at steady state.
#[tokio::test]
async fn d2_a_range_read_every_window_reads_nothing_cold() {
    let query = "max_over_time(m[3m])";
    let (_, uncached) = one_chain(query, 10_000, 1000, json!({})).await;
    let (_, cached) = one_chain(
        query,
        10_000,
        1000,
        json!({"view_window_cache": {"idle_ms": 60_000}}),
    )
    .await;
    assert_eq!(
        number(&cached["predicted_usage"], "scanned_bytes_per_second"),
        number(&uncached["predicted_usage"], "scanned_bytes_per_second")
    );
}

/// D3. A cache bounded below the windows its readers keep resident holds
/// only its bound, and the reads it can no longer serve are cold reads.
/// The chain of `max_over_time(m[5m] offset 1h)` every 15 s keeps its
/// last minute's reads and its newest windows resident (C11); a bound of
/// half of that memory lowers the plan's average memory and adds scanned
/// bytes.
#[tokio::test]
async fn d3_a_bounded_cache_holds_its_bound_and_reads_the_rest_cold() {
    let query = "max_over_time(m[5m] offset 1h)";
    let (_, unbounded) = one_chain(
        query,
        15_000,
        1000,
        json!({"view_window_cache": {"idle_ms": 60_000}}),
    )
    .await;
    let resident = number(&unbounded["predicted_usage"], "average_memory_bytes");
    let bound = (resident / 2.0) as u64;
    let (_, bounded) = one_chain(
        query,
        15_000,
        1000,
        json!({"view_window_cache": {"idle_ms": 60_000, "max_bytes": bound}}),
    )
    .await;
    assert!(
        number(&bounded["predicted_usage"], "average_memory_bytes") < resident,
        "bounded memory {bounded} against {resident}"
    );
    assert!(
        number(&bounded["predicted_usage"], "scanned_bytes_per_second")
            > number(&unbounded["predicted_usage"], "scanned_bytes_per_second"),
        "no cold reads past the bound"
    );
}

/// D4 (#1486). A read from disk reads a window's stored bytes, while the
/// cache holds decoded ones. D1's `max_over_time(m[1h])` every 5 min with
/// `stored_bytes_per_state` 2 (decoded 16 + 8 per row) reads its 11 cold
/// windows at 1,000 × 2 bytes each, every 300 s; its memory is as without
/// the entry.
#[tokio::test]
async fn d4_cold_reads_read_stored_bytes() {
    let query = "max_over_time(m[1h])";
    let (decoded_only, _) = chain_node(
        query,
        300_000,
        json!({"memory_bytes_per_state": 16.0}),
        json!({}),
    )
    .await
    .unwrap_or_else(|error| panic!("{error}"));
    let (node, (_, width_ms, _)) = chain_node(
        query,
        300_000,
        json!({"memory_bytes_per_state": 16.0, "stored_bytes_per_state": 2.0}),
        json!({}),
    )
    .await
    .unwrap_or_else(|error| panic!("{error}"));
    assert_eq!(width_ms, 300_000);
    assert_close(
        "cold read bytes per second",
        number(&node, "disk_read_bytes_per_second"),
        11.0 * 1000.0 * 2.0 / 300.0,
    );
    assert_close(
        "memory",
        number(&node, "average_memory_bytes"),
        number(&decoded_only, "average_memory_bytes"),
    );
}

/// D5 (#1486). The cache bound is compared with decoded resident bytes,
/// not stored ones: D3's chain under a bound of half its decoded resident
/// memory holds the same memory and reads the same windows cold whether
/// its stored bytes are a tenth of decoded or equal to them; only the
/// bytes each cold read takes scale, by the tenth.
#[tokio::test]
async fn d5_the_cache_bound_counts_decoded_bytes() {
    let query = "max_over_time(m[5m] offset 1h)";
    let decoded = json!({"memory_bytes_per_state": 20.0});
    let (unbounded, _) = chain_node(query, 15_000, decoded.clone(), json!({}))
        .await
        .unwrap_or_else(|error| panic!("{error}"));
    let bound = (number(&unbounded, "average_memory_bytes") / 2.0) as u64;
    let bounded = |chain_fields: Value| async move {
        chain_node(
            query,
            15_000,
            chain_fields,
            json!({"view_window_cache": {"idle_ms": 60_000, "max_bytes": bound}}),
        )
        .await
        .unwrap_or_else(|error| panic!("{error}"))
        .0
    };
    let same_size = bounded(json!({"memory_bytes_per_state": 20.0,
        "stored_bytes_per_state": 28.0}))
    .await;
    let tenth = bounded(json!({"memory_bytes_per_state": 20.0,
        "stored_bytes_per_state": 2.8}))
    .await;
    assert_close(
        "memory under the bound",
        number(&tenth, "average_memory_bytes"),
        number(&same_size, "average_memory_bytes"),
    );
    assert!(number(&same_size, "disk_read_bytes_per_second") > 0.0);
    assert_close(
        "cold reads at a tenth of the bytes",
        number(&tenth, "disk_read_bytes_per_second"),
        number(&same_size, "disk_read_bytes_per_second") / 10.0,
    );
}

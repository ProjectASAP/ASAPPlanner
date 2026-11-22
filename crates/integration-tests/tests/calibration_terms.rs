//! Calibration terms of the measured cost table
//! (`docs/design_docs/proposals/stage3-lowest-mean-latency.md`, sections
//! 1.5 and 6.2): what a deployment's measured costs add to each node's
//! time, as the plan report shows it. C10, the calibration gate against a
//! deployment's measurements, is `tools/calibration_gate/`, not a test.
//!
//! The chain plans read the gauge `m` without raw data retained, so the
//! only plan is an ingestion-time chain of 10 s windows, one state per
//! series per window, merged at query time.

use asap_cost::node::state_bytes::{assumed_magnitude_range, sketch_state_bytes, StateInputRows};
use asap_planner::{
    plan_installable, CostInputs, FrontendInput, PlanningModels, PlanningWorkload, SqlCatalog,
    UserInput,
};
use asap_types::ir::schema::{DataType, Field, Schema};
use asap_types::summary_type::SketchParams;
use asap_types::workload::ValueMagnitudeRange;
use serde_json::{json, Value};

use crate::lowest_mean_latency_common::{
    cluster, cost_inputs, entry, every, number, over_m, plan, planned, usage_over_time,
};

/// Assert `actual` is within 1% of `expected`.
fn assert_close(name: &str, actual: f64, expected: f64) {
    assert!(
        (actual - expected).abs() <= 0.01 * expected.abs().max(1e-9),
        "{name}: {actual} is not close to {expected}"
    );
}

/// The report's node of `operator`, panicking when the plan has none.
fn node<'a>(report: &'a Value, operator: &str) -> &'a Value {
    report["predicted_usage"]["per_node"]
        .as_array()
        .unwrap()
        .iter()
        .find(|node| node["operator"] == operator)
        .unwrap_or_else(|| panic!("no {operator} node in {report}"))
}

/// The chain plan's table: every class at 1 ns per unit, the chain with
/// `chain_fields` merged in, the merge's entries `merge_entries`.
fn chain_table(chain_fields: Value, merge_entries: Vec<Value>) -> Vec<Value> {
    let mut chain = json!({"exact": "Max", "ns_per_unit": 1.0});
    for (key, value) in chain_fields.as_object().unwrap() {
        chain[key] = value.clone();
    }
    let mut entries = vec![
        entry("Scan", "ingestion_time", json!({"ns_per_unit": 1.0})),
        entry("SummaryTumblingWindowChain", "ingestion_time", chain),
        entry("TimeRange", "query_time", json!({"ns_per_unit": 1.0})),
        entry(
            "FinalizeExactAccumulator",
            "query_time",
            json!({"exact": "Max", "ns_per_unit": 1.0}),
        ),
    ];
    entries.extend(if merge_entries.is_empty() {
        vec![entry(
            "SummaryMerge",
            "query_time",
            json!({"exact": "Max", "ns_per_unit": 1.0}),
        )]
    } else {
        merge_entries
    });
    entries
}

/// `max_over_time(m[5m])` every 10 s over `series` series.
fn max_chain_workload(series: u64) -> PlanningWorkload {
    over_m(vec![every("max_over_time(m[5m])", 10_000, None)], series)
}

/// C1. 9,234 ns per state emitted adds 9.234 ms of refresh CPU per window
/// (1,000 states), on top of the chain's per-row price, and leaves its
/// ingest CPU unchanged.
#[tokio::test]
async fn c1_states_emitted_are_charged_at_each_refresh() {
    let workload = max_chain_workload(1000);
    let report = |chain_fields| {
        let workload = workload.clone();
        async move {
            plan(
                &workload,
                &cost_inputs(
                    usage_over_time(),
                    chain_table(chain_fields, vec![]),
                    json!({}),
                    json!({}),
                ),
                false,
            )
            .await
            .report
        }
    };
    let without = report(json!({})).await;
    let with = report(json!({"ns_per_state_emitted": 9_234.0})).await;
    let chain = |report: &Value| node(report, "SummaryTumblingWindowChain").clone();
    assert_close(
        "refresh CPU per window",
        number(&chain(&with), "refresh_cpu_ms_per_window")
            - number(&chain(&without), "refresh_cpu_ms_per_window"),
        9.234,
    );
    assert_close(
        "ingest CPU",
        number(&chain(&with), "ingest_cpu_core_seconds_per_second"),
        number(&chain(&without), "ingest_cpu_core_seconds_per_second"),
    );
}

/// C2. 3.4 ms per evaluation step: an instant request pays it once, a range
/// request of 61 steps 61 times; 1 ms per request is paid once by either.
#[tokio::test]
async fn c2_the_fixed_cost_per_step_is_charged_per_step() {
    let mut range = every("max_over_time(m[5m])", 300_000, None);
    range["range_evaluation"] = json!({"step_ms": 60_000, "span_ms": 3_600_000});
    let workload = over_m(
        vec![every("max_over_time(m[5m])", 10_000, None), range],
        1000,
    );
    let execution = |fields: Value| {
        let workload = workload.clone();
        async move {
            let planned = plan(
                &workload,
                &cost_inputs(
                    usage_over_time(),
                    chain_table(json!({}), vec![]),
                    fields,
                    json!({}),
                ),
                false,
            )
            .await;
            (
                planned.query_ms(0, "query_execution_ms"),
                planned.query_ms(1, "query_execution_ms"),
            )
        }
    };
    let (instant, range) = execution(json!({})).await;
    let (instant_with, range_with) = execution(json!({
        "ns_per_evaluation": 1_000_000.0,
        "ns_per_evaluation_step": 3_400_000.0,
    }))
    .await;
    assert_close("instant", instant_with - instant, 1.0 + 3.4);
    assert_close("range", range_with - range, 1.0 + 61.0 * 3.4);
}

/// The plan report for `workload` without raw data retained, on a
/// deployment that runs neither wider tumbling-window levels nor covers:
/// the merge reads the chain's 30 windows per group, the scale C3 prices.
async fn plan_single_chain(workload: &PlanningWorkload, cost_inputs: &Value) -> Value {
    let cost_inputs: CostInputs = serde_json::from_value(cost_inputs.clone()).unwrap();
    let capabilities = asap_planner::DeploymentCapabilities {
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
    let (_, report) = plan_installable(UserInput::new(workload, FrontendInput::Promql, models))
        .await
        .unwrap();
    serde_json::to_value(report).unwrap()
}

/// C3. `SummaryMerge` entries measured at 1,000 groups (100 ns per state)
/// and 10,000 groups (1,000 ns): a merge over 1,000, 3,162 and 10,000
/// groups is priced at 100, about 316 and 1,000 ns per state (log-log
/// between the scales), one over 20,000 at the 10,000 price, named as
/// priced outside its measured scale. Each merge reads 30 states per group.
#[tokio::test]
async fn c3_prices_follow_the_series_scale() {
    let merges = vec![
        entry(
            "SummaryMerge",
            "query_time",
            json!({"exact": "Max", "ns_per_unit": 100.0, "measured_series_count": 1_000}),
        ),
        entry(
            "SummaryMerge",
            "query_time",
            json!({"exact": "Max", "ns_per_unit": 1_000.0, "measured_series_count": 10_000}),
        ),
    ];
    for (series, ns_per_state, outside) in [
        (1_000_u64, 100.0, false),
        (3_162, 100.0 * (3_162.0_f64 / 1_000.0), false),
        (10_000, 1_000.0, false),
        (20_000, 1_000.0, true),
    ] {
        let planned = plan_single_chain(
            &max_chain_workload(series),
            &cost_inputs(
                usage_over_time(),
                chain_table(json!({}), merges.clone()),
                json!({}),
                json!({}),
            ),
        )
        .await;
        let merge = node(&planned, "SummaryMerge");
        assert_close(
            &format!("{series} series"),
            number(merge, "query_ms_per_step"),
            30.0 * series as f64 * ns_per_state * 1e-6,
        );
        assert_eq!(
            merge["priced_outside_measured_scale"].as_bool(),
            Some(outside),
            "{series} series"
        );
    }
}

/// C4. 2 ms of I/O wait per refresh: the refresh's wall time, the query's
/// precompute execution and the precompute load rise by it; CPU and cost
/// model 1's cost do not.
#[tokio::test]
async fn c4_refresh_io_wait_is_wall_time_not_cpu() {
    let workload = max_chain_workload(1000);
    let planned = |fields: Value| {
        let workload = workload.clone();
        async move {
            plan(
                &workload,
                &cost_inputs(
                    usage_over_time(),
                    chain_table(json!({}), vec![]),
                    fields,
                    json!({}),
                ),
                false,
            )
            .await
        }
    };
    let without = planned(json!({})).await;
    let with = planned(json!({"refresh_io_wait": {"ns_per_refresh": 2_000_000.0}})).await;
    let chain = |p: &crate::lowest_mean_latency_common::Planned| {
        let chain = node(&p.report, "SummaryTumblingWindowChain");
        number(chain, "refresh_wall_ms_per_window") - number(chain, "refresh_cpu_ms_per_window")
    };
    assert_close("wall − CPU", chain(&with) - chain(&without), 2.0);
    assert_close(
        "precompute execution",
        with.query_ms(0, "precompute_execution_ms")
            - without.query_ms(0, "precompute_execution_ms"),
        2.0,
    );
    assert_close(
        "precompute load",
        with.usage("precompute_load") - without.usage("precompute_load"),
        2.0 / 10_000.0,
    );
    assert_eq!(
        with.usage("cpu_core_seconds_per_second"),
        without.usage("cpu_core_seconds_per_second")
    );
    assert_eq!(
        with.report["cost_per_second"],
        without.report["cost_per_second"]
    );
}

/// C5. 100 µs per arriving sample at 10,000 samples per second is one core
/// of ingest decode: in the ingest role and its recommended threads, and in
/// cost model 3's CPU limit; cost model 1's cost and plan are unchanged.
#[tokio::test]
async fn c5_ingest_decode_counts_in_the_ingest_role() {
    let workload = max_chain_workload(10_000);
    let decode = json!({"ns_per_ingested_sample": 100_000.0});
    let without = plan(
        &workload,
        &cost_inputs(
            usage_over_time(),
            chain_table(json!({}), vec![]),
            json!({}),
            json!({}),
        ),
        false,
    )
    .await;
    let with = plan(
        &workload,
        &cost_inputs(
            usage_over_time(),
            chain_table(json!({}), vec![]),
            decode.clone(),
            json!({}),
        ),
        false,
    )
    .await;
    let ingest = |p: &crate::lowest_mean_latency_common::Planned| {
        p.report["predicted_usage"]["recommended_configuration"]["ingest"].clone()
    };
    assert_close(
        "ingest cores",
        number(&ingest(&with), "predicted_busy_thread_seconds_per_second")
            - number(
                &ingest(&without),
                "predicted_busy_thread_seconds_per_second",
            ),
        1.0,
    );
    assert!(
        ingest(&with)["recommended_threads"].as_u64() >= Some(2),
        "{}",
        ingest(&with)
    );
    assert_eq!(
        with.report["cost_per_second"],
        without.report["cost_per_second"]
    );
    assert_eq!(with.plan.dag, without.plan.dag);
    // One core is over a cluster of one core with the decode, within it
    // without.
    let one_core = cluster("lowest_mean_latency", None, None, Some(1));
    let error = planned(
        &workload,
        &cost_inputs(
            one_core.clone(),
            chain_table(json!({}), vec![]),
            decode,
            json!({}),
        ),
        false,
    )
    .await
    .err()
    .expect("over one core with the decode");
    assert!(error.contains("CpuCores"), "{error}");
    planned(
        &workload,
        &cost_inputs(
            one_core,
            chain_table(json!({}), vec![]),
            json!({}),
            json!({}),
        ),
        false,
    )
    .await
    .expect("within one core without the decode");
}

/// `t(ts, k, x)`.
fn table_schema() -> Schema {
    Schema::with_time_index(
        vec![
            Field::plain("ts", DataType::Timestamp, false),
            Field::plain("k", DataType::Utf8, false),
            Field::plain("x", DataType::Float64, true),
        ],
        0,
        vec![],
    )
}

/// C6. `SUM(x), COUNT(x)` in one `Aggregate`: an entry for the combination
/// (0.5 ns per row) wins over the two single-measure entries (1 ns each),
/// so the node's CPU is a quarter of theirs.
#[tokio::test]
async fn c6_a_measure_combination_entry_wins() {
    let workload: PlanningWorkload = serde_json::from_value(json!({
        "query_workload": {"language": {"sql": "datafusion_sql"}, "repeating_queries": [{
            "query": "SELECT k, SUM(x), COUNT(x) FROM t GROUP BY k",
            "demand": {"fixed_interval_at": {"interval": 10000, "evaluation_phase": 0}},
            "requirements": {"max_latency_ms": null},
            "predictability": {"predictable": {"known_at": null}},
        }]},
        "data_workload": {"arrival": "at_rest",
            "sources": {"t": {"series_count": 10, "sample_interval": 10000, "rows_held": 1000}}},
    }))
    .unwrap();
    let catalog = SqlCatalog::new().with_table("t", table_schema());
    let aggregate_cpu = |entries: Vec<Value>| {
        let workload = workload.clone();
        let catalog = &catalog;
        async move {
            let settings: CostInputs = serde_json::from_value(cost_inputs(
                usage_over_time(),
                entries,
                json!({}),
                json!({}),
            ))
            .unwrap_or_else(|error| panic!("cost inputs do not load: {error}"));
            let models = PlanningModels::builtin().with_cost_inputs(&settings);
            let (_, report) = plan_installable(UserInput::new(
                &workload,
                FrontendInput::Sql { catalog },
                models,
            ))
            .await
            .unwrap();
            let report = serde_json::to_value(report).unwrap();
            number(node(&report, "Aggregate"), "cpu_core_seconds_per_second")
        }
    };
    let single = vec![
        entry(
            "Aggregate",
            "query_time",
            json!({"measure": "sum", "ns_per_unit": 1.0}),
        ),
        entry(
            "Aggregate",
            "query_time",
            json!({"measure": "count", "ns_per_unit": 1.0}),
        ),
    ];
    let mut combined = single.clone();
    combined.push(entry(
        "Aggregate",
        "query_time",
        json!({"measures": ["sum", "count"], "ns_per_unit": 0.5}),
    ));
    let single_cpu = aggregate_cpu(single).await;
    let combined_cpu = aggregate_cpu(combined).await;
    assert_close("ratio", combined_cpu / single_cpu, 0.25);
}

/// C7 (guard). A query-time fixed cost per computation, 5 ms, is charged
/// per run: 0.5 ms of CPU per second at one run every 10 s.
#[tokio::test]
async fn c7_a_query_time_fixed_cost_is_charged_per_run() {
    let workload = over_m(vec![every("max(m)", 10_000, None)], 1000);
    let aggregate_cpu = |ns_per_invocation: f64| {
        let workload = workload.clone();
        async move {
            let settings = json!({
                "cost_model": usage_over_time(),
                "measured_operation_costs": {"operators": [
                    {"operator": "Aggregate", "phase": "query_time", "measure": "max",
                     "ns_per_unit": 1.0, "ns_per_invocation": ns_per_invocation},
                    // The exact accumulator's path, priced out.
                    {"operator": "SummaryAgg", "phase": "query_time", "exact": "Max",
                     "ns_per_unit": 1e9},
                ]},
            });
            let planned = plan(&workload, &settings, true).await;
            number(
                node(&planned.report, "Aggregate"),
                "cpu_core_seconds_per_second",
            )
        }
    };
    let difference = aggregate_cpu(5_000_000.0).await - aggregate_cpu(0.0).await;
    assert_close("CPU per second", difference, 0.0005);
}

/// C8 (guard). A declared magnitude range replaces the DDSketch default
/// of [1, 1e9]: no range is assumed, and the state is smaller.
#[test]
fn c8_a_declared_value_range_replaces_the_ddsketch_default() {
    let params = SketchParams::DDSketch { alpha: 0.01 };
    let rows = |range| StateInputRows {
        folded_rows_per_state: Some((1_000_000) as f64),
        non_negative: true,
        value_magnitude_range: range,
        distinct_values: None,
    };
    assert!(assumed_magnitude_range(&params, rows(None)).is_some());
    let declared = Some(ValueMagnitudeRange {
        min_abs: 1.0,
        max_abs: 1e3,
    });
    assert_eq!(assumed_magnitude_range(&params, rows(declared)), None);
    assert!(sketch_state_bytes(&params, rows(declared)) < sketch_state_bytes(&params, rows(None)));
}

/// C9. Each node reports its time by phase: the chain its ingest CPU per
/// second and its refresh CPU and wall time per window, the query-time
/// nodes their time per step.
#[tokio::test]
async fn c9_each_node_reports_its_time_by_phase() {
    let planned = plan(
        &max_chain_workload(1000),
        &cost_inputs(
            usage_over_time(),
            chain_table(json!({"ns_per_state_emitted": 1_000.0}), vec![]),
            json!({}),
            json!({}),
        ),
        false,
    )
    .await;
    let chain = node(&planned.report, "SummaryTumblingWindowChain");
    for field in [
        "ingest_cpu_core_seconds_per_second",
        "refresh_cpu_ms_per_window",
        "refresh_wall_ms_per_window",
    ] {
        assert!(number(chain, field) > 0.0, "{field}: {chain}");
    }
    let merge = node(&planned.report, "SummaryMerge");
    assert!(number(merge, "query_ms_per_step") > 0.0, "{merge}");
}

/// C11. With a window cache of 60 s idle time, a chain read only an hour
/// back (`max_over_time(m[5m] offset 1h)` every 15 s) keeps resident the
/// windows its last minute of evaluations read and the newest it wrote,
/// not the 1 h 5 min it retains: its memory falls and the windows it no
/// longer holds in memory are charged as disk, byte for byte. Without the
/// cache (the default), every retained window is memory, as before.
#[tokio::test]
async fn c11_a_window_cache_keeps_only_recently_read_windows_in_memory() {
    let workload = over_m(
        vec![every("max_over_time(m[5m] offset 1h)", 15_000, None)],
        1000,
    );
    let usage = |data_plane_execution: Value| {
        let workload = workload.clone();
        async move {
            let report = plan(
                &workload,
                &cost_inputs(
                    usage_over_time(),
                    chain_table(json!({}), vec![]),
                    json!({}),
                    data_plane_execution,
                ),
                false,
            )
            .await
            .report;
            (
                number(&report["predicted_usage"], "average_memory_bytes"),
                number(&report["predicted_usage"], "disk_bytes"),
            )
        }
    };
    let (memory_without, disk_without) = usage(json!({})).await;
    let (memory_with, disk_with) = usage(json!({"view_window_cache": {"idle_ms": 60_000}})).await;
    assert!(
        memory_with < memory_without / 5.0,
        "resident {memory_with} bytes against {memory_without} retained"
    );
    assert_close(
        "memory moved to disk",
        disk_with - disk_without,
        memory_without - memory_with,
    );
}

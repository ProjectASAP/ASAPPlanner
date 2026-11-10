//! Hand-computed costs for the canonical plan shapes
//! (`docs/develop_docs/testing.md`, "Oracles"): each expected cost is
//! computed here from the concepts doc's cost terms (section 5, "Cost and
//! retention": executions per second ρ_κ times one execution's work, and
//! a materialized node's retained state), not by `asap_cost`'s node
//! costing, and compared with `asap_cost::plan_cost` of the plan Stage 3
//! selects.
//!
//! The plans are costed under two cost inputs that isolate one resource:
//! [`cpu_operations_only`] charges 1 per CPU operation, so a node's cost is
//! its CPU operations per second; [`retained_bytes_only`] charges 1 per
//! byte held for a second. The per-operation work is the cost model's
//! (`asap-cost`, module `analytical`): a scan, a time range and a merge do
//! one operation per row or state they produce or read, a finalize one per
//! accumulator, a hash aggregate one per input row and per key and
//! accumulator, an exact accumulator build one per row and per update
//! (depth 1), a time shift none. The counts are what this oracle checks:
//! rows per execution, windows a range selects, states a merge reads,
//! executions per second and retained windows.

use asap_cost::{plan_cost, CostInputs, CostModel, ResourceUnitCost};
use asap_devtools::corpus_registry::{plan_workload, repeating_promql_workload, PlannedWorkload};
use asap_devtools::planning_stages::RunUntil;
use asap_types::deployment_capabilities::DeploymentCapabilities;
use asap_types::ir::physical_export::PhysicalASAPDAG;
use asap_types::workload::PlanningWorkload;
use serde_json::json;

/// Series of the one metric `m`, each sampled every second: 1,000 rows
/// arrive per second.
const SERIES: f64 = 1_000.0;
const ROWS_PER_SECOND: f64 = 1_000.0;
/// Every query repeats every 60 s.
const EVALUATIONS_PER_SECOND: f64 = 1.0 / 60.0;
/// The bytes of one exact `sum` or `rate` accumulator state (a sum and a
/// count with their group key), as `asap_cost::state_bytes` sizes it.
const EXACT_ACCUMULATOR_STATE_BYTES: f64 = 56.0;

/// Cost model 1 charging 1 per CPU operation and nothing else.
fn cpu_operations_only() -> CostInputs {
    CostInputs {
        cost_model: CostModel::UsageOverTime(ResourceUnitCost {
            cost_per_cpu_op: 1.0,
            cost_per_scanned_byte: 0.0,
            cost_per_memory_byte_second: 0.0,
            cost_per_provisioned_memory_byte_second: 0.0,
            cost_per_provisioned_cpu_core_second: 0.0,
            cost_per_disk_byte_second: 0.0,
        }),
        ..CostInputs::default()
    }
}

/// Cost model 1 charging 1 per byte held in memory for a second and
/// nothing else.
fn retained_bytes_only() -> CostInputs {
    CostInputs {
        cost_model: CostModel::UsageOverTime(ResourceUnitCost {
            cost_per_cpu_op: 0.0,
            cost_per_scanned_byte: 0.0,
            cost_per_memory_byte_second: 1.0,
            cost_per_provisioned_memory_byte_second: 0.0,
            cost_per_provisioned_cpu_core_second: 0.0,
            cost_per_disk_byte_second: 0.0,
        }),
        ..CostInputs::default()
    }
}

/// One query over `m` (1,000 series every second), every 60 s at phase 0,
/// exact.
fn one_query(query: &str, metric_type: &str) -> PlanningWorkload {
    repeating_promql_workload(
        &[query],
        60_000,
        json!({"max_error": 0.0}),
        &[(
            "m",
            json!({"series_count": 1000, "sample_interval": 1000, "metric_type": metric_type}),
        )],
    )
    .unwrap()
}

/// `workload` planned on `capabilities` with the default cost inputs.
fn planned(workload: &PlanningWorkload, capabilities: &DeploymentCapabilities) -> PlannedWorkload {
    plan_workload(
        workload,
        &[],
        capabilities,
        &CostInputs::default(),
        false,
        RunUntil::Stage3,
    )
    .unwrap()
}

/// The selected plan, checked to have the operators `shape` (one per node,
/// in node order): a plan of another shape is not what the oracle computes.
fn plan_of_shape<'a>(planned: &'a PlannedWorkload, shape: &[&str]) -> &'a PhysicalASAPDAG {
    let (plan, _) = planned
        .stages
        .run
        .selection
        .as_ref()
        .expect("a plan is selected");
    let kinds: Vec<&str> = plan
        .nodes
        .iter()
        .map(|node| node.payload.kind_name())
        .collect();
    assert_eq!(kinds, shape, "the selected plan is not the canonical shape");
    plan
}

/// Each node's cost per second, in node order, and the plan's, of `plan`
/// under `inputs`.
fn costs(
    plan: &PhysicalASAPDAG,
    planned: &PlannedWorkload,
    inputs: &CostInputs,
) -> (Vec<f64>, f64) {
    let (cost, _) = plan_cost(plan, &planned.entries, &planned.data, inputs).unwrap();
    let per_node = plan
        .nodes
        .iter()
        .map(|node| cost.per_node[&node.id].cost_per_second.unwrap())
        .collect();
    (per_node, cost.cost_per_second.unwrap())
}

/// `actual` equals `expected` to a relative 1e-9.
fn assert_close(what: &str, actual: f64, expected: f64) {
    assert!(
        (actual - expected).abs() <= 1e-9 * expected.abs().max(1e-12),
        "{what}: plan_cost says {actual}, the oracle {expected}"
    );
}

/// `sum(m)` every 60 s on the default deployment: a query-time `Scan`
/// reads one second of rows (the instant selector's lookback, the sample
/// interval), 1,000 rows; the instant `TimeRange` keeps the latest row of
/// each of the 1,000 series; the aggregate reads 1,000 rows into one
/// group. Each runs 1/60 times per second: 50 operations per second.
#[test]
fn a_query_time_scan_costs_the_rows_its_lookback_reads() {
    let planned = planned(
        &one_query("sum(m)", "gauge"),
        &DeploymentCapabilities::default(),
    );
    let plan = plan_of_shape(&planned, &["Scan", "TimeRange", "Aggregate"]);
    let rows_read = ROWS_PER_SECOND * 1.0;
    let expected = [
        rows_read * EVALUATIONS_PER_SECOND,
        SERIES * EVALUATIONS_PER_SECOND,
        SERIES * (0.0 + 1.0) * EVALUATIONS_PER_SECOND,
    ];
    let (per_node, total) = costs(plan, &planned, &cpu_operations_only());
    for (node, (actual, expected)) in per_node.iter().zip(expected).enumerate() {
        assert_close(&format!("node {node}"), *actual, expected);
    }
    assert_close("plan", total, expected.iter().sum());
}

/// The plan of `sum(sum_over_time(m[5m]))` every 60 s on a deployment that
/// keeps no raw data: an ingestion-time `Scan`, a chain of 60 s windows
/// of exact sums materialized across evaluations, then at query time a 5
/// min `TimeRange` over the chain, a `SummaryMerge`, the finalize and the
/// sum.
fn chain_plan() -> PlannedWorkload {
    let no_raw_data = DeploymentCapabilities {
        raw_data_retained: false,
        ..DeploymentCapabilities::default()
    };
    planned(
        &one_query("sum(sum_over_time(m[5m]))", "gauge"),
        &no_raw_data,
    )
}

const CHAIN_SHAPE: [&str; 6] = [
    "Scan",
    "SummaryTumblingWindowChain",
    "TimeRange",
    "SummaryMerge",
    "FinalizeExactAccumulator",
    "Aggregate",
];

/// At ingestion time each second's 1,000 arriving rows are scanned once
/// (1,000 operations per second) and each updates one window's exact sum
/// state (depth 1, plus the row: 2,000 per second). The chain retains the
/// 5 windows its 5 min reader needs (h_v = 5 min of 60 s windows) plus the
/// window being built: 6 × 1,000 states of 56 bytes, 336,000 bytes held.
#[test]
fn an_ingestion_time_chain_costs_its_arriving_rows_and_retains_its_windows() {
    let planned = chain_plan();
    let plan = plan_of_shape(&planned, &CHAIN_SHAPE);
    let (cpu, _) = costs(plan, &planned, &cpu_operations_only());
    assert_close("ingestion-time Scan", cpu[0], ROWS_PER_SECOND);
    assert_close("chain build", cpu[1], ROWS_PER_SECOND * (1.0 + 1.0));
    let (memory, _) = costs(plan, &planned, &retained_bytes_only());
    let retained_windows = 300.0 / 60.0;
    assert_close(
        "chain retention",
        memory[1],
        (retained_windows + 1.0) * SERIES * EXACT_ACCUMULATOR_STATE_BYTES,
    );
}

/// The 5 min `TimeRange` over a chain of 60 s windows selects 5 windows of
/// 1,000 states, 5,000 states per evaluation, 1/60 evaluations per second.
#[test]
fn a_time_range_over_a_chain_selects_the_windows_its_range_covers() {
    let planned = chain_plan();
    let plan = plan_of_shape(&planned, &CHAIN_SHAPE);
    let (cpu, _) = costs(plan, &planned, &cpu_operations_only());
    let windows = 300.0 / 60.0;
    assert_close(
        "TimeRange over the chain",
        cpu[2],
        windows * SERIES * EVALUATIONS_PER_SECOND,
    );
}

/// The `SummaryMerge` reads every selected window's states (5 × 1,000)
/// into one per series (1,000), which the finalize reads and the sum
/// aggregates; the plan's CPU is 1,000 + 2,000 per second at ingestion
/// plus (5,000 + 5,000 + 1,000 + 1,000) / 60 at query time.
#[test]
fn a_summary_merge_reads_every_selected_window_state() {
    let planned = chain_plan();
    let plan = plan_of_shape(&planned, &CHAIN_SHAPE);
    let (cpu, total) = costs(plan, &planned, &cpu_operations_only());
    let states = 5.0 * SERIES;
    assert_close("SummaryMerge", cpu[3], states * EVALUATIONS_PER_SECOND);
    assert_close("finalize", cpu[4], SERIES * EVALUATIONS_PER_SECOND);
    assert_close("sum", cpu[5], SERIES * EVALUATIONS_PER_SECOND);
    assert_close(
        "plan",
        total,
        ROWS_PER_SECOND * 3.0 + (states + states + SERIES + SERIES) * EVALUATIONS_PER_SECOND,
    );
}

/// #1253 Example 1, o11y q21's shape: `sum(rate(m[5m] offset 1h))` as a
/// 1 h range query (step 60 s) refreshed every 60 s over a chain of 60 s
/// windows of exact rate states. Each refresh evaluates 61 steps, so the
/// query-time nodes run 61/60 times per second. At each step the 5 min
/// `TimeRange` over the 1 h `TimeShift` selects 5 windows of 1,000
/// states and the merge reads those 5,000 states into 1,000 (before the
/// fix, the shift hid the chain: one window, "merge 1000 states into
/// 1000"). The chain retains offset + range + span = 1 h + 5 min + 1 h =
/// 125 windows, plus the one being built.
#[test]
fn a_time_range_over_a_shifted_chain_merges_every_selected_window() {
    let mut workload = one_query("sum(rate(m[5m] offset 1h))", "counter");
    let entry = &mut workload.query_workload.repeating_queries.as_mut().unwrap()[0];
    entry.range_evaluation =
        Some(serde_json::from_value(json!({"step_ms": 60000, "span_ms": 3600000})).unwrap());
    let planned = planned(&workload, &DeploymentCapabilities::default());
    let plan = plan_of_shape(
        &planned,
        &[
            "Scan",
            "SummaryTumblingWindowChain",
            "TimeShift",
            "TimeRange",
            "SummaryMerge",
            "FinalizeExactAccumulator",
            "Aggregate",
        ],
    );
    let steps_per_refresh = 3_600.0 / 60.0 + 1.0;
    let query_time_executions_per_second = steps_per_refresh * EVALUATIONS_PER_SECOND;
    let states = 5.0 * SERIES;
    let expected = [
        ROWS_PER_SECOND,
        ROWS_PER_SECOND * (1.0 + 1.0),
        0.0,
        states * query_time_executions_per_second,
        states * query_time_executions_per_second,
        SERIES * query_time_executions_per_second,
        SERIES * query_time_executions_per_second,
    ];
    let (cpu, total) = costs(plan, &planned, &cpu_operations_only());
    for (node, (actual, expected)) in cpu.iter().zip(expected).enumerate() {
        assert_close(
            &format!("node {node} ({})", plan.nodes[node].payload.kind_name()),
            *actual,
            expected,
        );
    }
    assert_close("plan", total, expected.iter().sum());
    let (memory, _) = costs(plan, &planned, &retained_bytes_only());
    let retained_windows = (3_600.0 + 300.0 + 3_600.0) / 60.0;
    assert_close(
        "chain retention",
        memory[1],
        (retained_windows + 1.0) * SERIES * EXACT_ACCUMULATOR_STATE_BYTES,
    );
}

//! Hand-computed costs for the canonical plan shapes
//! (`docs/develop_docs/testing.md`, "Oracles"): each expected cost is
//! computed here from the concepts doc's cost terms (section 5, "Cost and
//! retention": executions per second ρ_κ times one execution's work, and
//! a materialized node's retained state), not by `asap_cost`'s node
//! costing, and compared with `asap_cost::plan_cost` of the plan Stage 3
//! selects.
//!
//! The plans are costed under cost inputs that charge one resource
//! ([`charging_only`]): [`CPU_OPERATION`] charges 1 per CPU operation, so a
//! node's cost is its CPU operations per second; [`RETAINED_BYTE_SECOND`]
//! charges 1 per byte held for a second. Two terms are the cost model's
//! (`asap-cost`, module `analytical`), stated here and not checked:
//!
//! - the per-operation work: a scan, a time range and a merge do one
//!   operation per row or state they produce or read, a finalize one per
//!   accumulator, a hash aggregate one per input row and per grouping key
//!   and accumulator, an exact accumulator build one per row and per update
//!   (depth 1), a time shift none;
//! - a chain retains, beside the windows its consumers read (offset + range
//!   + span, h_v), the one window being built.
//!
//! The counts are what this oracle checks: rows per execution, windows a
//! range selects, states a merge reads, executions per second and retained
//! windows. Each expected value is a literal or arithmetic over the named
//! constants; nothing is read from the planner but the plan's costs.
//!
//! Each test checks that Stage 3 selects the canonical shape before costing
//! it: the oracle's counts are for that shape only, so a change of the
//! selected plan fails with "not the canonical shape", not a wrong count.

use asap_cost::{plan_cost, CostInputs, CostModel, ResourceUnitCost};
use asap_devtools::corpus_registry::{plan_workload, repeating_promql_workload, PlannedWorkload};
use asap_devtools::planning_stages::RunUntil;
use asap_types::deployment_capabilities::DeploymentCapabilities;
use asap_types::ir::physical_export::PhysicalASAPDAG;
use asap_types::ir::properties::AccuracyTarget;
use asap_types::workload::{DurationMs, PrometheusMetricType, RangeEvaluation, SourceFacts};

/// Series of the one metric `m`, each sampled every second: 1,000 rows
/// arrive per second.
const SERIES: f64 = 1_000.0;
const ROWS_PER_SECOND: f64 = 1_000.0;
/// Every query repeats every 60 s.
const EVALUATIONS_PER_SECOND: f64 = 1.0 / 60.0;
/// The bytes of one exact `sum` or `rate` accumulator state (a sum and a
/// count with their group key), as `asap_cost::state_bytes` sizes it.
const EXACT_ACCUMULATOR_STATE_BYTES: f64 = 56.0;
/// `sum(..)`'s grouping keys and accumulators: a hash aggregate does one
/// operation per input row per (key + accumulator).
const GROUPING_KEYS: f64 = 0.0;
const ACCUMULATORS: f64 = 1.0;

/// No resource charged.
const NOTHING: ResourceUnitCost = ResourceUnitCost {
    cost_per_cpu_op: 0.0,
    cost_per_scanned_byte: 0.0,
    cost_per_memory_byte_second: 0.0,
    cost_per_provisioned_memory_byte_second: 0.0,
    cost_per_provisioned_cpu_core_second: 0.0,
    cost_per_disk_byte_second: 0.0,
};
/// 1 per CPU operation and nothing else.
const CPU_OPERATION: ResourceUnitCost = ResourceUnitCost {
    cost_per_cpu_op: 1.0,
    ..NOTHING
};
/// 1 per byte held in memory for a second and nothing else.
const RETAINED_BYTE_SECOND: ResourceUnitCost = ResourceUnitCost {
    cost_per_memory_byte_second: 1.0,
    ..NOTHING
};

/// Cost model 1 with `unit_costs` (one resource charged).
fn charging_only(unit_costs: ResourceUnitCost) -> CostInputs {
    CostInputs {
        cost_model: CostModel::UsageOverTime(unit_costs),
        ..CostInputs::default()
    }
}

/// `query` over the one metric `m` with facts `source`, repeating every
/// 60 s at phase 0 at `accuracy` (a range query when `range_evaluation` is
/// set), planned to Stage 3 on `capabilities` with the default cost inputs.
/// The oracles' one planning helper.
pub(crate) fn plan_one_promql_query(
    query: &str,
    accuracy: AccuracyTarget,
    source: SourceFacts,
    range_evaluation: Option<RangeEvaluation>,
    capabilities: &DeploymentCapabilities,
) -> PlannedWorkload {
    let mut workload = repeating_promql_workload(&[query], 60_000, accuracy, &[("m", source)]);
    workload.query_workload.repeating_queries.as_mut().unwrap()[0].range_evaluation =
        range_evaluation;
    plan_workload(
        &workload,
        &[],
        capabilities,
        &CostInputs::default(),
        false,
        RunUntil::Stage3,
    )
    .unwrap()
}

/// The operator of each of `plan`'s nodes, in node order.
pub(crate) fn operator_kinds(plan: &PhysicalASAPDAG) -> Vec<&'static str> {
    plan.nodes
        .iter()
        .map(|node| node.payload.kind_name())
        .collect()
}

/// `query` exactly, over `m`: 1,000 series of `metric_type` sampled every
/// second.
fn planned(
    query: &str,
    metric_type: PrometheusMetricType,
    range_evaluation: Option<RangeEvaluation>,
    capabilities: &DeploymentCapabilities,
) -> PlannedWorkload {
    let source = SourceFacts {
        series_count: Some(SERIES as u64),
        sample_interval: Some(DurationMs(1_000)),
        metric_type: Some(metric_type),
        ..SourceFacts::default()
    };
    plan_one_promql_query(
        query,
        AccuracyTarget::exact(),
        source,
        range_evaluation,
        capabilities,
    )
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
    assert_eq!(
        operator_kinds(plan),
        shape,
        "the selected plan is not the canonical shape"
    );
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
        "sum(m)",
        PrometheusMetricType::Gauge,
        None,
        &DeploymentCapabilities::default(),
    );
    let plan = plan_of_shape(&planned, &["Scan", "TimeRange", "Aggregate"]);
    let rows_read = ROWS_PER_SECOND * 1.0;
    let expected = [
        rows_read * EVALUATIONS_PER_SECOND,
        SERIES * EVALUATIONS_PER_SECOND,
        SERIES * (GROUPING_KEYS + ACCUMULATORS) * EVALUATIONS_PER_SECOND,
    ];
    let (per_node, total) = costs(plan, &planned, &charging_only(CPU_OPERATION));
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
        "sum(sum_over_time(m[5m]))",
        PrometheusMetricType::Gauge,
        None,
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
/// 5 windows its 5 min consumer reads (h_v = 5 min of 60 s windows) plus
/// the window being built: 6 × 1,000 states of 56 bytes, 336,000 bytes
/// held.
#[test]
fn an_ingestion_time_chain_costs_its_arriving_rows_and_retains_its_windows() {
    let planned = chain_plan();
    let plan = plan_of_shape(&planned, &CHAIN_SHAPE);
    let (cpu, _) = costs(plan, &planned, &charging_only(CPU_OPERATION));
    assert_close("ingestion-time Scan", cpu[0], ROWS_PER_SECOND);
    assert_close("chain build", cpu[1], ROWS_PER_SECOND * (1.0 + 1.0));
    let (memory, _) = costs(plan, &planned, &charging_only(RETAINED_BYTE_SECOND));
    let retained_windows = 300.0 / 60.0;
    let window_being_built = 1.0;
    assert_close(
        "chain retention",
        memory[1],
        (retained_windows + window_being_built) * SERIES * EXACT_ACCUMULATOR_STATE_BYTES,
    );
}

/// The 5 min `TimeRange` over a chain of 60 s windows selects 5 windows of
/// 1,000 states, 5,000 states per evaluation, 1/60 evaluations per second.
#[test]
fn a_time_range_over_a_chain_selects_the_windows_its_range_covers() {
    let planned = chain_plan();
    let plan = plan_of_shape(&planned, &CHAIN_SHAPE);
    let (cpu, _) = costs(plan, &planned, &charging_only(CPU_OPERATION));
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
    let (cpu, total) = costs(plan, &planned, &charging_only(CPU_OPERATION));
    let states = 5.0 * SERIES;
    assert_close("SummaryMerge", cpu[3], states * EVALUATIONS_PER_SECOND);
    assert_close("finalize", cpu[4], SERIES * EVALUATIONS_PER_SECOND);
    assert_close(
        "sum",
        cpu[5],
        SERIES * (GROUPING_KEYS + ACCUMULATORS) * EVALUATIONS_PER_SECOND,
    );
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
    let planned = planned(
        "sum(rate(m[5m] offset 1h))",
        PrometheusMetricType::Counter,
        Some(RangeEvaluation {
            step_ms: 60_000,
            span_ms: 3_600_000,
        }),
        &DeploymentCapabilities::default(),
    );
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
        SERIES * (GROUPING_KEYS + ACCUMULATORS) * query_time_executions_per_second,
    ];
    let (cpu, total) = costs(plan, &planned, &charging_only(CPU_OPERATION));
    for (node, (actual, expected)) in cpu.iter().zip(expected).enumerate() {
        assert_close(
            &format!("node {node} ({})", plan.nodes[node].payload.kind_name()),
            *actual,
            expected,
        );
    }
    assert_close("plan", total, expected.iter().sum());
    let (memory, _) = costs(plan, &planned, &charging_only(RETAINED_BYTE_SECOND));
    let retained_windows = (3_600.0 + 300.0 + 3_600.0) / 60.0;
    let window_being_built = 1.0;
    assert_close(
        "chain retention",
        memory[1],
        (retained_windows + window_being_built) * SERIES * EXACT_ACCUMULATOR_STATE_BYTES,
    );
}

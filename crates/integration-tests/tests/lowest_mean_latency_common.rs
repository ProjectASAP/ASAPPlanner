//! Shared inputs of the lowest-mean-latency tests
//! (`docs/design_docs/proposals/stage3-lowest-mean-latency.md`, section 6):
//! workloads over the gauge `m`, cost inputs written as the deployment's
//! JSON, and the plan and report the one Planner call returns.
//!
//! Every input is JSON, so the tests compile against today's types: a
//! field or variant the design adds is refused when the JSON is loaded,
//! which is how the new-feature tests fail before the implementation.

use asap_planner::{
    plan_installable, CostInputs, DeploymentCapabilities, FrontendInput, InstallablePlan,
    MaterializedViewRefresh, PlanningModels, PlanningWorkload, UserInput,
};
use serde_json::{json, Value};

/// One repeating PromQL query: `query` every `interval_ms` at phase 0, with
/// `max_latency_ms` when given.
pub(crate) fn every(query: &str, interval_ms: u32, max_latency_ms: Option<f64>) -> Value {
    json!({
        "query": query,
        "demand": {"fixed_interval_at": {"interval": interval_ms, "evaluation_phase": 0}},
        "requirements": {"max_latency_ms": max_latency_ms},
        "predictability": {"predictable": {"known_at": null}},
    })
}

/// `entries` over the gauge `m`: `series` series, one sample per second.
pub(crate) fn over_m(entries: Vec<Value>, series: u64) -> PlanningWorkload {
    serde_json::from_value(json!({
        "query_workload": {"language": "promql", "repeating_queries": entries},
        "data_workload": {
            "arrival": {"ingesting": {"rows_per_second": series as f64}},
            "sources": {"m": {"series_count": series, "metric_type": "gauge", "sample_interval": 1000}},
        },
    }))
    .unwrap()
}

/// Cost model 3 with `objective`; `null` limits are unchecked (`cpu_cores`)
/// or effectively unlimited (memory, disk).
pub(crate) fn cluster(
    objective: &str,
    memory_bytes: Option<u64>,
    disk_bytes: Option<u64>,
    cpu_cores: Option<u32>,
) -> Value {
    json!({"cluster_capacity": {
        "memory_bytes": memory_bytes.unwrap_or(1_000_000_000_000_000),
        "disk_bytes": disk_bytes.unwrap_or(1_000_000_000_000_000),
        "cpu_cores": cpu_cores,
        "objective": objective,
    }})
}

/// Cost model 1 at the illustrative prices, as JSON.
pub(crate) fn usage_over_time() -> Value {
    json!({"usage_over_time": {
        "cost_per_cpu_op": 1e-6, "cost_per_scanned_byte": 1e-7,
        "cost_per_memory_byte_second": 1.25e-7,
        "cost_per_provisioned_memory_byte_second": 1.25e-7,
        "cost_per_provisioned_cpu_core_second": 1000.0,
        "cost_per_disk_byte_second": 0.0}})
}

/// A measured operation cost entry: `operator` at `phase` with `fields`
/// (family, `ns_per_unit`, and any other) merged in.
pub(crate) fn entry(operator: &str, phase: &str, fields: Value) -> Value {
    let mut entry = json!({"operator": operator, "phase": phase});
    for (key, value) in fields.as_object().unwrap() {
        entry[key] = value.clone();
    }
    entry
}

/// The measured table pricing both plans of `max_over_time(m[5m])` (and
/// `min_over_time`): reading 5 minutes of raw rows (a query-time
/// `TimeRange` over the raw rows, kept at ingestion or scanned at query
/// time) at `scan_ns_per_sample` per row, and an ingestion-time chain of
/// exact accumulators whose refresh costs `state_ns` per window state
/// emitted (one built at query time reads the raw rows at the raw price),
/// its windows' states read by a `TimeRange` at the same price per row. Every other class is priced
/// at 1 ns per unit, so the plans differ only by these, except that each
/// query-time row a `min` folds (an `Aggregate` or a `SummaryAgg`) costs
/// `min_ns_per_row` more.
pub(crate) fn max_min_table(
    scan_ns_per_sample: f64,
    state_ns: f64,
    min_ns_per_row: f64,
) -> Vec<Value> {
    let mut entries = vec![
        entry("Scan", "ingestion_time", json!({"ns_per_unit": 1.0})),
        entry("Scan", "query_time", json!({"ns_per_unit": 0.0})),
        entry(
            "TimeRange",
            "query_time",
            json!({"ns_per_unit": scan_ns_per_sample}),
        ),
        entry("BinaryOp", "query_time", json!({"ns_per_unit": 0.0})),
    ];
    for (measure, exact, row_ns) in [("max", "Max", 1.0), ("min", "Min", 1.0 + min_ns_per_row)] {
        entries.push(entry(
            "Aggregate",
            "query_time",
            json!({"measure": measure, "ns_per_unit": row_ns}),
        ));
        entries.push(entry(
            "SummaryTumblingWindowChain",
            "ingestion_time",
            json!({"exact": exact, "ns_per_unit": 1.0, "ns_per_state_emitted": state_ns}),
        ));
        entries.push(entry(
            "SummaryAgg",
            "query_time",
            json!({"exact": exact, "ns_per_unit": row_ns}),
        ));
        // A chain built at query time reads the raw rows too.
        entries.push(entry(
            "SummaryTumblingWindowChain",
            "query_time",
            json!({"exact": exact, "ns_per_unit": scan_ns_per_sample + row_ns}),
        ));
        for operator in ["SummaryMerge", "FinalizeExactAccumulator"] {
            entries.push(entry(
                operator,
                "query_time",
                json!({"exact": exact, "ns_per_unit": 1.0}),
            ));
        }
    }
    entries
}

/// Cost inputs: `cost_model`, the measured table `operators` with `extra`
/// table fields (e.g. `refresh_io_wait`), and `data_plane_execution`.
pub(crate) fn cost_inputs(
    cost_model: Value,
    operators: Vec<Value>,
    extra_table_fields: Value,
    data_plane_execution: Value,
) -> Value {
    let mut table = json!({"operators": operators});
    for (key, value) in extra_table_fields.as_object().unwrap() {
        table[key] = value.clone();
    }
    let mut settings = json!({
        "cost_model": cost_model,
        "measured_operation_costs": table,
    });
    // Omitted when empty, as a file written before the field would.
    if data_plane_execution != json!({}) {
        settings["data_plane_execution"] = data_plane_execution;
    }
    settings
}

/// A plan the Planner selected and its report as JSON.
pub(crate) struct Planned {
    pub(crate) plan: InstallablePlan,
    pub(crate) report: Value,
}

impl Planned {
    /// Query `q`'s predicted usage (latency breakdown) in the report.
    pub(crate) fn query(&self, q: usize) -> &Value {
        &self.report["predicted_usage"]["per_query"][q]
    }

    /// Query `q`'s field `name` as a number; panics naming it when absent.
    pub(crate) fn query_ms(&self, q: usize, name: &str) -> f64 {
        number(self.query(q), name)
    }

    /// The report's `predicted_usage` field `name` as a number.
    pub(crate) fn usage(&self, name: &str) -> f64 {
        number(&self.report["predicted_usage"], name)
    }

    /// Whether query `q` waits for an ingestion-time view's refresh: its
    /// precompute execution is positive.
    pub(crate) fn query_reads_a_view(&self, q: usize) -> bool {
        self.query_ms(q, "precompute_execution_ms") > 0.0
    }

    /// Whether the plan refreshes any view at ingestion time.
    pub(crate) fn has_ingestion_views(&self) -> bool {
        self.plan
            .materialized_views()
            .unwrap()
            .iter()
            .any(|view| view.refresh() == MaterializedViewRefresh::AtIngestion)
    }

    /// The frequency-weighted mean latency over the report's queries.
    pub(crate) fn mean_latency_ms(&self) -> f64 {
        let per_query = self.report["predicted_usage"]["per_query"]
            .as_array()
            .expect("per_query");
        let (weighted, weights) = per_query.iter().fold((0.0, 0.0), |(sum, w), query| {
            let weight = number(query, "evaluations_per_second");
            (sum + weight * number(query, "latency_ms"), w + weight)
        });
        weighted / weights
    }
}

/// `value[name]` as a number, panicking with the field name and the value
/// when it is absent (a report field the implementation has not added yet).
pub(crate) fn number(value: &Value, name: &str) -> f64 {
    value[name]
        .as_f64()
        .unwrap_or_else(|| panic!("report field {name} missing in {value}"))
}

/// Plan `workload` (PromQL) under `cost_inputs` (JSON) on a deployment that
/// keeps raw data or not. Err: the cost inputs do not load (a field the
/// design adds) or planning fails, as text.
pub(crate) async fn planned(
    workload: &PlanningWorkload,
    cost_inputs: &Value,
    raw_data_retained: bool,
) -> Result<Planned, String> {
    let cost_inputs: CostInputs = serde_json::from_value(cost_inputs.clone())
        .map_err(|error| format!("cost inputs do not load: {error}"))?;
    let capabilities = DeploymentCapabilities {
        raw_data_retained,
        ..DeploymentCapabilities::default()
    };
    let models = PlanningModels::builtin()
        .with_cost_inputs(&cost_inputs)
        .with_deployment_capabilities(&capabilities);
    let (plan, report) = plan_installable(UserInput::new(workload, FrontendInput::Promql, models))
        .await
        .map_err(|error| format!("planning failed: {error}"))?;
    Ok(Planned {
        plan,
        report: serde_json::to_value(report).unwrap(),
    })
}

/// [`planned`], panicking with the reason.
pub(crate) async fn plan(
    workload: &PlanningWorkload,
    cost_inputs: &Value,
    raw_data_retained: bool,
) -> Planned {
    planned(workload, cost_inputs, raw_data_retained)
        .await
        .unwrap_or_else(|error| panic!("{error}"))
}

/// Default data-plane execution with `fields` merged in.
pub(crate) fn data_plane(fields: Value) -> Value {
    let mut execution = json!({});
    for (key, value) in fields.as_object().unwrap() {
        execution[key] = value.clone();
    }
    execution
}

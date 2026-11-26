//! The cost models and the deployment's cost inputs: what each unit of a
//! resource costs ([`ResourceUnitCost`]), how fast each resource runs
//! ([`ResourceSpeed`]), the cluster's limits and objective, and the
//! calibration of measured costs.

use crate::node::analytical::AnalyticalCostError;
use crate::MeasuredOperationCosts;

/// [`PlanCost::source`](crate::PlanCost::source) when every node is priced from the formulas.
pub const COST_SOURCE: &str = "analytical-cost-v2 (illustrative statistics)";

/// [`PlanCost::source`](crate::PlanCost::source) when every node is priced from the measured
/// table.
pub const MEASURED_COST_SOURCE: &str = "measured operation costs (illustrative statistics)";

/// [`PlanCost::source`](crate::PlanCost::source) when some nodes are priced from the measured
/// table and the others from the formulas.
pub const MIXED_COST_SOURCE: &str =
    "measured operation costs and analytical-cost-v2 (illustrative statistics)";

/// Seconds over which non-repeating queries (one-off, scheduled and
/// unknown) are spread by default: one hour.
pub(crate) const DEFAULT_NON_REPEATING_QUERY_HORIZON_SECONDS: f64 = 3_600.0;

/// The price of one unit of each resource, per second of wall time. Cost
/// model 1 reads the usage prices (CPU, scan, memory byte-seconds, disk);
/// cost model 2 the provisioned prices (memory, CPU cores) and disk.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResourceUnitCost {
    /// Price of one CPU operation.
    pub cost_per_cpu_op: f64,
    /// Price of scanning one byte.
    pub cost_per_scanned_byte: f64,
    /// Price of holding one byte of memory for one second (cost model 1).
    pub cost_per_memory_byte_second: f64,
    /// Price of provisioning one byte of memory for one second (cost model 2).
    pub cost_per_provisioned_memory_byte_second: f64,
    /// Price of provisioning one CPU core for one second (cost model 2).
    pub cost_per_provisioned_cpu_core_second: f64,
    /// Price of keeping one byte on disk for one second (both models).
    pub cost_per_disk_byte_second: f64,
}

impl ResourceUnitCost {
    /// The prices plan selection uses until a deployment is calibrated:
    /// 1 ns of CPU per operation (one operation is one ns of the reference
    /// executor's work, [`ResourceSpeed::ms_per_cpu_op`]), 0.1 ns per scanned byte, 1 GB retained for a
    /// second costs 1/8 vCPU (125 CPU-ms), held or provisioned alike; one
    /// provisioned core-second is 1,000 CPU-ms. A byte on disk for a
    /// second costs a fortieth of a byte in memory (3.125e-9), about what
    /// block storage costs against memory.
    pub fn illustrative() -> Self {
        const COST_PER_MEMORY_BYTE_SECOND: f64 = 1.25e-7;
        Self {
            cost_per_cpu_op: 1e-6,
            cost_per_scanned_byte: 1e-7,
            cost_per_memory_byte_second: COST_PER_MEMORY_BYTE_SECOND,
            cost_per_provisioned_memory_byte_second: COST_PER_MEMORY_BYTE_SECOND,
            cost_per_provisioned_cpu_core_second: 1_000.0,
            cost_per_disk_byte_second: COST_PER_MEMORY_BYTE_SECOND / 40.0,
        }
    }

    /// AWS Fargate on-demand prices, US East (N. Virginia), Linux/x86, for
    /// cost model 1 ([`CostModel::UsageOverTime`]); one cost unit is one
    /// US dollar. Source: <https://aws.amazon.com/fargate/pricing/>, checked
    /// 2026-10-18 (Pricing Example 1 there: $0.000011244 per vCPU-second,
    /// $0.000001235 per GB-second of memory, $0.0000000308 per GB-second of
    /// ephemeral storage beyond the 20 GB included). Prices change: re-check
    /// the page before relying on them.
    ///
    /// `speed` is the deployment's [`CostInputs::resource_speed`],
    /// the speed Stage 3 uses for latency, so the CPU prices charge the core
    /// time latency counts. At `ResourceSpeed::illustrative` (1e-6 ms per
    /// CPU operation, i.e. 1e9 operations per core-second; 1e-7 ms per
    /// scanned byte):
    ///
    /// * `cost_per_cpu_op` = $0.000011244 per vCPU-second ×
    ///   `ms_per_cpu_op` / 1,000 (1e-9 s per operation) = $1.1244e-14.
    /// * `cost_per_scanned_byte` = $0.000011244 per vCPU-second ×
    ///   `ms_per_scanned_byte` / 1,000 (1e-10 s per byte) = $1.1244e-15:
    ///   Fargate has no scan price, but a scan occupies the vCPU-seconds it
    ///   bills, for the time latency charges.
    /// * `cost_per_memory_byte_second` = $0.000001235 per GB-second ÷ 2^30
    ///   bytes per GB ≈ $1.1502e-15: the page says "on this page GB =
    ///   1024^3 bytes".
    /// * `cost_per_disk_byte_second` = $0.0000000308 per GB-second of
    ///   ephemeral storage ÷ 2^30 ≈ $2.87e-17. Fargate bills only the
    ///   storage configured beyond the 20 GB every task gets; the per-byte
    ///   cost ignores that allowance.
    /// * `cost_per_provisioned_memory_byte_second` and
    ///   `cost_per_provisioned_cpu_core_second` = 0: they belong to cost
    ///   model 2, and Fargate's per-task billing is cost model 1. Under
    ///   [`CostModel::ProvisionedCapacity`] this preset costs disk only.
    ///
    /// Not expressed by these per-second prices: the 1-minute billing
    /// minimum per task ("Pricing is calculated per second with a 1-minute
    /// minimum"), and the fixed task sizes a task is billed for (from 0.25
    /// vCPU with 0.5 GB up), which round usage up to the next size.
    pub fn aws_fargate_us_east_1(speed: ResourceSpeed) -> Self {
        // US dollars per vCPU-second and per GB-second of memory, from the
        // page above.
        const DOLLARS_PER_VCPU_SECOND: f64 = 0.000_011_244;
        const DOLLARS_PER_MEMORY_GB_SECOND: f64 = 0.000_001_235;
        const DOLLARS_PER_EPHEMERAL_STORAGE_GB_SECOND: f64 = 0.000_000_030_8;
        const BYTES_PER_GIB: f64 = (1u64 << 30) as f64;
        Self {
            cost_per_cpu_op: DOLLARS_PER_VCPU_SECOND * speed.ms_per_cpu_op / 1_000.0,
            cost_per_scanned_byte: DOLLARS_PER_VCPU_SECOND * speed.ms_per_scanned_byte / 1_000.0,
            cost_per_memory_byte_second: DOLLARS_PER_MEMORY_GB_SECOND / BYTES_PER_GIB,
            cost_per_provisioned_memory_byte_second: 0.0,
            cost_per_provisioned_cpu_core_second: 0.0,
            cost_per_disk_byte_second: DOLLARS_PER_EPHEMERAL_STORAGE_GB_SECOND / BYTES_PER_GIB,
        }
    }
}

/// How fast one core does work, and the batches of raw rows its work is
/// split into; the latency check reads it.
#[derive(Debug, Clone, Copy, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResourceSpeed {
    /// Milliseconds one CPU operation takes on one core. One CPU operation
    /// is one nanosecond of the reference executor's work
    /// (`node::operations_per_unit`), so the default is 1e-6; for another
    /// machine set this to 1e-6 × its time ÷ the reference time (a core
    /// half as fast: 2e-6).
    pub ms_per_cpu_op: f64,
    /// Milliseconds scanning one byte takes on one core.
    pub ms_per_scanned_byte: f64,
    /// The time one batch of raw rows covers in the deployment's data
    /// store, which returns raw rows to a query in those batches, each
    /// series' rows of a batch together. The executor has no batch width of
    /// its own: it groups the batches the store returns and looks up a
    /// series' group once per batch, so a grouping visits each group's
    /// state once per batch its rows are in. A 5 min window of a series
    /// sampled at 1 Hz in 10 s batches costs 30 visits per series, whether
    /// it holds 300 rows or (at 10 Hz) 3,000. Default 10,000 (10 s), an
    /// assumed value: the batch width of the store the reference
    /// measurements read from, not one the deployment declared; omitted
    /// from JSON, it takes that default. Ingestion-time work is charged
    /// one second's share of a batch per execution, which follows the
    /// arriving rows, not the store's batches.
    #[serde(default = "default_raw_batch_width_ms")]
    pub raw_batch_width_ms: u64,
}

/// The assumed [`ResourceSpeed::raw_batch_width_ms`]: 10 s.
const DEFAULT_RAW_BATCH_WIDTH_MS: u64 = 10_000;

fn default_raw_batch_width_ms() -> u64 {
    DEFAULT_RAW_BATCH_WIDTH_MS
}

impl ResourceSpeed {
    /// 1 ns per operation and 0.1 ns per scanned byte, the times
    /// [`ResourceUnitCost::illustrative`] prices: one cost unit is one
    /// millisecond of one core. Raw rows in 10 s batches (assumed).
    pub(crate) fn illustrative() -> Self {
        Self {
            ms_per_cpu_op: 1e-6,
            ms_per_scanned_byte: 1e-7,
            raw_batch_width_ms: DEFAULT_RAW_BATCH_WIDTH_MS,
        }
    }
}

/// How plan selection turns each valid candidate's [`ResourceUsage`](crate::ResourceUsage) into a
/// choice. Chosen by the deployment ([`CostInputs::cost_model`]).
/// In JSON, `{"usage_over_time": {...}}`, `{"provisioned_capacity": {...}}`
/// or `{"cluster_capacity": {...}}`.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CostModel {
    /// Cost model 1, pay-as-you-go: Σ unit cost × usage per second (CPU
    /// operations, scanned bytes, memory byte-seconds, disk byte-seconds);
    /// the cheapest valid candidate wins.
    UsageOverTime(ResourceUnitCost),
    /// Cost model 2, provisioned: unit cost × peak memory, peak CPU cores
    /// and disk, per second; the cheapest valid candidate wins.
    ProvisionedCapacity(ResourceUnitCost),
    /// Cost model 3, the deployment's own cluster: no prices; memory, disk
    /// and (when set) CPU cores are hard limits, and the objective chooses
    /// among the candidates that fit.
    ClusterCapacity(ClusterLimits),
}

impl CostModel {
    /// The per-unit costs each node's usage is priced with. Only cost
    /// model 1 prices per node: `UsageOverTime(u)` gives `Some(&u)`. Cost
    /// model 2 prices the plan's peaks (in `plan_cost`), not each node, and
    /// cost model 3 has no prices, so both give `None`.
    pub fn per_node_unit_costs(&self) -> Option<&ResourceUnitCost> {
        match self {
            CostModel::UsageOverTime(unit_cost) => Some(unit_cost),
            CostModel::ProvisionedCapacity(_) | CostModel::ClusterCapacity(_) => None,
        }
    }
}

/// A fixed cluster's capacity (cost model 3) and how to choose within it.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClusterLimits {
    /// Memory available; a candidate whose peak memory is above it is invalid.
    pub memory_bytes: u64,
    /// Disk available; a candidate keeping more on disk is invalid.
    pub disk_bytes: u64,
    /// CPU cores available; `None` leaves cores unchecked.
    pub cpu_cores: Option<u32>,
    /// How to choose among the candidates that fit.
    pub objective: ClusterObjective,
}

/// What cost model 3 optimizes among the candidates that fit.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ClusterObjective {
    /// The smallest worst-query latency; ties by peak memory, then disk.
    LowestLatency,
    /// The smallest peak memory; ties by disk, then worst-query latency.
    LeastMemory,
    /// The smallest frequency-weighted mean query latency (each query
    /// weighted by its evaluations per second; latency = freshness wait +
    /// query execution); ties by peak memory, then disk. Each query's
    /// `max_latency_ms` and the precompute load bound
    /// ([`DataPlaneExecution::max_thread_utilization`]) are hard limits.
    LowestMeanLatency,
}

/// How the deployment's data plane runs work: its threads, the lateness it
/// allows before closing a window, the utilisation its threads may run
/// at, and how it stores views (its window cache and files, the bytes a
/// per-series state's row holds beside its state). Latency, the precompute load and the recommended thread counts
/// read it, under every cost model; only cost model 3's
/// `lowest_mean_latency` selects with it. In JSON an object with these
/// fields; an omitted field takes its default.
#[derive(Debug, Clone, Copy, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct DataPlaneExecution {
    /// Threads inserting arriving rows, each building a partial copy of an
    /// in-memory state's newest window; `None` (default): one per core the
    /// ingestion work needs.
    pub ingest_threads: Option<u32>,
    /// Threads running window-close refreshes of ingestion-time views;
    /// default 1.
    pub precompute_threads: u32,
    /// Threads running query requests; default 1. A range request's steps
    /// split over them (`RequestFixedCost::step_rounds`): its per-step work takes
    /// ⌈steps ÷ threads⌉ rounds of latency, its CPU is unchanged. Not a
    /// limit: the report recommends how many a plan needs.
    pub query_threads: u32,
    /// Milliseconds the data plane waits after a window's end before
    /// closing it; every query waits for it. Default 0.
    pub allowed_lateness_ms: u64,
    /// The largest precompute load (the fraction of the precompute threads'
    /// time spent refreshing) a plan may have under `lowest_mean_latency`,
    /// and the utilisation the recommended thread counts are sized for;
    /// default 0.7.
    pub max_thread_utilization: f64,
    /// Whether the data plane buffers a window's arriving rows and inserts
    /// them when the window closes, in the refresh: its ingest work is
    /// then refresh work (on the precompute threads, in the freshness wait
    /// and the precompute load). Default false: rows are inserted as they
    /// arrive, on the ingest threads.
    pub ingest_work_at_refresh: bool,
    /// The data plane's cache of decoded view windows, when it keeps every
    /// window of an in-memory ingestion-time view in a file: a window is
    /// also in memory while it was written or read within the cache's idle
    /// time. `None` (default): every retained window is memory, and none is
    /// disk.
    pub view_window_cache: Option<ViewWindowCache>,
    /// Whether the data plane keeps each query-time `OneEvaluation` node's
    /// output, computed once per evaluation time and shared by the queries
    /// at that time, for a while after it; `None` (default): only while it
    /// is computed.
    pub kept_query_time_results: Option<KeptQueryTimeResults>,
    /// Whether the data plane keeps each query's answer per evaluation time,
    /// so a range request computes only the steps it has not answered;
    /// `None` (default): no answer is kept and every step is computed.
    pub answer_cache: Option<AnswerCache>,
    /// Bytes a stored per-series state's row holds beside its state: its
    /// window's time and its series key, no label text (a series' labels
    /// are derived from its identity). Default 72: an 8-byte window time
    /// and a 64-byte series key (a serialized label set, e.g.
    /// `{"instance":"host-17:9100","job":"api","region":"r1"}`, about 55
    /// bytes, and a 4-byte offset, rounded up; the data workload declares
    /// no label lengths).
    pub stored_row_bytes_beside_state: u64,
}

/// The default [`DataPlaneExecution::stored_row_bytes_beside_state`].
const DEFAULT_STORED_ROW_BYTES_BESIDE_STATE: u64 = 72;

/// Query-time results kept after their evaluation time
/// (`docs/design_docs/proposals/cost-terms-from-calibration.md`, section
/// 2). In JSON `{"retention_ms": 60000}`, with `max_bytes` optional.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct KeptQueryTimeResults {
    /// How long after its evaluation time a result is kept, in ms.
    pub retention_ms: u64,
    /// The most bytes of results kept; `None` (default): no bound.
    #[serde(default)]
    pub max_bytes: Option<u64>,
}

/// Answers kept per query and evaluation time
/// (`docs/design_docs/proposals/cost-terms-from-calibration.md`, section
/// 2). In JSON `{"retention_margin_ms": 60000}`, with `max_bytes` optional.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AnswerCache {
    /// How far beyond the plan's longest retention an answer is kept, in
    /// ms: answers are kept as far back as the plan's evaluations read,
    /// plus this.
    pub retention_margin_ms: u64,
    /// The most bytes of answers kept; `None` (default): no bound.
    #[serde(default)]
    pub max_bytes: Option<u64>,
}

/// A cache of decoded view windows over the files every window of a view
/// is kept in (`docs/design_docs/proposals/cost-terms-from-calibration.md`,
/// section 1). In JSON `{"idle_ms": 60000}`, with `max_bytes`,
/// `file_span_ms` and `retention_margin_ms` optional.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ViewWindowCache {
    /// Milliseconds after its last write or read at which a window leaves
    /// memory (stays in its file).
    pub idle_ms: u64,
    /// The most bytes of decoded windows the cache holds; `None` (default):
    /// no bound.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_bytes: Option<u64>,
    /// The span of windows one file holds, in ms: a file is deleted whole,
    /// once its newest window is past retention; 0 (default): each window
    /// is deleted on its own.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub file_span_ms: u64,
    /// How long past its retention a window is kept before its file may be
    /// deleted, in ms; default 0.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub retention_margin_ms: u64,
}

fn is_zero(value: &u64) -> bool {
    *value == 0
}

impl Default for DataPlaneExecution {
    fn default() -> Self {
        Self {
            ingest_threads: None,
            precompute_threads: 1,
            query_threads: 1,
            allowed_lateness_ms: 0,
            max_thread_utilization: 0.7,
            ingest_work_at_refresh: false,
            view_window_cache: None,
            kept_query_time_results: None,
            answer_cache: None,
            stored_row_bytes_beside_state: DEFAULT_STORED_ROW_BYTES_BESIDE_STATE,
        }
    }
}

impl DataPlaneExecution {
    /// Whether every field has its default, so JSON leaves it out.
    fn is_default(&self) -> bool {
        *self == Self::default()
    }

    /// Bad input: a thread count of 0, or a utilisation outside (0, 1].
    fn validate(&self) -> Result<(), AnalyticalCostError> {
        for (name, threads) in [
            (
                "data_plane_execution.ingest_threads",
                self.ingest_threads.unwrap_or(1),
            ),
            (
                "data_plane_execution.precompute_threads",
                self.precompute_threads,
            ),
            ("data_plane_execution.query_threads", self.query_threads),
        ] {
            if threads == 0 {
                return Err(AnalyticalCostError::InvalidCalibration(name, 0.0));
            }
        }
        if self
            .view_window_cache
            .is_some_and(|cache| cache.max_bytes == Some(0))
        {
            return Err(AnalyticalCostError::InvalidCalibration(
                "data_plane_execution.view_window_cache.max_bytes",
                0.0,
            ));
        }
        let utilization = self.max_thread_utilization;
        if !(utilization > 0.0 && utilization <= 1.0) {
            return Err(AnalyticalCostError::InvalidCalibration(
                "data_plane_execution.max_thread_utilization",
                utilization,
            ));
        }
        Ok(())
    }
}

/// The deployment's choices for plan selection (Stage 3): the cost model,
/// how fast a core works, and how long one-off queries are spread over.
/// Loadable from JSON (`asap_planner --cost-inputs <file>`);
/// an omitted field takes its default, an unknown one is refused.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct CostInputs {
    /// How candidates are compared; default cost model 1 with
    /// [`ResourceUnitCost::illustrative`].
    pub cost_model: CostModel,
    /// Time per operation and per scanned byte, for latency and cores;
    /// default `ResourceSpeed::illustrative`.
    pub resource_speed: ResourceSpeed,
    /// Seconds one-off, scheduled and unknown queries are spread over
    /// (`invocations / horizon` per second); default 3,600.
    pub non_repeating_query_horizon_seconds: f64,
    /// The deployment's measured CPU time per unit of each operator; a node
    /// with an entry is priced from it, every other node from the formulas
    /// scaled by the calibration factor (module doc, "Measured operation
    /// costs").
    /// Default empty: every node is priced from the formulas.
    pub measured_operation_costs: MeasuredOperationCosts,
    /// Externally established formula-to-measurement scale, held constant across
    /// candidates. Must be finite and positive. None fits the complete generated
    /// candidate catalog before pricing, preserving the default calibration policy.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub frozen_calibration_factor: Option<f64>,
    /// How the deployment's data plane runs work; default
    /// [`DataPlaneExecution::default`].
    #[serde(skip_serializing_if = "DataPlaneExecution::is_default")]
    pub data_plane_execution: DataPlaneExecution,
    /// Whether the times these inputs predict passed the deployment's
    /// calibration gate; cost model 3's `lowest_mean_latency` selects by
    /// latency only when they did, or when the deployment opts in to
    /// uncalibrated times for testing. Default not checked.
    #[serde(skip_serializing_if = "LatencyCalibration::is_not_checked")]
    pub latency_calibration: LatencyCalibration,
    /// How cost model 3's `lowest_mean_latency` solves: to a 1 % gap or
    /// 10,000 nodes by default, or exactly.
    #[serde(skip_serializing_if = "LatencySelection::is_default")]
    pub latency_selection: LatencySelection,
    /// The checking and measurement switches of `lowest_mean_latency`'s
    /// selection: not part of a cost inputs file (never serialized); the
    /// `asap_planner` tool and tests set them.
    #[serde(skip)]
    pub latency_selection_checks: LatencySelectionChecks,
}

/// How cost model 3's `lowest_mean_latency` solves
/// (`docs/design_docs/proposals/stage3-lowest-mean-latency.md`, decision
/// 27). In JSON an object with these fields; an omitted field takes its
/// default.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct LatencySelection {
    /// Solve the mean latency to optimality; default false: stop at a
    /// relative gap of 1 % or after 10,000 branch-and-bound nodes,
    /// whichever comes first, and report the gap and the lower bound
    /// beside the plan.
    pub exact_solve: bool,
}

impl LatencySelection {
    fn is_default(&self) -> bool {
        *self == Self::default()
    }
}

/// Switches that check `lowest_mean_latency`'s exact speed-ups (turning
/// one off must not change the optimum) and measure its solve (section
/// 2.5). Not serialized: tools and tests set them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LatencySelectionChecks {
    /// Leave out θ-families another family does as well as; default true.
    pub theta_dominance: bool,
    /// Leave out executions no plan with them beats a feasible plan with
    /// (cost model 1's); default true.
    pub feasible_plan_bound: bool,
    /// Run the tie-break solves only to find a smaller plan within 500
    /// nodes; default true (false: to optimality).
    pub lazy_tie_breaks: bool,
    /// Bound each query's reads from below along each path of its plan;
    /// default true (false: only the per-edge and per-execution rows).
    pub path_reads: bool,
    /// Solve the LP relaxation too and report its mean latency
    /// (`Selection::milp_root_lp_mean_latency_ms`); default false.
    pub report_root_lp_bound: bool,
    /// Print the solver's log and the model's size by family on standard
    /// error; default false.
    pub log_solver: bool,
    /// The most branch-and-bound nodes the default stop rule explores
    /// before it stops without a 1 % gap; default 10,000. A test lowers it
    /// to reach the retry from cost model 1's plan.
    pub node_limit: i32,
}

impl Default for LatencySelectionChecks {
    fn default() -> Self {
        Self {
            theta_dominance: true,
            feasible_plan_bound: true,
            lazy_tie_breaks: true,
            path_reads: true,
            report_root_lp_bound: false,
            log_solver: false,
            node_limit: 10_000,
        }
    }
}

/// Whether a deployment's predicted times passed its calibration gate
/// (`docs/design_docs/proposals/stage3-lowest-mean-latency.md`, section
/// 2.4): every node's time per phase within 1.5× of measured at every
/// series scale it runs. JSON: `"not_checked"`, `"passed_gate"` or
/// `"uncalibrated_for_testing"`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LatencyCalibration {
    /// Not checked against measurements: `lowest_mean_latency` refuses it.
    #[default]
    NotChecked,
    /// The calibration gate passed for these inputs.
    PassedGate,
    /// Uncalibrated, for testing: `lowest_mean_latency` selects with the
    /// times as predicted, which may rank plans by prediction errors.
    UncalibratedForTesting,
}

impl LatencyCalibration {
    fn is_not_checked(&self) -> bool {
        *self == Self::NotChecked
    }
}

impl Default for CostInputs {
    fn default() -> Self {
        Self {
            cost_model: CostModel::UsageOverTime(ResourceUnitCost::illustrative()),
            resource_speed: ResourceSpeed::illustrative(),
            non_repeating_query_horizon_seconds: DEFAULT_NON_REPEATING_QUERY_HORIZON_SECONDS,
            measured_operation_costs: MeasuredOperationCosts::default(),
            frozen_calibration_factor: None,
            data_plane_execution: DataPlaneExecution::default(),
            latency_calibration: LatencyCalibration::NotChecked,
            latency_selection: LatencySelection::default(),
            latency_selection_checks: LatencySelectionChecks::default(),
        }
    }
}

impl CostInputs {
    /// Bad input: a negative or non-finite speed, a raw batch width of 0, a
    /// horizon that is not positive, a bad measured table ([`MeasuredOperationCosts`]: a
    /// negative or non-finite time or size, an entry naming two families,
    /// a duplicate entry, or `ms_per_cpu_op` 0), or a price the chosen cost model reads that is negative or
    /// not finite, or every such price 0 (cost model 3 reads none).
    pub fn validate(&self) -> Result<(), AnalyticalCostError> {
        if let Some(factor) = self.frozen_calibration_factor {
            CalibrationFit::from_factor(factor)?;
        }
        self.data_plane_execution.validate()?;
        for (name, value) in [
            ("ms_per_cpu_op", self.resource_speed.ms_per_cpu_op),
            (
                "ms_per_scanned_byte",
                self.resource_speed.ms_per_scanned_byte,
            ),
        ] {
            if !value.is_finite() || value < 0.0 {
                return Err(AnalyticalCostError::InvalidCalibration(name, value));
            }
        }
        if self.resource_speed.raw_batch_width_ms == 0 {
            return Err(AnalyticalCostError::InvalidCalibration(
                "raw_batch_width_ms",
                0.0,
            ));
        }
        let horizon = self.non_repeating_query_horizon_seconds;
        if !horizon.is_finite() || horizon <= 0.0 {
            return Err(AnalyticalCostError::InvalidCalibration(
                "non_repeating_query_horizon_seconds",
                horizon,
            ));
        }
        crate::measured::validate(
            &self.measured_operation_costs,
            self.resource_speed.ms_per_cpu_op,
        )?;
        let read = match &self.cost_model {
            CostModel::UsageOverTime(p) => vec![
                ("cost_per_cpu_op", p.cost_per_cpu_op),
                ("cost_per_scanned_byte", p.cost_per_scanned_byte),
                ("cost_per_memory_byte_second", p.cost_per_memory_byte_second),
                ("cost_per_disk_byte_second", p.cost_per_disk_byte_second),
            ],
            CostModel::ProvisionedCapacity(p) => vec![
                (
                    "cost_per_provisioned_memory_byte_second",
                    p.cost_per_provisioned_memory_byte_second,
                ),
                (
                    "cost_per_provisioned_cpu_core_second",
                    p.cost_per_provisioned_cpu_core_second,
                ),
                ("cost_per_disk_byte_second", p.cost_per_disk_byte_second),
            ],
            CostModel::ClusterCapacity(_) => return Ok(()),
        };
        for &(name, value) in &read {
            if !value.is_finite() || value < 0.0 {
                return Err(AnalyticalCostError::InvalidCalibration(name, value));
            }
        }
        if read.iter().all(|(_, value)| *value == 0.0) {
            return Err(AnalyticalCostError::ZeroCalibration);
        }
        Ok(())
    }
}

/// The CPU time of the nodes priced from the measured table, per second of
/// wall time: as measured, and as the formulas would price them. Their
/// ratio is the calibration factor for the nodes without an entry.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct CalibrationFit {
    measured_ms_per_second: f64,
    formula_ms_per_second: f64,
}

impl CalibrationFit {
    /// A validated externally established scale, without fitting candidate data.
    pub fn from_factor(factor: f64) -> Result<Self, AnalyticalCostError> {
        if !factor.is_finite() || factor <= 0.0 {
            return Err(AnalyticalCostError::InvalidCalibration(
                "frozen_calibration_factor",
                factor,
            ));
        }
        Ok(Self {
            measured_ms_per_second: factor,
            formula_ms_per_second: 1.0,
        })
    }

    /// Record one measured node: its measured and formula CPU time per
    /// second of wall time.
    pub fn record(&mut self, measured_ms_per_second: f64, formula_ms_per_second: f64) {
        self.measured_ms_per_second += measured_ms_per_second;
        self.formula_ms_per_second += formula_ms_per_second;
    }

    /// Add `other`'s CPU time to this fit.
    pub fn add(&mut self, other: CalibrationFit) {
        self.measured_ms_per_second += other.measured_ms_per_second;
        self.formula_ms_per_second += other.formula_ms_per_second;
    }

    /// Measured ÷ formula time, 1 without measured nodes (or with only
    /// nodes the formulas price free).
    pub fn factor(self) -> f64 {
        match self.formula_ms_per_second > 0.0 && self.measured_ms_per_second > 0.0 {
            true => self.measured_ms_per_second / self.formula_ms_per_second,
            false => 1.0,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The AWS Fargate preset holds the page's prices converted as its doc
    /// says at the given speed: per vCPU-second × seconds per operation or
    /// per scanned byte (1e-9 and 1e-10 at the illustrative speed; twice
    /// that at half the speed), per GB-second ÷ 2^30, disk at the
    /// ephemeral-storage cost per GB-second ÷ 2^30; the provisioned
    /// costs 0. It validates under cost models 1 and 2 (under cost model 2
    /// it costs disk only).
    #[test]
    fn aws_fargate_us_east_1_converts_the_page_prices() {
        let preset = ResourceUnitCost::aws_fargate_us_east_1(ResourceSpeed::illustrative());
        let close = |actual: f64, expected: f64| {
            assert!(
                ((actual - expected) / expected).abs() < 1e-12,
                "{actual} != {expected}"
            )
        };
        close(preset.cost_per_cpu_op, 1.1244e-14);
        close(preset.cost_per_scanned_byte, 1.1244e-15);
        close(
            preset.cost_per_memory_byte_second,
            1.235e-6 / 1_073_741_824.0,
        );
        close(
            preset.cost_per_disk_byte_second,
            0.000_000_030_8 / 1_073_741_824.0,
        );
        assert_eq!(preset.cost_per_provisioned_memory_byte_second, 0.0);
        assert_eq!(preset.cost_per_provisioned_cpu_core_second, 0.0);
        // A core twice as slow takes twice the vCPU-seconds per operation and per byte.
        let slower = ResourceUnitCost::aws_fargate_us_east_1(ResourceSpeed {
            ms_per_cpu_op: 2e-6,
            ms_per_scanned_byte: 2e-7,
            ..ResourceSpeed::illustrative()
        });
        close(slower.cost_per_cpu_op, 2.2488e-14);
        close(slower.cost_per_scanned_byte, 2.2488e-15);
        close(
            slower.cost_per_memory_byte_second,
            preset.cost_per_memory_byte_second,
        );
        let settings = |cost_model| CostInputs {
            cost_model,
            ..CostInputs::default()
        };
        assert_eq!(
            settings(CostModel::UsageOverTime(preset.clone())).validate(),
            Ok(())
        );
        assert_eq!(
            settings(CostModel::ProvisionedCapacity(preset)).validate(),
            Ok(())
        );
    }

    /// Both presets cost disk (#1469): the illustrative preset at a fortieth
    /// of its memory cost per byte-second (block storage against memory);
    /// the Fargate preset at the page's ephemeral-storage cost,
    /// $0.0000000308 per GB-second ÷ 2^30 bytes per GB, the free 20 GB per
    /// task not expressed.
    #[test]
    fn presets_cost_disk() {
        let illustrative = ResourceUnitCost::illustrative();
        assert_eq!(
            illustrative.cost_per_disk_byte_second,
            illustrative.cost_per_memory_byte_second / 40.0
        );
        let fargate = ResourceUnitCost::aws_fargate_us_east_1(ResourceSpeed::illustrative());
        let expected = 0.000_000_030_8 / (1u64 << 30) as f64;
        assert!(
            ((fargate.cost_per_disk_byte_second - expected) / expected).abs() < 1e-12,
            "{} != {expected}",
            fargate.cost_per_disk_byte_second
        );
    }

    /// The demo deployment's inputs (#1469): cost model 3's lowest mean
    /// latency within 96 GiB of memory, 101,456,000,000 bytes of disk and
    /// 20 cores; view windows in files of 15 min, kept 60 s past their
    /// retention, under a 2 GiB cache of decoded windows.
    fn demo_inputs() -> serde_json::Value {
        serde_json::json!({
            "cost_model": {"cluster_capacity": {
                "memory_bytes": 103_079_215_104u64,
                "disk_bytes": 101_456_000_000u64,
                "cpu_cores": 20,
                "objective": "lowest_mean_latency"}},
            "data_plane_execution": {"view_window_cache": {
                "idle_ms": 60_000,
                "max_bytes": 2_147_483_648u64,
                "file_span_ms": 900_000,
                "retention_margin_ms": 60_000}},
            "latency_calibration": "uncalibrated_for_testing"
        })
    }

    /// The demo deployment's inputs load and validate.
    #[test]
    fn the_demo_inputs_load_and_validate() {
        let inputs: CostInputs =
            serde_json::from_value(demo_inputs()).expect("the demo's cost inputs load");
        assert_eq!(inputs.validate(), Ok(()));
    }

    /// A window cache that holds no bytes is refused.
    #[test]
    fn a_window_cache_of_zero_bytes_is_refused() {
        let mut json = demo_inputs();
        json["data_plane_execution"]["view_window_cache"]["max_bytes"] = serde_json::json!(0);
        let inputs: CostInputs = serde_json::from_value(json).expect("loads");
        assert!(inputs.validate().is_err());
    }

    /// Guard: a window cache of an idle time only, written before the new
    /// fields, loads and writes back as it was.
    #[test]
    fn a_window_cache_of_an_idle_time_only_still_loads() {
        let json =
            serde_json::json!({"data_plane_execution": {"view_window_cache": {"idle_ms": 60_000}}});
        let inputs: CostInputs = serde_json::from_value(json.clone()).expect("loads");
        assert_eq!(
            serde_json::to_value(&inputs).unwrap()["data_plane_execution"]["view_window_cache"],
            json["data_plane_execution"]["view_window_cache"]
        );
    }
}

//! #509 Stage 3, plan selection (MVP): the only stage that computes cost.
//!
//! Each CandidatePhysicalASAPDAG from Stage 2 is checked against every
//! query's accuracy target: the query's output meets it when its root
//! guarantee does (theoretical: Stage 1 composes it with the built-in
//! accuracy model, and this stage binds the statistics it left symbolic
//! from the data workload and the deployment's operand-domain contracts),
//! or when the deployment's empirical evidence shows this
//! candidate's output does (`AccuracyEvidenceProvider::output_guarantee`,
//! read only here). For a query with a target, Count-Min is admitted only
//! over weights proven non-negative; a query without one constrains nothing.
//! A miss rejects the candidate as invalid, with a reason; only this stage
//! discards valid candidates.
//! A candidate whose evaluation of some query takes longer than that
//! query's latency bound is rejected as invalid too ("Latency" below).
//! Every valid candidate's resource usage is measured node by node over its
//! physical DAG ([`ResourceUsage`]), and the deployment's [`CostModel`]
//! chooses among them ("Cost models" below). The rest are reported valid
//! but costlier (or slower, or larger).
//! Selection prices every candidate it is given; a search that avoids
//! building every candidate (e.g. a dynamic program over target nesting) is
//! deferred to #671.
//!
//! ## Cost model: cost per second of wall time
//!
//! A candidate's cost is what running it costs per second ([`COST_UNIT`]),
//! so queries with different recurrences add up. A node here is one
//! operator node of the candidate's physical plan DAG (`PhysicalASAPDAG`),
//! not a machine. Each node's price for one
//! computation is multiplied by how often it is computed per second
//! ([`NodeCost::cost_per_second`]):
//!
//! * A `NotMaterialized` node is recomputed for each consumer that reads it
//!   and is not stored: it is charged once per consumer
//!   ([`NodeCost::multiplier`]) at the sum of their rates. Its consumers are
//!   the queries whose sub-DAG reaches it and the materialized nodes above
//!   it, counted without looking through a materialized node: a node below
//!   one is computed when that node is. Stage 2 does not duplicate a shared
//!   node, so a shared `NotMaterialized` input costs what reading it
//!   separately does (#509 Example 4); it offers the node built once per
//!   evaluation (`MatAtQuery`) as another candidate.
//! * A `MatAtQuery` node is computed once for all the queries that reach
//!   it, at their combined rate: queries repeating at the same interval are
//!   evaluated together, so each distinct interval counts once.
//! * A `MatAtIngestion` node runs as data arrives: it is priced over one
//!   second of arriving rows, once per second, whatever its readers' rates.
//!   A tumbling-pane chain maintained this way updates only its newest
//!   pane: each arriving row is placed in it and updates it.
//! * A query repeating every `I` ms runs `1000 / I` times per second, and an
//!   estimated rate runs at that rate. One-off, scheduled and unknown
//!   queries are spread over the settings' `non_repeating_query_horizon_seconds`
//!   (default 3600 s): `invocations / horizon` per second.
//!
//! Under cost model 1, memory is charged only as state kept between
//! computations ([`ResourceUsage::average_memory_bytes`]), at
//! `cost_per_memory_byte_second` per byte per second. A `MatAtIngestion`
//! node keeps its state every second: twice for a whole window (the window
//! being built and the completed one queries read) and `N + 1` panes for a
//! chain of `N` (the `N` the query merges and the one being built). A
//! `MatAtQuery` node's state is reused only within one evaluation, so it is
//! held only while that evaluation runs: its bytes times the latency of the
//! longest query reading it, once per evaluation, which is close to nothing.
//! A `MatAtQueryKeptAcrossEvaluations` tumbling-pane chain (#509 Example
//! 4's B3, offered by Stage 2 when the deployment keeps query-time results,
//! [`crate::DeploymentCapabilities::query_time_results_retained`]) is kept across
//! evaluations: each evaluation scans one pane of raw data and builds the
//! newest pane, and the `N − 1` older panes, which complete the readers'
//! look-back, are kept every second (in memory, all `N` at the peak). When
//! Stage 2 offers both query-time options for a chain, plan selection
//! prices each and the cheaper wins.
//! Memory a computation
//! holds only while it runs is not charged under cost model 1: it counts in
//! [`ResourceUsage::peak_memory_bytes`], which cost models 2 and 3 read. A
//! computation is charged for its CPU operations and scanned bytes only.
//!
//! Prices come from [`crate::analytical_cost::estimate_operator`] over edge
//! statistics derived from the [`DataWorkload`] and a fixed default group
//! count; summary build and estimation are priced as rows × sketch depth and
//! groups × k. A query-time scan reads the longest window above it (a time
//! shift moves a window without widening it); a time shift is free, and a
//! time range is priced by the rows it keeps. A tumbling-pane chain is
//! priced as one build over its whole window plus one operation per row to
//! place it in its pane, and keeps one state per group and pane; a merge
//! costs one operation per state it reads. Kept state is sized by its bytes: a sketch by its parameters, a
//! fixed-size exact accumulator (sum, count, min, max, rate, ...) as one
//! value per group, and exact state that grows with the data (quantile,
//! top-k, distinct count, L2, entropy keep every value or item) as one value
//! per input row of its group, assuming rows spread evenly over groups and,
//! for distinct items, that every row may be distinct. These numbers are
//! illustrative, not calibrated: a binary, join or concat node is priced from
//! its first input only (MVP; follow-up).
//! A cost error on any candidate aborts selection; it is reachable only from
//! bad declared rates or bad [`PlanSelectionSettings`].
//!
//! ## Measured operation costs
//!
//! A deployment that measured its sketches gives the measured CPU time of
//! one update, merge or estimate per sketch configuration
//! ([`PlanSelectionSettings::measured_operation_costs`]). A sketch build
//! (one instance per group), merge or estimate with an entry is priced
//! from it: rows × measured updates (plus the one placement operation per
//! row of a pane chain), states × measured merges, groups × measured
//! estimates. The newest window's compaction (see "Latency") merges with
//! the state's measured merge too. Every other node is priced from the
//! formulas above, and each node reports which ([`NodeCost::price_source`]).
//! An entry matches only a node with exactly its sketch parameters (up to
//! the error and failure probability the sketch was sized for): a node whose
//! parameters differ, however slightly, is priced from the formulas, with no
//! interpolation between measured configurations. Measured times count as
//! CPU operations of [`ResourceSpeed`], so latency and all three cost
//! models read them like any other operation.
//!
//! ## Cost models
//!
//! [`PlanSelectionSettings::cost_model`] (in [`PlanningModels`]) chooses how
//! usage becomes a choice; the default is cost model 1 with
//! [`ResourceUnitCost::illustrative`]:
//!
//! * [`CostModel::UsageOverTime`] (cost model 1, pay-as-you-go): each
//!   node's CPU operations, scanned bytes and memory byte-seconds per second
//!   (and disk bytes, unpriced by default) times their prices; the cheapest
//!   candidate wins.
//! * [`CostModel::ProvisionedCapacity`] (cost model 2): the candidate's peak
//!   memory, peak CPU cores and disk times their prices per second; the
//!   cheapest wins. Peak memory assumes every evaluation overlaps: kept
//!   in-memory state plus each computation's working memory, once per
//!   consumer that recomputes it. Peak cores are the ingestion work's cores
//!   plus one per query evaluating at once.
//! * [`CostModel::ClusterCapacity`] (cost model 3, own cluster): no prices.
//!   A candidate over the cluster's memory, disk or cores is invalid, and
//!   [`ClusterObjective`] chooses among the rest (lowest worst-query
//!   latency, least peak memory, or the Pareto front of the three in
//!   [`Selection::front`]).
//!
//! Disk holds state materialized on `Medium::Disk` (Stage 2 places none
//! there yet) and the raw data query-time scans read: per source, the
//! longest window a scan reads. A deployment that does not retain raw data
//! ([`crate::DeploymentCapabilities::raw_data_retained`]) keeps no raw samples
//! after they arrive, so a candidate with a query-time scan is invalid,
//! with the window it would read as the reason.
//!
//! ## Latency
//!
//! Following the sketch-bench RQE cost model (§5, batch latency), a query's
//! latency per evaluation is its longest chain of work, each step on a
//! full core of elastic CPU (no queueing): the compaction of the newest
//! window of an ingestion-time state it reads, then its own query-time
//! work. Its own work is the sum, over every query-time node it reaches
//! (shared ones included, each once), of one computation's CPU operations
//! and scanned bytes timed by [`ResourceSpeed`]'s `ms_per_cpu_op` and
//! `ms_per_scanned_byte`. A state whose ingestion work needs `k` cores keeps
//! `k` partial copies of its newest window, and compacting them takes
//! `(k − 1)` merges for each of its states per window, each one operation,
//! or the state's measured merge time when the measured table has one; with
//! one core there is nothing to compact. A query with
//! `Some(bound)` whose latency is above the
//! bound makes the candidate invalid, with the latency as the reason.
//! [`ResourceUsage::latency_per_query`] reports every query's latency.
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::rc::Rc;

use asap_types::ir::operator_properties::{
    BinaryOpKind, PromQLVectorSetOpKind, RelationalSetOpKind,
};
use asap_types::ir::physical_export::compile_physical_asap_workload_with_node_ids;
use asap_types::ir::physical_export::{
    PhysicalASAPDAG, PhysicalASAPNodeId, PhysicalASAPOperatorPayload as Payload,
};
use asap_types::ir::{
    ASAPOp, BinaryOperator, NonASAPOp, Operator, OperatorNode, OperatorResultKind, QueryRoot,
    ScalarExpr, SchemaDerivationError, TimeRangeKind,
};
use asap_types::post_asap::sketch::KLL_MAX_K;
use asap_types::post_asap::{
    AccuracyGuarantee, AccuracyMetric, CompositionOperator, ExecutionTiming, FieldDataType,
    GroupingStrategy, Materialization, Medium, SketchAlgorithm, SketchParams, SketchStatistic,
    WeightDomain,
};
use asap_types::pre_asap::{AggIntent, DataType, Reduction, ScalarValue, Schema, Source};
use asap_types::types::{AccuracyScope, AccuracyTarget};
use asap_types::workload::{
    DataWorkload, MetricType, QueryRecurrence, QueryWorkloadEntry, RepeatedDemand,
};
use thiserror::Error;

use crate::accuracy::composition::{
    parse_samples_sign_statistic, parse_source_norm_statistic, DIVISION_OPERAND_DOMAINS,
    INPUT_ROW_COUNT, PRODUCT_OPERAND_DOMAINS, STREAM_L1_NORM,
};
use crate::accuracy::estimators::with_failure_probability;
use crate::accuracy::estimators::{hll, univmon};
use crate::accuracy::{OperandDomain, PropagationStats};
use crate::analytical_cost::{
    estimate_operator, AnalyticalCostError, PhysicalOperator, ResourceEstimate,
};
use crate::deployment_capabilities::DeploymentCapabilitiesError;
use crate::per_computation_candidates::{
    approximate_inputs, distinct_bound_source, operand_domain_statistic, operand_guarantee,
    operand_of, project_column_guarantee, root_guarantee, with_resized_summaries,
};
use crate::physical_operator_statistics::{
    EdgeStatistics, OperatorStatistics, PartitionStatistics, UnaryEdgeStatistics,
};
use crate::stage2_physical_asap_optimization::PhysicalCandidate;
use crate::PlanningModels;

mod measured_operation_costs;
use measured_operation_costs::measured_cpu_ns;
pub use measured_operation_costs::{MeasuredOperationCost, NodePriceSource, SummaryOperation};

/// Groups assumed for every `by (...)` reduction, absent group-count evidence.
const DEFAULT_GROUP_COUNT: u64 = 100;
/// Used only when the data workload does not declare them.
const DEFAULT_SERIES: u64 = 1_000;
const DEFAULT_ROWS_PER_SECOND: f64 = 1_000.0;
const DEFAULT_LOOKBACK_MS: u64 = 60_000;

/// Cost per second of wall time; one unit is one CPU-millisecond under
/// [`ResourceUnitCost::illustrative`].
pub const COST_UNIT: &str = "cost_per_second";
/// [`CandidateCost::source`] when every node is priced from the formulas.
pub const COST_SOURCE: &str = "analytical-cost-v2 (illustrative statistics)";
/// [`CandidateCost::source`] when every node is priced from the measured
/// table.
pub const MEASURED_COST_SOURCE: &str = "measured operation costs (illustrative statistics)";
/// [`CandidateCost::source`] when some nodes are priced from the measured
/// table and the others from the formulas.
pub const MIXED_COST_SOURCE: &str =
    "measured operation costs and analytical-cost-v2 (illustrative statistics)";

/// Seconds over which non-repeating queries (one-off, scheduled and
/// unknown) are spread by default: one hour.
pub const DEFAULT_NON_REPEATING_QUERY_HORIZON_SECONDS: f64 = 3_600.0;

/// What one candidate uses, per second of wall time unless the name says
/// otherwise, without prices: its nodes' CPU, scans, kept memory and disk
/// added up, with the candidate's peak cores and per-query latency. Every [`CostModel`] reads
/// it.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ResourceUsage {
    /// CPU operations per second: ingestion-time and query-time work.
    pub cpu_ops_per_second: f64,
    /// Bytes scanned per second.
    pub scanned_bytes_per_second: f64,
    /// Memory held between computations, averaged over time: retained
    /// byte-seconds per second (ingestion-time state always, query-time
    /// state while its evaluation runs).
    pub average_memory_bytes: f64,
    /// The most memory live at once, assuming every evaluation overlaps:
    /// in-memory kept state plus each computation's working memory, once
    /// per consumer that recomputes it.
    pub peak_memory_bytes: u64,
    /// Bytes kept on disk: state materialized on `Medium::Disk`, plus the
    /// raw data query-time scans read (the longest window of each source).
    pub disk_bytes: u64,
    /// CPU cores to provision for the peak: the ingestion work's cores plus
    /// one per query evaluating at once.
    pub peak_cpu_cores: u32,
    /// One per query, in workload order: its latency per evaluation.
    pub latency_per_query: Vec<QueryLatency>,
}

/// The price of one unit of each resource, per second of wall time. Cost
/// model 1 reads the usage prices (CPU, scan, memory byte-seconds, disk);
/// cost model 2 the provisioned prices (memory, CPU cores) and disk.
#[derive(Debug, Clone, PartialEq)]
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
    /// 1 ns of CPU per operation, 0.1 ns per scanned byte, 1 GB kept for a
    /// second costs 1/8 vCPU (125 CPU-ms), held or provisioned alike; one
    /// provisioned core-second is 1,000 CPU-ms. Disk is not priced (0).
    pub fn illustrative() -> Self {
        Self {
            cost_per_cpu_op: 1e-6,
            cost_per_scanned_byte: 1e-7,
            cost_per_memory_byte_second: 1.25e-7,
            cost_per_provisioned_memory_byte_second: 1.25e-7,
            cost_per_provisioned_cpu_core_second: 1_000.0,
            cost_per_disk_byte_second: 0.0,
        }
    }
}

/// How fast one core does work; the latency check reads it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ResourceSpeed {
    /// Milliseconds one CPU operation takes on one core.
    pub ms_per_cpu_op: f64,
    /// Milliseconds scanning one byte takes on one core.
    pub ms_per_scanned_byte: f64,
}

impl ResourceSpeed {
    /// 1 ns per operation and 0.1 ns per scanned byte, the times
    /// [`ResourceUnitCost::illustrative`] prices: one cost unit is one
    /// millisecond of one core.
    pub fn illustrative() -> Self {
        Self {
            ms_per_cpu_op: 1e-6,
            ms_per_scanned_byte: 1e-7,
        }
    }
}

/// How plan selection turns each valid candidate's [`ResourceUsage`] into a
/// choice. Chosen by the deployment ([`PlanSelectionSettings::cost_model`]).
#[derive(Debug, Clone, PartialEq)]
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

/// A fixed cluster's capacity (cost model 3) and how to choose within it.
#[derive(Debug, Clone, PartialEq)]
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
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClusterObjective {
    /// The smallest worst-query latency; ties by peak memory, then disk.
    LowestLatency,
    /// The smallest peak memory; ties by disk, then worst-query latency.
    LeastMemory,
    /// Report every candidate no other beats on peak memory, disk and
    /// worst-query latency at once ([`Selection::front`]); select the
    /// front's lowest-latency member.
    ParetoFront,
}

/// The deployment's choices for plan selection (Stage 3): the cost model,
/// how fast a core works, and how long one-off queries are spread over.
#[derive(Debug, Clone, PartialEq)]
pub struct PlanSelectionSettings {
    /// How candidates are compared; default cost model 1 with
    /// [`ResourceUnitCost::illustrative`].
    pub cost_model: CostModel,
    /// Time per operation and per scanned byte, for latency and cores;
    /// default [`ResourceSpeed::illustrative`].
    pub resource_speed: ResourceSpeed,
    /// Seconds one-off, scheduled and unknown queries are spread over
    /// (`invocations / horizon` per second); default 3,600.
    pub non_repeating_query_horizon_seconds: f64,
    /// The deployment's measured CPU time per sketch operation; a sketch
    /// node with an entry is priced from it, every other node from the
    /// formulas (module doc, "Measured operation costs"). Default empty:
    /// every node is priced from the formulas.
    pub measured_operation_costs: Vec<MeasuredOperationCost>,
}

impl Default for PlanSelectionSettings {
    fn default() -> Self {
        Self {
            cost_model: CostModel::UsageOverTime(ResourceUnitCost::illustrative()),
            resource_speed: ResourceSpeed::illustrative(),
            non_repeating_query_horizon_seconds: DEFAULT_NON_REPEATING_QUERY_HORIZON_SECONDS,
            measured_operation_costs: Vec::new(),
        }
    }
}

impl PlanSelectionSettings {
    /// Bad input: a negative or non-finite speed, a horizon that is not
    /// positive, a bad measured table ([`MeasuredOperationCost`]: a
    /// negative or non-finite time, a duplicate entry, or `ms_per_cpu_op`
    /// 0), or a price the chosen cost model reads that is negative or
    /// not finite, or every such price 0 (cost model 3 reads none).
    pub fn validate(&self) -> Result<(), AnalyticalCostError> {
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
        let horizon = self.non_repeating_query_horizon_seconds;
        if !horizon.is_finite() || horizon <= 0.0 {
            return Err(AnalyticalCostError::InvalidCalibration(
                "non_repeating_query_horizon_seconds",
                horizon,
            ));
        }
        measured_operation_costs::validate(
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

/// One node's price per second of wall time, with how it was measured. A
/// node is one operator node of the candidate's physical plan DAG
/// (`PhysicalASAPDAG`), keyed by its [`PhysicalASAPNodeId`] in
/// [`CandidateCost::per_node`]; not a machine.
#[derive(Debug, Clone, PartialEq)]
pub struct NodeCost {
    /// Its price per second under cost model 1; `None` under cost models 2
    /// and 3, which price (or limit) the whole candidate's peaks.
    pub cost_per_second: Option<f64>,
    /// The number of consumers it is computed for (1 when materialized,
    /// [`computations`]).
    pub multiplier: u64,
    /// How it was measured, e.g. "scan 1000 samples; x 1.0000/s; keeps
    /// ... bytes": one computation's work, the times it is computed per
    /// second and what it keeps.
    pub detail: String,
    /// Whether its CPU operations were priced from the measured table or
    /// from the formulas.
    pub price_source: NodePriceSource,
}

/// One candidate's usage and price over the whole workload: `usage` is
/// the whole candidate's, `per_node` one price per operator node of its
/// physical plan DAG.
#[derive(Debug, Clone, PartialEq)]
pub struct CandidateCost {
    pub usage: ResourceUsage,
    /// Its price per second under cost model 1 or 2 (under cost model 1, the
    /// sum of its nodes'); `None` under cost model 3.
    pub cost_per_second: Option<f64>,
    pub unit: &'static str,
    /// Where its prices came from, from its nodes' [`NodeCost::price_source`]:
    /// [`COST_SOURCE`] (all formulas), [`MEASURED_COST_SOURCE`] (all
    /// measured) or [`MIXED_COST_SOURCE`].
    pub source: &'static str,
    /// Each operator node of the candidate's physical plan DAG, by its id
    /// in that DAG, with its price.
    pub per_node: BTreeMap<PhysicalASAPNodeId, NodeCost>,
}

/// One query's latency per evaluation, its longest chain of work (module
/// doc, "Latency"): the compaction of the newest window of a state it reads
/// from ingestion time, then the query's own work, each on a full core.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct QueryLatency {
    /// Milliseconds to merge the ingest workers' partial copies of the
    /// newest window of the largest ingestion-time state the query reads;
    /// 0 when ingestion fits on one core or the query reads none.
    pub compaction_ms: f64,
    /// Milliseconds of the query-time work the query reaches, each node
    /// once.
    pub query_ms: f64,
}

impl QueryLatency {
    /// `compaction_ms + query_ms`: what is checked against the query's
    /// `max_latency_ms`.
    pub fn total_ms(&self) -> f64 {
        self.compaction_ms + self.query_ms
    }
}

/// The first query whose latency is over its bound, as a rejection reason.
fn latency_violation(cost: &CandidateCost, entries: &[QueryWorkloadEntry]) -> Option<String> {
    cost.usage
        .latency_per_query
        .iter()
        .zip(entries)
        .enumerate()
        .find_map(
            |(query, (latency, entry))| match entry.requirements.max_latency_ms {
                Some(bound) if latency.total_ms() > bound => Some(format!(
                    "q{}: latency {:.1} ms per evaluation (compaction {:.1} ms + query {:.1} ms) \
                 is over its {bound} ms bound",
                    query + 1,
                    latency.total_ms(),
                    latency.compaction_ms,
                    latency.query_ms
                )),
                _ => None,
            },
        )
}

/// Each entry's accuracy target, as the per-query targets Stage 3 checks.
pub(crate) fn accuracy_targets(entries: &[QueryWorkloadEntry]) -> Vec<AccuracyTarget> {
    entries
        .iter()
        .map(|entry| entry.requirements.accuracy.clone())
        .collect()
}

/// A candidate that was not selected: its id, why it lost
/// ([`RejectionCause`]) and a one-line reason with the numbers.
#[derive(Debug, Clone, PartialEq)]
pub struct Rejection {
    pub id: String,
    pub cause: RejectionCause,
    pub reason: String,
}

/// Why Stage 3 did not select a candidate. The first five mean the
/// candidate failed a check and is invalid ([`Self::failed_a_check`]); the
/// rest mean it was valid and lost under the cost model. JSON: the
/// snake_case variant name, e.g. `"costlier"`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RejectionCause {
    /// Its summaries, resized for the queries' failure probabilities, do
    /// not rebuild into a plan.
    FailureProbabilityResizeFailed,
    /// A query's accuracy target is not met: its output's guarantee misses
    /// it (or does not hold, as for a Count-Min sketch whose update weights
    /// are not proven non-negative) and no deployment evidence meets it.
    AccuracyTargetMissed,
    /// It scans raw data at query time in a deployment that does not
    /// retain raw data.
    RawDataNotRetained,
    /// A query's latency is over its `max_latency_ms` bound.
    LatencyBoundExceeded,
    /// Under cost model 3, its usage is over one of the cluster's limits.
    OverClusterCapacity,
    /// Under cost models 1 and 2, its price is higher than the selected
    /// candidate's.
    Costlier,
    /// Under cost model 3's `lowest-latency`, it is slower than the
    /// selected candidate.
    Slower,
    /// Under cost model 3's `least-memory`, it uses more memory than the
    /// selected candidate.
    MoreMemory,
    /// Under cost model 3's `pareto-front`, it is on the Pareto front but
    /// not its lowest-latency member, which is selected.
    NotLowestLatencyOnParetoFront,
    /// Under cost model 3's `pareto-front`, another candidate dominates it.
    Dominated,
    /// It ties with the selected candidate and comes later in enumeration
    /// order.
    TiedLostOnEnumerationOrder,
}

impl RejectionCause {
    /// Whether the candidate failed a check (accuracy, raw data, latency
    /// bound or cluster capacity) rather than losing under the cost model.
    pub fn failed_a_check(self) -> bool {
        matches!(
            self,
            Self::FailureProbabilityResizeFailed
                | Self::AccuracyTargetMissed
                | Self::RawDataNotRetained
                | Self::LatencyBoundExceeded
                | Self::OverClusterCapacity
        )
    }
}

/// The accuracy Stage 3 accepted for one query of a valid candidate: the
/// metric the query's target set (if any) and the metric of the guarantee
/// that met it. With no metric set, any metric meeting the target's ε, δ
/// and scope is accepted, so ε means different things across candidates
/// (e.g. 0.01 of the true value vs 0.01 of the rank); this record makes
/// the accepted metric visible.
#[derive(Debug, Clone, PartialEq)]
pub struct AcceptedAccuracy {
    /// The target's metric; `None` when the user did not set one.
    pub target_metric: Option<AccuracyMetric>,
    /// The metric the accepted guarantee is stated in; `None` for an exact
    /// output, which meets every metric.
    pub accepted_metric: Option<AccuracyMetric>,
    /// The sketch algorithms the query's output reads, in plan order
    /// (e.g. `["Kll"]`); empty for an exact plan.
    pub sketches: Vec<String>,
}

impl std::fmt::Display for AcceptedAccuracy {
    /// E.g. `metric not set; accepted rank (Kll)`, or
    /// `metric relative; accepted relative (DDSketch)`.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let name = |metric: AccuracyMetric| {
            serde_json::to_value(metric)
                .ok()
                .and_then(|value| value.as_str().map(str::to_owned))
                .unwrap_or_else(|| format!("{metric:?}"))
        };
        match self.target_metric {
            None => write!(f, "metric not set; ")?,
            Some(metric) => write!(f, "metric {}; ", name(metric))?,
        }
        match self.accepted_metric {
            None => write!(f, "accepted exact")?,
            Some(metric) => write!(f, "accepted {}", name(metric))?,
        }
        if !self.sketches.is_empty() {
            write!(f, " ({})", self.sketches.join(", "))?;
        }
        Ok(())
    }
}

/// Costs are present for valid candidates only.
#[derive(Debug, Clone, PartialEq)]
pub struct Selection {
    pub selected: String,
    /// The selected candidate's query roots with every weight requirement
    /// Stage 3 confirmed made a proof ([`with_confirmed_weights`]): the plan
    /// to run. The Stage 2 candidate keeps the open requirement.
    pub plan: Vec<QueryRoot>,
    pub costs: BTreeMap<String, CandidateCost>,
    /// For each valid candidate, per query, the accuracy Stage 3 accepted.
    pub accepted: BTreeMap<String, Vec<AcceptedAccuracy>>,
    pub rejected: Vec<Rejection>,
    /// Under [`ClusterObjective::ParetoFront`], the valid candidates no
    /// other beats on peak memory, disk and worst-query latency at once, in
    /// candidate order; empty under every other objective and cost model.
    pub front: Vec<String>,
}

#[derive(Debug, Error)]
pub enum SelectionError {
    /// Bad input: the deployment's plan selection settings
    /// ([`PlanSelectionSettings::validate`]).
    #[error("plan selection settings: {0}")]
    InvalidSettings(AnalyticalCostError),
    /// Bad input: the caller gave a different number of workload entries
    /// than the candidate has query roots.
    #[error("candidate {candidate} has {roots} roots but {entries} workload entries were given")]
    EntryCount {
        candidate: String,
        roots: usize,
        entries: usize,
    },
    /// Bad input: the deployment's capabilities contradict the data
    /// workload ([`crate::DeploymentCapabilities::validate`]).
    #[error("deployment capabilities: {0}")]
    InvalidDeploymentCapabilities(DeploymentCapabilitiesError),
    #[error("candidate {candidate}, node {node:?}: {error}")]
    Cost {
        candidate: String,
        node: PhysicalASAPNodeId,
        error: AnalyticalCostError,
    },
    #[error("no valid candidate: {0:?}")]
    NoValidCandidate(Vec<Rejection>),
}

/// Stage 3 (plan selection): reject candidates that miss a query's accuracy
/// target or latency bound (or, under cost model 3, the cluster's
/// capacity), measure the rest's resource usage per second and choose one
/// under the deployment's cost model (`models.plan_selection`): the
/// cheapest under cost models 1 and 2, the best by the cluster objective
/// under cost model 3 (the first on ties). `entries[i]` is the workload
/// entry of `candidate.roots[i]`: its accuracy target, its latency bound and
/// how often it is evaluated.
pub fn select_plan(
    cands: &[PhysicalCandidate],
    entries: &[QueryWorkloadEntry],
    data: &DataWorkload,
    models: PlanningModels<'_>,
) -> Result<Selection, SelectionError> {
    let settings = models.plan_selection;
    settings
        .validate()
        .map_err(SelectionError::InvalidSettings)?;
    models
        .deployment_capabilities
        .validate(data)
        .map_err(SelectionError::InvalidDeploymentCapabilities)?;
    let targets = &accuracy_targets(entries);
    let mut costs = BTreeMap::new();
    let mut accepted = BTreeMap::new();
    let mut rejected = Vec::new();
    // Valid candidates in candidate order: the index into `cands` and the
    // roots sized for their failure probabilities.
    let mut valid: Vec<(usize, Vec<QueryRoot>)> = Vec::new();
    let recurrence: Vec<QueryRecurrence> = entries.iter().map(|e| e.recurrence.clone()).collect();
    for (index, candidate) in cands.iter().enumerate() {
        if candidate.roots.len() != entries.len() {
            return Err(SelectionError::EntryCount {
                candidate: candidate.id.clone(),
                roots: candidate.roots.len(),
                entries: entries.len(),
            });
        }
        let candidate = match sized_for_failure_probability(candidate, targets, data) {
            Ok(sized) => sized,
            Err(reason) => {
                rejected.push(Rejection {
                    id: candidate.id.clone(),
                    cause: RejectionCause::FailureProbabilityResizeFailed,
                    reason,
                });
                continue;
            }
        };
        let accepted_accuracy = match accuracy_check(&candidate, targets, data, &models) {
            Ok(accepted_accuracy) => accepted_accuracy,
            Err(reason) => {
                rejected.push(Rejection {
                    id: candidate.id.clone(),
                    cause: RejectionCause::AccuracyTargetMissed,
                    reason,
                });
                continue;
            }
        };
        // Built once per candidate for the raw-data check and pricing.
        let nodes = node_map(&candidate.dag);
        let kept_chains = kept_pane_chains(&candidate.dag);
        if !models.deployment_capabilities.raw_data_retained {
            if let Some(reason) = raw_data_violation(&candidate.dag, &nodes, &kept_chains) {
                rejected.push(Rejection {
                    id: candidate.id.clone(),
                    cause: RejectionCause::RawDataNotRetained,
                    reason,
                });
                continue;
            }
        }
        let cost = price(
            &candidate.dag,
            &nodes,
            &kept_chains,
            &recurrence,
            data,
            settings,
        )
        .map_err(|(node, error)| SelectionError::Cost {
            candidate: candidate.id.clone(),
            node,
            error,
        })?;
        let violation = latency_violation(&cost, entries)
            .map(|reason| (RejectionCause::LatencyBoundExceeded, reason))
            .or_else(|| match &settings.cost_model {
                CostModel::ClusterCapacity(limits) => capacity_violation(&cost.usage, limits)
                    .map(|reason| (RejectionCause::OverClusterCapacity, reason)),
                _ => None,
            });
        if let Some((cause, reason)) = violation {
            rejected.push(Rejection {
                id: candidate.id.clone(),
                cause,
                reason,
            });
            continue;
        }
        costs.insert(candidate.id.clone(), cost);
        accepted.insert(candidate.id.clone(), accepted_accuracy);
        valid.push((index, candidate.roots));
    }
    let id = |index: usize| cands[index].id.as_str();
    let ranking = Ranking::of(&settings.cost_model);
    let valid_costs: Vec<&CandidateCost> = valid.iter().map(|(i, _)| &costs[id(*i)]).collect();
    let Some((best, front)) = choose(ranking, &valid_costs) else {
        return Err(SelectionError::NoValidCandidate(rejected));
    };
    let selected = id(valid[best].0).to_string();
    let front: Vec<String> = front
        .into_iter()
        .map(|f| id(valid[f].0).to_string())
        .collect();
    for (index, _) in &valid {
        let id = id(*index);
        if id != selected {
            let (cause, reason) = ranking.lost_reason(id, &selected, &costs, &front);
            rejected.push(Rejection {
                id: id.to_string(),
                cause,
                reason,
            });
        }
    }
    let plan = with_confirmed_weights(&valid.swap_remove(best).1, data);
    Ok(Selection {
        selected,
        plan,
        costs,
        accepted,
        rejected,
        front,
    })
}

/// The index of the candidate `ranking` selects among `costs` (the first on
/// ties) and, under [`Ranking::Front`], the indices of the Pareto front in
/// order; `None` without candidates.
fn choose(ranking: Ranking, costs: &[&CandidateCost]) -> Option<(usize, Vec<usize>)> {
    let front: Vec<usize> = match ranking {
        Ranking::Front => {
            // Each candidate's worst latency once, not once per comparison.
            let latencies: Vec<f64> = costs.iter().map(|c| worst_latency_ms(&c.usage)).collect();
            (0..costs.len())
                .filter(|&i| {
                    !(0..costs.len()).any(|j| {
                        dominates_at(
                            (&costs[j].usage, latencies[j]),
                            (&costs[i].usage, latencies[i]),
                        )
                    })
                })
                .collect()
        }
        _ => Vec::new(),
    };
    // Under the Pareto front, only a front member can be selected.
    let pool: Vec<usize> = match ranking {
        Ranking::Front => front.clone(),
        _ => (0..costs.len()).collect(),
    };
    let best =
        pool.into_iter().reduce(
            |best, next| match ranking.better(costs[next], costs[best]) {
                true => next,
                false => best,
            },
        )?;
    Some((best, front))
}

/// Whether `cost` is cheaper than `best`. Totals that differ only by
/// floating-point rounding (summing the same charges in another grouping)
/// tie, and a tie keeps the earlier candidate.
fn cheaper(cost: f64, best: f64) -> bool {
    cost < best - best.abs() * 1e-9
}

/// The worst latency per evaluation among a candidate's queries (0 with none).
fn worst_latency_ms(usage: &ResourceUsage) -> f64 {
    usage
        .latency_per_query
        .iter()
        .map(QueryLatency::total_ms)
        .fold(0.0, f64::max)
}

/// Under cost model 3, the first limit of the cluster `usage` is over, as a
/// rejection reason.
fn capacity_violation(usage: &ResourceUsage, limits: &ClusterLimits) -> Option<String> {
    if usage.peak_memory_bytes > limits.memory_bytes {
        return Some(format!(
            "peak memory {} bytes is over the cluster's {} bytes",
            usage.peak_memory_bytes, limits.memory_bytes
        ));
    }
    if usage.disk_bytes > limits.disk_bytes {
        return Some(format!(
            "disk {} bytes is over the cluster's {} bytes",
            usage.disk_bytes, limits.disk_bytes
        ));
    }
    match limits.cpu_cores {
        Some(cores) if usage.peak_cpu_cores > cores => Some(format!(
            "{} CPU cores at peak is over the cluster's {cores}",
            usage.peak_cpu_cores
        )),
        _ => None,
    }
}

/// Whether `a` is at least as good as `b` on peak memory, disk and
/// worst-query latency, and better on one of them.
fn dominates(a: &ResourceUsage, b: &ResourceUsage) -> bool {
    dominates_at((a, worst_latency_ms(a)), (b, worst_latency_ms(b)))
}

/// [`dominates`] with each usage's worst-query latency already computed.
fn dominates_at((a, la): (&ResourceUsage, f64), (b, lb): (&ResourceUsage, f64)) -> bool {
    a.peak_memory_bytes <= b.peak_memory_bytes
        && a.disk_bytes <= b.disk_bytes
        && !cheaper(lb, la)
        && (a.peak_memory_bytes < b.peak_memory_bytes
            || a.disk_bytes < b.disk_bytes
            || cheaper(la, lb))
}

/// How a cost model orders valid candidates.
#[derive(Clone, Copy)]
enum Ranking {
    /// Cost models 1 and 2: by price.
    Price,
    /// Latency, then peak memory, then disk.
    Latency,
    /// Peak memory, then disk, then latency.
    Memory,
    /// The Pareto front; the selected member by [`Ranking::Latency`].
    Front,
}

impl Ranking {
    fn of(model: &CostModel) -> Self {
        match model {
            CostModel::UsageOverTime(_) | CostModel::ProvisionedCapacity(_) => Ranking::Price,
            CostModel::ClusterCapacity(limits) => match limits.objective {
                ClusterObjective::LowestLatency => Ranking::Latency,
                ClusterObjective::LeastMemory => Ranking::Memory,
                ClusterObjective::ParetoFront => Ranking::Front,
            },
        }
    }

    /// Whether `cost` ranks strictly before `best`. Prices and latencies tie
    /// within rounding ([`cheaper`]); peak memory and disk are whole bytes,
    /// compared exactly, as [`dominates`] compares them.
    fn better(self, cost: &CandidateCost, best: &CandidateCost) -> bool {
        use std::cmp::Ordering;
        let rounded = |a: f64, b: f64| match (cheaper(a, b), cheaper(b, a)) {
            (true, _) => Ordering::Less,
            (_, true) => Ordering::Greater,
            _ => Ordering::Equal,
        };
        let (a, b) = (&cost.usage, &best.usage);
        let latency = || rounded(worst_latency_ms(a), worst_latency_ms(b));
        let memory = || a.peak_memory_bytes.cmp(&b.peak_memory_bytes);
        let disk = || a.disk_bytes.cmp(&b.disk_bytes);
        let order = match self {
            Ranking::Price => rounded(
                cost.cost_per_second.unwrap_or(f64::INFINITY),
                best.cost_per_second.unwrap_or(f64::INFINITY),
            ),
            Ranking::Latency | Ranking::Front => latency().then_with(memory).then_with(disk),
            Ranking::Memory => memory().then_with(disk).then_with(latency),
        };
        order == Ordering::Less
    }

    /// Why valid candidate `id` was not `selected`, and the reason with
    /// its numbers.
    fn lost_reason(
        self,
        id: &str,
        selected: &str,
        costs: &BTreeMap<String, CandidateCost>,
        front: &[String],
    ) -> (RejectionCause, String) {
        let (cost, best) = (&costs[id], &costs[selected]);
        if !self.better(best, cost) {
            let tied = match self {
                Ranking::Price => format!(
                    "{:.3} {COST_UNIT}",
                    cost.cost_per_second.unwrap_or_default()
                ),
                _ => describe_usage(&cost.usage),
            };
            return (
                RejectionCause::TiedLostOnEnumerationOrder,
                format!(
                    "tied with the selected candidate {selected} at {tied}; lost on enumeration order"
                ),
            );
        }
        match self {
            Ranking::Price => (
                RejectionCause::Costlier,
                format!(
                    "costlier: {:.3} vs {:.3} {COST_UNIT}",
                    cost.cost_per_second.unwrap_or_default(),
                    best.cost_per_second.unwrap_or_default()
                ),
            ),
            Ranking::Latency => (
                RejectionCause::Slower,
                format!(
                    "slower: {} vs {}",
                    describe_usage(&cost.usage),
                    describe_usage(&best.usage)
                ),
            ),
            Ranking::Memory => (
                RejectionCause::MoreMemory,
                format!(
                    "more memory: {} vs {}",
                    describe_usage(&cost.usage),
                    describe_usage(&best.usage)
                ),
            ),
            Ranking::Front if front.iter().any(|f| f == id) => (
                RejectionCause::NotLowestLatencyOnParetoFront,
                format!(
                    "on the Pareto front, not its lowest-latency member: {} vs {}",
                    describe_usage(&cost.usage),
                    describe_usage(&best.usage)
                ),
            ),
            Ranking::Front => {
                let by = front
                    .iter()
                    .find(|f| dominates(&costs[f.as_str()].usage, &cost.usage))
                    .map_or(selected, String::as_str);
                (
                    RejectionCause::Dominated,
                    format!(
                        "dominated by {by}: {} vs {}",
                        describe_usage(&cost.usage),
                        describe_usage(&costs[by].usage)
                    ),
                )
            }
        }
    }
}

/// E.g. `latency 126.0 ms, peak memory 9648000000 bytes, disk 480000000 bytes`.
fn describe_usage(usage: &ResourceUsage) -> String {
    format!(
        "latency {:.1} ms, peak memory {} bytes, disk {} bytes",
        worst_latency_ms(usage),
        usage.peak_memory_bytes,
        usage.disk_bytes
    )
}

/// A scan source's name: its PromQL metric or SQL table.
fn source_name(source: &Source) -> &str {
    match source {
        Source::TimeSeries { metric } => metric,
        Source::Table { table_ref } => table_ref,
    }
}

/// An upper bound on the rows one node and the operators below it see or
/// return per evaluation, in terms of the sources they read:
/// `copies_per_source[s]` times the series of `s`, plus `constant_rows`.
#[derive(Debug, Clone, Default, PartialEq)]
struct RowBound {
    copies_per_source: BTreeMap<String, u64>,
    constant_rows: u64,
}

impl RowBound {
    /// The bound of a node that returns at most as many rows as its largest
    /// input, source by source: a fold, a join, a filter.
    fn largest(inputs: &[RowBound]) -> Self {
        let mut bound = RowBound::default();
        for input in inputs {
            for (source, copies) in &input.copies_per_source {
                let entry = bound.copies_per_source.entry(source.clone()).or_default();
                *entry = (*entry).max(*copies);
            }
            bound.constant_rows = bound.constant_rows.max(input.constant_rows);
        }
        bound
    }

    /// The bound of a union, which returns every row of each input.
    fn sum(inputs: &[RowBound]) -> Self {
        let mut bound = RowBound::default();
        for input in inputs {
            for (source, copies) in &input.copies_per_source {
                *bound.copies_per_source.entry(source.clone()).or_default() += copies;
            }
            bound.constant_rows += input.constant_rows;
        }
        bound
    }
}

/// [`RowBound`] of `node`: a scan returns its source's series once; a union
/// (PromQL `or`, SQL `UNION`, `Concat`) the rows of all its branches; a
/// constant vector one row and `VALUES` its rows; any other operator at
/// most as many as its largest input. `memo` holds the shared nodes' bounds.
fn row_bound(
    node: &Rc<OperatorNode>,
    memo: &mut HashMap<*const OperatorNode, RowBound>,
) -> RowBound {
    if let Some(bound) = memo.get(&Rc::as_ptr(node)) {
        return bound.clone();
    }
    let inputs: Vec<RowBound> = node
        .children()
        .into_iter()
        .map(|child| row_bound(child, memo))
        .collect();
    let bound = match node.non_asap() {
        Some(NonASAPOp::Scan { source, .. }) => RowBound {
            copies_per_source: [(source_name(source).to_string(), 1)].into(),
            constant_rows: 0,
        },
        Some(NonASAPOp::Values { rows, .. }) => RowBound {
            constant_rows: rows.len() as u64,
            ..RowBound::largest(&inputs)
        },
        // One row out, but the operators below still read their sources'
        // rows, which the bound keeps; `at_most_one_row` makes `G` of a
        // `vector()` root 1.
        Some(NonASAPOp::PromqlVectorFromScalar(_)) => {
            let mut bound = RowBound::largest(&inputs);
            bound.constant_rows = bound.constant_rows.max(1);
            bound
        }
        Some(
            NonASAPOp::SetOp {
                kind: RelationalSetOpKind::Union,
                ..
            }
            | NonASAPOp::Concat { .. },
        ) => RowBound::sum(&inputs),
        Some(NonASAPOp::BinaryOp { operator, .. })
            if operator.kind == BinaryOpKind::Set(PromQLVectorSetOpKind::Or) =>
        {
            RowBound::sum(&inputs)
        }
        _ => RowBound::largest(&inputs),
    };
    memo.insert(Rc::as_ptr(node), bound.clone());
    bound
}

/// An upper bound on the rows any operator of `root` sees or returns in one
/// evaluation ([`row_bound`]), from the series count each source it reads
/// declares: the input row count of its folds, and `G` for a PromQL
/// instant vector ([`values_per_result`]). A join or fold of one source
/// counts it once; an `or` counts each branch. `None` when `root` reads no
/// source, or a source it reads declares no series count.
fn series_read(root: &QueryRoot, data: &DataWorkload) -> Option<u64> {
    let mut memo = HashMap::new();
    let inputs: Vec<RowBound> = root_operators(root)
        .into_iter()
        .map(|node| row_bound(node, &mut memo))
        .collect();
    let bound = RowBound::largest(&inputs);
    if bound.copies_per_source.is_empty() {
        return None;
    }
    let mut rows = bound.constant_rows;
    for (source, copies) in &bound.copies_per_source {
        rows += copies * data.sources.get(source)?.series_count?;
    }
    Some(rows)
}

/// How many values one evaluation of `root` returns at most (`G`): one for
/// a scalar root and for a root of at most one row ([`at_most_one_row`]),
/// provable from the query; otherwise `series_read` ([`series_read`]) for
/// a PromQL instant vector, which bounds the values it can return: each
/// branch of an `or` adds its own. `None` when unbound: `series_read` is,
/// any other SQL result (until a bound on the groups of its source is an
/// input), and a range vector.
fn values_per_result(root: &QueryRoot, series_read: Option<u64>) -> Option<u64> {
    match root {
        QueryRoot::Scalar(_) => Some(1),
        QueryRoot::Operator(node) if at_most_one_row(node) => Some(1),
        QueryRoot::Operator(node) if node.result_kind == OperatorResultKind::InstantVector => {
            series_read.map(|series| series.max(1))
        }
        QueryRoot::Operator(_) => None,
    }
}

/// Whether `node` returns at most one row whatever its input: a global
/// aggregate or summary (a reduction with no group keys), `vector(s)`, and
/// above one only operators that keep or drop its rows, or join two such
/// inputs without adding unmatched rows of both sides.
fn at_most_one_row(node: &OperatorNode) -> bool {
    let global = |reduction: &asap_types::pre_asap::Reduction| {
        reduction
            .group_keys()
            .is_some_and(|by| !by.is_without() && by.keys().is_empty())
    };
    match &node.operator {
        // A top-k or `count_values` returns several rows per group.
        Operator::NonASAP(NonASAPOp::Aggregate {
            reduction,
            measures,
            ..
        }) => {
            global(reduction)
                && !measures
                    .iter()
                    .any(|m| matches!(m, AggIntent::TopK { .. } | AggIntent::CountValues { .. }))
        }
        Operator::ASAP(ASAPOp::SummaryAgg { reduction, .. }) => global(reduction),
        // A merge gives one state per group, whatever the panes it merges.
        Operator::ASAP(ASAPOp::SummaryMerge { .. }) => matches!(
            node.summary_build(),
            Some(
                ASAPOp::SummaryAgg { reduction, .. } | ASAPOp::PaneChain { reduction, .. }
            ) if global(reduction)
        ),
        Operator::ASAP(ASAPOp::SummaryEstimate {
            summary_input,
            query,
        }) => !matches!(query, SketchStatistic::TopK { .. }) && at_most_one_row(summary_input),
        Operator::ASAP(ASAPOp::FinalizeExactAccumulator { child }) => at_most_one_row(child),
        // `vector(s)` turns one scalar into one row.
        Operator::NonASAP(NonASAPOp::PromqlVectorFromScalar(_)) => true,
        Operator::NonASAP(
            NonASAPOp::Project { child, .. }
            | NonASAPOp::Filter { child, .. }
            | NonASAPOp::Sort { child, .. }
            | NonASAPOp::Limit { child, .. }
            | NonASAPOp::Dedup { child, .. },
        ) => at_most_one_row(child),
        Operator::NonASAP(NonASAPOp::Join {
            kind, left, right, ..
        }) => {
            use asap_types::ir::operator_properties::JoinKind;
            match kind {
                JoinKind::Semi | JoinKind::Anti => at_most_one_row(left),
                JoinKind::Full => false,
                _ => at_most_one_row(left) && at_most_one_row(right),
            }
        }
        Operator::NonASAP(NonASAPOp::BinaryOp { lhs, rhs, .. }) => {
            at_most_one_row(lhs) && at_most_one_row(rhs)
        }
        _ => false,
    }
}

/// `candidate` with each summary's per-value failure probability enlarged
/// for its queries' targets (#669b), before pricing, so that cost reflects
/// the deeper sketch. A query with a target with failure probability `δ`,
/// over `n` approximate inputs ([`approximate_inputs`]) and `G` values per
/// evaluation ([`values_per_result`]; 1 for a per-value target), needs each
/// input to fail with probability at most `δ / (n · G)`: `n · G` such
/// failures union-bound to `δ` over the whole result. A summary several
/// queries read takes the smallest. The parameters that depend on it are
/// enlarged ([`with_failure_probability`]; classic HLL's precision from its
/// source's distinct bound), and the guarantees above are composed again.
/// A query whose `G` is unbound enlarges nothing: its whole-result target
/// stays unmet. Stage 1 sizes for ε and reads no data workload; only this
/// stage binds `G`. `Err` is the reason the candidate is rejected when its
/// resized summaries do not rebuild into a plan.
fn sized_for_failure_probability(
    candidate: &PhysicalCandidate,
    targets: &[AccuracyTarget],
    data: &DataWorkload,
) -> Result<PhysicalCandidate, String> {
    let resize_failed = |error: &dyn std::fmt::Display| {
        format!("resizing for the failure probability failed: {error}")
    };
    let roots = resize_for_failure_probability(&candidate.roots, targets, data)
        .map_err(|error| resize_failed(&error))?;
    if roots
        .iter()
        .zip(&candidate.roots)
        .all(|(sized, root)| sized == root)
    {
        return Ok(candidate.clone());
    }
    let mut dag = compile_physical_asap_workload_with_node_ids(&roots)
        .map_err(|error| resize_failed(&error))?
        .dag;
    // The DAG has the same nodes, so each keeps the placement Stage 2 gave it.
    if dag.nodes.len() == candidate.dag.nodes.len() {
        for (node, placed) in dag.nodes.iter_mut().zip(&candidate.dag.nodes) {
            if node.id == placed.id {
                node.output_state = placed.output_state;
                node.materialization = placed.materialization;
            }
        }
    }
    Ok(PhysicalCandidate {
        roots,
        dag,
        ..candidate.clone()
    })
}

/// `roots` with their summaries enlarged as [`sized_for_failure_probability`]
/// says: the plan plan selection prices and runs, for the logical roots of
/// the selected candidate too.
pub fn resize_for_failure_probability(
    roots: &[QueryRoot],
    targets: &[AccuracyTarget],
    data: &DataWorkload,
) -> Result<Vec<QueryRoot>, SchemaDerivationError> {
    let mut required: HashMap<*const OperatorNode, f64> = HashMap::new();
    for (root, target) in roots.iter().zip(targets) {
        let Some((target, delta)) = target
            .failure_probability
            .map(|delta| (target, delta))
            .filter(|(t, delta)| !t.is_exact() && *delta > 0.0)
        else {
            continue;
        };
        let values = match target.accuracy_scope {
            AccuracyScope::PerValue => Some(1),
            AccuracyScope::WholeResult => values_per_result(root, series_read(root, data)),
        };
        let Some(values) = values else { continue };
        let inputs = approximate_inputs(root);
        let share = delta / (inputs.len() as f64 * values as f64);
        for state in &inputs {
            let entry = required.entry(Rc::as_ptr(state)).or_insert(share);
            *entry = entry.min(share);
        }
    }
    if required.is_empty() {
        return Ok(roots.to_vec());
    }
    let resize = |state: &OperatorNode, params: &SketchParams| {
        let delta = *required.get(&std::ptr::from_ref(state))?;
        Some(match params {
            SketchParams::Hll {
                precision,
                relative_error,
            } => {
                let source = distinct_bound_source(state)?;
                let bound = data.sources.get(&source)?.max_distinct_per_evaluation?;
                // The guarantee keeps the relative error Pass 1 sized for.
                let relative_error = hll::classic_relative_error(*precision, *relative_error);
                SketchParams::Hll {
                    precision: hll::min_precision(relative_error, delta, bound, *precision),
                    relative_error: Some(relative_error),
                }
            }
            params => with_failure_probability(params, delta),
        })
    };
    with_resized_summaries(roots, &resize)
}

/// What Stage 3 accepted for each query ([`AcceptedAccuracy`]), or the
/// first query whose output is not shown to meet its target, as a reason. An output meets its target when its theoretical root guarantee
/// (composed in Stage 1 with the built-in model) satisfies it, or when the
/// deployment's empirical evidence shows that this candidate's output does
/// ([`crate::accuracy::AccuracyEvidenceProvider::output_guarantee`]). A
/// certified estimate inside the plan is not enough on its own: a difference
/// or ratio of certified estimates can be unbounded. The root guarantee's
/// symbolic statistics are bound first ([`bound_root_guarantee`]); one still
/// unknown leaves the target unmet. Count-Min is admitted only over update
/// weights proven non-negative, which its bound assumes: an open
/// [`WeightDomain::SamplesOfMetric`] requirement is met when `data` declares
/// the metric a counter ([`confirmed_weights`]), and unmet otherwise.
///
/// A target that sets no metric accepts a guarantee in any metric that meets
/// its ε, δ and scope ([`crate::accuracy::AccuracyModel::satisfies`]); one that sets a metric
/// accepts only guarantees in it (or convertible to it).
fn accuracy_check(
    candidate: &PhysicalCandidate,
    targets: &[AccuracyTarget],
    data: &DataWorkload,
    models: &PlanningModels<'_>,
) -> Result<Vec<AcceptedAccuracy>, String> {
    let mut accepted = Vec::with_capacity(targets.len());
    for (query, (root, target)) in candidate.roots.iter().zip(targets).enumerate() {
        let query = query + 1;
        let mut sketches: Vec<String> = Vec::new();
        for node in root_operators(root)
            .iter()
            .flat_map(|node| OperatorNode::reachable(node))
        {
            let Operator::ASAP(
                ASAPOp::SummaryAgg { family, input, .. } | ASAPOp::PaneChain { family, input, .. },
            ) = &node.operator
            else {
                continue;
            };
            if let FieldDataType::Sketch(kind, _) = family {
                let name = format!("{:?}", kind.algorithm());
                if !sketches.contains(&name) {
                    sketches.push(name);
                }
            }
            if matches!(family, FieldDataType::Sketch(kind, _)
                    if matches!(kind.algorithm(), SketchAlgorithm::Cms | SketchAlgorithm::CmsWithHeap))
                && !matches!(
                    confirmed_weights(&input.weight_domain, data),
                    WeightDomain::NonNegative { .. }
                )
            {
                let unconfirmed = match &input.weight_domain {
                    WeightDomain::SamplesOfMetric { metric } => {
                        format!(" (samples of `{metric}`, which is not declared a counter)")
                    }
                    _ => String::new(),
                };
                return Err(format!(
                    "q{query}: {} needs non-negative update weights, and these are not proven \
                     non-negative{unconfirmed}",
                    family_name(family)
                ));
            }
        }
        // A guarantee per value is stated over the whole result's `G`
        // values for a whole-result target.
        let series = series_read(root, data);
        let values = values_per_result(root, series);
        let over_scope = |g: AccuracyGuarantee| match target.accuracy_scope {
            AccuracyScope::WholeResult => g.over_whole_result(values),
            AccuracyScope::PerValue => g,
        };
        let guarantee = bound_root_guarantee(root, series, data, models).map(over_scope);
        let meets = |g: &AccuracyGuarantee| models.accuracy.satisfies(g, target);
        let met = guarantee.clone().filter(meets).or_else(|| {
            models
                .evidence
                .output_guarantee(root)
                .map(over_scope)
                .filter(meets)
        });
        if let Some(met) = met {
            accepted.push(AcceptedAccuracy {
                target_metric: target.accuracy_metric,
                accepted_metric: (!met.is_exact()).then_some(met.spec.accuracy_metric),
                sketches,
            });
            continue;
        }
        return Err(match guarantee {
            Some(g)
                if values.is_none()
                    && target.accuracy_scope == AccuracyScope::WholeResult
                    && target.failure_probability.is_some()
                    && g.spec.failure_probability.evaluate().is_none() =>
            {
                format!(
                    "q{query}: the output's guarantee holds for each value, and the number \
                     of values one evaluation returns is not bound (no declared series count \
                     for a source a PromQL vector reads; no bound on a SQL result's groups \
                     yet), \
                     so it does not meet whole-result target {target:?}, and the deployment \
                     has no evidence that it meets it"
                )
            }
            Some(g) => format!(
                "q{query}: the output guarantees a {:?} bound {:?}, failure probability {:?}, \
                 which misses target {target:?}, and the deployment has no evidence that it \
                 meets it",
                g.spec.accuracy_metric,
                g.spec.error.evaluate(),
                g.spec.failure_probability.evaluate(),
            ),
            None => format!(
                "q{query}: the output has no guarantee, and the deployment has no evidence \
                 that it meets target {target:?}"
            ),
        });
    }
    Ok(accepted)
}

/// `domain` with an open [`WeightDomain::SamplesOfMetric`] requirement
/// answered from `data`'s declared metric types: proven when the metric is
/// declared a counter, unknown otherwise. No type is inferred from a name.
pub fn confirmed_weights(domain: &WeightDomain, data: &DataWorkload) -> WeightDomain {
    domain.confirmed(|metric| declared_counter(data, metric))
}

/// Whether `data` declares `metric` a counter. A declared type does not
/// expire, so its evidence is not checked.
fn declared_counter(data: &DataWorkload, metric: &str) -> bool {
    data.sources
        .get(metric)
        .is_some_and(|facts| facts.metric_type == Some(MetricType::Counter))
}

/// `roots` with every open [`WeightDomain::SamplesOfMetric`] requirement
/// replaced by its answer from `data` ([`confirmed_weights`]): the form of a
/// selected plan the executor runs, which reads no data workload. The same
/// `Rc` where nothing changes.
pub fn with_confirmed_weights(roots: &[QueryRoot], data: &DataWorkload) -> Vec<QueryRoot> {
    fn confirm(
        node: &Rc<OperatorNode>,
        data: &DataWorkload,
        memo: &mut HashMap<*const OperatorNode, Rc<OperatorNode>>,
    ) -> Rc<OperatorNode> {
        if let Some(done) = memo.get(&Rc::as_ptr(node)) {
            return Rc::clone(done);
        }
        let mut changed = false;
        let mut operator = node.operator.map_children(|child| {
            let rebuilt = confirm(child, data, memo);
            changed |= !Rc::ptr_eq(&rebuilt, child);
            rebuilt
        });
        if let Operator::ASAP(ASAPOp::SummaryAgg { input, .. } | ASAPOp::PaneChain { input, .. }) =
            &mut operator
        {
            let confirmed = confirmed_weights(&input.weight_domain, data);
            if confirmed != input.weight_domain {
                input.weight_domain = confirmed;
                changed = true;
            }
        }
        let rebuilt = if changed {
            Rc::new(
                OperatorNode::with_schema(operator, node.schema.clone())
                    .with_guarantee(node.guarantee.clone())
                    .with_timing(node.timing)
                    .with_materialization(node.materialization),
            )
        } else {
            Rc::clone(node)
        };
        memo.insert(Rc::as_ptr(node), Rc::clone(&rebuilt));
        rebuilt
    }
    let mut memo = HashMap::new();
    roots
        .iter()
        .map(|root| match root {
            QueryRoot::Operator(n) => QueryRoot::Operator(confirm(n, data, &mut memo)),
            QueryRoot::Scalar(expr) => {
                QueryRoot::Scalar(expr.map_operator_refs(&mut |n| confirm(n, data, &mut memo)))
            }
        })
        .collect()
}

/// The operator nodes a query root reads.
fn root_operators(root: &QueryRoot) -> Vec<&Rc<OperatorNode>> {
    match root {
        QueryRoot::Operator(node) => vec![node],
        QueryRoot::Scalar(expr) => expr.operator_refs(),
    }
}

/// [`root_guarantee`] with the statistics Stage 1 left symbolic bound:
/// - the input row count, `series` ([`series_read`] of `root`);
/// - each source's stream norm ([`source_norm_statistic`]), from the data
///   workload's per-source `stream_norms`;
/// - classic HLL's failure probability and the chance a UnivMon's top layer
///   is incomplete, from the data workload's per-source
///   `max_distinct_per_evaluation`;
/// - a product's or quotient's bound, from the deployment's enforced
///   operand domains ([`operand_domain_bounds`]).
///
/// Every data-workload fact is declared: the caller drops facts that no
/// longer hold before planning. Any other statistic stays unknown, and an
/// unknown statistic never meets a target.
fn bound_root_guarantee(
    root: &QueryRoot,
    series: Option<u64>,
    data: &DataWorkload,
    models: &PlanningModels<'_>,
) -> Option<AccuracyGuarantee> {
    let guarantee = root_guarantee(root)?;
    let domains = operand_domain_bounds(root, series, data, models);
    Some(bind_statistics(&guarantee, &|statistic| {
        workload_statistic(statistic, series, data)
            .or_else(|| domains.get(statistic).copied().flatten())
    }))
}

/// `guarantee` with the statistics `value` supplies bound. A sign gate
/// ([`samples_sign_statistic`](crate::accuracy::composition::samples_sign_statistic))
/// is an indicator that only 1 (proven) opens: any other value leaves it
/// unbound, so it can never read as a perfect bound.
fn bind_statistics(
    guarantee: &AccuracyGuarantee,
    value: &dyn Fn(&str) -> Option<f64>,
) -> AccuracyGuarantee {
    guarantee.bind(&|statistic| {
        let supplied = value(statistic);
        match parse_samples_sign_statistic(statistic) {
            Some(_) => supplied.filter(|v| *v == 1.0),
            None => supplied,
        }
    })
}

/// A data-workload statistic Stage 1 left symbolic: the input row count
/// (`series`, the series of the sources the query reads), one source's
/// stream norm, or classic HLL's failure probability or a UnivMon's
/// incomplete-top-layer probability given one source's distinct bound,
/// from source facts that hold at any planning time, or the sign of one
/// metric's samples, from its declared type.
fn workload_statistic(statistic: &str, series: Option<u64>, data: &DataWorkload) -> Option<f64> {
    // The sign of one metric's samples: proven (1) when the metric is
    // declared a counter; no type is inferred from its name.
    if let Some(metric) = parse_samples_sign_statistic(statistic) {
        return declared_counter(data, metric).then_some(1.0);
    }
    if statistic == INPUT_ROW_COUNT {
        return series.map(|rows| rows as f64);
    }
    if let Some((precision, relative_error, source)) =
        hll::parse_classic_failure_statistic(statistic)
    {
        let bound = data.sources.get(source)?.max_distinct_per_evaluation?;
        return Some(hll::classic_failure_probability(
            precision,
            relative_error,
            bound,
        ));
    }
    if let Some((heap_size, layers, source)) =
        univmon::parse_incomplete_top_layer_statistic(statistic)
    {
        let bound = data.sources.get(source)?.max_distinct_per_evaluation?;
        return Some(univmon::incomplete_top_layer_probability(
            heap_size, layers, bound,
        ));
    }
    let (norm, source) = parse_source_norm_statistic(statistic)?;
    let norms = data.sources.get(source)?.stream_norms?;
    match norm {
        STREAM_L1_NORM => norms.l1,
        _ => norms.l2,
    }
}

/// One operand of a product or quotient, as Stage 3 finds its domain.
enum DomainOperand<'a> {
    /// A planned operator: the deployment's `OperandDomain` for it.
    Node(&'a Rc<OperatorNode>),
    /// A literal: itself.
    Literal(f64),
    /// Anything else: no domain.
    Other,
}

impl<'a> DomainOperand<'a> {
    fn of(expr: &'a ScalarExpr) -> Self {
        match expr {
            ScalarExpr::PromqlScalarFromVector(node) | ScalarExpr::ScalarSubquery(node) => {
                Self::Node(node)
            }
            ScalarExpr::Literal(ScalarValue::Float64(value)) => Self::Literal(*value),
            ScalarExpr::Literal(ScalarValue::Int64(value)) => Self::Literal(*value as f64),
            _ => Self::Other,
        }
    }

    fn domain(&self, models: &PlanningModels<'_>) -> Option<OperandDomain> {
        match self {
            Self::Node(node) => models.evidence.operand_domain(node),
            Self::Literal(value) => Some(OperandDomain {
                lower: *value,
                upper: *value,
                max_samples: 1,
                contract: "literal".into(),
            }),
            Self::Other => None,
        }
    }
}

/// A product or quotient whose bound Stage 1 left symbolic on its operands'
/// domains ([`operand_domain_statistic`]).
struct DomainOperation<'a> {
    statistic: &'static str,
    operands: [(DomainOperand<'a>, AccuracyGuarantee); 2],
}

/// Each operand-domain statistic in `root`, bound to the largest bound
/// among the operations that leave it symbolic, or `None` (unbound) when
/// any of them has no bound. Operations are found where the bound
/// originates (the same rule Stage 1 uses), in vector arithmetic, a scalar
/// root, and projection expressions. `series` is the root's input row
/// count ([`series_read`]).
fn operand_domain_bounds(
    root: &QueryRoot,
    series: Option<u64>,
    data: &DataWorkload,
    models: &PlanningModels<'_>,
) -> HashMap<&'static str, Option<f64>> {
    let mut operations = Vec::new();
    if let QueryRoot::Scalar(expr) = root {
        scalar_domain_operations(expr, &|_| None, &mut operations);
    }
    let nodes: Vec<Rc<OperatorNode>> = root_operators(root)
        .iter()
        .flat_map(|node| OperatorNode::reachable(node))
        .collect();
    for node in &nodes {
        match node.non_asap() {
            Some(NonASAPOp::BinaryOp {
                operator:
                    BinaryOperator {
                        kind: BinaryOpKind::Arithmetic(op),
                        ..
                    },
                lhs,
                rhs,
                ..
            }) => {
                let (Some(l), Some(r)) = (operand_guarantee(lhs), operand_guarantee(rhs)) else {
                    continue;
                };
                if let Some(statistic) = operand_domain_statistic(op, &l, &r) {
                    operations.push(DomainOperation {
                        statistic,
                        operands: [(DomainOperand::Node(lhs), l), (DomainOperand::Node(rhs), r)],
                    });
                }
            }
            Some(NonASAPOp::Project { cols, child, .. }) => {
                for item in cols {
                    scalar_domain_operations(
                        &item.expr,
                        &|column| project_column_guarantee(child, column),
                        &mut operations,
                    );
                }
            }
            _ => {}
        }
    }
    let mut bounds: HashMap<&'static str, Option<f64>> = HashMap::new();
    for operation in &operations {
        let bound = domain_bound(operation, series, data, models);
        let entry = bounds.entry(operation.statistic).or_insert(Some(0.0));
        *entry = entry.zip(bound).map(|(a, b)| a.max(b));
    }
    bounds
}

/// The operations in `expr` (and its sub-expressions) whose bound Stage 1
/// left symbolic on operand domains.
fn scalar_domain_operations<'a>(
    expr: &'a ScalarExpr,
    column: &dyn Fn(usize) -> Option<AccuracyGuarantee>,
    operations: &mut Vec<DomainOperation<'a>>,
) {
    if let ScalarExpr::Arithmetic {
        op, left, right, ..
    } = expr
    {
        if let (Some(l), Some(r)) = (operand_of(left, column), operand_of(right, column)) {
            if let Some(statistic) = operand_domain_statistic(op, &l, &r) {
                operations.push(DomainOperation {
                    statistic,
                    operands: [(DomainOperand::of(left), l), (DomainOperand::of(right), r)],
                });
            }
        }
    }
    for child in expr.children() {
        scalar_domain_operations(child, column, operations);
    }
}

/// The bound of one product or quotient from its operands' domains:
/// - a ratio of relative errors: the accuracy model's division rule;
/// - a product: `|x|·e_y + |y|·e_x + e_x·e_y`;
/// - a quotient: `(|x|·e_y + |y|·e_x) / (m·(m − e_y))` with `m` the least
///   `|y|`, which must exceed `e_y` (the denominator's domain is bounded
///   away from zero).
///
/// `|x|`, `|y|` are the domains' largest magnitudes, and `e` each
/// operand's absolute error (a relative or cardinality bound times that
/// magnitude) after binding its data-workload statistics (`series`, the
/// root's input row count). `None` when an operand has no domain or an
/// unbound error.
fn domain_bound(
    operation: &DomainOperation<'_>,
    series: Option<u64>,
    data: &DataWorkload,
    models: &PlanningModels<'_>,
) -> Option<f64> {
    let [(x, gx), (y, gy)] = &operation.operands;
    let (dx, dy) = (x.domain(models)?, y.domain(models)?);
    let bind =
        |g: &AccuracyGuarantee| g.bind(&|statistic| workload_statistic(statistic, series, data));
    let (gx, gy) = (bind(gx), bind(gy));
    if operation.statistic == DIVISION_OPERAND_DOMAINS {
        let stats = PropagationStats {
            division_operand_domains: Some([dx, dy]),
            ..PropagationStats::default()
        };
        return models
            .accuracy
            .propagate(&CompositionOperator::ExactDivision, &[gx, gy], None, &stats)
            .ok()?
            .spec
            .error
            .evaluate();
    }
    let valid =
        |d: &OperandDomain| d.lower.is_finite() && d.upper.is_finite() && d.lower <= d.upper;
    if !(valid(&dx) && valid(&dy)) {
        return None;
    }
    let magnitude = |d: &OperandDomain| d.lower.abs().max(d.upper.abs());
    let (mx, my) = (magnitude(&dx), magnitude(&dy));
    let error = |g: &AccuracyGuarantee, m: f64| {
        let bound = g.spec.error.evaluate()?;
        match g.spec.accuracy_metric {
            _ if g.is_exact() => Some(0.0),
            AccuracyMetric::ABSOLUTE => Some(bound),
            AccuracyMetric::Relative => Some(bound * m),
            _ => None,
        }
    };
    let (ex, ey) = (error(&gx, mx)?, error(&gy, my)?);
    let bound = if operation.statistic == PRODUCT_OPERAND_DOMAINS {
        mx * ey + my * ex + ex * ey
    } else {
        let least = if dy.lower > 0.0 {
            dy.lower
        } else if dy.upper < 0.0 {
            -dy.upper
        } else {
            return None;
        };
        if least <= ey {
            return None;
        }
        (mx * ey + my * ex) / (least * (least - ey))
    };
    bound.is_finite().then_some(bound)
}

fn family_name(family: &FieldDataType) -> String {
    match family {
        FieldDataType::Sketch(kind, _) => format!("{:?}", kind.algorithm()),
        FieldDataType::ExactAggregate(kind, _) => format!("exact {kind:?} accumulator"),
        other => format!("{other:?}"),
    }
}

/// The series pricing assumes a node reading `sources` sees: the sum of
/// their declared series counts, [`DEFAULT_SERIES`] for each source
/// without one and for a node that reads none.
fn priced_series(sources: &BTreeSet<&str>, data: &DataWorkload) -> u64 {
    if sources.is_empty() {
        return DEFAULT_SERIES;
    }
    sources
        .iter()
        .map(|source| {
            data.sources
                .get(*source)
                .and_then(|facts| facts.series_count)
                .unwrap_or(DEFAULT_SERIES)
        })
        .sum::<u64>()
        .max(1)
}

/// Evaluations per second of a node computed once for `queries` together.
/// Repeating queries with equal intervals are evaluated together, so each
/// interval counts once; estimated rates add. One-off, scheduled and unknown
/// queries run as one batch: the most invocations among them, spread over
/// `non_repeating_query_horizon_seconds`.
fn evaluation_rate<'r>(
    queries: impl IntoIterator<Item = &'r QueryRecurrence>,
    non_repeating_query_horizon_seconds: f64,
) -> Result<f64, AnalyticalCostError> {
    let mut intervals = BTreeSet::new();
    let mut estimated = 0.0;
    let mut invocations = 0u64;
    for recurrence in queries {
        match recurrence {
            QueryRecurrence::Repeated(
                RepeatedDemand::FixedInterval(interval)
                | RepeatedDemand::FixedIntervalAt { interval, .. },
            ) => {
                if interval.0 == 0 {
                    return Err(AnalyticalCostError::InvalidRecurrence);
                }
                intervals.insert(interval.0);
            }
            QueryRecurrence::Repeated(RepeatedDemand::EstimatedRate(estimate)) => {
                let rate = estimate.expected_rate.0;
                if !rate.is_finite() || rate < 0.0 {
                    return Err(AnalyticalCostError::InvalidRecurrence);
                }
                estimated += rate;
            }
            QueryRecurrence::Repeated(RepeatedDemand::Scheduled(times)) => {
                invocations = invocations.max(times.len() as u64);
            }
            QueryRecurrence::OneTime {
                invocations: count, ..
            } => invocations = invocations.max(*count),
            QueryRecurrence::Unknown => invocations = invocations.max(1),
        }
    }
    let repeated: f64 = intervals.into_iter().map(|ms| 1_000.0 / ms as f64).sum();
    Ok(repeated + estimated + invocations as f64 / non_repeating_query_horizon_seconds)
}

/// The nodes each query reads first, in query order: query `i` is the scalar
/// root keyed `i` when there is one, else the next operator root.
fn query_starts(dag: &PhysicalASAPDAG) -> Vec<Vec<PhysicalASAPNodeId>> {
    let mut roots = dag.roots.iter();
    (0..dag.roots.len() + dag.scalar_roots.len())
        .map(|query| {
            match dag
                .scalar_roots
                .iter()
                .find(|(position, _)| *position == query)
            {
                Some((_, expr)) => expr.operator_refs().into_iter().copied().collect(),
                None => roots.next().into_iter().copied().collect(),
            }
        })
        .collect()
}

/// How often each node of `dag` is computed: `(multiplier, per_second)`
/// (module doc, "Cost model"). A materialized node is computed once for all
/// its readers: once per second at ingestion time (`MatAtIngestion`), or at
/// the combined rate of the queries reaching it (`MatAtQuery`,
/// [`evaluation_rate`]). A `NotMaterialized` node is computed once per
/// consumer that reaches it, at that consumer's rate. A consumer is a query
/// (operator or scalar root), at its own rate, or a materialized node, at
/// that node's rate; a walk stops at the first materialized node it meets,
/// and a node read several times within one consumer's walk counts once for
/// it. Its `multiplier` is that number of consumers.
fn computations(
    dag: &PhysicalASAPDAG,
    recurrence: &[QueryRecurrence],
    non_repeating_query_horizon_seconds: f64,
) -> Result<HashMap<PhysicalASAPNodeId, (u64, f64)>, AnalyticalCostError> {
    let materialized: HashMap<_, _> = dag
        .nodes
        .iter()
        .filter(|n| n.materialization != Materialization::NotMaterialized)
        .map(|n| (n.id, n.materialization))
        .collect();
    let inputs = |id: PhysicalASAPNodeId| {
        dag.edges
            .iter()
            .filter(move |e| e.consumer == id)
            .map(|e| e.producer)
    };
    let starts = query_starts(dag);
    // The queries reaching each node, through any node.
    let mut reached: HashMap<PhysicalASAPNodeId, Vec<usize>> = HashMap::new();
    for (query, start) in starts.iter().enumerate() {
        let mut stack = start.clone();
        while let Some(id) = stack.pop() {
            let queries = reached.entry(id).or_default();
            if queries.last() == Some(&query) {
                continue;
            }
            queries.push(query);
            stack.extend(inputs(id));
        }
    }
    let mut rate_of_materialized = HashMap::new();
    for (&id, materialization) in &materialized {
        let rate = match materialization {
            Materialization::MatAtIngestion(_) => 1.0,
            _ => evaluation_rate(
                reached
                    .get(&id)
                    .into_iter()
                    .flatten()
                    .filter_map(|&q| recurrence.get(q)),
                non_repeating_query_horizon_seconds,
            )?,
        };
        rate_of_materialized.insert(id, rate);
    }
    // Each consumer's starting nodes and its rate.
    let mut consumers = Vec::new();
    for (query, start) in starts.into_iter().enumerate() {
        let rate = evaluation_rate(recurrence.get(query), non_repeating_query_horizon_seconds)?;
        consumers.push((start, rate));
    }
    for (&id, &rate) in &rate_of_materialized {
        consumers.push((inputs(id).collect(), rate));
    }
    let mut count: HashMap<_, (u64, f64)> = HashMap::new();
    for (start, rate) in consumers {
        let mut seen = HashSet::new();
        let mut stack: Vec<PhysicalASAPNodeId> = start;
        while let Some(id) = stack.pop() {
            if materialized.contains_key(&id) || !seen.insert(id) {
                continue;
            }
            let entry = count.entry(id).or_default();
            entry.0 += 1;
            entry.1 += rate;
            stack.extend(inputs(id));
        }
    }
    Ok(dag
        .nodes
        .iter()
        .map(|n| {
            let times = match rate_of_materialized.get(&n.id) {
                Some(&rate) => (1, rate),
                None => count.get(&n.id).copied().unwrap_or((0, 0.0)),
            };
            (n.id, times)
        })
        .collect())
}

/// Measure every node of `dag` per second of wall time (module doc, "Cost
/// model") and price it under `settings.cost_model`. Nodes are exported
/// children first, so each node's input statistics are known when it is
/// reached. A query-time node is measured per computation over the rows its
/// time ranges keep, times how often it is computed per second
/// ([`computations`]); an ingestion-time node over one second of arriving
/// rows. A materialized node's kept state is counted in byte-seconds per
/// second. `nodes` and `kept_chains` are `dag`'s ([`node_map`],
/// [`kept_pane_chains`]).
fn price(
    dag: &PhysicalASAPDAG,
    nodes: &NodeMap<'_>,
    kept_chains: &HashSet<PhysicalASAPNodeId>,
    recurrence: &[QueryRecurrence],
    data: &DataWorkload,
    settings: &PlanSelectionSettings,
) -> Result<CandidateCost, (PhysicalASAPNodeId, AnalyticalCostError)> {
    let first = dag.nodes.first().map(|n| n.id).unwrap_or_default();
    let speed = settings.resource_speed;
    // Cost model 1 prices each node as it is measured.
    let usage_prices = match &settings.cost_model {
        CostModel::UsageOverTime(prices) => Some(prices),
        _ => None,
    };
    let rows_per_ms = data
        .arrival
        .rows_per_second()
        .unwrap_or(DEFAULT_ROWS_PER_SECOND)
        / 1_000.0;
    // The sources each node reads, filled children first.
    let mut sources_read_by: HashMap<PhysicalASAPNodeId, BTreeSet<&str>> = HashMap::new();
    let multipliers = computations(
        dag,
        recurrence,
        settings.non_repeating_query_horizon_seconds,
    )
    .map_err(|error| (first, error))?;
    let mut output: HashMap<PhysicalASAPNodeId, EdgeStatistics> = HashMap::new();
    let mut per_node = BTreeMap::new();
    // The candidate's usage, accumulated node by node; peak cores and
    // latency are filled in once every node is measured.
    let mut usage = ResourceUsage::default();
    // Each node's work in ms on one core: one computation at query time,
    // one second of arriving rows at ingestion time.
    let mut work_ms: HashMap<PhysicalASAPNodeId, f64> = HashMap::new();
    // MatAtQuery state in memory, held only while one evaluation runs:
    // (bytes, per second), counted once the evaluations' latencies are known.
    let mut held: HashMap<PhysicalASAPNodeId, (u64, f64)> = HashMap::new();
    // The raw data query-time scans read, kept on disk: per source, the
    // largest scan's (bytes, node).
    let mut raw_data: HashMap<String, (u64, PhysicalASAPNodeId)> = HashMap::new();
    // Each in-memory ingestion-time state's bytes per window (what it keeps
    // for one window, its output or an exact aggregate's values).
    let mut maintained: Vec<(PhysicalASAPNodeId, u64)> = Vec::new();
    // Each node's producers, in edge order.
    let mut producers_of: HashMap<PhysicalASAPNodeId, Vec<PhysicalASAPNodeId>> = HashMap::new();
    for e in &dag.edges {
        producers_of.entry(e.consumer).or_default().push(e.producer);
    }
    // Each sketch node's sketch, to look up in the measured table: a
    // build's own (one instance per group; a shared Hydra structure is not
    // that sketch), inherited by the merges and estimates over it.
    let mut sketch_of: HashMap<PhysicalASAPNodeId, &SketchParams> = HashMap::new();
    // The measured cost of one `operation` on `sketch`, in operations of
    // `speed` (measured ns · 10⁻⁶ / ms_per_cpu_op), when the table has it.
    let measured_ops = |sketch: Option<&SketchParams>, operation| {
        measured_cpu_ns(&settings.measured_operation_costs, sketch?, operation)
            .map(|ns| ns * 1e-6 / speed.ms_per_cpu_op)
    };
    for node in &dag.nodes {
        let ingestion = node.output_state.timing == ExecutionTiming::IngestionTime;
        let producers = producers_of.get(&node.id).map_or(&[][..], Vec::as_slice);
        let inputs: Vec<_> = producers.iter().map(|p| output[p]).collect();
        let sources: BTreeSet<&str> = match &node.payload {
            Payload::NonASAP(NonASAPOp::Scan { source, .. }) => [source_name(source)].into(),
            _ => producers
                .iter()
                .flat_map(|p| sources_read_by[p].iter().copied())
                .collect(),
        };
        let series = priced_series(&sources, data);
        sources_read_by.insert(node.id, sources);
        let node_sketch = match &node.payload {
            Payload::ASAP(
                ASAPOp::SummaryAgg {
                    family: FieldDataType::Sketch(kind, _),
                    grouping: GroupingStrategy::PerSubpopulationInstance,
                    ..
                }
                | ASAPOp::PaneChain {
                    family: FieldDataType::Sketch(kind, _),
                    grouping: GroupingStrategy::PerSubpopulationInstance,
                    ..
                },
            ) => Some(kind.params()),
            Payload::ASAP(ASAPOp::SummaryMerge { .. } | ASAPOp::SummaryEstimate { .. }) => {
                producers.first().and_then(|p| sketch_of.get(p).copied())
            }
            _ => None,
        };
        if let Some(sketch) = node_sketch {
            sketch_of.insert(node.id, sketch);
        }
        let mut price_source = NodePriceSource::Analytical;
        let input = inputs
            .first()
            .copied()
            .unwrap_or(EdgeStatistics { rows: 1, bytes: 1 });
        let width = row_bytes(&node.output_schema);
        let edge = |rows: u64| EdgeStatistics {
            rows,
            bytes: rows * width,
        };
        let unary = |output| UnaryEdgeStatistics {
            input,
            output,
            promql: None,
        };
        let groups = |reduction: &Reduction| {
            let groups = match reduction {
                Reduction::Reduce(keys) if keys.keys().is_empty() && !keys.is_without() => 1,
                Reduction::Reduce(keys) if !keys.is_without() => DEFAULT_GROUP_COUNT,
                _ => series,
            };
            // One second of arriving rows does not bound the groups a
            // maintained state holds.
            match ingestion {
                true => groups,
                false => groups.min(input.rows.max(1)),
            }
        };
        let (out, estimate, detail) = match &node.payload {
            Payload::NonASAP(operator) => match operator {
                NonASAPOp::Scan { .. } => {
                    // At ingestion time, one second of arriving rows; at
                    // query time, the longest window read above it.
                    let lookback = match ingestion {
                        true => 1_000,
                        false => query_time_lookback_ms(dag, nodes, node.id, kept_chains),
                    };
                    let out = edge(((rows_per_ms * lookback as f64).round() as u64).max(1));
                    let estimate = estimate_operator(
                        PhysicalOperator::Scan,
                        OperatorStatistics::Scan {
                            edges: UnaryEdgeStatistics {
                                input: out,
                                output: out,
                                promql: None,
                            },
                            source_read_bytes: out.bytes,
                        },
                    );
                    (out, estimate, format!("scan {} samples", out.rows))
                }
                NonASAPOp::Aggregate {
                    reduction,
                    measures,
                    ..
                } => {
                    let group_count = groups(reduction);
                    let keys = match reduction {
                        Reduction::Reduce(keys) => keys.keys().len() as u64,
                        _ => 1,
                    };
                    let out = edge(group_count);
                    let held = input.rows.div_ceil(group_count.max(1));
                    let estimate = estimate_operator(
                        PhysicalOperator::HashAggregate {
                            grouping_key_count: keys,
                            accumulator_count: measures.len().max(1) as u64,
                        },
                        OperatorStatistics::HashAggregate {
                            edges: unary(out),
                            group_count,
                            key_bytes: 16 * keys,
                            accumulator_bytes_per_group: measures
                                .iter()
                                .map(|m| if keeps_every_value(m) { 8 * held } else { 8 })
                                .sum::<u64>()
                                .max(8),
                        },
                    );
                    (
                        out,
                        estimate,
                        format!(
                            "hash aggregate {} rows into {group_count} groups",
                            input.rows
                        ),
                    )
                }
                NonASAPOp::Sort {
                    keys, partition_by, ..
                } => {
                    let partitions = if partition_by.keys().is_empty() {
                        1
                    } else {
                        DEFAULT_GROUP_COUNT.min(input.rows.max(1))
                    };
                    let estimate = estimate_operator(
                        PhysicalOperator::InMemoryComparisonSort {
                            ordering_key_count: keys.len() as u64,
                            partitioned: partitions > 1,
                        },
                        OperatorStatistics::InMemoryComparisonSort {
                            edges: unary(input),
                            input_partitioning: split(input, partitions),
                        },
                    );
                    (
                        input,
                        estimate,
                        format!("sort {} rows in {partitions} partitions", input.rows),
                    )
                }
                NonASAPOp::Limit {
                    n,
                    offset,
                    partition_by,
                    ..
                } => {
                    let partitions = if partition_by.keys().is_empty() {
                        1
                    } else {
                        DEFAULT_GROUP_COUNT
                    };
                    let limit = n.map_or(u64::MAX, |n| (n as u64).saturating_mul(partitions));
                    let offset = (*offset as u64).saturating_mul(partitions);
                    let out = edge(input.rows.saturating_sub(offset).min(limit));
                    let estimate = estimate_operator(
                        PhysicalOperator::Limit { limit, offset },
                        OperatorStatistics::Limit { edges: unary(out) },
                    );
                    (out, estimate, format!("limit to {} rows", out.rows))
                }
                NonASAPOp::TimeShift { .. } => (
                    input,
                    estimate_operator(
                        PhysicalOperator::TimeShift,
                        OperatorStatistics::PassThrough {
                            edges: unary(input),
                        },
                    ),
                    format!("time shift: pass {} rows, free", input.rows),
                ),
                NonASAPOp::TimeRange { range, kind, .. } => {
                    // A range keeps its window's samples; an instant
                    // selector the latest sample of each series.
                    let kept = match kind {
                        TimeRangeKind::Range => {
                            (rows_per_ms * range.as_millis() as f64).round() as u64
                        }
                        TimeRangeKind::Instant => series,
                    };
                    let out = edge(input.rows.min(kept.max(1)));
                    let out = EdgeStatistics {
                        rows: out.rows,
                        bytes: out.bytes.min(input.bytes),
                    };
                    let estimate = estimate_operator(
                        PhysicalOperator::TimeRange,
                        OperatorStatistics::PassThrough { edges: unary(out) },
                    );
                    (
                        out,
                        estimate,
                        format!(
                            "time range {range:?}: keep {} of {} rows",
                            out.rows, input.rows
                        ),
                    )
                }
                _ => {
                    let estimate = estimate_operator(
                        PhysicalOperator::PassThrough,
                        OperatorStatistics::PassThrough {
                            edges: unary(input),
                        },
                    );
                    (
                        input,
                        estimate,
                        format!("operator: pass {} rows", input.rows),
                    )
                }
            },
            Payload::ASAP(ASAPOp::SummaryAgg {
                family, reduction, ..
            }) => {
                let group_count = groups(reduction);
                let (depth, state_bytes) = summary_shape(family);
                let out = EdgeStatistics {
                    rows: group_count,
                    bytes: group_count * state_bytes,
                };
                let (ops, per_row_text) = match measured_ops(node_sketch, SummaryOperation::Update)
                {
                    Some(per_update) => {
                        price_source = NodePriceSource::Measured;
                        (
                            input.rows as f64 * per_update,
                            format!("{per_update:.3} measured ops per update"),
                        )
                    }
                    None => (input.rows as f64 * depth, format!("depth {depth}")),
                };
                (
                    out,
                    Ok(ResourceEstimate::new(ops, out.bytes, 0)),
                    format!(
                        "build {} into {group_count} states: {} rows x {per_row_text}",
                        family_name(family),
                        input.rows
                    ),
                )
            }
            Payload::ASAP(ASAPOp::PaneChain {
                family,
                reduction,
                pane_width_ms,
                pane_count,
                ..
            }) => {
                // Each input row is kept by the one pane its time falls in,
                // as a time range keeps it (one operation), and updates
                // that pane.
                let group_count = groups(reduction);
                let (depth, state_bytes) = summary_shape(family);
                let states = group_count * u64::from(*pane_count);
                let out = EdgeStatistics {
                    rows: states,
                    bytes: states * state_bytes,
                };
                let (ops, per_row_text) = match measured_ops(node_sketch, SummaryOperation::Update)
                {
                    Some(per_update) => {
                        price_source = NodePriceSource::Measured;
                        (
                            input.rows as f64 * (per_update + 1.0),
                            format!("(1 + {per_update:.3} measured ops per update)"),
                        )
                    }
                    None => (
                        input.rows as f64 * (depth + 1.0),
                        format!("(1 + depth {depth})"),
                    ),
                };
                (
                    out,
                    Ok(ResourceEstimate::new(ops, out.bytes, 0)),
                    format!(
                        "build {} in {pane_count} panes of {pane_width_ms} ms for \
                         {group_count} groups: {} rows x {per_row_text}",
                        family_name(family),
                        input.rows
                    ),
                )
            }
            Payload::ASAP(ASAPOp::SummaryMerge { .. }) => {
                // One operation per input state; one state per group out,
                // each pane of a chain being a state of its group.
                let mut states = 0;
                let mut group_count = 0;
                for producer in producers {
                    let rows = output[producer].rows;
                    let panes = match &nodes[producer].payload {
                        Payload::ASAP(ASAPOp::PaneChain { pane_count, .. }) => {
                            u64::from(*pane_count)
                        }
                        _ => 1,
                    };
                    states += rows;
                    group_count = group_count.max(rows / panes.max(1));
                }
                let out = EdgeStatistics {
                    rows: group_count,
                    bytes: input.bytes.div_ceil(input.rows.max(1)) * group_count,
                };
                let (ops, per_merge_text) = match measured_ops(node_sketch, SummaryOperation::Merge)
                {
                    Some(per_merge) => {
                        price_source = NodePriceSource::Measured;
                        (
                            states as f64 * per_merge,
                            format!(" x {per_merge:.3} measured ops per merge"),
                        )
                    }
                    None => (states as f64, String::new()),
                };
                (
                    out,
                    Ok(ResourceEstimate::new(ops, out.bytes, 0)),
                    format!("merge {states} states into {group_count}{per_merge_text}"),
                )
            }
            Payload::ASAP(ASAPOp::SummaryEstimate { query, .. }) => {
                let per_group = match query {
                    SketchStatistic::TopK { k } => *k as u64,
                    _ => 1,
                };
                let out = edge(input.rows * per_group);
                // A measured estimate reads one group's result, all of a
                // top-k's `k` entries included.
                let (ops, per_group_text) =
                    match measured_ops(node_sketch, SummaryOperation::Estimate) {
                        Some(per_estimate) => {
                            price_source = NodePriceSource::Measured;
                            (
                                input.rows as f64 * per_estimate,
                                format!("{per_estimate:.3} measured ops per estimate"),
                            )
                        }
                        None => (out.rows as f64, per_group.to_string()),
                    };
                (
                    out,
                    Ok(ResourceEstimate::new(ops, 0, 0)),
                    format!("estimate {} groups x {per_group_text}", input.rows),
                )
            }
            Payload::ASAP(ASAPOp::FinalizeExactAccumulator { .. }) => (
                edge(input.rows),
                Ok(ResourceEstimate::new(input.rows as f64, 0, 0)),
                format!("finalize {} accumulators", input.rows),
            ),
            _ => (
                edge(input.rows),
                Ok(ResourceEstimate::new(input.rows as f64, 0, 0)),
                format!("{} rows", input.rows),
            ),
        };
        let estimate = estimate.map_err(|error| (node.id, error))?;
        work_ms.insert(
            node.id,
            estimate.cpu_ops() * speed.ms_per_cpu_op
                + estimate.scan_bytes() as f64 * speed.ms_per_scanned_byte,
        );
        let (multiplier, per_second) = multipliers[&node.id];
        usage.cpu_ops_per_second += estimate.cpu_ops() * per_second;
        usage.scanned_bytes_per_second += estimate.scan_bytes() as f64 * per_second;
        // What this node keeps: memory averaged over time (byte-seconds per
        // second), its peak memory and its disk bytes, added to the
        // candidate's usage below.
        let mut average_memory_bytes = 0.0;
        let mut peak_memory_bytes = 0u64;
        let mut disk_bytes = 0u64;
        let mut detail = format!("{detail}; x {per_second:.4}/s");
        // State kept between computations: at ingestion time, the window
        // being built and the completed ones queries read (a chain of N
        // panes keeps N + 1), every second. At query time the output is
        // reused only within one evaluation, so it is held only while that
        // evaluation runs (counted below, once latencies are known). The
        // state is what the node holds: its output, or the values an exact
        // aggregate keeps per group when that is more. A computation that
        // keeps nothing holds its working memory while it runs, once per
        // consumer that recomputes it.
        let state = out.bytes.max(estimate.peak_memory_bytes());
        let (kept, medium) = match (node.materialization, &node.payload) {
            (Materialization::NotMaterialized, _) => {
                peak_memory_bytes = estimate.peak_memory_bytes().saturating_mul(multiplier);
                (0, Medium::Memory)
            }
            // A pane chain kept across evaluations holds its N − 1 older
            // panes between evaluations, for the next ones, and all N
            // while one runs.
            (Materialization::MatAtQueryKeptAcrossEvaluations(medium), _) => {
                let panes = panes_of(node);
                let between = state / panes * (panes - 1);
                match medium {
                    Medium::Memory => {
                        average_memory_bytes = between as f64;
                        peak_memory_bytes = state;
                    }
                    Medium::Disk => disk_bytes = between,
                }
                detail += &format!("; keeps {between} bytes across evaluations");
                (0, medium)
            }
            (Materialization::MatAtQuery(medium), _) => {
                match medium {
                    Medium::Memory => {
                        held.insert(node.id, (state, per_second));
                        peak_memory_bytes = state;
                    }
                    Medium::Disk => disk_bytes = state,
                }
                (0, medium)
            }
            (
                Materialization::MatAtIngestion(medium),
                Payload::ASAP(ASAPOp::PaneChain { pane_count, .. }),
            ) => {
                let panes = u64::from(*pane_count).max(1);
                (state / panes * (panes + 1), medium)
            }
            (Materialization::MatAtIngestion(medium), _) => (2 * state, medium),
        };
        if node.materialization == Materialization::MatAtIngestion(Medium::Memory) {
            maintained.push((node.id, state / panes_of(node)));
        }
        if kept > 0 {
            match medium {
                Medium::Memory => {
                    average_memory_bytes = kept as f64;
                    peak_memory_bytes = kept;
                }
                Medium::Disk => disk_bytes = kept,
            }
            detail += &format!("; keeps {kept} bytes");
        }
        if let Payload::NonASAP(NonASAPOp::Scan { source, .. }) = &node.payload {
            if !ingestion {
                let largest = raw_data
                    .entry(format!("{source:?}"))
                    .or_insert((out.bytes, node.id));
                if out.bytes > largest.0 {
                    *largest = (out.bytes, node.id);
                }
            }
        }
        // One computation's CPU and scan price times its rate, then the
        // kept state's.
        let cost = usage_prices.map(|prices| {
            (estimate.cpu_ops() * prices.cost_per_cpu_op
                + estimate.scan_bytes() as f64 * prices.cost_per_scanned_byte)
                * per_second
                + prices.cost_per_memory_byte_second * average_memory_bytes
                + prices.cost_per_disk_byte_second * disk_bytes as f64
        });
        usage.average_memory_bytes += average_memory_bytes;
        usage.peak_memory_bytes = usage.peak_memory_bytes.saturating_add(peak_memory_bytes);
        usage.disk_bytes = usage.disk_bytes.saturating_add(disk_bytes);
        if cost.is_some_and(|cost| !cost.is_finite()) {
            return Err((node.id, AnalyticalCostError::Overflow));
        }
        output.insert(node.id, out);
        per_node.insert(
            node.id,
            NodeCost {
                cost_per_second: cost,
                multiplier,
                detail,
                price_source,
            },
        );
    }
    for (bytes, id) in raw_data.into_values() {
        let node = per_node.get_mut(&id).expect("every node is measured");
        usage.disk_bytes = usage.disk_bytes.saturating_add(bytes);
        node.detail += &format!("; keeps {bytes} bytes of raw data on disk");
        if let (Some(cost), Some(prices)) = (&mut node.cost_per_second, usage_prices) {
            *cost += prices.cost_per_disk_byte_second * bytes as f64;
        }
    }
    // With k ingest workers, each of the k − 1 beyond the first builds its
    // own partial copy of an in-memory ingestion-time state's newest window
    // (merged at query time, [`query_latencies`]), held at the peak too.
    for (id, window) in maintained {
        let copies = ingest_workers(dag, &work_ms, id) - 1;
        if copies == 0 {
            continue;
        }
        let bytes = copies.saturating_mul(window);
        let measured = per_node.get_mut(&id).expect("every node is measured");
        usage.peak_memory_bytes = usage.peak_memory_bytes.saturating_add(bytes);
        measured.detail +=
            &format!("; {copies} partial copies of its newest window, {bytes} bytes");
    }
    // Milliseconds of one merge of each state with a measured merge entry
    // (measured ns · 10⁻⁶), for the newest window's compaction.
    let merge_ms: HashMap<PhysicalASAPNodeId, f64> = sketch_of
        .iter()
        .filter_map(|(id, sketch)| {
            measured_ops(Some(sketch), SummaryOperation::Merge)
                .map(|ops| (*id, ops * speed.ms_per_cpu_op))
        })
        .collect();
    let latency = query_latencies(dag, nodes, &output, &work_ms, &merge_ms, &speed);
    // A MatAtQuery state is held for one evaluation: from its build until
    // the longest query reading it finishes, about that query's latency.
    let reach = query_reach(dag, nodes);
    for (id, (state, per_second)) in held {
        let evaluation_ms = reach
            .iter()
            .zip(&latency)
            .filter(|(reached, _)| reached.contains(&id))
            .map(|(_, l)| l.total_ms())
            .fold(0.0, f64::max);
        let byte_seconds = state as f64 * evaluation_ms / 1_000.0 * per_second;
        let node = per_node.get_mut(&id).expect("every node is measured");
        usage.average_memory_bytes += byte_seconds;
        node.detail += &format!("; holds {state} bytes for {evaluation_ms:.1} ms");
        if let (Some(cost), Some(prices)) = (&mut node.cost_per_second, usage_prices) {
            *cost += prices.cost_per_memory_byte_second * byte_seconds;
            if !cost.is_finite() {
                return Err((id, AnalyticalCostError::Overflow));
            }
        }
    }
    usage.peak_cpu_cores = peak_cpu_cores(dag, nodes, &work_ms, &multipliers, &latency);
    usage.latency_per_query = latency;
    let cost = match &settings.cost_model {
        CostModel::UsageOverTime(_) => per_node.values().map(|node| node.cost_per_second).sum(),
        CostModel::ProvisionedCapacity(prices) => {
            let cost = prices.cost_per_provisioned_memory_byte_second
                * usage.peak_memory_bytes as f64
                + prices.cost_per_provisioned_cpu_core_second * f64::from(usage.peak_cpu_cores)
                + prices.cost_per_disk_byte_second * usage.disk_bytes as f64;
            if !cost.is_finite() {
                return Err((first, AnalyticalCostError::Overflow));
            }
            Some(cost)
        }
        CostModel::ClusterCapacity(_) => None,
    };
    Ok(CandidateCost {
        usage,
        cost_per_second: cost,
        unit: COST_UNIT,
        source: cost_source(&per_node),
        per_node,
    })
}

/// [`CandidateCost::source`] for nodes priced as `per_node` says.
fn cost_source(per_node: &BTreeMap<PhysicalASAPNodeId, NodeCost>) -> &'static str {
    let measured = |node: &NodeCost| node.price_source == NodePriceSource::Measured;
    if !per_node.values().any(measured) {
        COST_SOURCE
    } else if per_node.values().all(measured) {
        MEASURED_COST_SOURCE
    } else {
        MIXED_COST_SOURCE
    }
}

/// The CPU cores a candidate needs at its peak: enough for its
/// ingestion-time work every second (none without any), plus one per
/// query with query-time work, all evaluating at once; at least the cores
/// its average query-time work fills.
fn peak_cpu_cores(
    dag: &PhysicalASAPDAG,
    nodes: &HashMap<PhysicalASAPNodeId, &asap_types::ir::physical_export::PhysicalASAPDAGNode>,
    work_ms: &HashMap<PhysicalASAPNodeId, f64>,
    multipliers: &HashMap<PhysicalASAPNodeId, (u64, f64)>,
    latency: &[QueryLatency],
) -> u32 {
    let mut ingestion_ms_per_second = 0.0;
    let mut query_ms_per_second = 0.0;
    for node in &dag.nodes {
        let per_second = multipliers[&node.id].1;
        if nodes[&node.id].output_state.timing == ExecutionTiming::IngestionTime {
            ingestion_ms_per_second += work_ms[&node.id] * per_second;
        } else {
            query_ms_per_second += work_ms[&node.id] * per_second;
        }
    }
    let cores = |ms_per_second: f64| (ms_per_second / 1_000.0).ceil() as u32;
    let evaluating = latency.iter().filter(|l| l.query_ms > 0.0).count() as u32;
    cores(ingestion_ms_per_second) + evaluating.max(cores(query_ms_per_second))
}

/// Each query's latency per evaluation (module doc, "Latency"): the sum of
/// the query-time work it reaches, each node once, plus the longest
/// compaction among the ingestion-time states it reads. Compacting a
/// state's newest window merges, for each of its `I` states per window,
/// the `k` ingest workers' partial copies, `(k − 1)·I` merges, where `k`
/// is the cores its ingestion work takes, at least one. Each merge takes
/// the state's measured merge time (`merge_ms`) when the measured table has
/// one, and one operation otherwise.
fn query_latencies(
    dag: &PhysicalASAPDAG,
    nodes: &HashMap<PhysicalASAPNodeId, &asap_types::ir::physical_export::PhysicalASAPDAGNode>,
    output: &HashMap<PhysicalASAPNodeId, EdgeStatistics>,
    work_ms: &HashMap<PhysicalASAPNodeId, f64>,
    merge_ms: &HashMap<PhysicalASAPNodeId, f64>,
    speed: &ResourceSpeed,
) -> Vec<QueryLatency> {
    let inputs = |id: PhysicalASAPNodeId| {
        dag.edges
            .iter()
            .filter(move |e| e.consumer == id)
            .map(|e| e.producer)
    };
    let at_ingestion =
        |id: PhysicalASAPNodeId| nodes[&id].output_state.timing == ExecutionTiming::IngestionTime;
    let compaction_ms = |state: PhysicalASAPNodeId| {
        let workers = ingest_workers(dag, work_ms, state) as f64;
        let states = (output[&state].rows / panes_of(nodes[&state])) as f64;
        let per_merge_ms = merge_ms.get(&state).copied().unwrap_or(speed.ms_per_cpu_op);
        (workers - 1.0) * states * per_merge_ms
    };
    query_starts(dag)
        .into_iter()
        .map(|start| {
            let mut latency = QueryLatency {
                compaction_ms: 0.0,
                query_ms: 0.0,
            };
            let mut seen = HashSet::new();
            let mut stack = start;
            while let Some(id) = stack.pop() {
                if !seen.insert(id) {
                    continue;
                }
                if at_ingestion(id) {
                    latency.compaction_ms = latency.compaction_ms.max(compaction_ms(id));
                    continue;
                }
                latency.query_ms += work_ms[&id];
                stack.extend(inputs(id));
            }
            latency
        })
        .collect()
}

/// The cores `state`'s ingestion work takes, at least one: the ingest
/// workers that each build a partial copy of its newest window.
fn ingest_workers(
    dag: &PhysicalASAPDAG,
    work_ms: &HashMap<PhysicalASAPNodeId, f64>,
    state: PhysicalASAPNodeId,
) -> u64 {
    let mut ingest_ms_per_s = 0.0;
    let mut seen = HashSet::new();
    let mut stack = vec![state];
    while let Some(id) = stack.pop() {
        if seen.insert(id) {
            ingest_ms_per_s += work_ms[&id];
            stack.extend(
                dag.edges
                    .iter()
                    .filter(|e| e.consumer == id)
                    .map(|e| e.producer),
            );
        }
    }
    (ingest_ms_per_s / 1_000.0).ceil().max(1.0) as u64
}

/// A node's panes: a pane chain's count, 1 for any other node.
fn panes_of(node: &asap_types::ir::physical_export::PhysicalASAPDAGNode) -> u64 {
    match &node.payload {
        Payload::ASAP(ASAPOp::PaneChain { pane_count, .. }) => u64::from(*pane_count).max(1),
        _ => 1,
    }
}

/// The nodes each query reaches at query time, in workload order: its walk
/// from its roots, stopping at (and including) ingestion-time nodes.
fn query_reach(
    dag: &PhysicalASAPDAG,
    nodes: &HashMap<PhysicalASAPNodeId, &asap_types::ir::physical_export::PhysicalASAPDAGNode>,
) -> Vec<HashSet<PhysicalASAPNodeId>> {
    query_starts(dag)
        .into_iter()
        .map(|mut stack| {
            let mut seen = HashSet::new();
            while let Some(id) = stack.pop() {
                if seen.insert(id)
                    && nodes[&id].output_state.timing != ExecutionTiming::IngestionTime
                {
                    stack.extend(
                        dag.edges
                            .iter()
                            .filter(|e| e.consumer == id)
                            .map(|e| e.producer),
                    );
                }
            }
            seen
        })
        .collect()
}

/// The raw data the query-time scan `scan` reads, in milliseconds back from
/// the evaluation: the longest window read above it ([`windows_read`]), or
/// [`DEFAULT_LOOKBACK_MS`] without one.
fn query_time_lookback_ms(
    dag: &PhysicalASAPDAG,
    nodes: &NodeMap<'_>,
    scan: PhysicalASAPNodeId,
    kept_chains: &HashSet<PhysicalASAPNodeId>,
) -> u64 {
    windows_read(dag, nodes, scan, kept_chains)
        .into_iter()
        .max()
        .unwrap_or(DEFAULT_LOOKBACK_MS)
}

/// The nodes of `dag` by id.
fn node_map(dag: &PhysicalASAPDAG) -> NodeMap<'_> {
    dag.nodes.iter().map(|n| (n.id, n)).collect()
}

/// The nodes of a physical DAG by id.
type NodeMap<'a> =
    HashMap<PhysicalASAPNodeId, &'a asap_types::ir::physical_export::PhysicalASAPDAGNode>;

/// The tumbling-pane chains of `dag` kept across evaluations
/// (`MatAtQueryKeptAcrossEvaluations`, which Stage 2 offers only when the
/// deployment keeps query-time results and every reader builds one new
/// pane per evaluation). Each evaluation builds the newest pane, and the
/// older ones, which complete the readers' look-back, are kept from
/// earlier evaluations.
fn kept_pane_chains(dag: &PhysicalASAPDAG) -> HashSet<PhysicalASAPNodeId> {
    dag.nodes
        .iter()
        .filter(|node| {
            matches!(
                (&node.payload, node.materialization),
                (
                    Payload::ASAP(ASAPOp::PaneChain { .. }),
                    Materialization::MatAtQueryKeptAcrossEvaluations(_),
                )
            )
        })
        .map(|node| node.id)
        .collect()
}

/// For a deployment that does not retain raw data, every query-time scan
/// of `dag` with the window it would read, as a rejection reason (`None`
/// without one). Raw samples are then not kept after they arrive, so no
/// window of them is there to scan at query time: only ingestion-time work
/// reads raw rows. `nodes` and `kept_chains` are `dag`'s ([`node_map`],
/// [`kept_pane_chains`]).
fn raw_data_violation(
    dag: &PhysicalASAPDAG,
    nodes: &NodeMap<'_>,
    kept_chains: &HashSet<PhysicalASAPNodeId>,
) -> Option<String> {
    let scans: Vec<String> = dag
        .nodes
        .iter()
        .filter_map(|node| match &node.payload {
            Payload::NonASAP(NonASAPOp::Scan { source, .. })
                if node.output_state.timing != ExecutionTiming::IngestionTime =>
            {
                Some(format!(
                    "{} ms of raw {}",
                    query_time_lookback_ms(dag, nodes, node.id, kept_chains),
                    source_name(source)
                ))
            }
            _ => None,
        })
        .collect();
    (!scans.is_empty()).then(|| {
        format!(
            "scans {} at query time, but the deployment does not retain raw data",
            scans.join(", ")
        )
    })
}

/// The windows, in milliseconds, read above the scan `scan` at query time:
/// each range `TimeRange` and `PaneChain` (its whole window, or one pane
/// for a chain in `kept_chains`, which builds only its newest pane per
/// evaluation) reached through time shifts, which move a window without
/// widening it.
fn windows_read(
    dag: &PhysicalASAPDAG,
    nodes: &HashMap<PhysicalASAPNodeId, &asap_types::ir::physical_export::PhysicalASAPDAGNode>,
    scan: PhysicalASAPNodeId,
    kept_chains: &HashSet<PhysicalASAPNodeId>,
) -> Vec<u64> {
    let mut windows = Vec::new();
    let mut stack = vec![scan];
    let mut seen = HashSet::new();
    while let Some(id) = stack.pop() {
        if !seen.insert(id) {
            continue;
        }
        for edge in dag.edges.iter().filter(|e| e.producer == id) {
            match &nodes[&edge.consumer].payload {
                Payload::NonASAP(NonASAPOp::TimeRange { range, .. }) => {
                    windows.push(range.as_millis() as u64)
                }
                Payload::ASAP(ASAPOp::PaneChain { pane_width_ms, .. })
                    if kept_chains.contains(&edge.consumer) =>
                {
                    windows.push(*pane_width_ms)
                }
                Payload::ASAP(ASAPOp::PaneChain {
                    pane_width_ms,
                    pane_count,
                    ..
                }) => windows.push(pane_width_ms.saturating_mul(u64::from(*pane_count))),
                Payload::NonASAP(NonASAPOp::TimeShift { .. }) => stack.push(edge.consumer),
                _ => {}
            }
        }
    }
    windows
}

/// `input` split as evenly as integers allow into `partitions` parts.
fn split(input: EdgeStatistics, partitions: u64) -> PartitionStatistics {
    let part = |total: u64, i: u64| total / partitions + u64::from(i < total % partitions);
    PartitionStatistics {
        partitions: (0..partitions)
            .map(|i| EdgeStatistics {
                rows: part(input.rows, i),
                bytes: part(input.bytes, i),
            })
            .collect(),
    }
}

/// Plain values are 8 bytes, strings 16; summary columns are sized apart.
fn row_bytes(schema: &Schema) -> u64 {
    schema
        .fields
        .iter()
        .map(|f| match &f.dtype {
            FieldDataType::Plain(DataType::Utf8) => 16,
            _ => 8,
        })
        .sum::<u64>()
        .max(1)
}

/// Exact state that keeps every value or item of its group, so grows with
/// the data; other exact accumulators are fixed-size.
fn keeps_every_value(measure: &AggIntent) -> bool {
    matches!(
        measure,
        AggIntent::Quantile { .. }
            | AggIntent::TopK { .. }
            | AggIntent::Cardinality { .. }
            | AggIntent::FrequencyL2 { .. }
            | AggIntent::FrequencyEntropy { .. }
    )
}

/// Update operations per input row and bytes per state, from the sketch's
/// parameters as `asap_sketchlib` allocates it:
/// - Count-Min / Count-Sketch: `depth × width` 8-byte counters, plus a
///   24-byte entry per heap slot for the top-k variants;
/// - KLL: the 8-byte item array it allocates up front ([`kll_items`]);
/// - DDSketch: one 8-byte count per bin it touches over an assumed nine
///   decades of values ([`ddsketch_bins`]);
/// - HLL: one 1-byte register per bucket, `2^precision`;
/// - KMV: its `k` smallest 64-bit hashes (Theta, which candidate generation
///   does not offer because `asap_sketchlib` has no Theta sketch, likewise);
/// - UnivMon: per layer, a `rows × cols` Count-Sketch of 8-byte counters and
///   a heap of `heap_size` 24-byte entries.
///
/// Update operations per input row: one counter per row of depth for
/// Count-Min and Count-Sketch, plus one heap update for their top-k variants;
/// for UnivMon, the same per layer it updates ([`univmon_expected_layers`]);
/// 1 for every other sketch.
fn summary_shape(family: &FieldDataType) -> (f64, u64) {
    match family {
        FieldDataType::Sketch(kind, _) => match kind.params() {
            SketchParams::Cms { width, depth } | SketchParams::CountSketch { width, depth } => {
                (f64::from(*depth), 8 * u64::from(*width) * u64::from(*depth))
            }
            SketchParams::CmsWithHeap {
                width,
                depth,
                heap_size,
            }
            | SketchParams::CountSketchWithHeap {
                width,
                depth,
                heap_size,
            } => (
                f64::from(*depth) + 1.0,
                8 * u64::from(*width) * u64::from(*depth) + 24 * u64::from(*heap_size),
            ),
            SketchParams::Kll { k, .. } => (1.0, 8 * kll_items(*k)),
            SketchParams::DDSketch { alpha } => (1.0, ddsketch_bins(*alpha).saturating_mul(8)),
            SketchParams::Hll { precision, .. } => (1.0, 1u64 << (*precision).min(63)),
            SketchParams::Kmv { k, .. } | SketchParams::Theta { k } => (1.0, 8 * u64::from(*k)),
            SketchParams::UnivMon {
                heap_size,
                sketch_rows,
                sketch_cols,
                layers,
                ..
            } => (
                univmon_expected_layers(*layers) * (f64::from(*sketch_rows) + 1.0),
                u64::from(*layers)
                    * (8 * u64::from(*sketch_rows) * u64::from(*sketch_cols)
                        + 24 * u64::from(*heap_size)),
            ),
        },
        _ => (1.0, 8),
    }
}

/// Items `asap_sketchlib`'s KLL allocates up front for `k`
/// (`KLL::init_internal` → `compute_max_capacity(k, 8)`): over its 61 levels,
/// `max(ceil(k · (2/3)^i), 8)` each, with `k` capped at [`KLL_MAX_K`] as the
/// library caps it. 1,206 items at `k` = 269.
fn kll_items(k: u32) -> u64 {
    const LEVELS: usize = 61;
    const MIN_LEVEL_CAPACITY: f64 = 8.0;
    let k = f64::from(k.min(KLL_MAX_K));
    let mut scale = 1.0_f64;
    let mut total = 0;
    for _ in 0..LEVELS {
        total += (k * scale).ceil().max(MIN_LEVEL_CAPACITY) as u64;
        scale *= 2.0 / 3.0;
    }
    total
}

/// Layers a UnivMon row updates in expectation. `asap_sketchlib` updates
/// layers `0..=b`, with bottom layer `b` the index of the first zero among
/// hash bits `1..layers`, capped at `layers - 1`; so it reaches layer `j`
/// with probability `2^-j`, and the expectation is
/// `sum_{j < layers} 2^-j = 2 - 2^(1 - layers)`.
fn univmon_expected_layers(layers: u8) -> f64 {
    2.0 - 2f64.powi(1 - i32::from(layers.max(1)))
}

/// Ratio of the largest to the smallest positive value a DDSketch is assumed
/// to see: nine decades, e.g. latencies from 1 ns to 1 s. An assumption, not
/// workload evidence: the data workload declares no value range.
const DDSKETCH_ASSUMED_VALUE_RANGE: f64 = 1e9;

/// Bins a DDSketch with relative accuracy `alpha` touches, assuming its
/// values span nine decades ([`DDSKETCH_ASSUMED_VALUE_RANGE`] = 1e9): its
/// bins grow geometrically by `gamma = (1 + alpha) / (1 - alpha)`, so
/// `ceil(ln(1e9) / ln(gamma))`, e.g. 1,037 bins at α = 0.01. The count is
/// proportional to the decades spanned, so values over three decades would
/// need a third of these bins and over twelve decades four thirds.
/// `asap_sketchlib`'s store has no bin limit; the cap is the bins spanning
/// every value it can index.
fn ddsketch_bins(alpha: f64) -> u64 {
    let log_gamma = ((1.0 + alpha) / (1.0 - alpha)).ln();
    let (min, max) = asap_sketchlib::sketches::ddsketch::ddsketch_indexable_bounds(alpha);
    let indexable = (max.ln() - min.ln()) / log_gamma;
    (DDSKETCH_ASSUMED_VALUE_RANGE.ln() / log_gamma)
        .min(indexable)
        .ceil()
        .max(1.0) as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    /// [`super::price`] with `dag`'s node map and kept pane chains.
    fn price(
        dag: &PhysicalASAPDAG,
        recurrence: &[QueryRecurrence],
        data: &DataWorkload,
        settings: &PlanSelectionSettings,
    ) -> Result<CandidateCost, (PhysicalASAPNodeId, AnalyticalCostError)> {
        super::price(
            dag,
            &node_map(dag),
            &kept_pane_chains(dag),
            recurrence,
            data,
            settings,
        )
    }
    use crate::accuracy::AccuracyEvidenceProvider;
    use crate::deployment_capabilities::DeploymentCapabilities;
    use crate::stage2_physical_asap_optimization::optimize_physical_asap;
    use crate::test_support::{entry, entry_with, lower_promql};
    use asap_types::ir::{ExprSemantics, ScalarExpr};
    use asap_types::post_asap::{
        AccuracyMetric, BoundExpr, GroupingStrategy, ProbabilityExpr, SketchKind,
    };
    use asap_types::pre_asap::expr_ir::ArithmeticOpKind;
    use asap_types::types::AccuracyTarget;
    use asap_types::workload::{DataArrival, MetricType, SourceFacts};

    /// `G` of `root` over `data` ([`values_per_result`]).
    fn values_of(root: &QueryRoot, data: &DataWorkload) -> Option<u64> {
        values_per_result(root, series_read(root, data))
    }

    /// [`bound_root_guarantee`] of `root` over `data`.
    fn bound_of(
        root: &QueryRoot,
        data: &DataWorkload,
        models: &PlanningModels<'_>,
    ) -> Option<AccuracyGuarantee> {
        bound_root_guarantee(root, series_read(root, data), data, models)
    }

    /// `n` queries repeating every 10 s, as Example 1's panels do.
    fn recurrences(n: usize) -> Vec<QueryRecurrence> {
        vec![repeating(10_000); n]
    }

    /// A query repeating every `interval_ms`.
    fn repeating(interval_ms: u32) -> QueryRecurrence {
        QueryRecurrence::Repeated(RepeatedDemand::FixedInterval(
            asap_types::workload::RepetitionInterval(interval_ms),
        ))
    }

    /// The metrics and tables the tests' queries read.
    const TEST_SOURCES: [&str; 5] = ["m", "x", "a", "b", "t"];

    /// Data arriving at 10 000 rows per second, each test source holding
    /// 10 000 series.
    fn data() -> DataWorkload {
        with_series(Some(10_000))
    }

    /// [`data`] with every test source holding `series` (`None`: unknown).
    fn with_series(series: Option<u64>) -> DataWorkload {
        DataWorkload {
            arrival: DataArrival::Ingesting {
                rows_per_second: 10_000.0,
            },
            sources: TEST_SOURCES
                .iter()
                .map(|source| {
                    let facts = SourceFacts {
                        series_count: series,
                        ..SourceFacts::default()
                    };
                    (source.to_string(), facts)
                })
                .collect(),
        }
    }

    /// `data` with each of `types`' metrics declared of its type.
    fn with_metric_types(mut data: DataWorkload, types: &[(&str, MetricType)]) -> DataWorkload {
        for (metric, metric_type) in types {
            data.sources
                .entry(metric.to_string())
                .or_default()
                .metric_type = Some(*metric_type);
        }
        data
    }

    /// Classic HLL's failure probability over a source binds only that
    /// source's distinct bound: a bound keyed to another source leaves it
    /// unknown.
    #[test]
    fn classic_hll_failure_probability_binds_only_its_own_sources_bound() {
        let statistic = hll::classic_failure_statistic(15, 0.05, "a");
        let bounded = |source: &str| {
            let mut data = data();
            data.sources
                .get_mut(source)
                .unwrap()
                .max_distinct_per_evaluation = Some(100);
            data
        };
        assert_eq!(workload_statistic(&statistic, None, &bounded("b")), None);
        assert_eq!(
            workload_statistic(&statistic, None, &bounded("a")),
            Some(hll::classic_failure_probability(15, 0.05, 100))
        );
        assert!(hll::classic_failure_probability(15, 0.05, 100) <= 0.01);
    }

    /// Exact (P1), whole-expression CMS+heap (P2) and CountSketch+heap (P3) realizations of
    /// one approximate top-k query, in Pass 1 catalog order.
    fn candidates() -> Vec<PhysicalCandidate> {
        let root = lower_promql(
            "topk by (job) (10, sum_over_time(m[1m]))",
            AccuracyTarget::max_error(0.01).failure_probability(0.001),
        );
        let inventory = crate::per_computation_candidates::generate_per_computation_candidates(
            vec![(0, QueryRoot::Operator(root))],
        )
        .unwrap();
        let topk = inventory
            .targets
            .iter()
            .position(|t| t.choice_count() > 2)
            .unwrap();
        [0, 1, 2]
            .into_iter()
            .map(|index| {
                let mut choice = vec![0; inventory.targets.len()];
                choice[topk] = index;
                let roots: Vec<_> = crate::per_computation_candidates::compose_logical_candidate(
                    &inventory, &choice,
                )
                .unwrap()
                .into_iter()
                .map(|(_, root)| root)
                .collect();
                let mut candidate = optimize_physical_asap(
                    "L",
                    &roots,
                    &[],
                    &data(),
                    &DeploymentCapabilities::default(),
                )
                .unwrap()
                .remove(0);
                candidate.id = format!("P{}", index + 1);
                candidate
            })
            .collect()
    }

    /// A summary whose analytical guarantee misses the target is rejected as
    /// invalid with a reason; the exact plan is then selected. (Top-K
    /// estimates carry no guarantee since #575, so a quantile summary is used.)
    #[test]
    fn accuracy_failing_candidate_is_rejected_as_invalid() {
        let planned = AccuracyTarget::max_error(0.01).failure_probability(0.001);
        let candidates = all_candidates("quantile_over_time(0.99, m[1m])", &planned);
        let strict = AccuracyTarget::max_error(1e-6).failure_probability(1e-9);
        let summaries: Vec<_> = candidates
            .iter()
            .filter(|c| has_summary_estimate(c))
            .map(|c| c.id.as_str())
            .collect();
        assert!(!summaries.is_empty());
        let selection = select_plan(
            &candidates,
            &[entry_with(strict, repeating(10_000))],
            &data(),
            PlanningModels::builtin(),
        )
        .unwrap();
        assert!(!summaries.contains(&selection.selected.as_str()));
        for id in &summaries {
            let rejection = selection.rejected.iter().find(|r| r.id == *id).unwrap();
            assert_eq!(rejection.cause, RejectionCause::AccuracyTargetMissed);
            assert!(
                rejection.reason.contains("misses target"),
                "{}",
                rejection.reason
            );
            assert!(!selection.costs.contains_key(*id));
        }
    }

    /// Totals that differ only by rounding tie, so the earlier candidate is
    /// kept; a real difference is cheaper.
    #[test]
    fn totals_differing_by_rounding_tie() {
        // One unit in the last place apart, as the same charges summed in
        // another grouping can be.
        let separate = 0.336006_f64;
        let together = f64::from_bits(separate.to_bits() - 1);
        assert!(!cheaper(separate, together));
        assert!(!cheaper(together, separate));
        assert!(cheaper(0.3, 0.4));
        assert!(!cheaper(0.4, 0.3));
    }

    /// For a query with a target, Count-Min over weights not proven
    /// non-negative is invalid.
    #[test]
    fn count_min_over_signed_weights_is_rejected_as_invalid() {
        let target = AccuracyTarget::max_error(0.01).failure_probability(0.001);
        let selection = select_plan(
            &candidates(),
            &[entry_with(
                target
                    .clone()
                    .accuracy_metric(AccuracyMetric::ABSOLUTE_STREAM_L1),
                repeating(10_000),
            )],
            &data(),
            PlanningModels::builtin(),
        )
        .unwrap();
        let p2 = selection.rejected.iter().find(|r| r.id == "P2").unwrap();
        assert_eq!(p2.cause, RejectionCause::AccuracyTargetMissed);
        assert!(p2.reason.contains("non-negative"), "{}", p2.reason);
    }

    /// Count-Min over raw samples of `m` is admitted only when the data
    /// workload declares `m` a counter: a gauge, an undeclared metric or a
    /// counter under another name leaves the requirement unmet. Admitted,
    /// the top-k still needs evidence about its output.
    #[test]
    fn count_min_over_samples_needs_a_declared_counter() {
        use asap_types::workload::MetricType;
        let target = AccuracyTarget::max_error(0.01).failure_probability(0.001);
        let with_types = |types: &[(&str, MetricType)]| with_metric_types(data(), types);
        for (types, admitted) in [
            (vec![("m", MetricType::Counter)], true),
            (vec![("m", MetricType::Gauge)], false),
            (vec![("m_total", MetricType::Counter)], false),
            (vec![], false),
        ] {
            let data = with_types(&types);
            let selection = select_plan(
                &candidates(),
                &[entry_with(
                    target
                        .clone()
                        .accuracy_metric(AccuracyMetric::ABSOLUTE_STREAM_L1),
                    repeating(10_000),
                )],
                &data,
                PlanningModels::builtin(),
            )
            .unwrap();
            let p2 = selection.rejected.iter().find(|r| r.id == "P2").unwrap();
            assert_eq!(p2.cause, RejectionCause::AccuracyTargetMissed);
            assert_eq!(
                !p2.reason.contains("non-negative"),
                admitted,
                "{types:?}: {}",
                p2.reason
            );
            if admitted {
                assert!(p2.reason.contains("no guarantee"), "{}", p2.reason);
            } else {
                assert!(
                    p2.reason.contains("not declared a counter"),
                    "{}",
                    p2.reason
                );
            }
        }
    }

    /// The sign gate is an indicator that only 1 (proven) opens: any other
    /// value a binder supplies, 0 included, leaves it unbound rather than
    /// reading as a perfect bound. Other statistics pass through.
    #[test]
    fn the_sign_gate_opens_only_at_one() {
        use crate::accuracy::composition::samples_sign_statistic;
        let gated = AccuracyGuarantee::new(
            AccuracyMetric::Relative,
            BoundExpr::Product {
                factors: vec![
                    BoundExpr::Constant { value: 0.01 },
                    BoundExpr::Unknown {
                        statistic: samples_sign_statistic("m"),
                    },
                ],
            },
            ProbabilityExpr::Constant { value: 0.001 },
            vec![],
        );
        for value in [0.0, 0.5, 2.0, -1.0] {
            assert_eq!(
                bind_statistics(&gated, &|_| Some(value))
                    .spec
                    .error
                    .evaluate(),
                None,
                "{value}"
            );
        }
        assert_eq!(
            bind_statistics(&gated, &|_| Some(1.0))
                .spec
                .error
                .evaluate(),
            Some(0.01)
        );
        let mut norm = gated;
        norm.spec.error = BoundExpr::Unknown {
            statistic: INPUT_ROW_COUNT.into(),
        };
        assert_eq!(
            bind_statistics(&norm, &|_| Some(0.0)).spec.error.evaluate(),
            Some(0.0)
        );
    }

    /// A relative bound gated on the sign of one metric's samples is bound
    /// when the data workload declares that metric a counter, and stays
    /// unknown for a gauge, an undeclared metric or a counter by another
    /// name.
    #[test]
    fn sign_of_counter_samples_is_bound_from_the_declared_type() {
        use crate::accuracy::composition::samples_sign_statistic;
        let gated = AccuracyGuarantee::new(
            AccuracyMetric::Relative,
            BoundExpr::Product {
                factors: vec![
                    BoundExpr::Constant { value: 0.01 },
                    BoundExpr::Unknown {
                        statistic: samples_sign_statistic("m"),
                    },
                ],
            },
            ProbabilityExpr::Constant { value: 0.001 },
            vec![],
        );
        let node = lower_promql("sum(m)", AccuracyTarget::exact());
        let root = QueryRoot::Operator(Rc::new((*node).clone().with_guarantee(Some(gated))));
        let with = |types: &[(&str, MetricType)]| with_metric_types(data(), types);
        let bound = |data: &DataWorkload| {
            bound_of(&root, data, &PlanningModels::builtin())
                .unwrap()
                .spec
                .error
                .evaluate()
        };
        assert_eq!(bound(&with(&[("m", MetricType::Counter)])), Some(0.01));
        for types in [
            vec![("m", MetricType::Gauge)],
            vec![("m_total", MetricType::Counter)],
            vec![],
        ] {
            assert_eq!(bound(&with(&types)), None, "{types:?}");
        }
    }

    /// Every consumer of a run sees the selected plan with its weight
    /// requirements answered: the logical plan (`selected_logical`) and the
    /// executable roots (`Selection::plan`) carry the counter proof, not the
    /// open requirement the executor rejects.
    #[test]
    fn the_selected_plan_carries_confirmed_weights() {
        use asap_types::workload::MetricType;
        let target = AccuracyTarget::max_error(0.01).failure_probability(0.001);
        let root = lower_promql("topk by (job) (10, sum_over_time(m[1m]))", target.clone());
        let root = asap_types::ir::schema_support::with_promql_series_identity(&root).unwrap();
        let data = with_metric_types(with_series(Some(10_000_000)), &[("m", MetricType::Counter)]);
        let run = crate::pass::run_stage1_to_stage3(
            vec![(0, QueryRoot::Operator(root))],
            &[entry_with(target, repeating(10_000))],
            &data,
            PlanningModels::builtin().with_evidence(&EveryOutputMeasuredExact),
        )
        .unwrap();
        let domains = |roots: &[QueryRoot]| -> Vec<WeightDomain> {
            roots
                .iter()
                .flat_map(root_operators)
                .flat_map(OperatorNode::reachable)
                .filter_map(|n| match n.asap() {
                    Some(ASAPOp::SummaryAgg {
                        input,
                        family: FieldDataType::Sketch(kind, _),
                        ..
                    }) if *kind.algorithm() == SketchAlgorithm::CmsWithHeap => {
                        Some(input.weight_domain.clone())
                    }
                    _ => None,
                })
                .collect()
        };
        let proven = [WeightDomain::NonNegative {
            proof: asap_types::post_asap::NonNegativeWeightProof::CounterSamples,
        }];
        let selection = run.stage3_plan_selection.as_ref().unwrap();
        assert_eq!(domains(&selection.plan), proven, "{selection:?}");
        assert_eq!(domains(run.selected_logical().unwrap()), proven);
    }

    /// The selected plan's open requirement becomes a proof when confirmed
    /// and stays unproven otherwise; the executor reads no data workload.
    #[test]
    fn confirmed_weights_rewrite_only_the_summary_update() {
        use asap_types::post_asap::NonNegativeWeightProof;
        use asap_types::workload::MetricType;
        let p2 = &candidates()[1];
        let domains = |roots: &[QueryRoot]| -> Vec<WeightDomain> {
            roots
                .iter()
                .flat_map(root_operators)
                .flat_map(OperatorNode::reachable)
                .filter_map(|n| match n.asap() {
                    Some(ASAPOp::SummaryAgg { input, .. }) => Some(input.weight_domain.clone()),
                    _ => None,
                })
                .collect()
        };
        assert_eq!(
            domains(&p2.roots),
            [WeightDomain::SamplesOfMetric { metric: "m".into() }]
        );
        let counter = with_metric_types(data(), &[("m", MetricType::Counter)]);
        let confirmed = with_confirmed_weights(&p2.roots, &counter);
        assert_eq!(
            domains(&confirmed),
            [WeightDomain::NonNegative {
                proof: NonNegativeWeightProof::CounterSamples
            }]
        );
        assert_eq!(
            domains(&with_confirmed_weights(&p2.roots, &data())),
            [WeightDomain::UnknownOrSigned]
        );
        // Nothing open: the same roots.
        let exact = &candidates()[0];
        let QueryRoot::Operator(before) = &exact.roots[0] else {
            panic!()
        };
        let QueryRoot::Operator(after) = &with_confirmed_weights(&exact.roots, &counter)[0] else {
            panic!()
        };
        assert!(Rc::ptr_eq(before, after));
    }

    /// Each node is priced under its DAG id, and the total is the sum;
    /// every candidate is either selected or rejected as costlier. The
    /// CountSketch+heap estimate has no analytical guarantee, so deployment
    /// evidence admits it.
    #[test]
    fn per_node_costs_cover_the_dag_and_sum_to_the_total() {
        let candidates = candidates();
        let target = AccuracyTarget::max_error(0.01).failure_probability(0.001);
        let selection = select_plan(
            &candidates,
            &[entry_with(
                target
                    .clone()
                    .accuracy_metric(AccuracyMetric::ABSOLUTE_STREAM_L1),
                repeating(10_000),
            )],
            &data(),
            PlanningModels::builtin().with_evidence(&EveryOutputMeasuredExact),
        )
        .unwrap();
        for candidate in candidates.iter().filter(|c| c.id != "P2") {
            let cost = &selection.costs[&candidate.id];
            let ids: Vec<_> = candidate.dag.nodes.iter().map(|n| n.id).collect();
            let mut keys: Vec<_> = cost.per_node.keys().copied().collect();
            keys.sort();
            let mut sorted = ids.clone();
            sorted.sort();
            assert_eq!(keys, sorted);
            let sum: f64 = cost
                .per_node
                .values()
                .map(|n| n.cost_per_second.unwrap())
                .sum();
            assert_eq!(cost.cost_per_second.unwrap(), sum);
            assert!(cost.cost_per_second.unwrap() > 0.0);
        }
        let costlier: Vec<_> = selection
            .rejected
            .iter()
            .filter(|r| r.cause == RejectionCause::Costlier)
            .collect();
        assert_eq!(costlier.len(), 1);
        assert_ne!(costlier[0].id, selection.selected);
    }

    /// Query 1 is `sum(...)` over query 2's output. Every node reached by
    /// both queries is `NotMaterialized`, so recomputed for each: charged
    /// twice. Materializing query 2's root (`MatAtQuery`) charges it once,
    /// and the nodes below it once, since they run only when it does.
    #[test]
    fn not_materialized_shared_nodes_are_charged_per_query() {
        let outer = lower_promql("sum(sum_over_time(m[1m]))", AccuracyTarget::exact());
        let inner = outer.children()[0].clone();
        let mut candidate = optimize_physical_asap(
            "L",
            &[QueryRoot::Operator(outer), QueryRoot::Operator(inner)],
            &[],
            &data(),
            &DeploymentCapabilities::default(),
        )
        .unwrap()
        .remove(0);
        let [outer, inner] = candidate.dag.roots[..] else {
            panic!("two roots")
        };
        // Kept state is priced apart; this compares computations only.
        let settings = unit_costs(|c| c.cost_per_memory_byte_second = 0.0);
        let cost = price(&candidate.dag, &recurrences(2), &data(), &settings).unwrap();
        let multipliers: BTreeMap<_, _> = cost
            .per_node
            .iter()
            .map(|(id, c)| (*id, c.multiplier))
            .collect();
        assert_eq!(multipliers[&outer], 1);
        assert_eq!(multipliers[&inner], 2);
        assert!(multipliers.len() > 2, "nodes below the shared root");
        for (id, times) in &multipliers {
            if *id != outer {
                assert_eq!(*times, 2, "node {id} is read by both queries");
            }
        }

        let node = candidate
            .dag
            .nodes
            .iter_mut()
            .find(|n| n.id == inner)
            .unwrap();
        node.materialization = Materialization::MatAtQuery(asap_types::ir::Medium::Memory);
        let stored = price(&candidate.dag, &recurrences(2), &data(), &settings).unwrap();
        for (id, c) in &stored.per_node {
            assert_eq!(c.multiplier, 1, "node {id} is computed once");
            let once = cost.per_node[id].cost_per_second.unwrap() / multipliers[id] as f64;
            assert!(
                (c.cost_per_second.unwrap() - once).abs() <= 1e-12 * once,
                "node {id}"
            );
        }
        assert!(stored.cost_per_second.unwrap() < cost.cost_per_second.unwrap());
    }

    /// Every combination of Pass 1 candidates of one query, as Stage 2
    /// candidates `P1`, `P2`, ...
    fn all_candidates(query: &str, target: &AccuracyTarget) -> Vec<PhysicalCandidate> {
        let root = lower_promql(query, target.clone());
        let inventory = crate::per_computation_candidates::generate_per_computation_candidates(
            vec![(0, QueryRoot::Operator(root))],
        )
        .unwrap();
        let mut choices = vec![vec![]];
        for target in &inventory.targets {
            choices = choices
                .into_iter()
                .flat_map(|choice: Vec<usize>| {
                    (0..target.choice_count()).map(move |index| {
                        let mut choice = choice.clone();
                        choice.push(index);
                        choice
                    })
                })
                .collect();
        }
        choices
            .iter()
            .enumerate()
            .map(|(index, choice)| {
                let roots: Vec<_> = crate::per_computation_candidates::compose_logical_candidate(
                    &inventory, choice,
                )
                .unwrap()
                .into_iter()
                .map(|(_, root)| root)
                .collect();
                let mut candidate = optimize_physical_asap(
                    "L",
                    &roots,
                    &[],
                    &data(),
                    &DeploymentCapabilities::default(),
                )
                .unwrap()
                .remove(0);
                candidate.id = format!("P{}", index + 1);
                candidate
            })
            .collect()
    }

    fn has_summary_estimate(candidate: &PhysicalCandidate) -> bool {
        candidate
            .dag
            .nodes
            .iter()
            .any(|n| matches!(n.payload, Payload::ASAP(ASAPOp::SummaryEstimate { .. })))
    }

    /// The KLL candidates of `quantile_over_time(0.99, x[5m])`, per series,
    /// within rank error 0.01 at δ = 0.01 over `scope`.
    fn per_series_kll(scope: AccuracyScope) -> (Vec<PhysicalCandidate>, AccuracyTarget) {
        let requested = AccuracyTarget::max_error(0.01)
            .failure_probability(0.01)
            .accuracy_metric(AccuracyMetric::Rank);
        let candidates = all_candidates("quantile_over_time(0.99, x[5m])", &requested)
            .into_iter()
            .filter(|c| !kll_params(&c.roots).is_empty())
            .collect::<Vec<_>>();
        assert!(!candidates.is_empty());
        let target = requested
            .clone()
            .accuracy_metric(AccuracyMetric::Rank)
            .accuracy_scope(scope);
        (candidates, target)
    }

    /// A candidate whose summaries, resized for its query's failure
    /// probability, do not rebuild into a plan (here a root with no
    /// execution timing) is rejected with the reason; selection goes on and
    /// selects another candidate instead of failing.
    #[test]
    fn a_candidate_that_fails_to_resize_is_rejected_alone() {
        let requested = AccuracyTarget::max_error(0.01)
            .failure_probability(0.01)
            .accuracy_metric(AccuracyMetric::Rank);
        let mut candidates = all_candidates("quantile_over_time(0.99, x[5m])", &requested);
        let broken = candidates
            .iter()
            .position(|c| !kll_params(&c.roots).is_empty())
            .expect("a KLL candidate");
        let QueryRoot::Operator(node) = &candidates[broken].roots[0] else {
            panic!("an operator root")
        };
        let mut untimed = (**node).clone();
        untimed.timing = None;
        candidates[broken].roots = vec![QueryRoot::Operator(Rc::new(untimed))];
        let selection = select_plan(
            &candidates,
            &[entry_with(requested, repeating(10_000))],
            &data(),
            PlanningModels::builtin(),
        )
        .expect("the other candidates are still judged");
        let rejection = selection
            .rejected
            .iter()
            .find(|r| r.id == candidates[broken].id)
            .unwrap();
        assert_eq!(
            rejection.cause,
            RejectionCause::FailureProbabilityResizeFailed
        );
        assert!(
            rejection
                .reason
                .starts_with("resizing for the failure probability failed"),
            "{}",
            rejection.reason
        );
        assert_ne!(selection.selected, candidates[broken].id);
    }

    /// Bad input: a workload with a different number of entries than a
    /// candidate has query roots is an error, not a rejection.
    #[test]
    fn an_entry_count_mismatch_is_an_error() {
        let target = AccuracyTarget::max_error(0.01).failure_probability(0.001);
        let entries = vec![entry_with(target, repeating(10_000)); 2];
        let Err(SelectionError::EntryCount {
            candidate,
            roots,
            entries,
        }) = select_plan(&candidates(), &entries, &data(), PlanningModels::builtin())
        else {
            panic!("an entry count mismatch is an error")
        };
        assert_eq!((candidate.as_str(), roots, entries), ("P1", 1, 2));
    }

    fn kll_params(roots: &[QueryRoot]) -> Vec<SketchParams> {
        roots
            .iter()
            .flat_map(root_operators)
            .flat_map(OperatorNode::reachable)
            .filter_map(|node| match node.asap() {
                Some(ASAPOp::SummaryAgg {
                    family: FieldDataType::Sketch(kind, _),
                    ..
                }) if *kind.algorithm() == SketchAlgorithm::Kll => Some(kind.params().clone()),
                _ => None,
            })
            .collect()
    }

    /// A whole-result target over `G` = 10 000 series (the declared input
    /// cardinality) needs each series' KLL to fail with probability at most
    /// δ / G = 1e-6 (#669b): Stage 3 enlarges it before pricing, and the
    /// selected plan runs the enlarged sketch. A per-value target keeps the
    /// Stage 1 size.
    #[test]
    fn a_whole_result_target_enlarges_each_sketch_for_g_values() {
        let (candidates, whole) = per_series_kll(AccuracyScope::WholeResult);
        let select = |target: &AccuracyTarget| {
            select_plan(
                &candidates,
                &[entry_with(target.clone(), repeating(10_000))],
                &data(),
                PlanningModels::builtin(),
            )
            .unwrap()
        };
        let stage1 = kll_params(&candidates[0].roots);
        let enlarged = kll_params(&select(&whole).plan);
        assert_eq!(enlarged.len(), 1);
        let (
            SketchParams::Kll { k: k1, .. },
            SketchParams::Kll {
                k,
                failure_probability,
            },
        ) = (&stage1[0], &enlarged[0])
        else {
            panic!("{stage1:?} {enlarged:?}")
        };
        assert_eq!(*failure_probability, Some(0.01 / 10_000.0));
        assert!(k > k1, "{k} vs {k1}");
        let per_value = whole.clone().accuracy_scope(AccuracyScope::PerValue);
        assert_eq!(kll_params(&select(&per_value).plan), stage1);
    }

    /// With its metric declared a counter, the whole-expression CMS+heap
    /// candidate (P2) proves its weights non-negative, is valid and is
    /// priced: its per-node costs cover its DAG and sum to its total.
    #[test]
    fn a_declared_counter_prices_the_count_min_heap_candidate() {
        use asap_types::workload::MetricType;
        let candidates = candidates();
        let target = AccuracyTarget::max_error(0.01)
            .failure_probability(0.001)
            .accuracy_metric(AccuracyMetric::ABSOLUTE_STREAM_L1);
        let counter = with_metric_types(data(), &[("m", MetricType::Counter)]);
        let selection = select_plan(
            &candidates,
            &[entry_with(target, repeating(10_000))],
            &counter,
            PlanningModels::builtin().with_evidence(&EveryOutputMeasuredExact),
        )
        .unwrap();
        let p2 = candidates.iter().find(|c| c.id == "P2").unwrap();
        let cost = &selection.costs["P2"];
        let mut ids: Vec<_> = p2.dag.nodes.iter().map(|n| n.id).collect();
        ids.sort();
        let mut keys: Vec<_> = cost.per_node.keys().copied().collect();
        keys.sort();
        assert_eq!(keys, ids);
        let sum: f64 = cost
            .per_node
            .values()
            .map(|n| n.cost_per_second.unwrap())
            .sum();
        assert_eq!(cost.cost_per_second.unwrap(), sum);
    }

    /// One summary two queries read is sized for the stricter one: its
    /// failure probability is the smallest share either query needs (the
    /// largest `n · G` over its readers).
    #[test]
    fn a_shared_summary_takes_the_smallest_failure_probability_share() {
        let (candidates, lenient) = per_series_kll(AccuracyScope::PerValue);
        let root = candidates[0].roots[0].clone();
        let strict = lenient.clone().failure_probability(0.001);
        let roots = [root.clone(), root];
        for targets in [[lenient.clone(), strict.clone()], [strict, lenient]] {
            let resized = resize_for_failure_probability(&roots, &targets, &data()).unwrap();
            let kll = kll_params(&resized);
            assert_eq!(kll.len(), 2, "{kll:?}");
            for params in kll {
                let SketchParams::Kll {
                    failure_probability,
                    ..
                } = params
                else {
                    panic!("{params:?}")
                };
                assert_eq!(failure_probability, Some(0.001));
            }
        }
    }

    /// Without a bound on the values one evaluation returns, a per-value
    /// guarantee does not meet a whole-result target: a PromQL vector whose
    /// source declares no series count, and a SQL relation (until a bound on
    /// its groups is an input). A scalar returns one value.
    #[test]
    fn an_unbound_number_of_values_leaves_a_whole_result_target_unmet() {
        let (candidates, whole) = per_series_kll(AccuracyScope::WholeResult);
        let no_series_count = with_series(None);
        let Err(SelectionError::NoValidCandidate(rejected)) = select_plan(
            &candidates,
            &[entry_with(whole, repeating(10_000))],
            &no_series_count,
            PlanningModels::builtin(),
        ) else {
            panic!("the KLL candidates are invalid")
        };
        assert!(
            rejected[0].reason.contains("is not bound"),
            "{}",
            rejected[0].reason
        );
        let table = crate::test_support::scan_from(
            asap_types::pre_asap::Source::Table {
                table_ref: "t".into(),
            },
            Schema::new(vec![asap_types::pre_asap::schema::Field::plain(
                "v",
                DataType::Float64,
                false,
            )]),
        );
        assert_eq!(values_of(&QueryRoot::Operator(table), &data()), None);
        assert_eq!(
            values_of(
                &QueryRoot::Scalar(ScalarExpr::literal_f64(1.0)),
                &DataWorkload::default()
            ),
            Some(1)
        );
    }

    /// A root that returns at most one row has `G` = 1, provable from the
    /// query with no data workload: a global aggregate, PromQL or SQL, and
    /// what selects, renames or joins single rows above it. A grouped SQL
    /// aggregate stays unbound, and a grouped PromQL one, or a global top-k
    /// (k rows), is bound by the series count of the source it reads.
    #[test]
    fn a_root_of_at_most_one_row_returns_one_value() {
        let none = DataWorkload::default();
        let root =
            |query: &str| QueryRoot::Operator(lower_promql(query, AccuracyTarget::max_error(0.01)));
        assert_eq!(values_of(&root("sum(rate(x[5m]))"), &none), Some(1));
        assert_eq!(values_of(&root("topk(5, rate(x[5m]))"), &none), None);
        assert_eq!(values_of(&root("sum by (job) (rate(x[5m]))"), &none), None);
        assert_eq!(
            values_of(&root("sum by (job) (rate(x[5m]))"), &data()),
            Some(10_000)
        );
        let table = || {
            crate::test_support::scan_from(
                asap_types::pre_asap::Source::Table {
                    table_ref: "t".into(),
                },
                Schema::new(vec![
                    asap_types::pre_asap::schema::Field::plain("k", DataType::Utf8, false),
                    asap_types::pre_asap::schema::Field::plain("v", DataType::Float64, false),
                ]),
            )
        };
        let count = || AggIntent::Count {
            accuracy: AccuracyTarget::exact(),
        };
        let global = crate::test_support::agg(vec![], count(), table());
        let grouped = crate::test_support::agg(vec![0], count(), table());
        assert_eq!(
            values_of(&QueryRoot::Operator(global.clone()), &none),
            Some(1)
        );
        assert_eq!(values_of(&QueryRoot::Operator(grouped), &data()), None);
        let cross = OperatorNode::new_shared(Operator::NonASAP(NonASAPOp::Join {
            kind: asap_types::ir::operator_properties::JoinKind::Cross,
            pred: asap_types::ir::Predicate(ScalarExpr::Literal(ScalarValue::Boolean(true))),
            left: global.clone(),
            right: global,
        }))
        .unwrap();
        assert_eq!(values_of(&QueryRoot::Operator(cross), &none), Some(1));
    }

    /// `G` and the input row count are bound per query from the sources
    /// it reads: another source's series do not count, a query over two
    /// sources is bound by their sum, and one source without a series
    /// count leaves it unbound. Pricing reads the same per-source series, with
    /// the default for an undeclared source.
    #[test]
    fn values_per_result_is_bound_by_the_series_of_the_sources_read() {
        let root =
            |query: &str| QueryRoot::Operator(lower_promql(query, AccuracyTarget::max_error(0.01)));
        let series = |declared: &[(&str, u64)]| DataWorkload {
            sources: declared
                .iter()
                .map(|(source, series)| {
                    let facts = SourceFacts {
                        series_count: Some(*series),
                        ..SourceFacts::default()
                    };
                    (source.to_string(), facts)
                })
                .collect(),
            ..DataWorkload::default()
        };
        let x = root("sum by (job) (rate(x[5m]))");
        let both = root("sum by (job) (rate(x[5m])) + sum by (job) (rate(m[5m]))");
        assert_eq!(values_of(&x, &series(&[("x", 10)])), Some(10));
        assert_eq!(values_of(&x, &series(&[("m", 10)])), None);
        let declared = series(&[("x", 10), ("m", 20)]);
        assert_eq!(values_of(&x, &declared), Some(10));
        assert_eq!(values_of(&both, &declared), Some(30));
        assert_eq!(series_read(&both, &declared), Some(30));
        assert_eq!(values_of(&both, &series(&[("x", 10)])), None);
        // Pricing sums the series of the sources a node reads; an
        // undeclared source counts the default.
        assert_eq!(priced_series(&["x", "m"].into(), &declared), 30);
        assert_eq!(
            priced_series(&["x", "y"].into(), &declared),
            10 + DEFAULT_SERIES
        );
        assert_eq!(priced_series(&BTreeSet::new(), &declared), DEFAULT_SERIES);
    }

    /// `G` counts the values the root can return: an `or` returns the
    /// values of both branches (here up to `series(x)` series plus the
    /// groups of `x`), a constant vector one more, while a join or a fold
    /// of one source counts it once.
    #[test]
    fn values_per_result_adds_the_branches_of_an_or() {
        let root =
            |query: &str| QueryRoot::Operator(lower_promql(query, AccuracyTarget::max_error(0.01)));
        let data = with_series(Some(10));
        for (query, values) in [
            ("rate(x[5m]) or sum without (instance) (rate(x[5m]))", 20),
            ("rate(x[5m]) or vector(0)", 11),
            ("rate(x[5m]) or rate(m[5m])", 20),
            ("(rate(x[5m]) or vector(0)) or vector(1)", 12),
            ("sum by (job) (rate(x[5m]) or rate(x[5m]))", 20),
            ("rate(x[5m]) / rate(x[5m])", 10),
            ("sum without (instance) (rate(x[5m])) * rate(x[5m])", 10),
            ("rate(x[5m]) + rate(m[5m])", 20),
        ] {
            assert_eq!(values_of(&root(query), &data), Some(values), "{query}");
        }
    }

    /// `vector(s)` returns one row, but the operators below it still read
    /// their sources' rows: the input row count of
    /// `vector(scalar(max(rate(x[5m]))))` counts the 10 series the `max`
    /// folds (plus the one constant row), never shrinking going up the tree,
    /// while its `G` is 1.
    #[test]
    fn a_vector_of_a_scalar_keeps_the_rows_read_below_it() {
        let root =
            |query: &str| QueryRoot::Operator(lower_promql(query, AccuracyTarget::max_error(0.01)));
        let data = with_series(Some(10));
        let vector = root("vector(scalar(max(rate(x[5m]))))");
        assert_eq!(series_read(&vector, &data), Some(11));
        assert_eq!(values_of(&vector, &data), Some(1));
        assert_eq!(values_of(&root("vector(1)"), &data), Some(1));
        let or = root("rate(x[5m]) or vector(scalar(max(rate(x[5m]))))");
        assert_eq!(series_read(&or, &data), Some(21));
    }

    /// Deployment evidence that every output was measured exact.
    struct EveryOutputMeasuredExact;

    impl AccuracyEvidenceProvider for EveryOutputMeasuredExact {
        fn output_guarantee(&self, _output: &QueryRoot) -> Option<AccuracyGuarantee> {
            Some(AccuracyGuarantee::exact("measured"))
        }
    }

    /// Deployment evidence of a small error in a fixed metric for every
    /// output that is not exact.
    struct EveryOutputMeasuredIn(AccuracyMetric);

    impl AccuracyEvidenceProvider for EveryOutputMeasuredIn {
        fn output_guarantee(&self, _output: &QueryRoot) -> Option<AccuracyGuarantee> {
            Some(AccuracyGuarantee::new(
                self.0,
                BoundExpr::Constant { value: 1e-6 },
                ProbabilityExpr::Constant { value: 1e-6 },
                vec![],
            ))
        }
    }

    /// A bound stated in another metric misses the target however small it
    /// is: a relative bound does not bound a top-k frequency's L1-scaled
    /// error, so only the exact candidate stays valid; the same evidence in
    /// the target's metric admits the sketches.
    #[test]
    fn a_bound_in_another_metric_misses_the_target() {
        let counter = with_metric_types(data(), &[("m", MetricType::Counter)]);
        let target = AccuracyTarget::max_error(0.5)
            .failure_probability(0.5)
            .accuracy_metric(AccuracyMetric::ABSOLUTE_STREAM_L1);
        let select = |metric| {
            select_plan(
                &candidates(),
                &[entry_with(target.clone(), repeating(10_000))],
                &counter,
                PlanningModels::builtin().with_evidence(&EveryOutputMeasuredIn(metric)),
            )
            .unwrap()
        };
        let relative = select(AccuracyMetric::Relative);
        assert_eq!(relative.selected, "P1");
        let mut invalid: Vec<_> = relative
            .rejected
            .iter()
            .filter(|r| r.cause.failed_a_check())
            .collect();
        invalid.sort_by(|a, b| a.id.cmp(&b.id));
        assert_eq!(
            invalid.iter().map(|r| r.id.as_str()).collect::<Vec<_>>(),
            ["P2", "P3"]
        );
        for rejected in invalid {
            assert!(
                rejected.reason.contains("no evidence that it meets"),
                "{}",
                rejected.reason
            );
        }
        let l1 = select(AccuracyMetric::ABSOLUTE_STREAM_L1);
        assert!(
            l1.rejected.iter().all(|r| !r.cause.failed_a_check()),
            "{:?}",
            l1.rejected
        );
    }

    /// A difference of two quantile estimates has no output guarantee, though
    /// each estimate is certified: its summary candidates stay listed as
    /// invalid with a reason. The deployment's empirical accuracy for the
    /// output admits them.
    #[test]
    fn combined_estimates_need_evidence_about_the_output() {
        let target = AccuracyTarget::max_error(0.01).failure_probability(0.001);
        let candidates = all_candidates(
            "quantile_over_time(0.99, m[1m]) - quantile_over_time(0.5, m[1m])",
            &target,
        );
        let summaries: Vec<_> = candidates
            .iter()
            .filter(|c| has_summary_estimate(c))
            .map(|c| c.id.as_str())
            .collect();
        assert!(!summaries.is_empty());

        let builtin = select_plan(
            &candidates,
            &[entry_with(
                target.clone().accuracy_metric(AccuracyMetric::ABSOLUTE),
                repeating(10_000),
            )],
            &data(),
            PlanningModels::builtin(),
        )
        .unwrap();
        for id in &summaries {
            let rejection = builtin.rejected.iter().find(|r| r.id == *id).unwrap();
            assert_eq!(rejection.cause, RejectionCause::AccuracyTargetMissed);
            assert!(
                rejection.reason.contains("no guarantee"),
                "{}",
                rejection.reason
            );
        }
        assert!(!summaries.contains(&builtin.selected.as_str()));

        let measured = select_plan(
            &candidates,
            &[entry_with(
                target.clone().accuracy_metric(AccuracyMetric::ABSOLUTE),
                repeating(10_000),
            )],
            &data(),
            PlanningModels::builtin().with_evidence(&EveryOutputMeasuredExact),
        )
        .unwrap();
        for id in &summaries {
            assert!(measured.costs.contains_key(*id), "{id} is valid");
        }
    }

    /// `candidates` with every state node (a summary build or an exact
    /// aggregate) kept at ingestion time, so selection charges its kept
    /// bytes every second (a `MatAtQuery` state is held only for one
    /// evaluation).
    fn keeping_state(mut candidates: Vec<PhysicalCandidate>) -> Vec<PhysicalCandidate> {
        for candidate in &mut candidates {
            for node in &mut candidate.dag.nodes {
                if matches!(
                    node.payload,
                    Payload::ASAP(ASAPOp::SummaryAgg { .. })
                        | Payload::NonASAP(NonASAPOp::Aggregate { .. })
                ) {
                    node.materialization =
                        Materialization::MatAtIngestion(asap_types::ir::Medium::Memory);
                }
            }
        }
        candidates
    }

    /// Kept exact quantile state keeps every value of its group: over one
    /// group of 10 000 values it costs more than a quantile sketch sized for
    /// ε = 0.01, δ = 0.01, which is selected; over 100 groups of 100 values it
    /// is smaller than the sketch and is selected. (State held only while a
    /// query runs is not charged.)
    #[test]
    fn exact_quantile_state_costs_more_than_a_sketch_over_large_groups() {
        let target = AccuracyTarget::max_error(0.01).failure_probability(0.01);
        for (query, sketch_wins) in [
            ("quantile(0.99, rate(x[5m]))", true),
            ("quantile by (job) (0.99, rate(x[5m]))", false),
        ] {
            let candidates = keeping_state(all_candidates(query, &target));
            let selection = select_plan(
                &candidates,
                &[entry_with(
                    target.clone().accuracy_metric(AccuracyMetric::Rank),
                    repeating(10_000),
                )],
                &data(),
                PlanningModels::builtin(),
            )
            .unwrap();
            let selected = candidates
                .iter()
                .find(|c| c.id == selection.selected)
                .unwrap();
            assert_eq!(
                has_summary_estimate(selected),
                sketch_wins,
                "{query}: {selection:?}"
            );
        }
    }

    /// Exact L2 state keeps one counter per distinct value of its group, so
    /// it is priced like exact quantile state, which keeps every value.
    #[test]
    fn exact_l2_state_is_priced_like_exact_quantile_state() {
        let aggregate_cost = |query: &str| {
            let candidates = all_candidates(query, &AccuracyTarget::exact());
            let selection = select_plan(
                &candidates,
                &[entry(repeating(10_000))],
                &data(),
                PlanningModels::builtin(),
            )
            .unwrap();
            let exact = candidates
                .iter()
                .find(|c| !has_summary_estimate(c))
                .unwrap();
            selection.costs[&exact.id]
                .per_node
                .values()
                .find(|n| n.detail.starts_with("hash aggregate"))
                .unwrap()
                .cost_per_second
        };
        assert_eq!(
            aggregate_cost("l2_over_time(x[5m])"),
            aggregate_cost("quantile_over_time(0.99, x[5m])")
        );
    }

    /// A DDSketch is sized for values over nine decades: 1,037 bins at
    /// α = 0.01, 8 bytes each.
    #[test]
    fn ddsketch_bins_span_nine_decades() {
        assert_eq!(ddsketch_bins(0.01), 1_037);
        assert_eq!(
            summary_shape(&FieldDataType::Sketch(
                SketchKind::new(
                    SketchAlgorithm::DDSketch,
                    SketchParams::DDSketch { alpha: 0.01 }
                ),
                GroupingStrategy::default(),
            ))
            .1,
            8 * 1_037
        );
    }

    /// DDSketch state grows as its relative accuracy α tightens.
    #[test]
    fn ddsketch_state_grows_as_alpha_shrinks() {
        let bytes = |alpha| {
            summary_shape(&FieldDataType::Sketch(
                SketchKind::new(SketchAlgorithm::DDSketch, SketchParams::DDSketch { alpha }),
                GroupingStrategy::default(),
            ))
            .1
        };
        assert!(
            bytes(0.1) < bytes(0.01),
            "{} vs {}",
            bytes(0.1),
            bytes(0.01)
        );
        assert!(
            bytes(0.01) < bytes(0.001),
            "{} vs {}",
            bytes(0.01),
            bytes(0.001)
        );
    }

    /// Every sketch family's state is sized from its parameters, as
    /// `asap_sketchlib` allocates it.
    #[test]
    fn summary_state_is_sized_from_sketch_params() {
        let bytes = |algorithm, params| {
            summary_shape(&FieldDataType::Sketch(
                SketchKind::new(algorithm, params),
                GroupingStrategy::default(),
            ))
            .1
        };
        assert_eq!(
            bytes(
                SketchAlgorithm::Hll,
                SketchParams::Hll {
                    precision: 14,
                    relative_error: None
                }
            ),
            1 << 14
        );
        assert_eq!(
            bytes(
                SketchAlgorithm::Kmv,
                SketchParams::Kmv {
                    k: 1_000,
                    relative_error: None
                }
            ),
            8 * 1_000
        );
        assert_eq!(
            bytes(
                SketchAlgorithm::UnivMon,
                SketchParams::UnivMon {
                    heap_size: 256,
                    sketch_rows: 5,
                    sketch_cols: 1_024,
                    layers: 16,
                    distinct_failure_probability: None,
                }
            ),
            16 * (8 * 5 * 1_024 + 24 * 256)
        );
    }

    /// KLL state is the item array `asap_sketchlib` allocates up front:
    /// `compute_max_capacity(k, 8)` 8-byte items, with `k` capped at 26 602.
    #[test]
    fn kll_state_is_sized_as_asap_sketchlib_allocates_it() {
        let bytes = |k| {
            summary_shape(&FieldDataType::Sketch(
                SketchKind::new(
                    SketchAlgorithm::Kll,
                    SketchParams::Kll {
                        k,
                        failure_probability: None,
                    },
                ),
                GroupingStrategy::default(),
            ))
            .1
        };
        assert_eq!(bytes(269), 8 * 1_206);
        assert_eq!(bytes(26_602), 8 * 80_118);
        assert_eq!(bytes(30_000), 8 * 80_118);
    }

    /// A UnivMon row updates layers `0..=b`, where its bottom layer `b` is
    /// geometric (`asap_sketchlib`'s `bottom_layer_for_hash`), so
    /// `2 - 2^(1 - layers)` layers in expectation; each updates one counter
    /// per sketch row and its heap.
    #[test]
    fn univmon_update_cost_is_expected_layers_times_rows_and_heap() {
        let ops = |layers| {
            summary_shape(&FieldDataType::Sketch(
                SketchKind::new(
                    SketchAlgorithm::UnivMon,
                    SketchParams::UnivMon {
                        heap_size: 256,
                        sketch_rows: 5,
                        sketch_cols: 1_024,
                        layers,
                        distinct_failure_probability: None,
                    },
                ),
                GroupingStrategy::default(),
            ))
            .0
        };
        assert_eq!(ops(1), 6.0);
        assert_eq!(ops(2), 1.5 * 6.0);
        assert_eq!(ops(16), (2.0 - 2f64.powi(-15)) * 6.0);
    }

    /// Kept exact distinct-count state keeps every item of its group: over
    /// 10 000 groups of 300 rows it is smaller than a distinct-count sketch
    /// sized for ε = 0.01 and is selected; over one group of 3 000 000 rows a
    /// sketch is.
    #[test]
    fn exact_distinct_count_beats_a_sketch_over_small_groups() {
        let target = AccuracyTarget::max_error(0.01).failure_probability(0.01);
        let one_series = with_series(Some(1));
        for (data, sketch_wins) in [(data(), false), (one_series, true)] {
            let candidates = keeping_state(all_candidates("distinct_over_time(x[5m])", &target));
            let selection = select_plan(
                &candidates,
                &[entry_with(
                    target.clone().accuracy_metric(AccuracyMetric::Relative),
                    repeating(10_000),
                )],
                &data,
                PlanningModels::builtin(),
            )
            .unwrap();
            let selected = candidates
                .iter()
                .find(|c| c.id == selection.selected)
                .unwrap();
            assert_eq!(has_summary_estimate(selected), sketch_wins, "{selection:?}");
        }
    }

    /// A candidate that ties the selected one is rejected for losing on
    /// enumeration order, not as costlier.
    #[test]
    fn a_tie_is_reported_as_lost_on_enumeration_order() {
        let mut candidates = candidates();
        candidates.truncate(1);
        let mut twin = candidates[0].clone();
        twin.id = "P1-twin".into();
        candidates.push(twin);
        let selection = select_plan(
            &candidates,
            &[entry(repeating(10_000))],
            &data(),
            PlanningModels::builtin(),
        )
        .unwrap();
        assert_eq!(selection.selected, "P1");
        let reason = &selection.rejected[0].reason;
        assert!(
            reason.contains("tied with the selected candidate P1")
                && reason.contains("enumeration order"),
            "{reason}"
        );
    }

    /// At δ = 0.001 a KLL candidate meets the target and is valid.
    #[test]
    fn kll_candidate_meets_a_failure_probability_below_one_percent() {
        let target = AccuracyTarget::max_error(0.01).failure_probability(0.001);
        let candidates = all_candidates("quantile(0.99, rate(x[5m]))", &target);
        let selection = select_plan(
            &candidates,
            &[entry_with(
                target.clone().accuracy_metric(AccuracyMetric::Rank),
                repeating(10_000),
            )],
            &data(),
            PlanningModels::builtin(),
        )
        .unwrap();
        let kll = candidates
            .iter()
            .find(|c| {
                c.dag.nodes.iter().any(|n| {
                    matches!(&n.payload, Payload::ASAP(ASAPOp::SummaryAgg {
                        family: FieldDataType::Sketch(kind, _), ..
                    }) if *kind.algorithm() == SketchAlgorithm::Kll)
                })
            })
            .unwrap();
        assert!(selection.costs.contains_key(&kll.id), "{selection:?}");
    }

    /// `scalar(v)` inherits `v`'s guarantee; arithmetic over it composes
    /// (`c · x` scales an absolute bound, `x − y` adds them); a rank error
    /// does not compose (a rank interval counts as none); one over exact
    /// operands only is exact.
    #[test]
    fn scalar_root_guarantees_compose_through_arithmetic() {
        let node = lower_promql("sum(m)", AccuracyTarget::exact());
        let approximate = AccuracyGuarantee::new(
            AccuracyMetric::ABSOLUTE,
            BoundExpr::Constant { value: 0.01 },
            ProbabilityExpr::Constant { value: 0.001 },
            vec![],
        );
        let with = |guarantee: Option<AccuracyGuarantee>| {
            ScalarExpr::PromqlScalarFromVector(Rc::new((*node).clone().with_guarantee(guarantee)))
        };
        let doubled = |operand: ScalarExpr| {
            QueryRoot::Scalar(ScalarExpr::Arithmetic {
                op: ArithmeticOpKind::Mul,
                left: Box::new(operand),
                right: Box::new(ScalarExpr::literal_f64(2.0)),
                semantics: ExprSemantics::Promql,
            })
        };
        assert_eq!(
            root_guarantee(&QueryRoot::Scalar(with(Some(approximate.clone())))),
            Some(approximate.clone())
        );
        let scaled = root_guarantee(&doubled(with(Some(approximate.clone())))).unwrap();
        assert_eq!(scaled.spec.accuracy_metric, AccuracyMetric::ABSOLUTE);
        assert!((scaled.spec.error.evaluate().unwrap() - 0.02).abs() < 1e-12);
        assert_eq!(scaled.spec.failure_probability.evaluate(), Some(0.001));
        let difference = QueryRoot::Scalar(ScalarExpr::Arithmetic {
            op: ArithmeticOpKind::Sub,
            left: Box::new(with(Some(approximate.clone()))),
            right: Box::new(with(Some(approximate.clone()))),
            semantics: ExprSemantics::Promql,
        });
        let sum = root_guarantee(&difference).unwrap();
        assert!((sum.spec.error.evaluate().unwrap() - 0.02).abs() < 1e-12);
        let mut rank = approximate;
        rank.spec.accuracy_metric = AccuracyMetric::Rank;
        assert_eq!(root_guarantee(&doubled(with(Some(rank)))), None);
        assert!(
            root_guarantee(&doubled(with(Some(AccuracyGuarantee::exact("exact")))))
                .is_some_and(|g| g.is_exact())
        );
    }

    /// Every candidate of the Stage 1 to Stage 3 run of one query.
    fn run(
        query: &str,
        target: &AccuracyTarget,
        data: &DataWorkload,
        models: PlanningModels<'_>,
    ) -> (Vec<PhysicalCandidate>, Selection) {
        run_selecting(query, target, target, data, models)
    }

    /// As [`run`], with summaries sized for `planned` and outputs checked
    /// against `selected`.
    fn run_selecting(
        query: &str,
        planned: &AccuracyTarget,
        selected: &AccuracyTarget,
        data: &DataWorkload,
        models: PlanningModels<'_>,
    ) -> (Vec<PhysicalCandidate>, Selection) {
        let root = lower_promql(query, planned.clone());
        let run = crate::pass::run_stage1_to_stage3(
            vec![(0, QueryRoot::Operator(root))],
            &[entry_with(selected.clone(), QueryRecurrence::Unknown)],
            data,
            models,
        )
        .unwrap();
        (
            run.stage2_physical_asap_optimization,
            run.stage3_plan_selection.unwrap(),
        )
    }

    fn sketches(candidate: &PhysicalCandidate) -> Vec<SketchAlgorithm> {
        candidate
            .dag
            .nodes
            .iter()
            .filter_map(|n| match &n.payload {
                Payload::ASAP(ASAPOp::SummaryAgg {
                    family: FieldDataType::Sketch(kind, _),
                    ..
                }) => Some(kind.algorithm().clone()),
                _ => None,
            })
            .collect()
    }

    fn reads_only(candidate: &PhysicalCandidate, algorithm: SketchAlgorithm) -> bool {
        let sketches = sketches(candidate);
        !sketches.is_empty() && sketches.iter().all(|a| *a == algorithm)
    }

    fn output_guarantee(candidate: &PhysicalCandidate) -> Option<AccuracyGuarantee> {
        root_guarantee(&candidate.roots[0])
    }

    /// `max by (job)` over per-series DDSketch quantiles keeps their relative
    /// bound, and `2 · x` keeps it too (scaling preserves a relative error);
    /// so those candidates meet the target. Over KLL (a rank error) the
    /// result is a rank interval: no guarantee in the MVP.
    #[test]
    fn extremum_and_scaling_over_per_series_estimates_compose() {
        let target = AccuracyTarget::max_error(0.01).accuracy_metric(AccuracyMetric::Relative);
        for query in [
            "max by (job) (quantile_over_time(0.99, m[1m]))",
            "2 * max by (job) (quantile_over_time(0.99, m[1m]))",
        ] {
            let (candidates, selection) = run(query, &target, &data(), PlanningModels::builtin());
            let ddsketch: Vec<_> = candidates
                .iter()
                .filter(|c| reads_only(c, SketchAlgorithm::DDSketch))
                .collect();
            assert!(!ddsketch.is_empty(), "{query}");
            for candidate in ddsketch {
                let g = output_guarantee(candidate).expect(query);
                assert_eq!(g.spec.accuracy_metric, AccuracyMetric::Relative, "{query}");
                assert!(g.spec.error.evaluate().unwrap() <= 0.01, "{query}");
                assert!(selection.costs.contains_key(&candidate.id), "{query}");
            }
            for candidate in candidates
                .iter()
                .filter(|c| reads_only(c, SketchAlgorithm::Kll))
            {
                assert_eq!(output_guarantee(candidate), None, "{query}");
            }
        }
    }

    /// Negation, `abs` and `clamp` keep a DDSketch quantile's relative bound,
    /// so those candidates are selectable; a shift by a constant, a
    /// comparison (filter or `bool`) over the estimate compose to no
    /// guarantee in the MVP, so those are not.
    #[test]
    fn lipschitz_functions_keep_and_shifts_and_comparisons_drop_a_relative_bound() {
        let target = AccuracyTarget::max_error(0.01).accuracy_metric(AccuracyMetric::Relative);
        let q = "quantile_over_time(0.99, m[1m])";
        for (query, kept) in [
            (format!("-{q}"), true),
            (format!("abs({q})"), true),
            (format!("clamp({q}, 1, 100)"), true),
            (format!("clamp_max({q}, 100)"), true),
            (format!("{q} + 1"), false),
            (format!("{q} > 5"), false),
            (format!("{q} > bool 5"), false),
        ] {
            let (candidates, selection) = run(&query, &target, &data(), PlanningModels::builtin());
            let ddsketch: Vec<_> = candidates
                .iter()
                .filter(|c| reads_only(c, SketchAlgorithm::DDSketch))
                .collect();
            assert!(!ddsketch.is_empty(), "{query}");
            for candidate in ddsketch {
                let g = output_guarantee(candidate);
                if kept {
                    let g = g.expect(&query);
                    assert_eq!(g.spec.accuracy_metric, AccuracyMetric::Relative, "{query}");
                    assert!(g.spec.error.evaluate().unwrap() <= 0.01, "{query}");
                    assert!(selection.costs.contains_key(&candidate.id), "{query}");
                } else {
                    assert_eq!(g, None, "{query}");
                    assert!(!selection.costs.contains_key(&candidate.id), "{query}");
                }
            }
        }
    }

    /// A time-window fold over a subquery of estimates folds one estimate
    /// per subquery step and series, so its failure probability is scaled
    /// by the step count `⌈range / resolution⌉` (here 10), not by the input
    /// series count. Without an explicit resolution the step count is not in
    /// the query, and the fold has no guarantee.
    #[test]
    fn a_fold_over_subquery_estimates_counts_its_steps() {
        let target = AccuracyTarget::max_error(0.01).accuracy_metric(AccuracyMetric::Relative);
        let q = "quantile_over_time(0.99, m[1m])";
        let (candidates, selection) = run(
            &format!("max_over_time({q}[10m:1m])"),
            &target,
            &data(),
            PlanningModels::builtin(),
        );
        let ddsketch: Vec<_> = candidates
            .iter()
            .filter(|c| reads_only(c, SketchAlgorithm::DDSketch))
            .collect();
        assert!(!ddsketch.is_empty());
        for candidate in ddsketch {
            let g = output_guarantee(candidate).expect("a guarantee");
            assert_eq!(g.spec.accuracy_metric, AccuracyMetric::Relative);
            assert!(g.spec.error.evaluate().unwrap() <= 0.01);
            let ProbabilityExpr::Scaled { count, .. } = &g.spec.failure_probability else {
                panic!("{g:?}");
            };
            assert_eq!(count, &BoundExpr::Constant { value: 10.0 });
            assert!(selection.costs.contains_key(&candidate.id));
        }
        let (candidates, _) = run(
            &format!("max_over_time({q}[10m:])"),
            &target,
            &data(),
            PlanningModels::builtin(),
        );
        for candidate in candidates
            .iter()
            .filter(|c| reads_only(c, SketchAlgorithm::DDSketch))
        {
            assert_eq!(output_guarantee(candidate), None);
        }
    }

    /// Counting estimates by a label keeps the count exact: the group keys
    /// are labels, not the estimates (#669).
    #[test]
    fn counting_estimates_by_a_label_stays_exact() {
        let target = AccuracyTarget::max_error(0.01);
        let (candidates, _) = run(
            "count by (le) (quantile_over_time(0.99, m[1m]))",
            &target,
            &data(),
            PlanningModels::builtin(),
        );
        // The inner quantile is a sketch; the count itself is exact.
        let sketched: Vec<_> = candidates
            .iter()
            .filter(|c| {
                let sketches = sketches(c);
                !sketches.is_empty()
                    && sketches
                        .iter()
                        .all(|a| matches!(a, SketchAlgorithm::Kll | SketchAlgorithm::DDSketch))
            })
            .collect();
        assert!(!sketched.is_empty());
        for candidate in sketched {
            assert!(
                output_guarantee(candidate).is_some_and(|g| g.is_exact()),
                "{:?}",
                sketches(candidate)
            );
        }
    }

    /// `avg by (job)` over per-series DDSketch quantiles has a relative bound
    /// that needs the values' sign, a statistic no input supplies: it stays
    /// symbolic and plan selection treats it as unmet.
    #[test]
    fn average_over_relative_estimates_needs_the_sign() {
        let target = AccuracyTarget::max_error(0.01);
        let (candidates, selection) = run(
            "avg by (job) (quantile_over_time(0.99, m[1m]))",
            &target,
            &data(),
            PlanningModels::builtin(),
        );
        let ddsketch: Vec<_> = candidates
            .iter()
            .filter(|c| reads_only(c, SketchAlgorithm::DDSketch))
            .collect();
        assert!(!ddsketch.is_empty());
        for candidate in ddsketch {
            let g = output_guarantee(candidate).expect("a symbolic bound");
            assert!(g.spec.error.has_unknown(), "{g:?}");
            assert!(!selection.costs.contains_key(&candidate.id));
        }
    }

    /// `max` folds one value per series, so its failure probability is
    /// scaled by the symbolic input row count. Stage 3 binds it from the
    /// series count of the source the query reads; without one, the target
    /// is unmet.
    #[test]
    fn stage3_binds_the_input_row_count_from_the_data_workload() {
        let target = AccuracyTarget::max_error(0.01)
            .failure_probability(0.001)
            .accuracy_metric(AccuracyMetric::Relative);
        let query = "max by (job) (quantile_over_time(0.99, m[1m]))";
        let ddsketch = |candidates: &[PhysicalCandidate]| -> Vec<String> {
            candidates
                .iter()
                .filter(|c| reads_only(c, SketchAlgorithm::DDSketch))
                .map(|c| c.id.clone())
                .collect()
        };
        let (candidates, with) = run(query, &target, &data(), PlanningModels::builtin());
        let ids = ddsketch(&candidates);
        assert!(!ids.is_empty());
        assert!(ids.iter().all(|id| with.costs.contains_key(id)));
        let unknown = with_series(None);
        let (_, without) = run(query, &target, &unknown, PlanningModels::builtin());
        assert!(ids.iter().all(|id| !without.costs.contains_key(id)));
    }

    /// Deployment contract: every quantile operand lies in `[1, 1000]`.
    struct BoundedQuantiles;

    impl AccuracyEvidenceProvider for BoundedQuantiles {
        fn operand_domain(
            &self,
            _operand: &OperatorNode,
        ) -> Option<crate::accuracy::OperandDomain> {
            Some(crate::accuracy::OperandDomain {
                lower: 1.0,
                upper: 1_000.0,
                max_samples: 1_000_000,
                contract: "test source enforces [1, 1000]".into(),
            })
        }
    }

    /// A ratio of two DDSketch quantiles has a relative bound only with an
    /// enforced operand domain: Stage 3 binds it from the deployment's
    /// `OperandDomain` contract; without one the target is unmet. (A
    /// DDSketch quantile over an exact one keeps its relative bound.) Each
    /// operand is sized for the entry's target, so the ratio's bound, about
    /// twice that, is checked against a looser one.
    #[test]
    fn ddsketch_ratio_meets_its_target_only_with_operand_domains() {
        let planned = AccuracyTarget::max_error(0.01).accuracy_metric(AccuracyMetric::Relative);
        let target = AccuracyTarget::max_error(0.05).accuracy_metric(AccuracyMetric::Relative);
        let query = "quantile_over_time(0.99, m[1m]) / quantile_over_time(0.5, m[1m])";
        let run = |models| run_selecting(query, &planned, &target, &data(), models);
        let (candidates, without) = run(PlanningModels::builtin());
        let ids: Vec<_> = candidates
            .iter()
            .filter(|c| sketches(c) == [SketchAlgorithm::DDSketch, SketchAlgorithm::DDSketch])
            .map(|c| c.id.clone())
            .collect();
        // The Pass 1 candidate and its Pass 2 candidates sharing the input.
        assert!(!ids.is_empty());
        assert!(ids.iter().all(|id| !without.costs.contains_key(id)));
        let (_, with) = run(PlanningModels::builtin().with_evidence(&BoundedQuantiles));
        assert!(ids.iter().all(|id| with.costs.contains_key(id)), "{with:?}");
    }

    /// `query` carrying `guarantee`, as a `scalar(…)` operand.
    fn scalar_of(query: &str, guarantee: AccuracyGuarantee) -> ScalarExpr {
        let node = lower_promql(query, AccuracyTarget::exact());
        ScalarExpr::PromqlScalarFromVector(Rc::new((*node).clone().with_guarantee(Some(guarantee))))
    }

    fn guarantee(metric: AccuracyMetric, bound: f64, delta: f64) -> AccuracyGuarantee {
        AccuracyGuarantee::new(
            metric,
            BoundExpr::Constant { value: bound },
            ProbabilityExpr::Constant { value: delta },
            vec![],
        )
    }

    fn scalar_root(op: ArithmeticOpKind, left: ScalarExpr, right: ScalarExpr) -> QueryRoot {
        QueryRoot::Scalar(ScalarExpr::Arithmetic {
            op,
            left: Box::new(left),
            right: Box::new(right),
            semantics: ExprSemantics::Promql,
        })
    }

    fn norms(sources: &[(&str, f64)]) -> DataWorkload {
        let mut data = data();
        for (source, l1) in sources {
            data.sources
                .entry(source.to_string())
                .or_default()
                .stream_norms = Some(asap_types::workload::StreamNorms {
                l1: Some(*l1),
                l2: None,
            });
        }
        data
    }

    /// A difference of two Count-Min estimates of the raw values of
    /// different metrics is bound by each metric's own stream norm from the
    /// data workload
    /// (`0.01 · 100 + 0.02 · 1000`); a metric without a norm leaves the
    /// bound unmet.
    #[test]
    fn stage3_binds_each_metric_stream_norm() {
        // Count-Min summaries of each metric's raw value column.
        let raw = |query: &str, g: AccuracyGuarantee| {
            ScalarExpr::PromqlScalarFromVector(crate::test_support::estimate_over(
                crate::test_support::weighted(
                    crate::test_support::cms_summary(query),
                    asap_types::post_asap::SummaryInputExpr::Column(
                        asap_types::pre_asap::ColumnRef::SampleValue,
                    ),
                ),
                g,
            ))
        };
        let root = scalar_root(
            ArithmeticOpKind::Sub,
            raw(
                "count by (job) (a)",
                guarantee(AccuracyMetric::ABSOLUTE_STREAM_L1, 0.01, 0.001),
            ),
            raw(
                "count by (job) (b)",
                guarantee(AccuracyMetric::ABSOLUTE_STREAM_L1, 0.02, 0.001),
            ),
        );
        let models = PlanningModels::builtin();
        let both = bound_of(&root, &norms(&[("a", 100.0), ("b", 1_000.0)]), &models).unwrap();
        assert!(
            (both.spec.error.evaluate().unwrap() - 21.0).abs() < 1e-9,
            "{both:?}"
        );
        let one = bound_of(&root, &norms(&[("a", 100.0)]), &models).unwrap();
        assert!(one.spec.error.has_unknown());
    }

    /// A stream norm is bound only for a summary of the source's raw value
    /// column: a count over the same source (constant weight) needs the
    /// source's row count, which nothing binds, and a summary of a computed
    /// value (`m * 1000`) needs a norm nothing binds; both stay unmet.
    #[test]
    fn stage3_binds_a_stream_norm_only_for_the_raw_value_column() {
        use asap_types::post_asap::SummaryInputExpr;
        use asap_types::pre_asap::ColumnRef;
        let frequency = guarantee(AccuracyMetric::ABSOLUTE_STREAM_L1, 0.01, 0.001);
        let doubled = |summary: Rc<OperatorNode>| {
            scalar_root(
                ArithmeticOpKind::Mul,
                ScalarExpr::literal_f64(2.0),
                ScalarExpr::PromqlScalarFromVector(crate::test_support::estimate_over(
                    summary,
                    frequency.clone(),
                )),
            )
        };
        let models = PlanningModels::builtin();
        let data = norms(&[("m", 100.0)]);
        let column = || SummaryInputExpr::Column(ColumnRef::SampleValue);
        let raw = doubled(crate::test_support::weighted(
            crate::test_support::cms_summary("count by (job) (m)"),
            column(),
        ));
        let g = bound_of(&raw, &data, &models).unwrap();
        assert!(
            (g.spec.error.evaluate().unwrap() - 2.0).abs() < 1e-12,
            "{g:?}"
        );
        for unbound in [
            doubled(Rc::new(crate::test_support::cms_summary(
                "count by (job) (m)",
            ))),
            doubled(crate::test_support::weighted(
                crate::test_support::cms_summary("count by (job) (m * 1000)"),
                column(),
            )),
        ] {
            let g = bound_of(&unbound, &data, &models).unwrap();
            assert!(g.spec.error.has_unknown(), "{g:?}");
        }
    }

    /// A product and a quotient of two estimates are bound from the
    /// deployment's operand domains (`[1, 1000]`):
    /// `|x|·e_y + |y|·e_x + e_x·e_y` and `(|x|·e_y + |y|·e_x) / (|y|·(|y| − e_y))`
    /// at the domain's extremes; without domains they stay unmet. A ratio of
    /// relative errors with no division operator to bind stays unmet too.
    #[test]
    fn stage3_binds_products_and_quotients_from_operand_domains() {
        let (x, y) = (
            scalar_of("sum(a)", guarantee(AccuracyMetric::ABSOLUTE, 0.01, 0.001)),
            scalar_of("sum(b)", guarantee(AccuracyMetric::ABSOLUTE, 0.02, 0.001)),
        );
        let product = scalar_root(ArithmeticOpKind::Mul, x.clone(), y.clone());
        let quotient = scalar_root(ArithmeticOpKind::Div, x, y);
        let bounded = PlanningModels::builtin().with_evidence(&BoundedQuantiles);
        for (root, expected) in [
            (&product, 1_000.0 * 0.02 + 1_000.0 * 0.01 + 0.01 * 0.02),
            (&quotient, (1_000.0 * 0.02 + 1_000.0 * 0.01) / (1.0 - 0.02)),
        ] {
            let g = bound_of(root, &data(), &bounded).unwrap();
            assert!(
                (g.spec.error.evaluate().unwrap() - expected).abs() < 1e-9,
                "{g:?}"
            );
            assert!((g.spec.failure_probability.evaluate().unwrap() - 0.002).abs() < 1e-12);
            let unbound = bound_of(root, &data(), &PlanningModels::builtin()).unwrap();
            assert!(unbound.spec.error.has_unknown(), "{unbound:?}");
        }
        let ratio = scalar_root(
            ArithmeticOpKind::Div,
            scalar_of("sum(a)", guarantee(AccuracyMetric::Relative, 0.01, 0.0)),
            scalar_of("sum(b)", guarantee(AccuracyMetric::Relative, 0.01, 0.0)),
        );
        let g = bound_of(&ratio, &data(), &PlanningModels::builtin()).unwrap();
        assert!(g.spec.error.has_unknown(), "{g:?}");
    }

    /// A ratio divided by an exact vector, `(q99 / q50) / max_over_time(m)`,
    /// keeps the inner ratio's symbolic relative bound; Stage 3 binds it at
    /// the division of the two estimates, not at the outer division that
    /// inherited it, so the candidate is selectable.
    #[test]
    fn a_scaled_ratio_binds_at_its_division() {
        let planned = AccuracyTarget::max_error(0.01).accuracy_metric(AccuracyMetric::Relative);
        let target = AccuracyTarget::max_error(0.05).accuracy_metric(AccuracyMetric::Relative);
        let query = "(quantile_over_time(0.99, m[1m]) / quantile_over_time(0.5, m[1m])) \
                     / max_over_time(m[1m])";
        let (candidates, selection) = run_selecting(
            query,
            &planned,
            &target,
            &data(),
            PlanningModels::builtin().with_evidence(&BoundedQuantiles),
        );
        let ids: Vec<_> = candidates
            .iter()
            .filter(|c| reads_only(c, SketchAlgorithm::DDSketch))
            .map(|c| c.id.clone())
            .collect();
        assert!(!ids.is_empty());
        assert!(
            ids.iter().all(|id| selection.costs.contains_key(id)),
            "{selection:?}"
        );
    }

    /// The input row count is bound only from the series count of the
    /// sources the query reads: another source's declared series count
    /// leaves it unbound.
    #[test]
    fn stage3_binds_only_the_series_count_of_the_sources_read() {
        let target = AccuracyTarget::max_error(0.01).failure_probability(0.001);
        let query = "max by (job) (quantile_over_time(0.99, m[1m]))";
        let mut other_source = data();
        other_source.sources.remove("m");
        let (candidates, selection) = run(query, &target, &other_source, PlanningModels::builtin());
        let ddsketch: Vec<_> = candidates
            .iter()
            .filter(|c| reads_only(c, SketchAlgorithm::DDSketch))
            .collect();
        assert!(!ddsketch.is_empty());
        assert!(ddsketch
            .iter()
            .all(|c| !selection.costs.contains_key(&c.id)));
    }

    /// `quantile_over_time(0.99, m[1m])` with its KLL candidate chosen, its
    /// state materialized as `materialization`.
    fn kll_dag(materialization: Materialization) -> PhysicalASAPDAG {
        use asap_types::ir::{
            apply_materialization_timings, MaterializationAssignment, TimingMemo,
        };
        let root = lower_promql(
            "quantile_over_time(0.99, m[1m])",
            AccuracyTarget::max_error(0.01).failure_probability(0.01),
        );
        let inventory = crate::per_computation_candidates::generate_per_computation_candidates(
            vec![(0, QueryRoot::Operator(root))],
        )
        .unwrap();
        let choice: Vec<_> = inventory
            .targets
            .iter()
            .map(|t| {
                t.choices
                    .candidates
                    .iter()
                    .position(|c| {
                        *c == crate::per_computation_candidates::ComputationCandidate::Summary(
                            SketchAlgorithm::Kll,
                        )
                    })
                    .unwrap_or(0)
            })
            .collect();
        let roots =
            crate::per_computation_candidates::compose_logical_candidate(&inventory, &choice)
                .unwrap();
        let QueryRoot::Operator(root) = &roots[0].1 else {
            panic!("operator root")
        };
        let state = OperatorNode::reachable(root)
            .into_iter()
            .find(|n| matches!(n.asap(), Some(ASAPOp::SummaryAgg { .. })))
            .expect("a KLL state");
        let mut assignment =
            MaterializationAssignment::with_default(Materialization::NotMaterialized);
        assignment.set(&state, materialization);
        let timed =
            apply_materialization_timings(root, &assignment, &mut TimingMemo::new()).unwrap();
        asap_types::ir::physical_export::compile_physical_asap_workload_with_node_ids(&[
            QueryRoot::Operator(timed),
        ])
        .unwrap()
        .dag
    }

    /// The physical DAG of `query`'s first per-computation candidates,
    /// with nothing materialized.
    fn first_candidate_dag(query: &str) -> PhysicalASAPDAG {
        let root = lower_promql(query, AccuracyTarget::exact());
        let inventory = crate::per_computation_candidates::generate_per_computation_candidates(
            vec![(0, QueryRoot::Operator(root))],
        )
        .unwrap();
        let choice = vec![0; inventory.targets.len()];
        let roots: Vec<_> =
            crate::per_computation_candidates::compose_logical_candidate(&inventory, &choice)
                .unwrap()
                .into_iter()
                .map(|(_, root)| root)
                .collect();
        optimize_physical_asap(
            "L1",
            &roots,
            &[],
            &data(),
            &DeploymentCapabilities::default(),
        )
        .unwrap()
        .remove(0)
        .dag
    }

    /// A time shift is free and does not change what the scan below it
    /// reads: the scan reads the window above the shift, as without it. A
    /// time range is priced by the rows it keeps.
    #[test]
    fn time_shift_is_free_and_the_scan_reads_the_shifted_window() {
        let every_minute = [repeating(60_000)];
        let plain = first_candidate_dag("sum_over_time(m[5m])");
        let shifted = first_candidate_dag("sum_over_time(m[5m] offset 1h)");
        let (plain_cost, shifted_cost) = (
            priced(&plain, &every_minute),
            priced(&shifted, &every_minute),
        );
        let cost_of = |dag: &PhysicalASAPDAG, cost: &CandidateCost, pick: fn(&Payload) -> bool| {
            cost.per_node[&node_of(dag, pick)].cost_per_second.unwrap()
        };
        let is_shift = |p: &Payload| matches!(p, Payload::NonASAP(NonASAPOp::TimeShift { .. }));
        let is_range = |p: &Payload| matches!(p, Payload::NonASAP(NonASAPOp::TimeRange { .. }));
        assert_eq!(cost_of(&shifted, &shifted_cost, is_shift), 0.0);
        assert!(close(
            cost_of(&shifted, &shifted_cost, is_scan),
            cost_of(&plain, &plain_cost, is_scan)
        ));
        assert!(close(
            shifted_cost.cost_per_second.unwrap(),
            plain_cost.cost_per_second.unwrap()
        ));
        let range = &plain_cost.per_node[&node_of(&plain, is_range)];
        assert!(range.detail.contains("keep"), "{}", range.detail);
    }

    const AT_INGESTION: Materialization =
        Materialization::MatAtIngestion(asap_types::ir::Medium::Memory);
    const AT_QUERY: Materialization = Materialization::MatAtQuery(asap_types::ir::Medium::Memory);

    fn node_of(dag: &PhysicalASAPDAG, pick: impl Fn(&Payload) -> bool) -> PhysicalASAPNodeId {
        dag.nodes.iter().find(|n| pick(&n.payload)).unwrap().id
    }

    fn is_scan(payload: &Payload) -> bool {
        matches!(payload, Payload::NonASAP(NonASAPOp::Scan { .. }))
    }

    fn is_build(payload: &Payload) -> bool {
        matches!(payload, Payload::ASAP(ASAPOp::SummaryAgg { .. }))
    }

    fn priced(dag: &PhysicalASAPDAG, recurrence: &[QueryRecurrence]) -> CandidateCost {
        price(dag, recurrence, &data(), &PlanSelectionSettings::default()).unwrap()
    }

    /// The default settings with cost model 1's unit costs edited.
    fn unit_costs(edit: impl FnOnce(&mut ResourceUnitCost)) -> PlanSelectionSettings {
        let mut prices = ResourceUnitCost::illustrative();
        edit(&mut prices);
        PlanSelectionSettings {
            cost_model: CostModel::UsageOverTime(prices),
            ..PlanSelectionSettings::default()
        }
    }

    fn close(a: f64, b: f64) -> bool {
        (a - b).abs() <= 1e-12 * a.abs().max(b.abs()).max(1e-300)
    }

    /// An ingestion-time node is priced over one second of arriving rows,
    /// once per second, whatever its readers' evaluation rate; a query-time
    /// node scales with that rate.
    #[test]
    fn ingestion_time_node_is_priced_per_second_of_arriving_rows() {
        let dag = kll_dag(AT_INGESTION);
        let (fast, slow) = (
            priced(&dag, &[repeating(1_000)]),
            priced(&dag, &[repeating(10_000)]),
        );
        for id in [node_of(&dag, is_scan), node_of(&dag, is_build)] {
            assert_eq!(
                fast.per_node[&id].cost_per_second,
                slow.per_node[&id].cost_per_second
            );
            assert!(fast.per_node[&id].detail.contains("; x 1.0000/s"));
        }
        let estimate = dag.roots[0];
        assert!(close(
            fast.per_node[&estimate].cost_per_second.unwrap(),
            10.0 * slow.per_node[&estimate].cost_per_second.unwrap()
        ));
    }

    /// Kept state is charged per byte per second: at ingestion time, twice
    /// the state's bytes (the window being built and the completed one) for
    /// every second. At query time the state is reused only within one
    /// evaluation, so it is held only while that evaluation runs: its bytes
    /// times the evaluation's latency, once per evaluation. State computed
    /// and dropped within a query has none.
    #[test]
    fn kept_state_is_charged_per_byte_per_second() {
        let cost = |dag: &PhysicalASAPDAG, w| {
            let settings = unit_costs(|c| c.cost_per_memory_byte_second = w);
            price(dag, &recurrences(1), &data(), &settings).unwrap()
        };
        for materialization in [AT_INGESTION, AT_QUERY] {
            let dag = kll_dag(materialization);
            let build = node_of(&dag, is_build);
            let (with, without) = (cost(&dag, 1.25e-7), cost(&dag, 0.0));
            let Payload::ASAP(ASAPOp::SummaryAgg { family, .. }) =
                &dag.nodes.iter().find(|n| n.id == build).unwrap().payload
            else {
                unreachable!()
            };
            // One state per series: data() declares 10 000.
            let state = 10_000.0 * summary_shape(family).1 as f64;
            let byte_seconds = if materialization == AT_INGESTION {
                2.0 * state
            } else {
                let evaluation_s = with.usage.latency_per_query[0].total_ms() / 1_000.0;
                assert!(evaluation_s > 0.0 && evaluation_s < 1.0, "{evaluation_s}");
                // Once per evaluation of the query repeating every 10 s.
                assert!(with.per_node[&build].detail.contains("; x 0.1000/s"));
                state * evaluation_s * 0.1
            };
            let term = with.per_node[&build].cost_per_second.unwrap()
                - without.per_node[&build].cost_per_second.unwrap();
            assert!(
                close(term, 1.25e-7 * byte_seconds),
                "{materialization:?}: {term} vs {}",
                1.25e-7 * byte_seconds
            );
            assert!(close(
                with.cost_per_second.unwrap() - without.cost_per_second.unwrap(),
                term
            ));
        }
        let transient = kll_dag(Materialization::NotMaterialized);
        assert_eq!(
            cost(&transient, 1.25e-7).cost_per_second.unwrap(),
            cost(&transient, 0.0).cost_per_second.unwrap()
        );
    }

    /// Under cost model 1, memory is charged only as kept bytes per second
    /// (average memory): the memory a computation holds while it runs (a
    /// sketch being built, an exact quantile's values), counted in peak
    /// memory, adds nothing. The memory price's term is exactly the average
    /// memory, though peak memory is larger.
    #[test]
    fn memory_held_only_while_a_query_runs_is_not_charged() {
        let memory_term = |dag: &PhysicalASAPDAG| {
            let cost = |price_per_byte_second| {
                let settings =
                    unit_costs(|c| c.cost_per_memory_byte_second = price_per_byte_second);
                price(dag, &recurrences(1), &data(), &settings).unwrap()
            };
            let (with, without) = (cost(1.0), cost(0.0));
            let term = with.cost_per_second.unwrap() - without.cost_per_second.unwrap();
            (term, with.usage)
        };
        let mut dags: Vec<(String, PhysicalASAPDAG)> = [
            "quantile_over_time(0.99, m[1m])",
            "quantile by (job) (0.99, m)",
        ]
        .into_iter()
        .map(|query| {
            let root = lower_promql(query, AccuracyTarget::exact());
            let dag = optimize_physical_asap(
                "L",
                &[QueryRoot::Operator(root)],
                &[],
                &data(),
                &DeploymentCapabilities::default(),
            )
            .unwrap()
            .remove(0)
            .dag;
            (query.to_string(), dag)
        })
        .collect();
        for materialization in [AT_INGESTION, AT_QUERY, Materialization::NotMaterialized] {
            dags.push((format!("{materialization:?}"), kll_dag(materialization)));
        }
        for (name, dag) in &dags {
            let (term, usage) = memory_term(dag);
            assert!(
                close(term, usage.average_memory_bytes),
                "{name}: {term} vs {}",
                usage.average_memory_bytes
            );
            assert!(
                usage.peak_memory_bytes as f64 > usage.average_memory_bytes,
                "{name}: {usage:?}"
            );
        }
        // Not materialized, nothing is kept: the memory price changes nothing.
        let (term, _) = memory_term(&kll_dag(Materialization::NotMaterialized));
        assert_eq!(term, 0.0);
    }

    /// Kept exact state that grows with the data is charged by the values it
    /// keeps: a materialized exact quantile keeps every value of its group,
    /// a sum one value per group.
    #[test]
    fn kept_exact_state_is_charged_by_the_values_it_keeps() {
        let kept = |query: &str| {
            let root = lower_promql(query, AccuracyTarget::exact());
            let mut dag = optimize_physical_asap(
                "L",
                &[QueryRoot::Operator(root)],
                &[],
                &data(),
                &DeploymentCapabilities::default(),
            )
            .unwrap()
            .remove(0)
            .dag;
            let aggregate = dag
                .nodes
                .iter_mut()
                .find(|n| matches!(n.payload, Payload::NonASAP(NonASAPOp::Aggregate { .. })))
                .unwrap();
            aggregate.materialization = AT_QUERY;
            let id = aggregate.id;
            let cost = |w| {
                let settings = unit_costs(|c| c.cost_per_memory_byte_second = w);
                price(&dag, &recurrences(1), &data(), &settings)
                    .unwrap()
                    .per_node[&id]
                    .cost_per_second
                    .unwrap()
            };
            (cost(1.0) - cost(0.0)).round() as u64
        };
        let (quantile, sum) = (
            kept("quantile by (job) (0.99, m)"),
            kept("sum by (job) (m)"),
        );
        assert!(quantile > 10 * sum, "{quantile} vs {sum}");
    }

    /// A `NotMaterialized` node read by two queries is computed for each, at
    /// each one's rate; `MatAtQuery`, it is computed once at their combined
    /// rate, where queries at the same interval run together.
    #[test]
    fn rates_follow_per_consumer_charging() {
        let root = lower_promql("sum by (job) (rate(m[1m]))", AccuracyTarget::exact());
        let one = optimize_physical_asap(
            "L",
            &[QueryRoot::Operator(root.clone())],
            &[],
            &data(),
            &DeploymentCapabilities::default(),
        )
        .unwrap()
        .remove(0)
        .dag;
        let mut two = optimize_physical_asap(
            "L",
            &[QueryRoot::Operator(root.clone()), QueryRoot::Operator(root)],
            &[],
            &data(),
            &DeploymentCapabilities::default(),
        )
        .unwrap()
        .remove(0)
        .dag;
        let without_memory = unit_costs(|c| c.cost_per_memory_byte_second = 0.0);
        let total = |dag: &PhysicalASAPDAG, recurrence: &[QueryRecurrence]| {
            price(dag, recurrence, &data(), &without_memory)
                .unwrap()
                .cost_per_second
                .unwrap()
        };
        let single = total(&one, &recurrences(1));
        let mixed = [repeating(10_000), repeating(20_000)];
        assert!(close(total(&two, &recurrences(2)), 2.0 * single));
        assert!(close(total(&two, &mixed), 1.5 * single));
        assert_eq!(two.roots[0], two.roots[1], "one root node for both queries");
        for node in &mut two.nodes {
            node.materialization = AT_QUERY;
        }
        assert!(close(total(&two, &recurrences(2)), single));
        assert!(close(total(&two, &mixed), 1.5 * single));
    }

    /// One-off and unknown recurrence are spread over the horizon:
    /// invocations × per-evaluation cost / non_repeating_query_horizon_seconds.
    #[test]
    fn one_off_recurrence_is_spread_over_the_horizon() {
        let root = lower_promql("sum by (job) (rate(m[1m]))", AccuracyTarget::exact());
        let dag = optimize_physical_asap(
            "L",
            &[QueryRoot::Operator(root)],
            &[],
            &data(),
            &DeploymentCapabilities::default(),
        )
        .unwrap()
        .remove(0)
        .dag;
        let horizon = PlanSelectionSettings::default().non_repeating_query_horizon_seconds;
        assert_eq!(horizon, 3_600.0);
        let per_evaluation = priced(&dag, &[repeating(1_000)]).cost_per_second.unwrap();
        let once = priced(
            &dag,
            &[QueryRecurrence::OneTime {
                invocations: 3,
                execute_at: None,
            }],
        )
        .cost_per_second
        .unwrap();
        assert!(close(once, 3.0 * per_evaluation / horizon), "{once}");
        assert!(close(
            priced(&dag, &[QueryRecurrence::Unknown])
                .cost_per_second
                .unwrap(),
            per_evaluation / horizon
        ));
    }

    /// Settings outside their domain are an error, not a price, checked once
    /// before any candidate: a price the cost model reads that is negative,
    /// the model's prices all 0 (cost model 1 with only provisioned prices
    /// set), or a horizon that is not positive. A price the model does not
    /// read is not checked.
    #[test]
    fn invalid_settings_are_rejected() {
        let candidates = pane_candidates(10_000.0);
        let select = |settings: &PlanSelectionSettings| {
            select_plan(
                &candidates,
                &[every_minute()],
                &arriving(10_000.0),
                PlanningModels::builtin().with_plan_selection(settings),
            )
        };
        let only_provisioned = ResourceUnitCost {
            cost_per_cpu_op: 0.0,
            cost_per_scanned_byte: 0.0,
            cost_per_memory_byte_second: 0.0,
            cost_per_disk_byte_second: 0.0,
            ..ResourceUnitCost::illustrative()
        };
        for settings in [
            unit_costs(|c| c.cost_per_memory_byte_second = -1.0),
            under(CostModel::UsageOverTime(only_provisioned.clone())),
            PlanSelectionSettings {
                non_repeating_query_horizon_seconds: 0.0,
                ..PlanSelectionSettings::default()
            },
        ] {
            assert!(
                matches!(select(&settings), Err(SelectionError::InvalidSettings(_))),
                "{settings:?}"
            );
        }
        assert!(select(&under(CostModel::ProvisionedCapacity(only_provisioned))).is_ok());
        assert!(select(&unit_costs(
            |c| c.cost_per_provisioned_cpu_core_second = -1.0
        ))
        .is_ok());
    }

    // ── Stage 2 materialization and the latency check ────────────────────

    const MINUTE: u32 = 60_000;

    /// The p99 target [`pane_candidates`] are sized for.
    fn p99_target() -> AccuracyTarget {
        AccuracyTarget::max_error(0.01)
            .failure_probability(0.001)
            .accuracy_metric(AccuracyMetric::Rank)
    }

    /// A predictable p99 query evaluated every minute, with no latency bound.
    fn every_minute() -> QueryWorkloadEntry {
        QueryWorkloadEntry {
            predictability: asap_types::workload::Predictability::Predictable { known_at: None },
            ..entry_with(p99_target(), repeating(MINUTE))
        }
    }

    /// [`data`] arriving at `rows_per_s`.
    fn arriving(rows_per_s: f64) -> DataWorkload {
        DataWorkload {
            arrival: DataArrival::Ingesting {
                rows_per_second: rows_per_s,
            },
            ..data()
        }
    }

    /// The physical candidates of a per-series p99 over 5 min with its KLL
    /// in five 1-min tumbling panes, every minute over data arriving at
    /// `rows_per_s`: rebuilt at query time (B2), then maintained at
    /// ingestion time (B1).
    fn pane_candidates(rows_per_s: f64) -> Vec<PhysicalCandidate> {
        let candidates = pane_candidates_for(rows_per_s, &DeploymentCapabilities::default());
        assert_eq!(candidates.len(), 2);
        candidates
    }

    /// [`pane_candidates`] for a deployment with `deployment`'s
    /// capabilities: B2, B1, then, when it keeps query-time results, B3
    /// (built at query time and kept across evaluations).
    fn pane_candidates_for(
        rows_per_s: f64,
        deployment: &DeploymentCapabilities,
    ) -> Vec<PhysicalCandidate> {
        let root = lower_promql("quantile_over_time(0.99, m[5m])", p99_target());
        let inventory = crate::per_computation_candidates::generate_per_computation_candidates(
            vec![(0, QueryRoot::Operator(root))],
        )
        .unwrap();
        let kll = inventory.targets[0]
            .choices
            .candidates
            .iter()
            .position(|c| {
                matches!(
                    c,
                    crate::per_computation_candidates::ComputationCandidate::Summary(
                        SketchAlgorithm::Kll
                    )
                )
            })
            .unwrap();
        let roots: Vec<_> =
            crate::per_computation_candidates::compose_logical_candidate(&inventory, &[kll])
                .unwrap()
                .into_iter()
                .map(|(_, root)| root)
                .collect();
        let rule_candidates =
            crate::asap_aware_cse::pane_candidates(&roots, &[Some(u64::from(MINUTE))]);
        let roots =
            crate::asap_aware_cse::window_composition_pass2_candidates(&roots, &rule_candidates)
                .unwrap()
                .pop()
                .unwrap()
                .roots
                .unwrap();
        let mut candidates = optimize_physical_asap(
            "L1",
            &roots,
            &[every_minute()],
            &arriving(rows_per_s),
            deployment,
        )
        .unwrap();
        for (k, c) in candidates.iter_mut().enumerate() {
            c.id = format!("P1-m{k}");
        }
        candidates
    }

    /// The raw samples the scan `scan` reads per computation, from its
    /// priced detail (`scan <n> samples; ...`).
    fn scanned_samples(cost: &CandidateCost, scan: PhysicalASAPNodeId) -> u64 {
        let detail = &cost.per_node[&scan].detail;
        detail
            .strip_prefix("scan ")
            .and_then(|rest| rest.split(' ').next())
            .and_then(|rows| rows.parse().ok())
            .unwrap_or_else(|| panic!("not a scan: {detail}"))
    }

    /// A deployment that keeps query-time results across evaluations.
    fn keeping_query_time_results() -> DeploymentCapabilities {
        DeploymentCapabilities {
            query_time_results_retained: true,
            ..DeploymentCapabilities::default()
        }
    }

    /// `query_time_results_retained` gates keeping a query-time result across
    /// evaluations, and only that. Without it, Stage 2 offers the 1-min
    /// panes of a query repeating every minute rebuilt (B2) or maintained
    /// (B1); with it, also built at query time and kept (B3,
    /// `MatAtQueryKeptAcrossEvaluations`). B3 is computed once per
    /// evaluation, reads one pane (1 min) of raw data, keeps that much raw
    /// data on disk, and holds its 4 older panes in memory between
    /// evaluations (on disk, if it were placed there). The same chain
    /// `MatAtQuery` is reused only within one evaluation: the scan reads the
    /// 5-min window and nothing is kept between evaluations.
    #[test]
    fn query_time_results_retained_keeps_a_pane_chain_across_evaluations() {
        assert_eq!(pane_candidates(10_000.0).len(), 2);
        let candidates = pane_candidates_for(10_000.0, &keeping_query_time_results());
        assert_eq!(candidates.len(), 3);
        let b2 = &candidates[0].dag;
        let b3 = &candidates[2].dag;
        let chain = node_of(b3, is_chain);
        let with_chain = |materialization: Materialization| {
            let mut dag = b3.clone();
            for node in dag.nodes.iter_mut().filter(|n| n.id == chain) {
                node.materialization = materialization;
            }
            dag
        };
        assert_eq!(
            *b3,
            with_chain(Materialization::MatAtQueryKeptAcrossEvaluations(
                Medium::Memory
            ))
        );
        let recurrence = [repeating(MINUTE)];
        let settings = PlanSelectionSettings::default();
        let kept = price(b3, &recurrence, &arriving(10_000.0), &settings).unwrap();
        let rebuilt = price(b2, &recurrence, &arriving(10_000.0), &settings).unwrap();
        let scan = node_of(b3, is_scan);
        assert_eq!(scanned_samples(&kept, scan), 10_000 * 60);
        assert_eq!(
            scanned_samples(&rebuilt, node_of(b2, is_scan)),
            10_000 * 300
        );
        assert_eq!(kept.usage.disk_bytes * 5, rebuilt.usage.disk_bytes);
        let Payload::ASAP(ASAPOp::PaneChain { family, .. }) =
            &b3.nodes.iter().find(|n| n.id == chain).unwrap().payload
        else {
            unreachable!()
        };
        let (_, state_bytes) = summary_shape(family);
        let chain_cost = &kept.per_node[&chain];
        assert!(chain_cost.detail.contains("; x 0.0167/s"), "{chain_cost:?}");
        // One state per series: arriving() declares 10 000.
        let between = 4 * 10_000 * state_bytes;
        assert!(chain_cost
            .detail
            .contains(&format!("keeps {between} bytes across evaluations")));
        // On disk, the same N − 1 older panes are kept.
        let on_disk = with_chain(Materialization::MatAtQueryKeptAcrossEvaluations(
            Medium::Disk,
        ));
        let on_disk = price(&on_disk, &recurrence, &arriving(10_000.0), &settings).unwrap();
        assert_eq!(
            on_disk.usage.disk_bytes,
            kept.usage.disk_bytes + between,
            "{on_disk:?}"
        );
        // Shared within one evaluation, nothing is kept between them.
        let shared = with_chain(Materialization::MatAtQuery(Medium::Memory));
        let within_one = price(&shared, &recurrence, &arriving(10_000.0), &settings).unwrap();
        assert_eq!(scanned_samples(&within_one, scan), 10_000 * 300);
        assert!(!within_one.per_node[&chain]
            .detail
            .contains("across evaluations"));
        assert!(within_one.usage.average_memory_bytes < kept.usage.average_memory_bytes);
    }

    /// Without raw data retained, a candidate that scans raw data at query
    /// time is invalid, with the window it would read: B2 (5 min) and B3
    /// (one 1-min pane) are rejected, and B1, which reads raw rows only at
    /// ingestion time, is selected. With raw data retained, all three are
    /// valid.
    #[test]
    fn without_raw_data_only_ingestion_time_scans_are_valid() {
        let candidates = pane_candidates_for(10_000.0, &keeping_query_time_results());
        let select = |deployment: &DeploymentCapabilities| {
            select_plan(
                &candidates,
                &[every_minute()],
                &arriving(10_000.0),
                PlanningModels::builtin().with_deployment_capabilities(deployment),
            )
            .unwrap()
        };
        let retained = select(&keeping_query_time_results());
        assert!(
            retained.rejected.iter().all(|r| !r.cause.failed_a_check()),
            "{retained:?}"
        );
        let dropped = select(&DeploymentCapabilities {
            raw_data_retained: false,
            ..keeping_query_time_results()
        });
        assert_eq!(dropped.selected, candidates[1].id);
        let reasons: BTreeMap<&str, (RejectionCause, &str)> = dropped
            .rejected
            .iter()
            .map(|r| (r.id.as_str(), (r.cause, r.reason.as_str())))
            .collect();
        let unretained = "at query time, but the deployment does not retain raw data";
        assert_eq!(
            reasons[candidates[0].id.as_str()],
            (
                RejectionCause::RawDataNotRetained,
                format!("scans 300000 ms of raw m {unretained}").as_str()
            )
        );
        assert_eq!(
            reasons[candidates[2].id.as_str()],
            (
                RejectionCause::RawDataNotRetained,
                format!("scans 60000 ms of raw m {unretained}").as_str()
            )
        );
    }

    /// Without raw data retained, the rejection lists every query-time
    /// scan with the window it would read, not just the first.
    #[test]
    fn without_raw_data_every_query_time_scan_is_listed() {
        let roots: Vec<(usize, QueryRoot)> = ["sum_over_time(a[5m])", "sum_over_time(b[1m])"]
            .into_iter()
            .map(|q| QueryRoot::Operator(lower_promql(q, AccuracyTarget::exact())))
            .enumerate()
            .collect();
        // The exact candidate of each query.
        let inventory =
            crate::per_computation_candidates::generate_per_computation_candidates(roots).unwrap();
        let exact = vec![0; inventory.targets.len()];
        let roots: Vec<QueryRoot> =
            crate::per_computation_candidates::compose_logical_candidate(&inventory, &exact)
                .unwrap()
                .into_iter()
                .map(|(_, root)| root)
                .collect();
        let mut candidate = optimize_physical_asap(
            "L1",
            &roots,
            &[],
            &data(),
            &DeploymentCapabilities::default(),
        )
        .unwrap()
        .remove(0);
        candidate.id = "P1".into();
        let entries = [entry(repeating(MINUTE)), entry(repeating(MINUTE))];
        let no_raw_data = DeploymentCapabilities {
            raw_data_retained: false,
            ..DeploymentCapabilities::default()
        };
        let error = select_plan(
            &[candidate],
            &entries,
            &data(),
            PlanningModels::builtin().with_deployment_capabilities(&no_raw_data),
        )
        .unwrap_err();
        let SelectionError::NoValidCandidate(rejected) = error else {
            panic!("{error}")
        };
        assert_eq!(
            rejected[0].reason,
            "scans 300000 ms of raw a, 60000 ms of raw b at query time, \
             but the deployment does not retain raw data"
        );
    }

    /// Stored data (at rest, or mixed) with no raw data retained is a
    /// contradiction in the inputs, refused before any candidate is
    /// measured; arriving or undeclared data is not.
    #[test]
    fn stored_data_without_raw_data_retained_is_bad_input() {
        let candidates = pane_candidates(10_000.0);
        let no_raw_data = DeploymentCapabilities {
            raw_data_retained: false,
            ..DeploymentCapabilities::default()
        };
        let select = |arrival: DataArrival| {
            select_plan(
                &candidates,
                &[every_minute()],
                &DataWorkload { arrival, ..data() },
                PlanningModels::builtin().with_deployment_capabilities(&no_raw_data),
            )
        };
        for arrival in [
            DataArrival::AtRest,
            DataArrival::Mixed {
                rows_per_second: 10_000.0,
            },
        ] {
            let error = select(arrival).unwrap_err();
            assert_eq!(
                error.to_string(),
                format!(
                    "deployment capabilities: the data is stored ({arrival:?}), \
                     but the deployment does not retain raw data"
                ),
            );
        }
        for arrival in [
            DataArrival::Ingesting {
                rows_per_second: 10_000.0,
            },
            DataArrival::Unknown,
        ] {
            assert!(select(arrival).is_ok());
        }
    }

    fn is_chain(payload: &Payload) -> bool {
        matches!(payload, Payload::ASAP(ASAPOp::PaneChain { .. }))
    }

    /// #509 Example 4, B1: maintained at ingestion time, a chain of N panes
    /// pays the update of the newest pane only, one second of arriving rows
    /// once per second, and keeps N + 1 panes (the N the query merges and
    /// the one being built) per series.
    #[test]
    fn a_chain_at_ingestion_pays_the_newest_pane_and_keeps_n_plus_one_panes() {
        let candidates = pane_candidates(10_000.0);
        let dag = &candidates[1].dag;
        let chain = node_of(dag, is_chain);
        let Payload::ASAP(ASAPOp::PaneChain {
            family, pane_count, ..
        }) = &dag.nodes.iter().find(|n| n.id == chain).unwrap().payload
        else {
            unreachable!()
        };
        assert_eq!(*pane_count, 5);
        let (depth, state_bytes) = summary_shape(family);
        let recurrence = [repeating(MINUTE)];
        let without_memory = unit_costs(|c| c.cost_per_memory_byte_second = 0.0);
        let work = &price(dag, &recurrence, &arriving(10_000.0), &without_memory)
            .unwrap()
            .per_node[&chain];
        assert!(work.detail.contains("; x 1.0000/s"), "{work:?}");
        assert!(
            close(
                work.cost_per_second.unwrap(),
                10_000.0 * (depth + 1.0) * 1e-6
            ),
            "{work:?}"
        );
        let with_memory = &price(
            dag,
            &recurrence,
            &arriving(10_000.0),
            &PlanSelectionSettings::default(),
        )
        .unwrap()
        .per_node[&chain];
        let kept = 6 * 10_000 * state_bytes;
        assert!(
            close(
                with_memory.cost_per_second.unwrap() - work.cost_per_second.unwrap(),
                1.25e-7 * kept as f64
            ),
            "{with_memory:?}"
        );
        assert!(with_memory.detail.contains(&format!("keeps {kept} bytes")));
    }

    /// The latency check (S6, sketch-bench cost model §5): a query's
    /// latency is its query-time work on one core. Rebuilding the panes at
    /// query time (B2) takes longer than merging the maintained ones (B1);
    /// with a bound between the two, B2 is rejected as invalid with its
    /// latency and B1 is selected.
    #[test]
    fn a_candidate_over_a_querys_latency_bound_is_rejected_with_its_latency() {
        let candidates = pane_candidates(10_000.0);
        let latency = |c: &PhysicalCandidate| {
            let cost = price(
                &c.dag,
                &[repeating(MINUTE)],
                &arriving(10_000.0),
                &PlanSelectionSettings::default(),
            )
            .unwrap();
            cost.usage.latency_per_query[0]
        };
        let (b2, b1) = (latency(&candidates[0]), latency(&candidates[1]));
        assert_eq!((b2.compaction_ms, b1.compaction_ms), (0.0, 0.0));
        assert!(b1.query_ms < b2.query_ms, "{b1:?} vs {b2:?}");
        let bound = (b1.query_ms + b2.query_ms) / 2.0;
        let mut bounded = every_minute();
        bounded.requirements.max_latency_ms = Some(bound);
        let selection = select_plan(
            &candidates,
            &[bounded],
            &arriving(10_000.0),
            PlanningModels::builtin(),
        )
        .unwrap();
        assert_eq!(selection.selected, candidates[1].id);
        let rejected = &selection.rejected[0];
        assert_eq!(rejected.id, candidates[0].id);
        assert_eq!(rejected.cause, RejectionCause::LatencyBoundExceeded);
        assert!(
            rejected.reason.starts_with(&format!(
                "q1: latency {:.1} ms per evaluation (compaction 0.0 ms + query",
                b2.total_ms()
            )) && rejected
                .reason
                .ends_with(&format!("is over its {bound} ms bound")),
            "{}",
            rejected.reason
        );
        assert!(!selection.costs.contains_key(&candidates[0].id));
    }

    /// When a maintained state's ingestion work needs more than one core,
    /// its `k` workers' partial copies of the newest window are merged
    /// before the query reads it: `(k − 1)` merges of one operation for
    /// each of its states per window, on the query's chain.
    #[test]
    fn ingestion_work_over_one_core_adds_the_newest_windows_compaction() {
        let latency = |rows_per_s: f64| {
            let candidates = pane_candidates(rows_per_s);
            price(
                &candidates[1].dag,
                &[repeating(MINUTE)],
                &arriving(rows_per_s),
                &PlanSelectionSettings::default(),
            )
            .unwrap()
            .usage
            .latency_per_query[0]
        };
        assert_eq!(latency(10_000.0).compaction_ms, 0.0);
        let fast = latency(1e10);
        // 10,000 series, one state each per window, 1 ns per merge.
        let merges = fast.compaction_ms / (10_000.0 * 1e-6);
        assert!(
            merges >= 1.0 && (merges - merges.round()).abs() < 1e-6,
            "{fast:?}"
        );
    }

    /// The newest window's compaction merges the `k` workers' partial
    /// sketches with the state's measured merge when the table has one:
    /// a 1,200 ns KLL merge makes each of the `(k − 1)·I` merges 1,200
    /// operations of 1 ns instead of one, and nothing else in the
    /// compaction changes.
    #[test]
    fn compaction_prices_merges_from_the_measured_merge() {
        let candidates = pane_candidates(1e10);
        let dag = &candidates[1].dag;
        let kll = dag
            .nodes
            .iter()
            .find_map(|n| match &n.payload {
                Payload::ASAP(
                    ASAPOp::SummaryAgg {
                        family: FieldDataType::Sketch(kind, _),
                        ..
                    }
                    | ASAPOp::PaneChain {
                        family: FieldDataType::Sketch(kind, _),
                        ..
                    },
                ) => Some(kind.params().clone()),
                _ => None,
            })
            .expect("a KLL build");
        let latency = |settings: &PlanSelectionSettings| {
            price(dag, &[repeating(MINUTE)], &arriving(1e10), settings)
                .unwrap()
                .usage
                .latency_per_query[0]
        };
        let analytical = latency(&PlanSelectionSettings::default());
        assert!(analytical.compaction_ms > 0.0, "{analytical:?}");
        let measured = latency(&PlanSelectionSettings {
            measured_operation_costs: vec![MeasuredOperationCost {
                sketch: kll,
                operation: SummaryOperation::Merge,
                cpu_ns_per_operation: 1200.0,
            }],
            ..PlanSelectionSettings::default()
        });
        assert!(
            close(measured.compaction_ms, 1200.0 * analytical.compaction_ms),
            "{analytical:?} vs {measured:?}"
        );
    }

    // ── Cost models ──────────────────────────────────────────────────────

    /// [`PlanSelectionSettings`] with `cost_model`.
    fn under(cost_model: CostModel) -> PlanSelectionSettings {
        PlanSelectionSettings {
            cost_model,
            ..PlanSelectionSettings::default()
        }
    }

    /// The candidate's usage is what its nodes are charged for: priced at
    /// one unit for a single resource under cost model 1 (CPU, scanned
    /// bytes, memory byte-seconds or disk), the nodes' prices add up to the
    /// candidate's usage of it.
    #[test]
    fn node_prices_add_up_to_the_candidates_usage() {
        let only = |set: fn(&mut ResourceUnitCost)| {
            let mut prices = ResourceUnitCost {
                cost_per_cpu_op: 0.0,
                cost_per_scanned_byte: 0.0,
                cost_per_memory_byte_second: 0.0,
                cost_per_provisioned_memory_byte_second: 0.0,
                cost_per_provisioned_cpu_core_second: 0.0,
                cost_per_disk_byte_second: 0.0,
            };
            set(&mut prices);
            PlanSelectionSettings {
                cost_model: CostModel::UsageOverTime(prices),
                ..PlanSelectionSettings::default()
            }
        };
        // Each resource's unit price and the candidate's usage of it.
        type Resource = (fn(&mut ResourceUnitCost), fn(&ResourceUsage) -> f64);
        let resources: [Resource; 4] = [
            (|p| p.cost_per_cpu_op = 1.0, |u| u.cpu_ops_per_second),
            (
                |p| p.cost_per_scanned_byte = 1.0,
                |u| u.scanned_bytes_per_second,
            ),
            (
                |p| p.cost_per_memory_byte_second = 1.0,
                |u| u.average_memory_bytes,
            ),
            (
                |p| p.cost_per_disk_byte_second = 1.0,
                |u| u.disk_bytes as f64,
            ),
        ];
        for candidate in pane_candidates(10_000.0) {
            for (set, used) in resources {
                let cost = price(
                    &candidate.dag,
                    &[repeating(MINUTE)],
                    &arriving(10_000.0),
                    &only(set),
                )
                .unwrap();
                let charged: f64 = cost
                    .per_node
                    .values()
                    .filter_map(|n| n.cost_per_second)
                    .sum();
                assert!(
                    close(charged, used(&cost.usage)),
                    "{}: {charged} vs {}",
                    candidate.id,
                    used(&cost.usage)
                );
                assert_eq!(cost.usage.latency_per_query.len(), 1);
            }
        }
    }

    /// Cost model 2 prices the candidate's peaks per second: provisioned
    /// memory, cores and disk; its nodes carry no price. B1 (maintained
    /// panes) needs a core for ingestion and one for the query; B2 (rebuilt
    /// at query time) one.
    #[test]
    fn provisioned_capacity_prices_peak_memory_cores_and_disk() {
        let prices = ResourceUnitCost {
            cost_per_disk_byte_second: 1e-9,
            ..ResourceUnitCost::illustrative()
        };
        let settings = under(CostModel::ProvisionedCapacity(prices.clone()));
        let candidates = pane_candidates(10_000.0);
        let cores: Vec<u32> = candidates
            .iter()
            .map(|candidate| {
                let cost = price(
                    &candidate.dag,
                    &[repeating(MINUTE)],
                    &arriving(10_000.0),
                    &settings,
                )
                .unwrap();
                let usage = &cost.usage;
                let expected = prices.cost_per_provisioned_memory_byte_second
                    * usage.peak_memory_bytes as f64
                    + prices.cost_per_provisioned_cpu_core_second * f64::from(usage.peak_cpu_cores)
                    + prices.cost_per_disk_byte_second * usage.disk_bytes as f64;
                assert_eq!(cost.cost_per_second, Some(expected));
                assert!(cost.per_node.values().all(|n| n.cost_per_second.is_none()));
                usage.peak_cpu_cores
            })
            .collect();
        assert_eq!(cores, [1, 2]);
    }

    /// Cost model 3 has no prices; a candidate over the cluster's cores is
    /// invalid with the reason, and the rest are chosen by the objective.
    #[test]
    fn cluster_capacity_rejects_a_candidate_over_its_cores() {
        let limits = |cpu_cores| ClusterLimits {
            memory_bytes: u64::MAX,
            disk_bytes: u64::MAX,
            cpu_cores,
            objective: ClusterObjective::LowestLatency,
        };
        let candidates = pane_candidates(10_000.0);
        let select = |cpu_cores| {
            let settings = under(CostModel::ClusterCapacity(limits(cpu_cores)));
            select_plan(
                &candidates,
                &[every_minute()],
                &arriving(10_000.0),
                PlanningModels::builtin().with_plan_selection(&settings),
            )
            .unwrap()
        };
        let unlimited = select(None);
        assert_eq!(unlimited.selected, candidates[1].id, "B1 merges faster");
        assert!(unlimited
            .costs
            .values()
            .all(|c| c.cost_per_second.is_none()));
        let one_core = select(Some(1));
        assert_eq!(one_core.selected, candidates[0].id);
        let b1 = one_core
            .rejected
            .iter()
            .find(|r| r.id == candidates[1].id)
            .unwrap();
        assert_eq!(b1.cause, RejectionCause::OverClusterCapacity);
        assert_eq!(b1.reason, "2 CPU cores at peak is over the cluster's 1");
    }

    /// Raw data kept on disk for query-time scans is counted once per
    /// source, for the longest window read: two queries over `m` keep its
    /// last 5 minutes, as the 5-minute query alone does.
    #[test]
    fn raw_data_on_disk_is_the_longest_window_per_source() {
        let disk = |queries: &[&str]| {
            let roots: Vec<_> = queries
                .iter()
                .map(|q| QueryRoot::Operator(lower_promql(q, AccuracyTarget::exact())))
                .collect();
            let dag = optimize_physical_asap(
                "L",
                &roots,
                &[],
                &data(),
                &DeploymentCapabilities::default(),
            )
            .unwrap()
            .remove(0)
            .dag;
            priced(&dag, &recurrences(queries.len())).usage.disk_bytes
        };
        let five_minutes = disk(&["sum_over_time(m[5m])"]);
        assert!(five_minutes > disk(&["sum_over_time(m[1m])"]));
        assert_eq!(
            disk(&["sum_over_time(m[1m])", "sum_over_time(m[5m])"]),
            five_minutes
        );
    }

    /// One candidate dominates another only when it is no worse on peak
    /// memory, disk and latency and better on one; equal usage does not.
    #[test]
    fn dominance_needs_one_strictly_better_resource() {
        let usage = |memory, latency_ms| ResourceUsage {
            peak_memory_bytes: memory,
            latency_per_query: vec![QueryLatency {
                compaction_ms: 0.0,
                query_ms: latency_ms,
            }],
            ..ResourceUsage::default()
        };
        assert!(dominates(&usage(1, 5.0), &usage(2, 5.0)));
        assert!(!dominates(&usage(1, 5.0), &usage(1, 5.0)));
        assert!(!dominates(&usage(1, 6.0), &usage(2, 5.0)));
    }

    /// Cost model 3's disk limit: B2 reads its 5-minute window of raw data
    /// at query time and keeps it on disk; B1 keeps none. A cluster with
    /// less disk than B2's raw data leaves only B1.
    #[test]
    fn cluster_capacity_rejects_a_candidate_over_its_disk() {
        let candidates = pane_candidates(10_000.0);
        let select = |disk_bytes| {
            let settings = under(CostModel::ClusterCapacity(ClusterLimits {
                memory_bytes: u64::MAX,
                disk_bytes,
                cpu_cores: None,
                objective: ClusterObjective::LeastMemory,
            }));
            select_plan(
                &candidates,
                &[every_minute()],
                &arriving(10_000.0),
                PlanningModels::builtin().with_plan_selection(&settings),
            )
            .unwrap()
        };
        let unlimited = select(u64::MAX);
        let raw = unlimited.costs[&candidates[0].id].usage.disk_bytes;
        assert!(raw > 0);
        assert_eq!(unlimited.costs[&candidates[1].id].usage.disk_bytes, 0);
        assert_eq!(unlimited.selected, candidates[0].id, "B2 needs less memory");
        let small = select(raw - 1);
        assert_eq!(small.selected, candidates[1].id);
        let b2 = small
            .rejected
            .iter()
            .find(|r| r.id == candidates[0].id)
            .unwrap();
        assert_eq!(b2.cause, RejectionCause::OverClusterCapacity);
        assert_eq!(
            b2.reason,
            format!("disk {raw} bytes is over the cluster's {} bytes", raw - 1)
        );
    }

    /// State materialized on disk is counted as disk, not memory: the KLL
    /// state kept on disk at ingestion time (its window being built and the
    /// completed one) or for one evaluation at query time adds its bytes to
    /// disk, nothing to average memory, and costs only its disk price.
    #[test]
    fn state_on_disk_is_counted_as_disk_not_memory() {
        use asap_types::ir::Medium;
        let settings = unit_costs(|c| c.cost_per_disk_byte_second = 1e-9);
        let work_only = unit_costs(|c| c.cost_per_memory_byte_second = 0.0);
        let state = |dag: &PhysicalASAPDAG| node_of(dag, is_build);
        for (on_disk, in_memory) in [
            (Materialization::MatAtIngestion(Medium::Disk), AT_INGESTION),
            (Materialization::MatAtQuery(Medium::Disk), AT_QUERY),
        ] {
            let (disk_dag, memory_dag) = (kll_dag(on_disk), kll_dag(in_memory));
            let disk = price(&disk_dag, &recurrences(1), &data(), &settings).unwrap();
            let memory = price(&memory_dag, &recurrences(1), &data(), &settings).unwrap();
            // The bytes kept are the same, on disk instead of in memory.
            let bytes = disk.usage.disk_bytes - memory.usage.disk_bytes;
            assert!(bytes > 0);
            assert!(
                memory.per_node[&state(&memory_dag)]
                    .detail
                    .contains(&format!(" {bytes} bytes")),
                "{in_memory:?}: {bytes}"
            );
            assert!(
                disk.usage.average_memory_bytes < memory.usage.average_memory_bytes,
                "{on_disk:?}"
            );
            assert_eq!(
                disk.usage.peak_memory_bytes + bytes,
                memory.usage.peak_memory_bytes,
                "{on_disk:?}"
            );
            // On disk, the state costs only its disk price beside the work.
            let work = price(&memory_dag, &recurrences(1), &data(), &work_only)
                .unwrap()
                .per_node[&state(&memory_dag)]
                .cost_per_second
                .unwrap();
            let on_disk_node = &disk.per_node[&state(&disk_dag)];
            assert!(
                close(
                    on_disk_node.cost_per_second.unwrap(),
                    work + 1e-9 * bytes as f64
                ),
                "{on_disk_node:?}"
            );
        }
    }

    /// Under every cluster objective, two candidates with the same usage
    /// tie, and the earlier is selected; the later is reported as tied.
    /// Under the Pareto front neither dominates the other: both are on it.
    #[test]
    fn cluster_objectives_keep_the_earlier_of_tied_candidates() {
        let b2 = pane_candidates(10_000.0).remove(0);
        let twin = PhysicalCandidate {
            id: format!("{}-twin", b2.id),
            ..b2.clone()
        };
        let candidates = [b2, twin];
        for objective in [
            ClusterObjective::LowestLatency,
            ClusterObjective::LeastMemory,
            ClusterObjective::ParetoFront,
        ] {
            let settings = under(CostModel::ClusterCapacity(ClusterLimits {
                memory_bytes: u64::MAX,
                disk_bytes: u64::MAX,
                cpu_cores: None,
                objective,
            }));
            let selection = select_plan(
                &candidates,
                &[every_minute()],
                &arriving(10_000.0),
                PlanningModels::builtin().with_plan_selection(&settings),
            )
            .unwrap();
            assert_eq!(selection.selected, candidates[0].id, "{objective:?}");
            let twin = &selection.rejected[0];
            assert_eq!(twin.id, candidates[1].id);
            assert_eq!(twin.cause, RejectionCause::TiedLostOnEnumerationOrder);
            assert!(
                twin.reason.starts_with(&format!(
                    "tied with the selected candidate {}",
                    candidates[0].id
                )),
                "{objective:?}: {}",
                twin.reason
            );
            let expected_front = match objective {
                ClusterObjective::ParetoFront => {
                    vec![candidates[0].id.clone(), candidates[1].id.clone()]
                }
                _ => Vec::new(),
            };
            assert_eq!(selection.front, expected_front, "{objective:?}");
        }
    }

    /// With more than one ingest worker, each worker beyond the first holds
    /// its own partial copy of a maintained state's newest window: B1's
    /// peak memory grows by (k − 1) panes of states, its average memory
    /// (and so cost model 1's price) does not.
    #[test]
    fn partial_copies_of_the_newest_window_add_to_peak_memory() {
        let rows_per_s = 1e9;
        let b1 = pane_candidates(rows_per_s).remove(1);
        let chain = node_of(&b1.dag, is_chain);
        let cost = price(
            &b1.dag,
            &[repeating(MINUTE)],
            &arriving(rows_per_s),
            &PlanSelectionSettings::default(),
        )
        .unwrap();
        let detail = &cost.per_node[&chain].detail;
        let workers = cost.usage.peak_cpu_cores - 1;
        assert!(workers > 1, "{workers}");
        // The chain keeps 6 panes (5 read and the one being built).
        let kept: u64 = detail
            .split("; keeps ")
            .nth(1)
            .and_then(|rest| rest.split(' ').next())
            .and_then(|bytes| bytes.parse().ok())
            .unwrap_or_else(|| panic!("{detail}"));
        let copies = workers - 1;
        let bytes = u64::from(copies) * (kept / 6);
        assert!(
            detail.contains(&format!(
                "; {copies} partial copies of its newest window, {bytes} bytes"
            )),
            "{detail}"
        );
        // At the peak, not on average: the kept bytes are B1's only
        // averaged memory beside negligible query-time holds.
        assert!(cost.usage.peak_memory_bytes >= kept + bytes);
        assert!(cost.usage.average_memory_bytes < (kept + bytes) as f64);
    }

    /// A candidate with the given latency, peak memory and disk under cost
    /// model 3.
    fn cluster_cost(latency_ms: f64, peak_memory_bytes: u64, disk_bytes: u64) -> CandidateCost {
        CandidateCost {
            usage: ResourceUsage {
                peak_memory_bytes,
                disk_bytes,
                latency_per_query: vec![QueryLatency {
                    compaction_ms: 0.0,
                    query_ms: latency_ms,
                }],
                ..ResourceUsage::default()
            },
            cost_per_second: None,
            unit: COST_UNIT,
            source: COST_SOURCE,
            per_node: BTreeMap::new(),
        }
    }

    /// Peak memory and disk are compared exactly, as dominance compares
    /// them: 100 bytes less at 124 GB is less memory, not a tie. Under the
    /// Pareto front the selected candidate is a front member, and under
    /// least memory the smaller one wins.
    #[test]
    fn memory_differing_by_a_few_bytes_is_compared_exactly() {
        let larger = cluster_cost(7.0, 124_000_000_100, 0);
        let smaller = cluster_cost(7.0, 124_000_000_000, 0);
        let costs = [&larger, &smaller];
        let (best, front) = choose(Ranking::Front, &costs).unwrap();
        assert_eq!(front, [1]);
        assert_eq!(best, 1, "the selection is on the front");
        assert_eq!(choose(Ranking::Memory, &costs).unwrap().0, 1);
        assert_eq!(choose(Ranking::Latency, &costs).unwrap().0, 1);
    }

    /// The KLL build of a `quantile_over_time` candidate, with its sketch
    /// parameters, and the candidate.
    fn kll_build() -> (PhysicalCandidate, PhysicalASAPNodeId, SketchParams) {
        let (candidates, _) = per_series_kll(AccuracyScope::PerValue);
        let candidate = candidates.into_iter().next().unwrap();
        let (build, sketch) = candidate
            .dag
            .nodes
            .iter()
            .find_map(|n| match &n.payload {
                Payload::ASAP(ASAPOp::SummaryAgg {
                    family: FieldDataType::Sketch(kind, _),
                    ..
                }) => Some((n.id, kind.params().clone())),
                _ => None,
            })
            .unwrap();
        (candidate, build, sketch)
    }

    /// A measured update time for the candidate's KLL changes that build's
    /// price and nothing else: its CPU is rows × measured ns, counted in
    /// operations of `ms_per_cpu_op`, and it reports `Measured`; every
    /// other node keeps its analytical price and reports `Analytical`.
    #[test]
    fn a_measured_entry_prices_exactly_its_node_and_reports_it() {
        let (candidate, build, sketch) = kll_build();
        let sketch_of_build = sketch.clone();
        let analytical = PlanSelectionSettings::default();
        let before = price(&candidate.dag, &recurrences(1), &data(), &analytical).unwrap();
        let cpu_ns_per_operation = 250.0;
        let measured = PlanSelectionSettings {
            measured_operation_costs: vec![MeasuredOperationCost {
                sketch,
                operation: SummaryOperation::Update,
                cpu_ns_per_operation,
            }],
            ..PlanSelectionSettings::default()
        };
        let after = price(&candidate.dag, &recurrences(1), &data(), &measured).unwrap();
        for (id, node) in &after.per_node {
            if *id == build {
                assert_eq!(node.price_source, NodePriceSource::Measured);
                assert_ne!(node.cost_per_second, before.per_node[id].cost_per_second);
            } else {
                assert_eq!(node.price_source, NodePriceSource::Analytical, "node {id}");
                assert_eq!(node, &before.per_node[id], "node {id}");
            }
        }
        assert!(after.per_node[&build]
            .detail
            .contains("250.000 measured ops per update"));
        assert!(after.cost_per_second.unwrap() > before.cost_per_second.unwrap());
        assert_eq!(before.source, COST_SOURCE);
        assert_eq!(after.source, MIXED_COST_SOURCE);
        // The build's CPU is rows × measured ns, counted in operations of
        // `ms_per_cpu_op`: the candidate's CPU operations grow linearly in
        // the measured time, (cpu(250) - cpu(100)) / (cpu(400) - cpu(100))
        // = 150 / 300.
        let cpu_at = |ns: f64| {
            let settings = PlanSelectionSettings {
                measured_operation_costs: vec![MeasuredOperationCost {
                    sketch: sketch_of_build.clone(),
                    operation: SummaryOperation::Update,
                    cpu_ns_per_operation: ns,
                }],
                ..PlanSelectionSettings::default()
            };
            price(&candidate.dag, &recurrences(1), &data(), &settings)
                .unwrap()
                .usage
                .cpu_ops_per_second
        };
        assert!(close(
            (cpu_at(cpu_ns_per_operation) - cpu_at(100.0)) / (cpu_at(400.0) - cpu_at(100.0)),
            0.5
        ));
    }

    /// A planner-sized KLL carries the failure probability it was sized for;
    /// the converter's entry has none. They are the same configuration, so
    /// the entry matches, while another `k` does not (no interpolation).
    #[test]
    fn lookup_ignores_the_kll_failure_probability() {
        let table = [MeasuredOperationCost {
            sketch: SketchParams::Kll {
                k: 331,
                failure_probability: None,
            },
            operation: SummaryOperation::Update,
            cpu_ns_per_operation: 85.0,
        }];
        let sized = |k| SketchParams::Kll {
            k,
            failure_probability: Some(0.001),
        };
        assert_eq!(
            measured_cpu_ns(&table, &sized(331), SummaryOperation::Update),
            Some(85.0)
        );
        assert_eq!(
            measured_cpu_ns(&table, &sized(330), SummaryOperation::Update),
            None
        );
    }

    /// With no measured entry for its sketch, or an entry for another
    /// operation, every node is priced from the formulas, as without a table.
    #[test]
    fn entries_for_other_sketches_or_operations_change_nothing() {
        let (candidate, _, sketch) = kll_build();
        let before = price(
            &candidate.dag,
            &recurrences(1),
            &data(),
            &PlanSelectionSettings::default(),
        )
        .unwrap();
        let other = PlanSelectionSettings {
            measured_operation_costs: vec![
                MeasuredOperationCost {
                    sketch: SketchParams::Cms {
                        width: 1024,
                        depth: 3,
                    },
                    operation: SummaryOperation::Update,
                    cpu_ns_per_operation: 50.0,
                },
                MeasuredOperationCost {
                    sketch,
                    operation: SummaryOperation::Merge,
                    cpu_ns_per_operation: 50.0,
                },
            ],
            ..PlanSelectionSettings::default()
        };
        let after = price(&candidate.dag, &recurrences(1), &data(), &other).unwrap();
        assert_eq!(after, before);
    }

    /// Bad input: a negative measured time, two entries for one sketch and
    /// operation, or a table with `ms_per_cpu_op` 0.
    #[test]
    fn invalid_measured_tables_are_rejected() {
        let entry = |cpu_ns_per_operation| MeasuredOperationCost {
            sketch: SketchParams::Cms {
                width: 1024,
                depth: 3,
            },
            operation: SummaryOperation::Update,
            cpu_ns_per_operation,
        };
        let validate = |table, ms_per_cpu_op| {
            let mut settings = PlanSelectionSettings {
                measured_operation_costs: table,
                ..PlanSelectionSettings::default()
            };
            settings.resource_speed.ms_per_cpu_op = ms_per_cpu_op;
            settings.validate()
        };
        for ns in [-1.0, f64::NAN] {
            assert!(matches!(
                validate(vec![entry(ns)], 1e-6),
                Err(AnalyticalCostError::InvalidCalibration(..))
            ));
        }
        let duplicate = validate(vec![entry(1.0), entry(2.0)], 1e-6).unwrap_err();
        assert_eq!(
            duplicate,
            AnalyticalCostError::DuplicateMeasuredOperationCost {
                sketch: entry(1.0).sketch,
                operation: SummaryOperation::Update,
            }
        );
        assert!(
            duplicate.to_string().contains("Update on Cms"),
            "{duplicate}"
        );
        assert_eq!(
            validate(vec![entry(1.0)], 0.0),
            Err(AnalyticalCostError::MeasuredCostsWithoutCpuSpeed(0.0))
        );
        let mut empty = PlanSelectionSettings::default();
        empty.resource_speed.ms_per_cpu_op = 0.0;
        assert!(
            empty.validate().is_ok(),
            "no table, no positive speed needed"
        );
    }
}

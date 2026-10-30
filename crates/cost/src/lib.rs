//! `asap-cost` — the cost model the planning stages share: how much of each
//! resource a plan uses and what that usage costs. It depends only on
//! `asap-types`, so any stage can price with it.
//!
//! - Resource-usage estimation: [`analytical`] and [`operator_statistics`]
//!   (the analytical per-operator resource model), `measured` (measured
//!   operation costs and their lookup), [`execution_demand`] (how often
//!   each node of a physical DAG is computed), [`state_bytes`] (the bytes a
//!   summary or exact state holds) and `resources` (resource dimensions
//!   with explicit CPU units).
//! - Cost models: [`ResourceUsage`], [`ResourceUnitCost`],
//!   [`ResourceSpeed`], [`CostModel`] ([`ClusterLimits`],
//!   [`ClusterObjective`]) and the deployment's [`CostInputs`], with the
//!   per-node and per-plan costs ([`NodeCost`], [`PlanCost`]).
//! - Costing a physical plan DAG: [`node_usage`] gives one node's
//!   [`NodeUsage`]; [`plan_cost`] the whole plan's [`PlanCost`]. Stage 3
//!   computes no cost of its own.
//!
//! Plan selection (Stage 3, `asap-plan-selection`) documents how it prices
//! a candidate with these.
use std::collections::BTreeMap;

use asap_types::ir::physical_export::PhysicalASAPNodeId;
use asap_types::ir::properties::ExecutionTiming;

pub mod analytical;
pub mod execution_demand;
mod measured;
pub mod operator_statistics;
mod ranking;
mod resources;
pub mod state_bytes;
mod usage;

use analytical::AnalyticalCostError;
pub use measured::{
    aggregate_lookup, lookup, CostTableProvenance, MeasuredOperationCost, MeasuredOperationCosts,
    NodePriceSource, OperatorCost, OperatorKind, StateFamily, SummaryOperation,
};
pub use ranking::{cheaper, worst_latency_ms, PlanRanking};
pub use usage::{
    family_name, kept_pane_chains, maintained_window_ms, node_map, node_usage, pane_retention_ms,
    pane_retention_ms_of_plan, panes_of, plan_cost, query_reach, retained_panes, trailing_window,
    NodeMap, NodeUsage, ProducerOutput,
};

/// Cost per second of wall time; one unit is one CPU-millisecond under
/// [`ResourceUnitCost::illustrative`].
pub const COST_UNIT: &str = "cost_per_second";
/// [`PlanCost::source`] when every node is priced from the formulas.
pub const COST_SOURCE: &str = "analytical-cost-v2 (illustrative statistics)";
/// [`PlanCost::source`] when every node is priced from the measured
/// table.
pub const MEASURED_COST_SOURCE: &str = "measured operation costs (illustrative statistics)";
/// [`PlanCost::source`] when some nodes are priced from the measured
/// table and the others from the formulas.
pub const MIXED_COST_SOURCE: &str =
    "measured operation costs and analytical-cost-v2 (illustrative statistics)";

/// Seconds over which non-repeating queries (one-off, scheduled and
/// unknown) are spread by default: one hour.
pub(crate) const DEFAULT_NON_REPEATING_QUERY_HORIZON_SECONDS: f64 = 3_600.0;

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
    /// * `cost_per_disk_byte_second` = 0: Fargate bills only ephemeral
    ///   storage configured beyond the 20 GB every task gets, which a
    ///   per-byte price cannot express, and the disk bytes plan selection
    ///   counts (the raw data query-time scans read) are held by the data
    ///   source, not by the task.
    /// * `cost_per_provisioned_memory_byte_second` and
    ///   `cost_per_provisioned_cpu_core_second` = 0: they belong to cost
    ///   model 2, and Fargate's per-task billing is cost model 1. Under
    ///   [`CostModel::ProvisionedCapacity`] every price this preset gives
    ///   is 0, which [`CostInputs::validate`] refuses.
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
        const BYTES_PER_GIB: f64 = (1u64 << 30) as f64;
        Self {
            cost_per_cpu_op: DOLLARS_PER_VCPU_SECOND * speed.ms_per_cpu_op / 1_000.0,
            cost_per_scanned_byte: DOLLARS_PER_VCPU_SECOND * speed.ms_per_scanned_byte / 1_000.0,
            cost_per_memory_byte_second: DOLLARS_PER_MEMORY_GB_SECOND / BYTES_PER_GIB,
            cost_per_provisioned_memory_byte_second: 0.0,
            cost_per_provisioned_cpu_core_second: 0.0,
            cost_per_disk_byte_second: 0.0,
        }
    }
}

/// How fast one core does work; the latency check reads it.
#[derive(Debug, Clone, Copy, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
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
    pub(crate) fn illustrative() -> Self {
        Self {
            ms_per_cpu_op: 1e-6,
            ms_per_scanned_byte: 1e-7,
        }
    }
}

/// How plan selection turns each valid candidate's [`ResourceUsage`] into a
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
}

/// The deployment's choices for plan selection (Stage 3): the cost model,
/// how fast a core works, and how long one-off queries are spread over.
/// Loadable from JSON (`stage_pipeline --plan-selection-settings <file>`);
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
    /// costs"). Also loads the earlier array of [`MeasuredOperationCost`].
    /// Default empty: every node is priced from the formulas.
    pub measured_operation_costs: MeasuredOperationCosts,
    /// Externally established formula-to-measurement scale, held constant across
    /// candidates. Must be finite and positive. None fits the complete generated
    /// candidate catalog before pricing, preserving the default calibration policy.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub frozen_calibration_factor: Option<f64>,
}

impl Default for CostInputs {
    fn default() -> Self {
        Self {
            cost_model: CostModel::UsageOverTime(ResourceUnitCost::illustrative()),
            resource_speed: ResourceSpeed::illustrative(),
            non_repeating_query_horizon_seconds: DEFAULT_NON_REPEATING_QUERY_HORIZON_SECONDS,
            measured_operation_costs: MeasuredOperationCosts::default(),
            frozen_calibration_factor: None,
        }
    }
}

impl CostInputs {
    /// Bad input: a negative or non-finite speed, a horizon that is not
    /// positive, a bad measured table ([`MeasuredOperationCosts`]: a
    /// negative or non-finite time or size, an entry naming two families,
    /// a duplicate entry, or `ms_per_cpu_op` 0), or a price the chosen cost model reads that is negative or
    /// not finite, or every such price 0 (cost model 3 reads none).
    pub fn validate(&self) -> Result<(), AnalyticalCostError> {
        if let Some(factor) = self.frozen_calibration_factor {
            CalibrationFit::from_factor(factor)?;
        }
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
        measured::validate(
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
/// [`PlanCost::per_node`]; not a machine.
#[derive(Debug, Clone, PartialEq)]
pub struct NodeCost {
    /// Its operator, as the measured table names it.
    pub operator: OperatorKind,
    /// When it runs: ingestion or query time.
    pub timing: ExecutionTiming,
    /// Its price per second under cost model 1; `None` under cost models 2
    /// and 3, which price (or limit) the whole candidate's peaks.
    pub cost_per_second: Option<f64>,
    /// Core-seconds of CPU it takes per second of wall time: its CPU
    /// operations and scanned bytes timed by [`ResourceSpeed`], what a
    /// runtime measures it against. A query's fixed per-request cost
    /// ([`MeasuredOperationCosts::ns_per_evaluation`]) is counted on the
    /// node the query reads first.
    pub cpu_core_seconds_per_second: f64,
    /// The number of consumers it is computed for (1 when materialized,
    /// [`execution_demand::computations`]).
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
pub struct PlanCost {
    pub usage: ResourceUsage,
    /// Its price per second under cost model 1 or 2 (under cost model 1, the
    /// sum of its nodes' charges plus request overhead for queries without
    /// operator nodes); `None` under cost model 3.
    pub cost_per_second: Option<f64>,
    /// The unit of `cost_per_second`: always [`COST_UNIT`].
    pub cost_unit: &'static str,
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
    /// that at half the speed), per GB-second ÷ 2^30; disk and the
    /// provisioned prices 0. It validates under cost model 1 and is
    /// refused under cost model 2, where every price it gives is 0.
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
        assert_eq!(preset.cost_per_disk_byte_second, 0.0);
        assert_eq!(preset.cost_per_provisioned_memory_byte_second, 0.0);
        assert_eq!(preset.cost_per_provisioned_cpu_core_second, 0.0);
        // A core twice as slow takes twice the vCPU-seconds per operation and per byte.
        let slower = ResourceUnitCost::aws_fargate_us_east_1(ResourceSpeed {
            ms_per_cpu_op: 2e-6,
            ms_per_scanned_byte: 2e-7,
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
            Err(AnalyticalCostError::ZeroCalibration)
        );
    }
}

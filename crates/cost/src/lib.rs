//! `asap-cost` — the cost model the planning stages share: how much of each
//! resource a plan uses and what that usage costs. It depends only on
//! `asap-types`, so any stage can cost with it.
//!
//! - Resource-usage estimation of one node ([`node`]):
//!   [`node::analytical`] and `node::operator_statistics` (the analytical
//!   per-operator resource model), [`node::state_bytes`] (the bytes a
//!   summary or exact state holds), `measured` (measured operation costs and
//!   their lookup) and `resources` (resource dimensions with explicit CPU
//!   units).
//! - The whole plan ([`plan`]): [`plan::execution_demand`] (how often each
//!   node of a physical DAG is computed), its cost, how far back its
//!   time-indexed nodes are read and how plans rank.
//! - Cost models (`model`): [`ResourceUsage`], [`ResourceUnitCost`],
//!   [`ResourceSpeed`], [`CostModel`] ([`ClusterLimits`],
//!   [`ClusterObjective`]) and the deployment's [`CostInputs`], with the
//!   per-node and per-plan costs ([`NodeCost`], [`PlanCost`]).
//! - Costing a physical plan DAG: [`node_resource_usage_and_cost`] gives one node's
//!   [`NodeResourceUsageAndCost`]; [`plan_cost`] the whole plan's [`PlanCost`].
//!   Stage 3 (`asap-stage3-plan-selection`) costs each candidate as one node
//!   with the same function and the rules `plan_cost` applies:
//!   [`plan::execution_demand::executions_per_second`],
//!   `RefreshWindow::computed_from` (an ingestion-time node's window), [`read_back_ms`],
//!   [`raw_data_disk_cost_per_second`] and
//!   [`request_overhead_cost_per_second`].
mod measured;
mod model;
pub mod node;
pub mod plan;
mod resources;

pub use asap_types::plan::time_window::read_back_ms;
pub(crate) use measured::aggregate_lookup;
pub use measured::{
    lookup, lookup_at_scale, MeasuredOperationCosts, NodeCostSource, OperatorCost, OperatorKind,
    RefreshIoWait, StateFamily,
};
pub use model::{
    CalibrationFit, ClusterLimits, ClusterObjective, CostInputs, CostModel, DataPlaneExecution,
    LatencyCalibration, LatencySelection, LatencySelectionChecks, ResourceSpeed, ResourceUnitCost,
    COST_SOURCE, MEASURED_COST_SOURCE, MIXED_COST_SOURCE,
};
pub use node::cost_terms::{node_cost_terms, CostConstant, NodeCostTerms};
pub use node::operator_statistics::EdgeStatistics;
pub use node::usage::{
    node_resource_usage_and_cost, output_interval_ms, reads_one_batch_of, summary_type_name,
    NodeCost, NodeResourceUsageAndCost, PhaseTimes, ProducerOutput,
};
pub use plan::cost::{
    close_together, ingest_decode_cpu_core_seconds_per_second, ingest_threads_building_copies,
    partial_copy_merge_ms, plan_cost, raw_data_disk_cost_per_second,
    request_overhead_cost_per_second, step_rounds, PlanCost, QueryLatency, RecommendedThreads,
    RoleThreads,
};
pub use resources::ResourceUsage;

//! `asap-cost` — the cost model the planning stages share: how much of each
//! resource a plan uses and what that usage costs. It depends only on
//! `asap-types`, so any stage can price with it.
//!
//! - Resource-usage estimation of one node ([`node`]):
//!   [`node::analytical`] and `node::operator_statistics` (the analytical
//!   per-operator resource model), [`node::state_bytes`] (the bytes a
//!   summary or exact state holds), `measured` (measured operation costs and
//!   their lookup) and `resources` (resource dimensions with explicit CPU
//!   units).
//! - The whole plan ([`plan`]): [`plan::execution_demand`] (how often each
//!   node of a physical DAG is computed), its cost, retention and ranking.
//! - Cost models (`model`): [`ResourceUsage`], [`ResourceUnitCost`],
//!   [`ResourceSpeed`], [`CostModel`] ([`ClusterLimits`],
//!   [`ClusterObjective`]) and the deployment's [`CostInputs`], with the
//!   per-node and per-plan costs ([`NodeCost`], [`PlanCost`]).
//! - Costing a physical plan DAG: [`node_usage`] gives one node's
//!   [`NodeUsage`]; [`plan_cost`] the whole plan's [`PlanCost`]. Stage 3
//!   computes no cost of its own.
//!
//! Plan selection (Stage 3, `asap-stage3-plan-selection`) documents how it prices
//! a candidate with these.
mod measured;
mod model;
pub mod node;
pub mod plan;
mod resources;

pub(crate) use measured::aggregate_lookup;
pub use measured::{
    lookup, CostTableProvenance, MeasuredOperationCost, MeasuredOperationCosts, NodeCostSource,
    OperatorCost, OperatorKind, StateFamily, SummaryOperation,
};
pub use model::{
    CalibrationFit, ClusterLimits, ClusterObjective, CostInputs, CostModel, ResourceSpeed,
    ResourceUnitCost, COST_SOURCE, MEASURED_COST_SOURCE, MIXED_COST_SOURCE,
};
pub use node::operator_statistics::EdgeStatistics;
pub use node::usage::{
    node_map, node_usage, summary_type_name, NodeCost, NodeMap, NodeUsage, ProducerOutput,
};
pub use plan::cost::{plan_cost, PlanCost, QueryLatency};
pub use plan::ranking::{cheaper, worst_latency_ms, PlanRanking};
pub use plan::retention::{
    maintained_window_ms, query_reach, retained_tumbling_windows, tumbling_window_retention_ms,
    tumbling_window_retention_ms_of_plan, tumbling_windows_of,
};
pub use resources::ResourceUsage;

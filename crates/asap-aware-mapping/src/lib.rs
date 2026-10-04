//! `asap-aware-mapping` — #509 Stage 3 and the planner facade, over the
//! Stage 1 candidates of [`asap_logical_optimizer`] and the Stage 2
//! candidates of [`asap_physical_optimizer`].
//!
//! It is being split into one crate per stage (#572): Stage 1 lives in
//! `asap-logical-optimizer` and Stage 2 in `asap-physical-optimizer`; Stage 3
//! (`plan_selection`, the cost model and its inputs) and the facade (`pass`)
//! remain here for now.
//!
//! ## Planning workflows
//!
//! - Run the #509 stage pipeline through [`optimize`] with [`StagePipeline`]:
//!   Stage 1 local alternatives, Stage 2 physical candidates, and Stage 3
//!   selection, the only stage that prices plans.
//! - Legacy: search the workload with
//!   [`asap_logical_optimizer::search_workload`], then rank with
//!   [`cost_sorted`](plan_selection::candidate_selection::cost_sorted) or
//!   select with
//!   [`global_selection`](plan_selection::candidate_selection::global_selection)
//!   and assemble each query root with
//!   [`GlobalSelection::assemble_selected_dag`](asap_logical_optimizer::GlobalSelection::assemble_selected_dag).
//!   Every summary runs at query time until Stage 2 materialization (#509)
//!   owns that choice. This path is deleted under #580.
//!
//! Physical operator binding, placement, storage, deployment, and execution
//! remain downstream responsibilities.
//!
//! ## Supporting components
//!
//! - [`cost_model`] — the [`CostModel`](cost_model::CostModel) trait every
//!   deployment's cost-based selection plugs into (issues #6, #33). This crate
//!   ships [`DefaultCostModel`](cost_model::DefaultCostModel), which keeps the
//!   built-in static preference order and exposes a numeric cost per
//!   candidate through [`CostModel::estimate_cost`](cost_model::CostModel::estimate_cost).
//! - [`recurrence`] — recurring and one-shot cost rates over a horizon.
//! - [`analytical_cost`], [`physical_plan_cost_model`], [`empirical_cost`] —
//!   analytical and evidence-based pricing for Stage 3.

pub mod analytical_cost;
pub mod cost_model;
pub mod empirical_comparison;
pub mod empirical_cost;
pub mod empirical_resources;
pub mod erp;
pub mod pane_sharing;
pub mod pass;
pub mod physical_handoff_cost;
pub mod physical_operator_statistics;
pub mod physical_plan_cost_model;
pub mod query_physical_lowering;
pub mod recurrence;
pub mod storage_io;
#[cfg(test)]
mod test_support;

pub use cost_model::{
    maintenance_operation_plan_cost_rate, raw_recompute_cost_rate, read_operation_plan_cost_rate,
    CostModel, CostProvenance, CostUnit, DefaultCostModel, ExactCompositionCostInputs,
    ExactCompositionCostRequest, ValueOperationCapabilities,
};
pub use pass::{
    optimize, OptimizationInput, OptimizationInputError, OptimizationPass, OptimizeError,
    PassNameConflict, PassRegistry, PlanOutput, PlanningModels, QueryPlan, StagePipeline,
};
pub use plan_selection::candidate_selection::{
    CompositionDecision, CostedGlobalSelection, RankedTargetSubDAGCandidates, RecurrenceProfileMap,
};
pub use recurrence::{
    evaluation_rate_of, total_cost, update_rate_from_data_workload, CostRate, EvaluationRate,
    Horizon, RecurrenceCostExplanation, RecurrenceError, RecurrenceProfile, RootRecurrence,
    UpdateRate,
};

/// #509 Stage 3 MVP: accuracy check, analytical pricing, cheapest valid plan.
pub mod plan_selection;

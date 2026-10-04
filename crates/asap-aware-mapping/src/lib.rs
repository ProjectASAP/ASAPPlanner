//! `asap-aware-mapping` — the planner facade over the #509 stage crates:
//! Stage 1 [`asap_logical_optimizer`], Stage 2 `asap-physical-optimizer` and
//! Stage 3 [`asap_plan_selection`].
//!
//! It is being retired (#572): the facade (`pass`) moves to `asap-planner`.
//!
//! ## Planning workflows
//!
//! - Run the #509 stage pipeline through [`optimize`] with [`StagePipeline`]:
//!   Stage 1 local alternatives, Stage 2 physical candidates, and Stage 3
//!   selection, the only stage that prices plans.
//! - Legacy: search the workload with
//!   [`asap_logical_optimizer::search_workload`], then rank with
//!   [`cost_sorted`](asap_plan_selection::candidate_selection::cost_sorted) or
//!   select with
//!   [`global_selection`](asap_plan_selection::candidate_selection::global_selection)
//!   and assemble each query root with
//!   [`GlobalSelection::assemble_selected_dag`](asap_logical_optimizer::GlobalSelection::assemble_selected_dag).
//!   Every summary runs at query time until Stage 2 materialization (#509)
//!   owns that choice. This path is deleted under #580.
//!
//! Physical operator binding, placement, storage, deployment, and execution
//! remain downstream responsibilities.

pub mod pass;

pub use pass::{
    optimize, OptimizationInput, OptimizationInputError, OptimizationPass, OptimizeError,
    PassNameConflict, PassRegistry, PlanOutput, QueryPlan, StagePipeline,
};

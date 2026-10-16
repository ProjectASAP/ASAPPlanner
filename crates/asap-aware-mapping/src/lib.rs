//! `asap-plan` — the cost-aware optimizer layer over the pre-ASAP intent algebra.
//!
//! This crate sits between the language-agnostic IR ([`asap_ir`]) and
//! any runtime: it consumes pre-ASAP [`OperatorNode`](asap_types::ir::OperatorNode)
//! DAGs and makes the cost-aware decisions the pre-ASAP IR deliberately
//! leaves open — which sketch (if any) realises each approximate intent.
//!
//! Identical-sub-DAG sharing over the pre-ASAP IR (`asap_types::ir::cse`)
//! runs before the stages; the stages add the ASAP-aware sharing that is
//! invisible at the pre-ASAP level ([`asap_aware_cse`], Pass 2), e.g.
//! `Quantile(x, 0.99)` and `Quantile(x, 0.95)` reading one built sketch.
//!
//! It depends only on the IR crate, never on a front end — the layering
//! invariant (arrows point up) holds here too.
//!
//! Post-lowering **canonicalization** is *not* here: it lives in
//! `asap_types::ir::canonicalize`, run inside `asap_frontend_common`'s shared
//! `resolve_root` so every front end normalizes before the IR leaves resolution
//! (issue #34, closed).
//!
//! ## Planning workflows
//!
//! The planner runs Stage 1 to Stage 3 in order: Stage 1, logical ASAP-aware
//! optimization ([`per_computation_candidates`]: Pass 1 and composition;
//! [`asap_aware_cse`]: Pass 2); Stage 2, physical ASAP-aware optimization
//! ([`stage2_physical_asap_optimization`]); Stage 3, plan selection
//! ([`stage3_plan_selection`]).
//!
//! - Run them for a parsed workload through [`optimize`] with
//!   [`Stage1ToStage3Pass`], the default pass.
//! - Call [`run_stage1_to_stage3`] over the frontends' roots to read every stage's
//!   candidates as well as the selected plan.
//!
//! Models and evidence determine which choices the helpers can justify.
//! Physical operator binding, placement, storage, deployment, and execution
//! remain downstream responsibilities. Neither taking the first candidate nor
//! assembling a logical DAG creates an executable deployment plan.
//!
//! ## Supporting components
//!
//! - [`realization`] — the summary families that can serve an aggregate
//!   intent, the intent's accuracy target and `(ε, δ)` budget, and default
//!   sketch sizing, shared by Pass 1 and Pass 2.
//! - [`accuracy`] — the [`AccuracyModel`](accuracy::AccuracyModel)
//!   extension point (issue #172): the planning-time algebra that derives
//!   a machine-readable [`AccuracyGuarantee`](asap_types::post_asap::AccuracyGuarantee)
//!   for every finalized post-ASAP value and propagates it through
//!   approximate-over-approximate compositions under conservative rules
//!   (no independence assumptions, unknown statistics stay unknown). Plan
//!   selection checks each candidate's guarantee against its query's
//!   `AccuracyTarget` before pricing it.
//! - [`analytical_cost`] and [`physical_operator_statistics`] — the
//!   analytical per-operator resource model Stage 3 prices with.
//! - [`maintained_population`] — builds a maintained-population sub-DAG for
//!   an instant top-k/quantile/sum query; Pass 1 does not offer it yet (#681).

pub mod accuracy;
pub mod analytical_cost;
/// The deployment's capabilities: supported summaries and what it retains.
pub mod deployment_capabilities;
mod function_rules;
pub mod pane_sharing;
pub mod pass;
pub mod physical_handoff_cost;
pub mod physical_operator_statistics;
pub mod query_physical_lowering;
pub mod realization;
pub mod storage_io;
#[cfg(test)]
mod test_support;

pub use accuracy::{
    AccuracyEvidenceProvider, AccuracyModel, DefaultAccuracyModel, NoAccuracyEvidence,
    PropagationStats,
};
pub use deployment_capabilities::{
    DeploymentCapabilities, DeploymentCapabilitiesError, SupportedSummary,
};
pub use pass::{
    optimize, run_stage1_to_stage3, OptimizationInput, OptimizationInputError, OptimizationPass,
    OptimizeError, PassNameConflict, PassRegistry, PlanOutput, PlanningModels, QueryPlan,
    Stage1ToStage3Pass, Stage1ToStage3Run,
};
pub use realization::summary_candidates;

pub mod maintained_population;

/// Stage 1, Pass 1: per-computation candidate generation. No execution timing is
/// assigned: that is a Stage 2 materialization decision.
pub mod per_computation_candidates;

/// Stage 1, Pass 2: ASAP-aware common-subexpression elimination (the
/// identical-expression rule).
pub mod asap_aware_cse;

/// #509 Stage 2, physical ASAP-aware optimization (MVP): materialization
/// (every sub-DAG `NotMaterialized`) and physical operator implementation,
/// producing the CandidatePhysicalASAPDAGs.
pub mod stage2_physical_asap_optimization;

/// #509 Stage 3, plan selection (MVP): accuracy check, analytical pricing
/// (a `NotMaterialized` node once per query that reads it, a materialized
/// one once), cheapest valid plan.
pub mod stage3_plan_selection;

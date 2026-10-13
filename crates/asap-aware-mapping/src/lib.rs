//! `asap-plan` — the cost-aware optimizer layer over the pre-ASAP intent algebra.
//!
//! This crate sits between the language-agnostic IR ([`asap_ir`]) and
//! any runtime: it consumes pre-ASAP [`OperatorNode`](asap_types::ir::OperatorNode)
//! DAGs and makes the cost-aware decisions the pre-ASAP IR deliberately
//! leaves open — which sketch (if any) realises each approximate intent.
//!
//! **Common sub-expression elimination (CSE) is not this crate's job.**
//! Detection is a primary pass over the pre-ASAP operator IR itself
//! (`asap_types::ir::cse`, design tracked in issue #223), run before a
//! tree ever reaches Stage 1 — see issue #222
//! for why (batch query optimization needs to see shared work across a
//! `QueryWorkload` before summary binding, not after). This crate may
//! eventually run a second, narrower CSE pass of its own over an
//! already-bound post-ASAP `OperatorNode` DAG, recognizing sharing that's invisible
//! at the pre-ASAP level by construction — e.g. `Quantile(x, 0.99)` and
//! `Quantile(x, 0.95)` are structurally distinct `AggIntent`s but can
//! still share one built sketch, read out twice. That post-ASAP pass is
//! secondary to, and downstream of, the primary pre-ASAP pass, not a
//! replacement for it.
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
//! optimization ([`per_computation_candidates`]: Pass 1 and composition);
//! Stage 2, physical ASAP-aware optimization
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
//! - [`realization`] — Pass 1's realization rules: the summary families that
//!   can serve an intent, the accuracy budget a target resolves to, summary
//!   sizing and the input a summary reads.
//! - [`maintained_population`] — exact maintained-population candidates.
//! - [`accuracy`] — the [`AccuracyModel`](accuracy::AccuracyModel) extension
//!   point (issue #172): the per-family error bounds that derive a
//!   machine-readable [`ResultGuarantee`](asap_types::post_asap::ResultGuarantee)
//!   for a summary's output, and the
//!   [`AccuracyEvidenceProvider`](accuracy::AccuracyEvidenceProvider) through
//!   which a deployment supplies empirical accuracy for a candidate's output.
//!   Its composition and budget-allocation rules, and classic HLL confidence,
//!   are not called by the stage pipeline yet.
//! - [`empirical_cost`] and [`empirical_resources`] — offline measured
//!   evidence; not called by the stage pipeline yet.
//! - [`analytical_cost`], [`physical_operator_statistics`],
//!   [`query_physical_lowering`], [`storage_io`] and
//!   [`physical_handoff_cost`] — the analytical pricing plan selection uses,
//!   and the physical lowering and storage I/O profiles a deployment can price.

pub mod accuracy;
pub mod analytical_cost;
pub mod empirical_cost;
pub mod empirical_resources;
pub mod function_rules;
pub mod pass;
pub mod physical_handoff_cost;
pub mod physical_operator_statistics;
pub mod query_physical_lowering;
/// Stage 1, Pass 1: how an aggregate intent may be realized by a summary.
pub mod realization;
pub mod storage_io;
#[cfg(test)]
mod test_support;

pub use accuracy::{
    AccuracyAllocation, AccuracyBudgetAllocator, AccuracyEvidenceProvider, AccuracyModel,
    CompositionShape, DefaultAccuracyModel, EqualSplitAllocator, NoAccuracyEvidence,
    PropagationStats, WorkloadAccuracyEvidence,
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

/// #509 Stage 2, physical ASAP-aware optimization (MVP): materialization
/// (every sub-DAG `NotMaterialized`) and physical operator implementation,
/// producing the CandidatePhysicalASAPDAGs.
pub mod stage2_physical_asap_optimization;

/// #509 Stage 3, plan selection (MVP): accuracy check, analytical pricing
/// (a `NotMaterialized` node once per query that reads it, a materialized
/// one once), cheapest valid plan.
pub mod stage3_plan_selection;

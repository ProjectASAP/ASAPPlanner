//! `asap-logical-asap-optimization` — #509 Stage 1, logical ASAP-aware
//! optimization: from the query-language frontends' logical DAGs to the
//! CandidateLogicalASAPDAGs.
//!
//! Identical-sub-DAG sharing over the logical DAG (`asap_types::ir::cse`)
//! runs before the stages; Stage 1 adds the ASAP-aware sharing that is
//! invisible at the logical level ([`asap_aware_cse`], Pass 2), e.g.
//! `Quantile(x, 0.99)` and `Quantile(x, 0.95)` reading one built sketch.
//!
//! It depends only on `asap-types`, never on a front end or a later stage:
//! Stage 2 (`asap-physical-asap-optimization`) and Stage 3
//! (`asap-plan-selection`) depend on it, and Cargo enforces that one-way
//! flow.
//!
//! Post-lowering **canonicalization** is *not* here: it lives in
//! `asap_types::ir::canonicalize`, run inside `asap_frontend_common`'s shared
//! `resolve_root` so every front end normalizes before the IR leaves resolution
//! (issue #34, closed).
//!
//! ## Components
//!
//! - [`per_computation_candidates`] — Pass 1: per-computation candidate
//!   generation and composition.
//! - [`asap_aware_cse`] — Pass 2: ASAP-aware common-subexpression elimination.
//! - [`workload_candidates`] — the whole-workload Stage 1 candidates: each
//!   composed Pass 1 candidate followed by its Pass 2 candidates.
//! - [`realization`] — the summary families that can serve an aggregate
//!   intent, the intent's accuracy target and `(ε, δ)` budget, and default
//!   sketch sizing, shared by Pass 1 and Pass 2.
//! - [`accuracy`] — the built-in analytical accuracy rules: per-family
//!   estimators, composition and budget allocation. They derive a
//!   machine-readable
//!   [`AccuracyGuarantee`](asap_types::ir::properties::AccuracyGuarantee)
//!   for every finalized value and propagate it through
//!   approximate-over-approximate compositions under conservative rules
//!   (no independence assumptions, unknown statistics stay unknown). A
//!   composition with no rule leaves the guarantee `None` and keeps the
//!   candidate. Stage 1 has no pluggable accuracy model: plan selection
//!   (Stage 3) checks each candidate's guarantee against its query's
//!   `AccuracyTarget` with the deployment's `AccuracyModel` before pricing it.
//! - [`deployment_capabilities`] — the deployment's supported summaries.
//! - [`maintained_population`] — builds a maintained-population sub-DAG for
//!   an instant top-k/quantile/sum query; Pass 1 does not offer it yet (#681).

pub mod accuracy;
/// The deployment's capabilities: supported summaries and what it retains.
pub mod deployment_capabilities;
mod function_rules;
pub mod realization;
/// Test helpers shared by the stage crates' tests (feature `test-support`).
#[cfg(any(test, feature = "test-support"))]
pub mod test_support;

pub use accuracy::PropagationStats;
pub use deployment_capabilities::{
    DeploymentCapabilities, DeploymentCapabilitiesError, SupportedSummary,
};
pub use realization::summary_candidates;

pub mod maintained_population;

/// Stage 1, Pass 1: per-computation candidate generation. No execution timing is
/// assigned: that is a Stage 2 materialization decision.
pub mod per_computation_candidates;

/// Stage 1, Pass 2: ASAP-aware common-subexpression elimination (the
/// identical-expression rule).
pub mod asap_aware_cse;

pub mod workload_candidates;

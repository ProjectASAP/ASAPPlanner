//! `asap-logical-asap-optimization` — #509 Stage 0 hash-consing and
//! Stage 1, logical ASAP-aware optimization: from the query-language
//! frontends' logical DAGs to the memo of logical candidates
//! (`asap_types::stages::LogicalASAPMemoCandidates`) that Stage 2 implements.
//!
//! Identical-sub-DAG sharing over the logical DAG (Stage 0, the `stage0`
//! module, behind [`memo_of_queries`]) runs before the
//! stages; Stage 1 adds the ASAP-aware sharing that is invisible at the
//! logical level (Pass 2, the `asap_aware_cse` module), e.g.
//! `Quantile(x, 0.99)` and `Quantile(x, 0.95)` reading one built sketch.
//!
//! It depends only on `asap-types` and the shared accuracy model
//! (`asap-accuracy`). It depends on no other stage crate, and no other
//! stage crate depends on it except in their tests (CI checks the crate
//! graph): the stage-boundary types (its input
//! `Memo`, Pass 1's `Memo` and Pass 2's `LogicalASAPMemoCandidates`) are in `asap_types::stages`, and
//! the planner facade calls each stage in turn. It depends on no
//! query-language frontend either, except with the `test-support` feature,
//! whose test helpers (`test_support`) lower PromQL test queries through
//! `asap-frontend-promql`; other crates enable that feature only for their
//! tests.
//!
//! Post-lowering **canonicalization** is *not* here: it lives in
//! `asap_frontend_common::canonicalize`, run inside that crate's shared
//! `resolve_root` so every front end normalizes before the IR leaves resolution
//! (issue #34, closed).
//!
//! ## Components
//!
//! The modules are crate-private; the crate root re-exports the items other
//! crates use, one path each, and `test_support` (feature `test-support`)
//! re-exports the internals other crates' tests call.
//!
//! - `stage0` — Stage 0: hash-consing the workload's logical DAG into memo
//!   groups through `Memo::find_or_add_group`, behind [`memo_of_queries`].
//! - `pass1::per_computation_candidates` — Pass 1: per-computation candidate
//!   generation and composition, behind
//!   [`pass1_memo`].
//! - `pass2::memo` — Pass 2: the rules over groups, behind
//!   [`generate_logical_asap_memo_candidates`].
//! - `pass1::realization` — the summary families that can serve an aggregate
//!   intent, the intent's accuracy target and `(ε, δ)` budget, and default
//!   sketch sizing, shared by Pass 1 and Pass 2.
//! - `deployment_capabilities` — the deployment's supported summaries
//!   ([`DeploymentCapabilities`]).
//!
//! Accuracy guarantees come from the built-in analytical accuracy rules in
//! `asap-accuracy` (per-family estimators, composition and budget
//! allocation). They derive a machine-readable
//! [`AccuracyGuarantee`](asap_types::ir::properties::AccuracyGuarantee)
//! for every finalized value and propagate it through
//! approximate-over-approximate compositions under conservative rules (no
//! independence assumptions, unknown statistics stay unknown). A
//! composition with no rule leaves the guarantee `None` and keeps the
//! candidate. Pass 1 takes the deployment's `AccuracyModel` and evidence
//! only to prune summaries no query reading them can use (when they cannot
//! accept an output without its guarantee) and sizes every summary for its
//! consumers; plan selection (Stage 3) checks each selected candidate's
//! guarantee against its query's `AccuracyTarget` and never resizes.

/// The deployment's capabilities: supported summaries and what it retains.
mod deployment_capabilities;
mod pass1;
mod pass2;
/// Stage 0: the memo groups of the workload's query roots.
mod stage0;
/// Test helpers shared by the stage crates' tests, and the Stage 1
/// internals those tests call (feature `test-support`).
#[cfg(any(test, feature = "test-support"))]
pub mod test_support;

// Stage 1's public API: one path per item, for the planner facade and the
// developer tools. Every other item is crate-private; the ones other crates'
// tests call are re-exported by `test_support`.

pub use deployment_capabilities::{
    DeploymentCapabilities, DeploymentCapabilitiesError, SupportedSummary,
};
// Stage 0: the memo groups of the workload's query roots, without
// candidates (`asap_types::stages::Memo`).
pub use stage0::memo_of_queries;
// Pass 1: its entry and its errors. Its output and the types the output
// holds are in `asap_types::stages`.
pub use pass1::{pass1_memo, Pass1Error, PerComputationCandidateError};
// Pass 2: its entry and its error.
pub use pass2::logical_memo_candidates::{
    generate_logical_asap_memo_candidates, pass2_after_local_window_composition,
    pass2_local_window_composition,
};
pub use pass2::memo::rules::Pass2RuleError;

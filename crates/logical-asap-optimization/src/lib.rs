//! `asap-logical-asap-optimization` — #509 Stage 1, logical ASAP-aware
//! optimization: from the query-language frontends' logical DAGs to the
//! CandidateLogicalASAPDAGs.
//!
//! Identical-sub-DAG sharing over the logical DAG (Stage 0, the `cse`
//! module, behind [`DagOfLogicalMemoGroups::from_roots`]) runs before the
//! stages; Stage 1 adds the ASAP-aware sharing that is invisible at the
//! logical level (Pass 2, the `asap_aware_cse` module), e.g.
//! `Quantile(x, 0.99)` and `Quantile(x, 0.95)` reading one built sketch.
//!
//! It depends only on `asap-types` and the shared accuracy model
//! (`asap-accuracy`), never on a later stage: Stage 2
//! (`asap-physical-asap-optimization`) and Stage 3 (`asap-plan-selection`)
//! depend on it, and Cargo enforces that one-way flow. It depends on no
//! query-language frontend either, except with the `test-support` feature,
//! whose test helpers (`test_support`) lower PromQL test queries through
//! `asap-frontend-promql`; the later stages enable that feature only for
//! their tests.
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
//! - `pass1::per_computation_candidates` — Pass 1: per-computation candidate
//!   generation and composition, behind
//!   [`generate_per_computation_logical_memo_candidates`].
//! - `pass2::asap_aware_cse` and `pass2::memo` — Pass 2: ASAP-aware
//!   common-subexpression elimination and the memo rules, behind
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
//! candidate. Stage 1 has no pluggable accuracy model: plan selection
//! (Stage 3) checks each candidate's guarantee against its query's
//! `AccuracyTarget` with the deployment's `AccuracyModel` before pricing it.

/// The deployment's capabilities: supported summaries and what it retains.
mod deployment_capabilities;
mod pass1;
mod pass2;
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
// Stage 0: the memo groups of the workload's query roots.
pub use pass2::memo::DagOfLogicalMemoGroups;
// Pass 1: its entry, its output and the types the output holds.
pub use pass1::{
    generate_per_computation_logical_memo_candidates, ComputationTarget, FusedComputation,
    HavingEntry, HavingPlacement, HowToComputeAggregateMeasure, PerComputationCandidateError,
    PerComputationLogicalMemoCandidates, PerComputationLogicalMemoCandidatesError,
};
// Pass 2: its entry, the state it keeps between pane-width rounds, and the
// pane-width domain of a round.
pub use pass2::logical_memo_candidates::{
    generate_logical_asap_memo_candidates, LogicalASAPMemoCandidatesError, Pass2RoundState,
};
pub use pass2::memo::candidates::CandidatePopulationError;
pub use pass2::memo::shared_configurations::{PaneWidthDomain, PaneWidthFrontier};
pub use pass2::memo::state_groups::StateGroupExpansionError;
// The CandidateLogicalASAPDAGs a memo stands for (developer tools'
// reference pricing).
pub use pass2::memo::enumeration::{
    build_logical_asap_dags_from_chosen_candidates, enumerate, AllChosenCandidatesPerMemoGroup,
    ChosenCandidatePerMemoGroup, ErrorBuildingLogicalASAPDagsFromChosenCandidates,
};

//! Stage 1, Pass 1: per-computation candidate generation and composition,
//! and the summary realizations it offers. Its one entry is
//! [`generate_per_computation_logical_memo_candidates`], whose output
//! ([`PerComputationLogicalMemoCandidates`]) Pass 2 extends.

/// Stage 1, Pass 1's entry and output: the per-computation candidates on
/// the memo groups ([`logical_memo_candidates::PerComputationLogicalMemoCandidates`]).
pub mod logical_memo_candidates;
/// Stage 1, Pass 1's computation targets, their candidates and their
/// composition. No execution timing is assigned: that is a Stage 2
/// materialization decision. Its target list
/// (`PerComputationCandidates`) is Pass 1's working form, public only to
/// tests (feature `test-support`).
#[cfg(any(test, feature = "test-support"))]
pub mod per_computation_candidates;
#[cfg(not(any(test, feature = "test-support")))]
pub(crate) mod per_computation_candidates;
pub mod realization;

pub use logical_memo_candidates::{
    generate_per_computation_logical_memo_candidates, PerComputationLogicalMemoCandidates,
    PerComputationLogicalMemoCandidatesError,
};
pub use per_computation_candidates::{
    ComputationTarget, FusedComputation, HavingEntry, HavingPlacement,
    HowToComputeAggregateMeasure, PerComputationCandidateError,
};

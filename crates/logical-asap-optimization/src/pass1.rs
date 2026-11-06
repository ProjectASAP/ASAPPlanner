//! Stage 1, Pass 1: per-computation candidate generation and composition,
//! and the summary realizations it offers. Its one entry is
//! [`pass1_memo`], whose output
//! memo Pass 2 extends.

/// Stage 1, Pass 1's entry: the initial candidates of the memo groups.
pub(crate) mod logical_memo_candidates;
/// Stage 1, Pass 1's computation targets, their candidates and their
/// composition. No execution timing is assigned: that is a Stage 2
/// materialization decision. Its target list
/// (`PerComputationCandidates`) is Pass 1's working form, public only to
/// tests (`crate::test_support`, feature `test-support`).
pub(crate) mod per_computation_candidates;
/// Frontend-to-Pass-1 acceptance tests over Pass 1's working form.
#[cfg(test)]
mod per_computation_candidates_tests;
pub(crate) mod realization;

pub use logical_memo_candidates::{pass1_memo, Pass1Error};
pub use per_computation_candidates::PerComputationCandidateError;

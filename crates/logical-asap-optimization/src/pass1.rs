//! Stage 1, Pass 1: per-computation candidate generation and composition,
//! and the summary realizations it offers.

/// Stage 1, Pass 1's output: the per-computation candidates on the memo
/// groups ([`logical_memo_candidates::PerComputationLogicalMemoCandidates`]).
pub mod logical_memo_candidates;
/// Stage 1, Pass 1: per-computation candidate generation. No execution timing is
/// assigned: that is a Stage 2 materialization decision.
pub mod per_computation_candidates;
pub mod realization;

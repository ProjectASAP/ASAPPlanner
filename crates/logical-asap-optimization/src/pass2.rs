//! Stage 1, Pass 2: ASAP-aware sharing over the Pass 1 candidates, through the
//! logical memo and its rules.

/// Stage 1, Pass 2: ASAP-aware common-subexpression elimination (the
/// identical-expression rule).
pub mod asap_aware_cse;
pub mod cse;
pub(crate) mod finer_aggregate;
/// Stage 1, Pass 2's entry and its output, [`logical_memo_candidates::LogicalASAPMemoCandidates`].
pub mod logical_memo_candidates;
/// Logical group identity available before Pass 1 candidate generation.
pub mod memo;
pub mod workload_candidates;

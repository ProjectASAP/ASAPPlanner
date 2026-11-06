//! Stage 1, Pass 2: the rules over groups and the logical edges.

/// Stage 1, Pass 2's entry, producing [`asap_types::stages::LogicalASAPMemoCandidates`].
pub(crate) mod logical_memo_candidates;
/// The rules over groups.
pub(crate) mod memo;

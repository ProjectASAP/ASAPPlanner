//! `asap-stage1-pass2-reuse-candidates` — #509 Stage 1, Pass 2: the reuse
//! rules over memo groups and the logical edges. Its entry,
//! [`generate_logical_asap_memo_candidates`], extends Pass 1's memo into
//! the [`LogicalASAPMemoCandidates`](asap_types::stages::LogicalASAPMemoCandidates)
//! Stage 2 expands, e.g. `Quantile(x, 0.99)` and `Quantile(x, 0.95)` reading
//! one built sketch.
//!
//! It depends only on `asap-types`, where its input and output are. It
//! depends on no other stage crate, and the planner facade calls it after
//! Pass 1.

/// The error caps, parameter floors and logical edges after the rules.
mod logical_edges;
/// Stage 1, Pass 2's entry, producing [`asap_types::stages::LogicalASAPMemoCandidates`].
mod logical_memo_candidates;
/// The rules over groups.
mod memo;
/// Parameter floors from error caps.
mod parameter_floors;

pub use logical_memo_candidates::{
    generate_logical_asap_memo_candidates, pass2_after_local_window_composition,
    pass2_local_window_composition,
};
pub use memo::rules::Pass2RuleError;
/// Pass 2's tests over memos built through the `Memo` methods.
#[cfg(test)]
mod tests;

//! `asap-stage1-pass2-reuse-candidates` — #509 Stage 1, Pass 2: the reuse
//! rules over memo groups, the readouts' required accuracy, dead-candidate
//! elimination and the logical edges. Its entry,
//! [`generate_logical_asap_memo_candidates`], extends Pass 1's memo into
//! the [`LogicalASAPMemoCandidates`](asap_types::stages::LogicalASAPMemoCandidates)
//! Stage 2 expands, e.g. `sum by (job)` and `sum by (job, instance)` of one
//! count reading one state.
//!
//! It depends on `asap-types`, where its input and output are, and on
//! `asap-accuracy` for $\alpha^{\log}$. It depends on no other stage crate,
//! and the planner facade calls it after Pass 1.

/// Step 4: dead-candidate elimination and its root causes.
mod dead_candidates;
/// Stage 1, Pass 2's entry, producing [`asap_types::stages::LogicalASAPMemoCandidates`].
mod logical_memo_candidates;
/// Step 3: reach, caps and readout requirements.
mod requirements;
/// Steps 1, 2 and 5: the rules that add groups and candidates.
mod rules;
#[cfg(test)]
mod tests;

pub use logical_memo_candidates::generate_logical_asap_memo_candidates;
pub use rules::Pass2RuleError;

//! `asap-stage1-pass1-per-computation-candidates` — #509 Stage 1, Pass 1:
//! each memo group's initial candidates. Its one entry is [`pass1_memo`],
//! which fills the memo groups of Stage 0's
//! [`Memo`](asap_types::stages::Memo) with their initial candidates (exact,
//! exact accumulator, one summary build per listed parameter configuration
//! with separate readouts, fused builds), every intermediate result its own
//! group; Pass 2 extends that memo.
//!
//! It depends only on `asap-types` (its input and output memo are in
//! `asap_types::stages`) and the shared accuracy model (`asap-accuracy`,
//! for the statistic each intent reads). It depends on no other stage
//! crate, and the planner facade calls it after Stage 0. It depends on no
//! query-language frontend either; its tests build their input memo through
//! `Memo::find_or_add_group` and `Memo::add_query`.
//!
//! Pass 1 reads no accuracy target and sets no input requirement: the error
//! of each build follows from its parameters (`parameter_list`), and Pass 2
//! gives each readout its required accuracy, decides the logical edges and
//! eliminates the candidates no query can use.

/// Stage 1, Pass 1's entry: the initial candidates of the memo groups.
mod logical_memo_candidates;
/// Stage 1, Pass 1's computation targets over the memo groups and the
/// candidates that compute them.
mod per_computation_candidates;
/// Pass 1's tests over memos built through the `Memo` methods.
#[cfg(test)]
mod per_computation_candidates_tests;
/// The sketch algorithms that can serve an aggregate intent, the input a
/// summary consumes and the column a top-k ranks.
mod realization;
/// Fixtures this crate's own tests share.
#[cfg(test)]
mod test_fixtures;

pub use logical_memo_candidates::pass1_memo;
pub use per_computation_candidates::PerComputationCandidateError;

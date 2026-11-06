//! `asap-stage1-pass1-per-computation-candidates` — #509 Stage 1, Pass 1:
//! per-computation candidate generation and the summary realizations it
//! offers. Its one entry is [`pass1_memo`], which fills the
//! memo groups of Stage 0's [`Memo`](asap_types::stages::Memo) with their
//! initial candidates; Pass 2 extends that memo.
//!
//! It depends only on `asap-types` (its input and output memo are in
//! `asap_types::stages`) and the shared accuracy model (`asap-accuracy`).
//! It depends on no other stage crate, and the planner facade calls it
//! after Stage 0. It depends on no query-language frontend either, except
//! with the `test-support` feature, whose test helpers (`test_support`)
//! lower PromQL test queries through `asap-frontend-promql`; other crates
//! enable that feature only for their tests.
//!
//! Accuracy guarantees come from the built-in analytical accuracy rules in
//! `asap-accuracy` (per-family estimators, composition and budget
//! allocation). They derive a machine-readable
//! [`AccuracyGuarantee`](asap_types::ir::properties::AccuracyGuarantee)
//! for every finalized value and propagate it through
//! approximate-over-approximate compositions under conservative rules (no
//! independence assumptions, unknown statistics stay unknown). A
//! composition with no rule leaves the guarantee `None` and keeps the
//! candidate. Pass 1 prunes nothing for accuracy; Pass 2 gives each readout
//! its required accuracy and eliminates the candidates no query can use,
//! with the deployment's measured accuracy (`AccuracyEvidence`) read before
//! each summary's theoretical guarantee.

/// Stage 1, Pass 1's entry: the initial candidates of the memo groups.
mod logical_memo_candidates;
/// Stage 1, Pass 1's computation targets and their candidates. No execution timing is assigned: that is a Stage 2
/// materialization decision. Its target list
/// (`PerComputationCandidates`) is Pass 1's working form, public only to
/// tests (`crate::test_support`, feature `test-support`).
mod per_computation_candidates;
/// Frontend-to-Pass-1 acceptance tests over Pass 1's working form.
#[cfg(test)]
mod per_computation_candidates_tests;
/// The summary families that can serve an aggregate intent, the input a
/// summary consumes and the column a top-k ranks.
mod realization;
/// Fixtures this crate's own tests share.
#[cfg(test)]
mod test_fixtures;
/// Test helpers other crates' tests call (feature `test-support`).
#[cfg(any(test, feature = "test-support"))]
pub mod test_support;

pub use logical_memo_candidates::{pass1_memo, Pass1Error};
pub use per_computation_candidates::PerComputationCandidateError;

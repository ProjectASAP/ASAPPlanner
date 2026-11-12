//! Helpers that only tests call (this crate's integration tests and other
//! crates' tests), exposed under the `test-support` feature so the normal
//! public API stays as narrow as its production users need.

/// Forwards to this crate's private functions, plus fixtures the executor's
/// tests build.
pub mod forwarders;
/// Memo builders the stage and accuracy crates' tests share.
pub mod memo_builders;
/// Operator expressions the memo builders and tests share.
pub mod memo_fixtures;

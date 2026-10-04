//! `asap-physical-optimizer` — #509 Stage 2: physical candidates.
//!
//! It turns each Stage 1 logical candidate into physical candidates: how each
//! operator is implemented, and which summary states are maintained at
//! ingestion time. Cargo enforces the stage order: this
//! crate depends only on `asap-types`, never on Stage 3, the facade or the
//! executor.
//!
//! - [`implementation`] — physical operator implementation, and one
//!   candidate per materialization choice.
//! - [`materialization`] — which summaries may run at ingestion time, and
//!   the down-closed sets of them.

pub mod implementation;
pub mod materialization;

#[cfg(test)]
mod test_support;

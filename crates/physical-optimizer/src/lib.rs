//! `asap-physical-optimizer` — #509 Stage 2: physical candidates.
//!
//! It turns each Stage 1 logical candidate into physical candidates: how each
//! operator is implemented, and (once Stage 2 materialization exists) which
//! sub-DAGs are materialized and when. Cargo enforces the stage order: this
//! crate depends only on `asap-types`, never on Stage 3, the facade or the
//! executor.
//!
//! - [`implementation`] — physical operator implementation. Every node runs at
//!   query time until Stage 2 materialization (#509) exists.

pub mod implementation;

#[cfg(test)]
mod test_support;

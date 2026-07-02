//! `asap-plan` — the cost-aware optimizer layer over the L3 intent algebra.
//!
//! This crate sits between the language-agnostic IR ([`asap_ir`]) and
//! any runtime: it consumes L3 [`QueryExpr`](asap_ir::intent_algebra::QueryExpr)
//! trees and makes the cost-aware decisions that L3 deliberately leaves open —
//! which shared sub-expressions to hoist, which sketch (if any) realises each
//! approximate intent, and the canonical form both front ends should agree on.
//!
//! It depends only on the IR crate, never on a front end — the layering
//! invariant (arrows point up) holds here too.
//!
//! ## Status
//!
//! Landed today as a **placeholder** with one real occupant, [`cse`]
//! (workload-level common-sub-expression elimination). The remaining modules
//! are intentional stubs marking where the optimizer work lands:
//!
//! - [`cost_model`] — cost traits + the model CSE credits a shared producer
//!   against (issue #6).
//! - [`boundary`] — the per-node sketch-vs-exact (accuracy) decision (issue #34
//!   cross-cutting item; an L4 concern, not carried in L3).
//! - [`canonicalize`] — a single post-lowering normalization pass both front
//!   ends run through, so semantically-equal SQL and PromQL produce identical
//!   L3 (issue #34).

pub mod cse;

pub mod boundary;
pub mod cost_model;
pub mod canonicalize;

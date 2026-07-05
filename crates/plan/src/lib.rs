//! `asap-plan` — the cost-aware optimizer layer over the L3 intent algebra.
//!
//! This crate sits between the language-agnostic IR ([`asap_ir`]) and
//! any runtime: it consumes L3 [`QueryExpr`](asap_ir::intent_algebra::QueryExpr)
//! trees and makes the cost-aware decisions that L3 deliberately leaves open —
//! which shared sub-expressions to hoist and which sketch (if any) realises
//! each approximate intent.
//!
//! It depends only on the IR crate, never on a front end — the layering
//! invariant (arrows point up) holds here too.
//!
//! Post-lowering **canonicalization** is *not* here: it landed in
//! `asap_l2::canonicalize`, run inside the shared `convert_root` so every
//! front end normalizes before L3 leaves the converter (issue #34, closed).
//!
//! ## Status
//!
//! A **placeholder** with one real occupant, [`cse`] (workload-level
//! common-sub-expression elimination). The remaining modules are intentional
//! stubs marking where the optimizer work lands:
//!
//! - [`cost_model`] — cost traits + the model CSE credits a shared producer
//!   against (issues #6, #33).
//! - [`boundary`] — the per-node sketch-vs-exact (accuracy) decision, part of
//!   the L3→L4 binding (issue #98).

pub mod cse;

pub mod boundary;
pub mod cost_model;

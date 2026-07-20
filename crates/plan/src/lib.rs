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
//! Three real occupants and one stub:
//!
//! - [`cse`] — workload-level common-sub-expression elimination.
//! - [`boundary`] — the per-intent sketch-vs-exact (accuracy) decision:
//!   `AggIntent → SummaryKind + SummaryParams` sized to the `AccuracyTarget`
//!   (issue #98).
//! - [`bind`] — the L3→L4 binding pass: walks a `QueryExpr` tree, fires the
//!   [`boundary`] decision per node, and emits the sketch-bound
//!   [`SummaryExpr`](asap_sketch::SummaryExpr) DAG (issue #98).
//! - [`cost_model`] — the [`CostModel`](cost_model::CostModel) trait every
//!   deployment's cost-based sketch selection plugs into (issues #6, #33).
//!   `asap-plan` itself only ships [`DefaultCostModel`](cost_model::DefaultCostModel),
//!   which preserves [`boundary`]'s built-in static preference order.

pub mod cse;

pub mod bind;
pub mod boundary;
pub mod cost_model;

pub use bind::{bind, bind_in, bind_in_with, bind_with, BindError};
pub use boundary::{realize, realize_with, summary_candidates, Realization};
pub use cost_model::{CostModel, DefaultCostModel};

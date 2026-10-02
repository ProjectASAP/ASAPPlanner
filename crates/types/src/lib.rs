//! `asap-types` — shared vocabulary for the whole workspace.
//!
//! - [`ir`] — the unified operator IR: one operator language before and
//!   after ASAP optimization ([`ir::OperatorNode`]), plus its passes
//!   (canonicalize, CSE, timing) and the wire export ([`ir::export`]).
//! - [`pre_asap`] — the shared field vocabulary the IR's operators are
//!   built from (grouping keys, reductions, sources, aggregation intents,
//!   scalar literal / operator kinds, [`pre_asap::Schema`]).
//! - [`post_asap`] — summary-state types (families, kinds, parameters,
//!   grouping strategy), accuracy guarantees, and the execution-timing
//!   vocabulary. No execution logic lives in this workspace (issue #190).
//!   [`post_asap::query_time`] holds pure posterior error-bound math
//!   (issue #239) a future sketch runtime's readout path can call; see its
//!   docs for why it is unwired today.
//! - [`types`] / [`workload`] / [`parsed_workload`] / [`dag_export`] /
//!   [`cost`] / [`resources`] — workload, batch, export and cost types.
pub mod cost;
pub mod dag_export;
pub mod ir;
pub mod parsed_workload;
pub mod post_asap;
pub mod pre_asap;
pub mod resources;
pub mod serde_f64;
pub mod types;
pub mod workload;

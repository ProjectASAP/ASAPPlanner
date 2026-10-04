//! `asap-types` — shared vocabulary for the whole workspace.
//!
//! - [`ir`] — the unified operator IR (#511): one operator DAG before and
//!   after ASAP optimization ([`ir::OperatorNode`]), arranged by #511 section
//!   ([`ir::operator`], [`ir::scalar`], [`ir::schema`], [`ir::properties`]),
//!   plus its passes (canonicalize, CSE, timing) and the wire export
//!   ([`ir::export`]). No execution logic lives in this crate (issue #190).
//! - [`workload`] — planner inputs (#509): query and data workloads, the
//!   lowered [`workload::parsed_workload`], and [`workload::resources`].
//! - [`deployment`] — the deployment's capabilities, a planner input (#509)
//!   beside the cost and accuracy models.
//! - [`physical`] — #509 Stage 2 decision data: exact-operator schema helpers
//!   and window-summary pane primitives.
//! - [`types`] / [`dag_export`] / [`cost`] — accuracy targets, the generic
//!   DAG export, and cost annotations.
pub mod cost;
pub mod dag_export;
pub mod deployment;
pub mod ir;
pub mod physical;
pub mod serde_f64;
pub mod types;
pub mod workload;

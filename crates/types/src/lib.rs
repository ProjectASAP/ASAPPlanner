//! `asap-types` — shared vocabulary for the whole workspace. Every planning
//! stage depends on it.
//!
//! - [`ir`] — the unified operator IR (#511): one operator DAG for logical
//!   and physical plans ([`ir::OperatorNode`]), arranged by IR section:
//!   [`ir::operator`] (§1 operators and their parameter types),
//!   [`ir::scalar`] (§2.2 scalar expressions), [`ir::schema`] (§2.1 the
//!   per-edge schema and the summary-state types it carries) and
//!   [`ir::properties`] (§2.2–2.3 accuracy, guarantee, execution timing and
//!   summary coverage), plus its passes (canonicalize, CSE), the flat DAG
//!   form ([`ir::flat`]) and the physical ASAP DAG ([`ir::physical_export`]).
//! - [`workload`] — the planner inputs (#509): the query and data
//!   workloads, the parsed workload the query-language frontends produce
//!   ([`workload::parsed_workload`]) and resource dimensions
//!   ([`workload::resources`]).
//! - [`physical`] — Stage 2 decision data: the materialization of a
//!   sub-DAG ([`physical::Materialization`]) and window-summary panes.
//!
//! No execution logic lives in this crate (issue #190).
pub mod ir;
pub mod physical;
pub mod serde_f64;
pub mod workload;

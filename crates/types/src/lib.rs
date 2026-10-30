//! `asap-types` — shared vocabulary for the whole workspace. Every planning
//! stage depends on it.
//!
//! - [`ir`] — the unified operator IR (#511): one operator DAG for logical
//!   and physical plans ([`ir::OperatorNode`]), arranged by IR section:
//!   [`ir::operator`] (§1 operators and their parameter types),
//!   [`ir::scalar`] (§2.2 scalar expressions), [`ir::schema`] (§2.1 the
//!   per-edge schema and the summary-state types it carries) and
//!   [`ir::properties`] (§2.2–2.3 accuracy, guarantee, execution timing and
//!   summary coverage), the flat DAG
//!   form ([`ir::flat`]) and the physical ASAP DAG ([`ir::physical_export`]).
//! - [`workload`] — the planner inputs (#509): the query and data
//!   workloads and the parsed workload the query-language frontends produce
//!   ([`workload::parsed_workload`]).
//! - [`physical`] — Stage 2 decision data: the materialization of a
//!   sub-DAG ([`physical::Materialization`]).
//! - [`stages`] — the types one planning stage hands the next: the Stage 1
//!   → Stage 2 memo of groups and implementation candidates
//!   ([`stages::LogicalASAPMemoCandidates`]) and the Stage 2 → Stage 3
//!   physical candidates ([`stages::PhysicalASAPMemoCandidates`]).
//!
//! No execution logic lives in this crate (issue #190).
pub mod ir;
pub mod physical;
pub mod serde_f64;
pub mod stages;
pub mod workload;

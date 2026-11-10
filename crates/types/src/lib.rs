//! `asap-types` — shared vocabulary for the whole workspace. Every planning
//! stage depends on it.
//!
//! - [`ir`] — the unified operator IR (#511): one operator DAG for logical
//!   and physical plans ([`ir::OperatorNode`]), arranged by IR section:
//!   [`ir::operator`] (§1 operators and their argument types),
//!   [`ir::scalar`] (§2.2 scalar expressions), [`ir::schema`] (§2.1 the
//!   per-edge schema) and [`ir::properties`] (§2.3 execution timing and
//!   summary coverage), and the flat DAG form ([`ir::flat`]).
//! - [`summary_type`] — each summary type's parameters and predefined
//!   parameter lists, the statistics read from it and its capabilities.
//! - [`accuracy`] — accuracy targets, the error metrics they are stated in,
//!   and accuracy guarantees.
//! - [`deployment_capabilities`] — what the deployment's executor can run and
//!   keep ([`deployment_capabilities::DeploymentCapabilities`]), the deployment
//!   input Stages 1–3 read.
//! - [`workload`] — the planner inputs (#509): the query and data
//!   workloads and the parsed workload the query-language frontends produce
//!   ([`workload::parsed_workload`]).
//! - [`stages`] — the types one planning stage hands the next: the Stage 1
//!   → Stage 2 memo of groups and implementation candidates
//!   ([`stages::LogicalASAPMemoCandidates`]), the Stage 2 → Stage 3
//!   physical candidates ([`stages::PhysicalASAPMemoCandidates`]) and their
//!   physical properties ([`stages::PhysicalProperties`]: timing and lifetime).
//! - [`plan`] — Stage 3's output: the physical ASAP DAG
//!   ([`plan::physical_asap_dag`]) and the materialized views it installs.
//!
//! No execution logic lives in this crate (issue #190).
pub mod accuracy;
pub mod deployment_capabilities;
pub mod ir;
pub mod plan;
pub mod serde_f64;
pub mod stages;
pub mod summary_type;
#[cfg(feature = "test-support")]
pub mod test_support;
pub mod workload;

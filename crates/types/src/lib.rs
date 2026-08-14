//! `asap-types` — shared vocabulary for the whole workspace.
//!
//! Merges the former `asap-ir` crate (the L3 intent algebra, workload/batch
//! types, and DAG export) with the data-type-only modules of the former
//! `asap-sketch` crate (the L4 sketch-bound IR types, under [`post_asap`]).
//!
//! - [`intent_algebra`] / [`types`] / [`workload`] / [`dag_export`] — the
//!   pre-ASAP L3 IR: language-agnostic query intent, independent of any
//!   sketch decision.
//! - [`post_asap`] — the post-ASAP L4 IR: sketch-bound types
//!   ([`post_asap::sketch`], [`post_asap::expr`], [`post_asap::schema`])
//!   that commit to a concrete `SummaryKind`/`SummaryParams` realization.
//!   No execution logic lives in this workspace (see issue #190) — a
//!   downstream deployment crate is expected to supply that.
pub mod dag_export;
pub mod intent_algebra;
pub mod post_asap;
pub mod types;
pub mod workload;

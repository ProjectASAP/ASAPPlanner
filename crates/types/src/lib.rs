//! `asap-types` — shared vocabulary for the whole workspace.
//!
//! Merges the former `asap-ir` crate (the pre-ASAP intent algebra,
//! workload/batch types, and DAG export) with the data-type-only modules of
//! the former `asap-sketch` crate (the post-ASAP sketch-bound IR types,
//! under [`post_asap`]).
//!
//! - [`pre_asap`] / [`types`] / [`workload`] / [`dag_export`] — the
//!   pre-ASAP IR: language-agnostic query intent, independent of any
//!   sketch decision.
//! - [`post_asap`] — the post-ASAP IR: sketch-bound types
//!   ([`post_asap::sketch`], [`post_asap::expr`], [`post_asap::schema`])
//!   that commit to a concrete `SummaryKind`/`SummaryParams` realization.
//!   No execution logic lives in this workspace (see issue #190) — a
//!   downstream deployment crate is expected to supply that.
pub mod dag_export;
pub mod post_asap;
pub mod pre_asap;
pub mod types;
pub mod workload;

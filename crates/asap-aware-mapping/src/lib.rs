//! `asap-plan` — the cost-aware optimizer layer over the L3 intent algebra.
//!
//! This crate sits between the language-agnostic IR ([`asap_ir`]) and
//! any runtime: it consumes L3 [`QueryExpr`](asap_types::pre_asap::QueryExpr)
//! trees and makes the cost-aware decisions that L3 deliberately leaves open —
//! which shared sub-expressions to hoist and which sketch (if any) realises
//! each approximate intent.
//!
//! It depends only on the IR crate, never on a front end — the layering
//! invariant (arrows point up) holds here too.
//!
//! Post-lowering **canonicalization** is *not* here: it landed in
//! `asap_types::pre_asap::canonicalize`, run inside the shared `convert_root` so every
//! front end normalizes before L3 leaves the converter (issue #34, closed).
//!
//! ## Status
//!
//! Two real occupants and one stub:
//!
//! - [`boundary`] — the per-intent sketch-vs-exact (accuracy) decision:
//!   `AggIntent → SummaryKind + SummaryParams` sized to the `AccuracyTarget`
//!   (issue #98). [`boundary::implementation_for`] is the per-node decision;
//!   [`bind::implement_tree`] drives it over a whole tree — see the
//!   terminology section below for why these two are named around
//!   "implementation" rather than "bind".
//! - [`cost_model`] — the [`CostModel`](cost_model::CostModel) trait every
//!   deployment's cost-based sketch selection plugs into (issues #6, #33).
//!   `asap-plan` itself only ships [`DefaultCostModel`](cost_model::DefaultCostModel),
//!   which preserves [`boundary`]'s built-in static preference order.
//!
//! ## Terminology — "bind" already means three different things nearby;
//! this crate's own logical→physical step is named "implementation" instead
//!
//! `asap-plan` and its downstream consumers (e.g. `ASAPQuery-backend`'s
//! `control_plane`) independently reused the word "bind" for three
//! *different*, layer-specific meanings — none of which is what this
//! crate's [`boundary`]/[`bind`] modules do. To avoid becoming a fourth,
//! colliding sense of the same word, this crate names its own logical
//! intent → physical realization step after the term the query-optimization
//! literature already uses for exactly that step: **implementation**
//! (Cascades/Volcano's "implementation rule", logical → physical, as
//! distinct from a *transformation rule*, logical → logical — see Graefe,
//! *The Cascades Framework for Query Optimization*):
//!
//! | Term | Layer | Meaning | Lives in |
//! |---|---|---|---|
//! | **Parse** | L1 | text (PromQL/SQL) → AST | `asap-frontend-promql` / `asap-frontend-sql` |
//! | **Bind #1** | L2 | *name resolution*: `ColumnRef` (a name) → `ColumnId` (a concrete schema column) — the classic RDBMS "Parse → **Bind** → Optimize" pipeline sense (e.g. SQL Server's query-processor terminology) | [`asap_types::pre_asap::binder::Binder`](https://docs.rs/asap-types) |
//! | **Implementation** — [`boundary::implementation_for`] | L3→L4, *one node* | choosing a concrete physical realization (a sketch family, an exact accumulator, or pass-through) for one [`AggIntent`](asap_types::pre_asap::agg_intent::AggIntent) | [`boundary`] |
//! | **`implement_tree`** — [`bind::implement_tree`] | L3→L4, *whole tree* | walk a whole `QueryExpr` tree, calling [`boundary::implementation_for`] per node, and emit the complete L4 [`SummaryExpr`](asap_types::post_asap::SummaryExpr)/`L4Node` DAG — named after "implementation" too rather than reusing "bind" a second time | [`bind`] |
//! | **Bind #2** (downstream, not in this crate) | L4→L5 | a *deployment's* own physical binder, additionally deciding **placement** (edge vs. backend, wire format, …) — a genuinely different, deployment-specific decision this crate doesn't model at all | e.g. `control_plane::sketch_algebra::rules::bind_*` (as of this writing; expected to fold into that deployment's cost-model layer rather than stay a separate "bind" concept) |
//!
//! A related question (tracked alongside issues #6/#33): whether this
//! crate should also own a **matching** predicate — "does an already
//! *available* `Implementation` satisfy a *required* one" — the way a
//! database's materialized-view matching / "answering queries using
//! views" layer does. It owns the *question*, not an *answer*:
//! [`boundary::Matcher`] is a trait with no default implementation and no
//! shipped instance, the same shape as [`cost_model::CostModel`] and for
//! the same reason — which `Implementation`s are actually *available*
//! anywhere is entirely a downstream deployment's concern (an inventory
//! this crate has no way to see), and even the pure sketch-algebra
//! compatibility rules (e.g. a heap-bearing top-k sketch also satisfying a
//! bare frequency point-query) turned out to have deployment-specific
//! competitors (e.g. single-vs-multi-population re-aggregation) that
//! don't reduce to a fact about `SummaryKind` alone. `control_plane`'s own
//! `sketch_algebra::capability::Capability`/`is_satisfied_by` is the
//! reference downstream implementation.

pub mod bind;
pub mod boundary;
pub mod cost_model;

pub use bind::{implement_tree, implement_tree_with, ImplementError};
pub use boundary::{
    implementation_for, implementation_for_with, summary_candidates, Implementation, Matcher,
};
pub use cost_model::{CostModel, DefaultCostModel};

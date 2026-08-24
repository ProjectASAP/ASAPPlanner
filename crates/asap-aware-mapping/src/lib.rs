//! `asap-plan` — the cost-aware optimizer layer over the pre-ASAP intent algebra.
//!
//! This crate sits between the language-agnostic IR ([`asap_ir`]) and
//! any runtime: it consumes pre-ASAP [`QueryExpr`](asap_types::pre_asap::QueryExpr)
//! trees and makes the cost-aware decisions the pre-ASAP IR deliberately
//! leaves open — which sketch (if any) realises each approximate intent.
//!
//! **Common sub-expression elimination (CSE) is not this crate's job.**
//! Detection is a primary pass over the pre-ASAP `QueryExpr` IR itself
//! (`asap_types::pre_asap`, design tracked in issue #223), run before a
//! tree ever reaches [`bind::implement_tree`] — see issue #222 for why
//! (batch query optimization needs to see shared work across a
//! `QueryWorkload` before summary binding, not after). This crate may
//! eventually run a second, narrower CSE pass of its own over an
//! already-[`implement_tree`](bind::implement_tree)'d
//! `SummaryExpr`/`SummaryNode` DAG, recognizing sharing that's invisible
//! at the pre-ASAP level by construction — e.g. `Quantile(x, 0.99)` and
//! `Quantile(x, 0.95)` are structurally distinct `AggIntent`s but can
//! still share one built sketch, read out twice. That post-ASAP pass is
//! secondary to, and downstream of, the primary pre-ASAP pass, not a
//! replacement for it.
//!
//! It depends only on the IR crate, never on a front end — the layering
//! invariant (arrows point up) holds here too.
//!
//! Post-lowering **canonicalization** is *not* here: it landed in
//! `asap_types::pre_asap::canonicalize`, run inside the shared `resolve_root`
//! so every front end normalizes before the pre-ASAP IR leaves resolution
//! (issue #34, closed).
//!
//! ## Status
//!
//! Three real occupants and one stub:
//!
//! - [`implementation`] — [`implementation::implementations_for_with`] is
//!   where every valid realization of an `AggIntent` gets decided: exhaustive
//!   and ranked (most-preferred first via a `CostModel`), sized to the
//!   `AccuracyTarget` (issue #98). Nothing else in this crate computes an
//!   `Implementation` independently of this list.
//! - [`replacement`] — the `TargetSubDAG`/`ReplacementSubDAG`/
//!   `ReplacementStrategy` vocabulary `docs/design_docs/asap_aware_mapping.md` stubs out
//!   under "Key concepts (not yet implemented)", implemented for real (issue
//!   #251, part of #33): [`replacement::SketchFamilyStrategy`] wraps
//!   [`implementation::implementations_for_with`]'s list directly, keeping
//!   *every* candidate as its own bound [`SummaryNode`](asap_types::post_asap::SummaryNode)
//!   instead of just one — [`replacement::SharedSubtreeStrategy`] does the
//!   same for the build-independently-vs-build-once-and-share choice at a
//!   CSE-detected shared subtree. No search/ranking-across-a-whole-plan logic
//!   lives here — see that module's docs for what's deliberately left to a
//!   future Cascades/Volcano-style search engine.
//! - [`bind`] — [`bind::implement_tree`]/[`bind::implement_tree_with`] walk a
//!   whole tree, keeping only the first (`cost_model`-preferred) candidate
//!   [`replacement::SketchFamilyStrategy`] enumerates at each node — a thin
//!   selector over `replacement`, not a second decision path (see
//!   `bind.rs`'s and `replacement.rs`'s own module docs for the full
//!   relationship). [`bind::implement_workload`] drives the same selection
//!   over a whole workload's roots, memoized on `Rc` identity so two roots
//!   that `asap_types::pre_asap::cse::share_common_subtrees` already
//!   collapsed onto one shared subtree bind to one shared `SummaryNode` too
//!   (issue #212, #222, #223) — see the terminology section below for why
//!   this module (and `implement_tree`) are named around "implementation"
//!   rather than "bind".
//! - [`cost_model`] — the [`CostModel`](cost_model::CostModel) trait every
//!   deployment's cost-based sketch selection plugs into (issues #6, #33).
//!   `asap-plan` itself only ships [`DefaultCostModel`](cost_model::DefaultCostModel),
//!   which preserves [`implementation`]'s built-in static preference order.
//!
//! ## Terminology — "bind" already means three different things nearby;
//! this crate's own logical→physical step is named "implementation" instead
//!
//! `asap-plan` and its downstream consumers (e.g. `ASAPQuery-backend`'s
//! `control_plane`) independently reused the word "bind" for three
//! *different*, layer-specific meanings — none of which is what this
//! crate's [`implementation`]/[`bind`] modules do. To avoid becoming a
//! fourth, colliding sense of the same word, this crate names its own
//! logical intent → physical realization step after the term the
//! query-optimization literature already uses for exactly that step:
//! **implementation** (Cascades/Volcano's "implementation rule", logical →
//! physical, as distinct from a *transformation rule*, logical → logical —
//! see Graefe, *The Cascades Framework for Query Optimization*) — and this
//! module is named after that same term for the same reason.
//!
//! | Term | Stage | Meaning | Lives in |
//! |---|---|---|---|
//! | **Parse** | parse | text (PromQL/SQL) → AST | `asap-frontend-promql` / `asap-frontend-sql` |
//! | **Bind #1** | name resolution | `ColumnRef` (a name) → `ColumnId` (a concrete schema column) — the classic RDBMS "Parse → **Bind** → Optimize" pipeline sense (e.g. SQL Server's query-processor terminology) | [`asap_types::pre_asap::binder::Binder`](https://docs.rs/asap-types) |
//! | **Implementation** — [`implementation::implementations_for_with`] | pre-ASAP → post-ASAP, *one node* | enumerating every concrete physical realization (a sketch family, an exact accumulator, or pass-through) for one [`AggIntent`](asap_types::pre_asap::agg_intent::AggIntent) | [`implementation`] |
//! | **`implement_tree`** — [`bind::implement_tree`] | pre-ASAP → post-ASAP, *whole tree* | walk a whole `QueryExpr` tree, keeping the first candidate [`implementation::implementations_for_with`] produces per node, and emit the complete post-ASAP [`SummaryExpr`](asap_types::post_asap::SummaryExpr)/`SummaryNode` DAG — named after "implementation" too rather than reusing "bind" a second time | [`bind`] |
//! | **Bind #2** (downstream, not in this crate) | post-ASAP → deployment placement | a *deployment's* own physical binder, additionally deciding **placement** (edge vs. backend, wire format, …) — a genuinely different, deployment-specific decision this crate doesn't model at all | e.g. `control_plane::sketch_algebra::rules::bind_*` (as of this writing; expected to fold into that deployment's cost-model layer rather than stay a separate "bind" concept) |
//!
//! A related question (tracked alongside issues #6/#33): whether this
//! crate should also own a **matching** predicate — "does an already
//! *available* `Implementation` satisfy a *required* one" — the way a
//! database's materialized-view matching / "answering queries using
//! views" layer does. It owns the *question*, not an *answer*:
//! [`implementation::Matcher`] is a trait with no default implementation and
//! no shipped instance, the same shape as [`cost_model::CostModel`] and for
//! the same reason — which `Implementation`s are actually *available*
//! anywhere is entirely a downstream deployment's concern (an inventory
//! this crate has no way to see), and even the pure sketch-algebra
//! compatibility rules (e.g. a heap-bearing top-k sketch also satisfying a
//! bare frequency point-query) turned out to have deployment-specific
//! competitors (e.g. single-vs-multi-population re-aggregation) that
//! don't reduce to a fact about a summary family's kind alone. `control_plane`'s own
//! `sketch_algebra::capability::Capability`/`is_satisfied_by` is the
//! reference downstream implementation.

pub mod bind;
pub mod cost_model;
pub mod implementation;
pub mod replacement;

pub use bind::{
    implement_tree, implement_tree_with, implement_workload, implement_workload_with,
    ImplementError,
};
pub use cost_model::{CostModel, DefaultCostModel};
pub use implementation::{implementations_for_with, summary_candidates, Implementation, Matcher};
pub use replacement::{
    Replacement, ReplacementStrategy, ReplacementSubDAG, SharedSubtreeStrategy,
    SketchFamilyStrategy, TargetSubDAG,
};

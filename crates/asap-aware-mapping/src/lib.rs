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
//! tree ever reaches [`replacement::SketchFamilyStrategy`] — see issue #222
//! for why (batch query optimization needs to see shared work across a
//! `QueryWorkload` before summary binding, not after). This crate may
//! eventually run a second, narrower CSE pass of its own over an
//! already-bound `SummaryExpr`/`SummaryNode` DAG, recognizing sharing that's invisible
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
//! - [`replacement`] — the `TargetSubDAG`/`ReplacementSubDAG`/
//!   `ReplacementStrategy` vocabulary `docs/design_docs/asap_aware_mapping.md` stubs out
//!   under "Key concepts (not yet implemented)", implemented for real (issue
//!   #251, part of #33) — plus, merged into the same module (issue #252,
//!   part of #33), the workload-level search engine that decides *and*
//!   constructs *and* searches across a whole workload's worth of targets,
//!   all as one module's job. One module, three connected steps:
//!   1. [`replacement::SketchFamilyStrategy::replacements`] both *decides*
//!      what an `AggIntent` may become ([`replacement::implementations_for_with`],
//!      exhaustive and ranked via a `CostModel`, sized to the
//!      `AccuracyTarget`) and *constructs* each candidate's bound
//!      [`SummaryNode`](asap_types::post_asap::SummaryNode) — every
//!      candidate comes back, not just one.
//!      [`replacement::SharedSubtreeStrategy`] does the analogous job for
//!      the build-independently-vs-build-once-and-share choice at a
//!      CSE-detected shared subtree.
//!   2. [`replacement::search_workload`]/[`replacement::search_workload_with`]
//!      *search* — discover every candidate `TargetSubDAG` across a whole
//!      workload (not just one target in isolation) and run both strategies
//!      above against each one, to a fixpoint, without ever materializing a
//!      flat `2^N`-sized candidate-plan list: [`replacement::PlanSpace`] holds
//!      one Cascades-style [`replacement::MemoGroup`] per distinct target,
//!      each carrying every alternative discovered for it.
//!   3. [`replacement::PlanSpace::cost_sorted`] is the final
//!      `sorted_by(cost_model)` step, ranking each group's candidates
//!      best-first via the same [`CostModel`](cost_model::CostModel) the
//!      single-target steps above already consult — see
//!      [`replacement`]'s own module docs for the full design (MEMO groups
//!      vs. flat plans, dedup discipline, termination, cost-based ranking).
//! - [`bind`] — has no "bind me one tree" entry point for a single target.
//!   [`replacement::SketchFamilyStrategy`] is the only public way to get
//!   bound output for a target, and it always returns *every* candidate; a
//!   caller that wants one answer takes the first entry itself. What `bind`
//!   provides is workload-wide orchestration:
//!   [`bind::implement_workload`]/[`bind::implement_workload_with`], which
//!   drive rank-and-take-first selection over a whole workload's roots,
//!   memoized on `Rc` identity so two roots that
//!   `asap_types::pre_asap::cse::share_common_subtrees` already collapsed
//!   onto one shared subtree bind to one shared `SummaryNode` too (issue
//!   #212, #222, #223) — memoization needs one canonical decision per
//!   shared root to key sharing on, so that entry point is the one place a
//!   single-answer selection still lives. Everything else `bind` used to
//!   hold ([`replacement::select_and_bind`], the single-target rank-and-
//!   take-first helper, and [`replacement::keep_pre_asap`], the pass-through
//!   fallback) moved into [`replacement`] itself — both are single-target-
//!   scoped helpers with no real workload-level state, the same category as
//!   everything else that module owns; only the genuine cross-root
//!   `Rc`-identity memoization state above still needs its own module. See
//!   the terminology section below for why this crate's logical→physical
//!   step is named "implementation" rather than "bind".
//! - [`cost_model`] — the [`CostModel`](cost_model::CostModel) trait every
//!   deployment's cost-based sketch selection plugs into (issues #6, #33).
//!   `asap-plan` itself only ships [`DefaultCostModel`](cost_model::DefaultCostModel),
//!   which preserves [`replacement`]'s built-in static preference order and
//!   — via [`CostModel::estimate_cost`](cost_model::CostModel::estimate_cost)
//!   — exposes an actual numeric cost per candidate, not just a relative
//!   rank, for a caller (e.g. a DAG-visualization view) that wants to show
//!   "candidate A costs ≈ X" next to "candidate B costs ≈ Y".
//!
//! ## Terminology — "bind" already means three different things nearby;
//! this crate's own logical→physical step is named "implementation" instead
//!
//! `asap-plan` and its downstream consumers (e.g. `ASAPQuery-backend`'s
//! `control_plane`) independently reused the word "bind" for three
//! *different*, layer-specific meanings — none of which is what this
//! crate's [`replacement`]/[`bind`] modules do. To avoid becoming a
//! fourth, colliding sense of the same word, this crate names its own
//! logical intent → physical realization step after the term the
//! query-optimization literature already uses for exactly that step:
//! **implementation** (Cascades/Volcano's "implementation rule", logical →
//! physical, as distinct from a *transformation rule*, logical → logical —
//! see Graefe, *The Cascades Framework for Query Optimization*).
//!
//! | Term | Stage | Meaning | Lives in |
//! |---|---|---|---|
//! | **Parse** | parse | text (PromQL/SQL) → AST | `asap-frontend-promql` / `asap-frontend-sql` |
//! | **Bind #1** | name resolution | `ColumnRef` (a name) → `ColumnId` (a concrete schema column) — the classic RDBMS "Parse → **Bind** → Optimize" pipeline sense (e.g. SQL Server's query-processor terminology) | [`asap_types::pre_asap::binder::Binder`](https://docs.rs/asap-types) |
//! | **Implementation** — `replacement::implementations_for_with` | pre-ASAP → post-ASAP, *one node* | enumerating every concrete physical realization (a sketch family, an exact accumulator, or pass-through) for one [`AggIntent`](asap_types::pre_asap::agg_intent::AggIntent) | [`replacement`] |
//! | **Replacement** — [`replacement::SketchFamilyStrategy::replacements`] | pre-ASAP → post-ASAP, *one target, every candidate* | wrap each `implementations_for_with` candidate into its own bound [`SummaryNode`](asap_types::post_asap::SummaryNode), ranked — a caller wanting one answer takes the first entry itself | [`replacement`] |
//! | **Search** — [`replacement::search_workload`]/[`replacement::search_workload_with`] | pre-ASAP → post-ASAP, *whole workload, every candidate* | a Cascades/Volcano-style MEMO search: discover every candidate `TargetSubDAG` across a whole workload (not just one target in isolation), run every registered `ReplacementStrategy` against each to a fixpoint, and dedup into a [`replacement::PlanSpace`] — one [`replacement::MemoGroup`] per distinct `TargetSubDAG` holding every alternative discovered for it, never a flat `2^N`-sized list of whole candidate plans | [`replacement`] |
//! | **`implement_workload`** — [`bind::implement_workload`] | pre-ASAP → post-ASAP, *whole workload, one answer* | walk every root of a `QueryWorkload`, keeping the first (`cost_model`-preferred) candidate per node, sharing one bound `SummaryNode` across roots CSE already collapsed onto one `Rc<QueryExpr>` — emits the complete post-ASAP `SummaryExpr`/`SummaryNode` DAG | [`bind`] |
//! | **Bind #2** (downstream, not in this crate) | post-ASAP → deployment placement | a *deployment's* own physical binder, additionally deciding **placement** (edge vs. backend, wire format, …) — a genuinely different, deployment-specific decision this crate doesn't model at all | e.g. `control_plane::sketch_algebra::rules::bind_*` (as of this writing; expected to fold into that deployment's cost-model layer rather than stay a separate "bind" concept) |
//!
//! A related question (tracked alongside issues #6/#33): whether this
//! crate should also own a **matching** predicate — "does an already
//! *available* `Implementation` satisfy a *required* one" — the way a
//! database's materialized-view matching / "answering queries using
//! views" layer does. It owns the *question*, not an *answer*:
//! [`replacement::Matcher`] is a trait with no default implementation and
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
pub mod replacement;

pub use bind::{implement_workload, implement_workload_with, ImplementError};
pub use cost_model::{CostModel, DefaultCostModel};
pub use replacement::{
    default_strategies, default_strategies_with, search_workload, search_workload_with,
    summary_candidates, Implementation, Matcher, MemoGroup, PlanSpace, RankedGroup, Replacement,
    ReplacementStrategy, ReplacementSubDAG, SharedSubtreeStrategy, SketchFamilyStrategy,
    TargetSubDAG, MAX_SEARCH_ITERATIONS,
};

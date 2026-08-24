//! `TargetSubDAG` / `ReplacementSubDAG` / `ReplacementStrategy` — the
//! candidate-replacement vocabulary `docs/design_docs/asap_aware_mapping.md` stubs out
//! under "Key concepts (not yet implemented)", implemented for real (issue
//! #251, part of #33).
//!
//! ## One step, not two: `SketchAlgorithmStrategy::replacements()` decides *and* builds
//!
//! For a bindable `Aggregate`, `SketchAlgorithmStrategy::replacements()` is the
//! single place this crate both decides what an `AggIntent` may become and
//! turns each of those candidates into a real, executable
//! [`ReplacementSubDAG`]:
//!
//! 1. **Decide**: [`implementations_for_with`] enumerates every valid
//!    [`Implementation`] for the target's intent — exhaustive, and ranked
//!    most-preferred-first via a [`CostModel`] (candidate sketch family/kind,
//!    already sized to the target's own accuracy target: `Implementation::Sketch`'s
//!    `params` are the output of inverting that accuracy target through
//!    `CostModel::size_params`, not a placeholder filled in later).
//! 2. **Build**: for each candidate in that list, [`construct_summary`]
//!    mechanically turns the already-decided `(kind, params)` into a real
//!    [`SummaryNode`] — derives the child schema, resolves the summarized
//!    column, builds the readout query, recurses into the child (via
//!    [`realize_child`], so a nested aggregate gets its own
//!    independent enumeration, never the outer target's forced choice), and
//!    assembles the `SummaryAgg`/`SummaryEstimate` node.
//!
//! There is no separate decision step and construction step living in
//! different modules bridged by a named "given an `Implementation`, bind it"
//! function — step 2 is *not* a second decision (nothing about which
//! candidate to prefer happens there), it is mechanical construction that
//! has to run regardless of how `(kind, params)` were chosen, so it lives
//! directly inside the one method that needs it.
//!
//! - [`TargetSubDAG`] — a reference to a pre-ASAP [`QueryExpr`] node that is a
//!   candidate for replacement, plus how many places in the workload already
//!   reference it (its `consumer_count`) — the one piece of cross-node
//!   context [`SharedSubtreeStrategy`] needs that a bare node reference alone
//!   doesn't carry.
//! - [`ReplacementSubDAG`] — one candidate replacement for a `TargetSubDAG`:
//!   either a fully bound [`SummaryNode`] or a pre-ASAP [`QueryExpr`] rewrite
//!   (still logical, structurally different from the target but semantically
//!   equivalent) — see [`Replacement`] — plus a human-readable `rationale`.
//! - [`ReplacementStrategy`] — `matches` + `replacements`, the same
//!   extension-point shape [`CostModel`] and [`Matcher`] already use in this
//!   crate: a new replacement source is a new `impl ReplacementStrategy`, not
//!   a restructuring of this trait or of any existing strategy. `replacements`
//!   is **exhaustive, not ranked, not filtered** — reporting "every valid
//!   candidate" is core's job; picking the best one is left to the caller.
//!   [`crate::applicability`] (issue #257) is this trait's own downstream
//!   consumer, not a second extension point: it reports "which optimizations
//!   are applicable" as a pure view over the candidates strategies registered
//!   here already produced, rather than re-deriving applicability with a rule
//!   of its own.
//!
//! A caller that wants one executable answer takes the first
//! (`cost_model`-preferred) entry off `replacements()` itself
//! (`.into_iter().next()`) — that "keep the head" step lives entirely on the
//! calling side, not behind a second module-level entry point. This
//! module's own [`realize_child`] performs that take-first step
//! internally, but only for one, narrow, single-target purpose: recursing
//! into a node's child while constructing one concrete candidate (see
//! [`construct_summary_agg`]) and, symmetrically, recovering one
//! representative bound node for a [`CostModel::cse_share_decision`]
//! comparison (see [`realize_one`]) — never a workload-wide "commit to one
//! final answer" step. Committing to one physically-materialized answer for
//! a whole workload (previously `bind::implement_workload`/
//! `implement_workload_with`) is out of this crate's scope — see the crate
//! doc's `## Status` section for why. Every other caller goes through
//! `SketchAlgorithmStrategy::replacements` directly and decides for itself.
//!
//! This means an ordinary single-target bind sizes and fully constructs
//! *every* sketch candidate at every sketch-capable node (not just the one a
//! caller keeps) — a deliberate tradeoff, made so there is exactly one place
//! in this crate that decides what an `AggIntent` may become, at the cost of
//! extra work per bind proportional to each node's own candidate count.
//!
//! ## The two strategies, and why these two
//!
//! - [`SketchAlgorithmStrategy`] wraps [`implementations_for_with`]'s exhaustive,
//!   ranked list directly: for the same bindable-`Aggregate` shape this crate
//!   binds (single intent, no `HAVING`), every entry becomes its own bound
//!   candidate.
//! - [`SharedSubtreeStrategy`] wraps
//!   `asap_types::pre_asap::cse::share_common_subtrees`'s sharing decision.
//!   Wherever a [`TargetSubDAG`] already has two or more consumers (i.e.
//!   `share_common_subtrees` already collapsed two or more workload
//!   locations onto the same `Rc<QueryExpr>` — [`discover_targets`] below
//!   does the identical workload-wide discovery for [`search_workload_with`];
//!   this module's own tests reuse the same dedup logic to build realistic
//!   fixtures), it reports the two-way candidate CSE's own detection pass
//!   deliberately declines to pick between on its own: build once and share
//!   the already-interned subtree, or build it independently at each
//!   consumer. [`crate::cost_model::CostModel::cse_share_decision`] is where
//!   that choice actually gets made *today* (a fixed comparison, not a
//!   search) — this strategy exposes the same two-way choice as an explicit,
//!   inspectable pair of candidates instead of a cost model's already-decided
//!   boolean.
//!
//! ## Non-goals (tracked separately, not attempted here)
//!
//! - **[`implementations_for_with`]'s own outward-facing behavior is
//!   unchanged.** Same inputs still produce the same exhaustive, ranked
//!   list — only its home moved (from a separate `implementation` module
//!   into this one) and its own visibility dropped to module-private, since
//!   [`SketchAlgorithmStrategy`] is now its only caller.
//!
//! ## Workload-wide search — merged in from the former `search.rs` (issue #252, part of #33)
//!
//! This section used to carry two more "non-goals" bullets here — "no
//! search/selection-across-a-whole-plan logic" and "no workload-wide
//! `TargetSubDAG` discovery pass" — describing work deliberately left for a
//! future Cascades/Volcano-style search engine (PR #263,
//! `feat/cascades-search-252`, over the [`ReplacementStrategy`] extension
//! point above). That engine is [`PlanSpace`]/[`MemoGroup`]/
//! [`search_workload`]/[`search_workload_with`] below, merged into this
//! module rather than kept as a separate `search` module — the same "one
//! module, one step" reasoning the top of this file already uses for
//! decide-and-build: searching *across* a whole workload's worth of
//! [`TargetSubDAG`]s is a natural continuation of deciding and building
//! replacements *for* one, not a different concern that deserves its own
//! file. What follows (through "Cost-based final selection" below) is that
//! engine's own design documentation, preserved from `search.rs`.
//!
//! ### The pseudocode, and the two things it deliberately leaves open
//!
//! ```text
//! candidate_plans = { input_workload_plan }
//! loop:
//!     new_plans = {}
//!     for plan in candidate_plans:
//!         for site in plan.bindable_sites():
//!             for strategy in registered_strategies:
//!                 if strategy.matches(site):
//!                     for replacement in strategy.replacements(site):
//!                         new_plans += substitute(plan, site, replacement)
//!     new_plans -= candidate_plans
//!     candidate_plans += new_plans
//! until new_plans is empty
//! return candidate_plans.sorted_by(cost_model)
//! ```
//!
//! Read literally, this enumerates whole *plans* — full copies of the
//! workload's tree, one per combination of per-target choices. A workload
//! with `N` independently-choosable targets would produce up to `2^N` flat
//! plans, each one duplicating every untouched sibling subtree. This module
//! does not do that:
//!
//! 1. **MEMO groups, not flat plans.** [`MemoGroup`] is this engine's
//!    Cascades-style "group": one distinct [`TargetSubDAG`] (identified by
//!    its own `Rc<QueryExpr>` pointer identity — the same currency
//!    [`asap_types::pre_asap::cse::share_common_subtrees`] already
//!    established across the workload) holding every
//!    [`ReplacementSubDAG`] alternative discovered for it. [`PlanSpace`] is
//!    a collection of these groups, keyed by `TargetSubDAG` — a candidate
//!    "plan" is never materialized as a distinct top-level `Rc<QueryExpr>`
//!    at all; two logically-different overall choices at two different
//!    targets are just two different entries in two different groups,
//!    sharing every other node in the workload by construction (they *are*
//!    the same `Rc`s — nothing was copied to make a second "plan").
//! 2. **Dedup by structural hash + `PartialEq`, reusing `pre_asap::cse`'s own
//!    discipline.** [`asap_types::pre_asap::cse::structural_hash`] (made
//!    `pub` for exactly this reuse) is only ever a candidate-narrowing
//!    filter; [`MemoGroup::add_candidate`]'s actual duplicate check is
//!    `QueryExpr`'s derived `PartialEq` — the same "hash is a filter,
//!    `PartialEq` is the decision, no exceptions" rule `cse.rs`'s own
//!    "Correctness" section states and this module inherits rather than
//!    reinvents. See [`is_duplicate_rewrite`] for the one deliberate
//!    wrinkle this reuse needs (a `Rc`-identity case pure value equality
//!    would get wrong).
//!
//! ### Where `TargetSubDAG` discovery comes from
//!
//! [`discover_targets`] is the workload-wide `TargetSubDAG` discovery pass
//! this section used to flag as explicitly *not* implemented ("no
//! workload-wide `TargetSubDAG` discovery pass is shipped either... wiring
//! it up automatically belongs to the same future search engine, not this
//! issue") — this is that future engine, so it's this module's job now, and
//! it's what the quoted pseudocode's `for site in plan.bindable_sites()`
//! line above stands for: every `TargetSubDAG` this pass discovers is one
//! iteration of that loop. It walks every workload root's whole DAG (the
//! same **relational-skeleton** operator-child scope
//! `asap_types::pre_asap::cse::share_common_subtrees` itself uses — see
//! that module's "Algorithm" section), discovering one `TargetSubDAG` per
//! distinct `Rc` and a *real* `consumer_count`: how many operator-child
//! positions anywhere in the workload reference that exact `Rc`, not just
//! how many of the workload's own top-level roots happen to be it — a
//! `SharedSubtreeStrategy` candidate three levels under an unshared
//! `Filter` is exactly as real a target as a shared whole root, so this
//! module's discovery can't stop at the top level.
//!
//! `discover_targets` duplicates (rather than reuses) this module's own
//! `#[cfg(test)]`-only `count_consumers` traversal (in the test module
//! below), which mirrors this exact shape for this module's own test
//! fixtures — that copy is intentionally test-only, so it isn't reachable
//! from this module's production code without either moving it into
//! shared, non-test-gated code or duplicating the (small, self-contained)
//! traversal here. Duplicating was judged simpler than restructuring a test
//! helper into shared production code for one caller.
//!
//! ### Termination
//!
//! Every discovered target is asked *once* per registered strategy, never
//! re-asked — [`search_workload_with`]'s loop processes each round's
//! frontier of not-yet-visited targets exactly one time each, so there is
//! no scenario where the same `(target, strategy)` pair is queried twice
//! (the `new_plans -= candidate_plans` dedup step the module-level
//! pseudocode describes is therefore never asked to recognize "the same
//! candidate, proposed again" as a special case — see
//! [`MemoGroup::add_candidate`]'s own doc on why that distinction matters
//! for [`Replacement::Summary`] specifically, where no real equality check
//! exists to make it safely).
//!
//! What *can* grow the frontier is a candidate's own reachable structure:
//! after a target is processed, every [`Replacement::Rewrite`] candidate's
//! **children** (never the candidate's own top-level node — that value is
//! an alternative *for* the target just processed, not a new target of its
//! own; see [`discover_new_descendant_targets`]) are scanned for pointers
//! not already known, and any found become next round's frontier. Both shipped
//! strategies are idempotent in exactly this sense: [`SketchAlgorithmStrategy`]
//! produces terminal [`Replacement::Summary`] candidates (no `QueryExpr`
//! children to scan at all), and [`SharedSubtreeStrategy`]'s two
//! [`Replacement::Rewrite`] candidates both reuse the target's own
//! already-known child `Rc`s verbatim (`Rc::clone`/a shallow top-level
//! `.clone()` — see that strategy's own doc). So for both, the frontier is
//! always empty after round one: real workloads converge in exactly one
//! round, regardless of size.
//!
//! That said, a future strategy whose `Replacement::Rewrite` candidates
//! invent brand-new descendant structure every time they're computed (e.g.
//! internal state that fabricates a fresh child node on every call) could
//! in principle keep the frontier non-empty forever. Since this crate has
//! no cardinality/statistics estimation to bound anything by,
//! [`search_workload_with`] enforces a generous, documented round cap
//! ([`MAX_SEARCH_ITERATIONS`]) instead: exceeding it panics with a clear
//! message naming the actual cause, rather than hanging silently — a test
//! ([`tests::a_pathologically_growing_strategy_trips_the_iteration_cap`])
//! pins that this guard actually fires, by using exactly that shape of
//! pathological strategy.
//!
//! ### Cost-based final selection — reusing `CostModel`, not a second interface
//!
//! [`PlanSpace::cost_sorted`] is the `sorted_by(cost_model)` step, and it
//! reuses this crate's existing [`CostModel`] trait rather than inventing a
//! second cost interface (`docs/design_docs/cse-cost-model-decision.md`,
//! issue #237, explicitly reasoned about *why* a narrow, direct cost
//! comparison was enough for the CSE share/recompute decision alone, and
//! flagged that a real search engine — this module — is where that stops
//! being the whole story; it isn't a contradiction of #237, it's the scope
//! change #237 itself named). Concretely, per [`MemoGroup`]:
//!
//! - A group whose candidates are the [`SharedSubtreeStrategy`]
//!   share-vs-recompute pair is ranked by calling
//!   [`CostModel::cse_share_decision`] via this module's own
//!   [`cse_preference`] — rather than re-deriving a competing comparison.
//! - A group whose candidates are [`SketchAlgorithmStrategy`]'s sketch-family
//!   candidates is ranked via [`CostModel::rank_candidates`] (the same hook
//!   `implementations_for_with` itself consults), applied to the
//!   candidates' own [`SketchAlgorithm`]s.
//! - Any other shape (a single candidate, or a mix this module doesn't have
//!   a defined comparison for) keeps discovery order — there is nothing to
//!   rank, or no [`CostModel`] hook this module knows how to apply; it never
//!   invents a comparison `CostModel` doesn't already define.

use std::collections::HashMap;

use asap_types::post_asap::{
    ExactKind, ExactParams, SamplingKind, SamplingParams, SketchAlgorithm, SketchKind,
    SketchParams, SketchQuery as PostAsapSketchQuery, StatModelKind, StatModelParams, SummaryExpr,
    SummaryFamilyType, SummaryField, SummaryNode, SummarySchema, WaveletKind, WaveletParams,
};
use asap_types::pre_asap::agg_intent::{agg_is_mergeable, AggIntent};
use asap_types::pre_asap::cse::{share_common_subtrees, structural_hash, HashCache};
use asap_types::pre_asap::expr_ir::ColumnRef;
use asap_types::pre_asap::query_expr::{QueryExpr, QueryExprError, Reduction};
use asap_types::pre_asap::schema::Schema;
use asap_types::types::AccuracyTarget;
use std::rc::Rc;
use thiserror::Error;

use crate::cost_model::{CostModel, CseCandidate, DefaultCostModel, ShareDecision};

/// Errors from the pre-ASAP → post-ASAP replacement/construction path
/// ([`realize_child`] and [`keep_pre_asap`]). Moved here from the former
/// `bind.rs` (issue #251): this is what a [`ReplacementStrategy`]
/// implementor's own construction path can realistically fail with —
/// schema derivation over a pre-ASAP [`QueryExpr`] — not something specific
/// to workload-wide orchestration.
#[derive(Debug, Error)]
pub enum ImplementError {
    /// Schema derivation failed while lifting an edge to `SummarySchema`.
    #[error("schema derivation failed during pre-ASAP → post-ASAP binding: {0}")]
    Schema(#[from] QueryExprError),
}

/// A pre-ASAP sub-DAG a [`ReplacementStrategy`] knows how to replace.
///
/// `root` is a reference into the workload's own [`QueryExpr`] tree (an
/// `Rc<QueryExpr>`, the same currency [`search_workload`] and
/// `asap_types::pre_asap::cse::share_common_subtrees` already thread through
/// this crate's public API — not a bare `&QueryExpr` — so a strategy that
/// needs the node's own `Rc` identity, not just its shape, has it available
/// without the caller re-deriving it).
///
/// `consumer_count` is how many locations across the workload already
/// reference this exact `Rc` — 1 for an ordinary single-use node, 2+ when the
/// caller already ran `share_common_subtrees` and found this subtree shared.
/// A strategy that only cares about `root`'s own shape (e.g.
/// [`SketchAlgorithmStrategy`]) can ignore it entirely; [`SharedSubtreeStrategy`]
/// is the one strategy that consults it. Computing a *real* consumer count
/// across a whole workload is a traversal this module deliberately does not
/// own (see the module docs' "Non-goals") — [`TargetSubDAG::new`] defaults it
/// to `1`, the safe assumption for a caller inspecting one node in isolation.
#[derive(Debug, Clone, Copy)]
pub struct TargetSubDAG<'a> {
    pub root: &'a Rc<QueryExpr>,
    pub consumer_count: usize,
}

impl<'a> TargetSubDAG<'a> {
    /// A target assumed to have exactly one consumer — the common case for a
    /// caller that isn't already tracking cross-workload sharing.
    pub fn new(root: &'a Rc<QueryExpr>) -> Self {
        Self {
            root,
            consumer_count: 1,
        }
    }

    /// A target with an explicit `consumer_count` — for a caller that already
    /// knows (e.g. from `share_common_subtrees`, or from this module's own
    /// test helpers) how many workload locations reference `root`.
    pub fn with_consumer_count(root: &'a Rc<QueryExpr>, consumer_count: usize) -> Self {
        Self {
            root,
            consumer_count,
        }
    }
}

/// What a [`ReplacementSubDAG`] actually substitutes a [`TargetSubDAG`] with.
///
/// Generalizes [`implementations_for_with`]'s two possible *kinds* of answer
/// — a post-ASAP binding decision, or a still-pre-ASAP structural alternative
/// — into "one candidate among several", each with its own
/// [`ReplacementSubDAG`].
#[derive(Debug, Clone)]
pub enum Replacement {
    /// A fully bound post-ASAP summary decision, for one particular
    /// candidate realization of the target.
    Summary(Rc<SummaryNode>),
    /// A pre-ASAP rewrite: still a logical [`QueryExpr`], structurally
    /// different from the target's own `root` (e.g. sharing vs. not sharing
    /// a subtree) but semantically equivalent to it.
    Rewrite(Rc<QueryExpr>),
}

/// One candidate replacement for a [`TargetSubDAG`], plus a human-readable
/// `rationale` explaining why it's a valid candidate (meant for a
/// report/log/debugging a search engine's choices, not machine parsing —
/// [`crate::applicability::ApplicabilityFinding::reason`] literally reuses
/// this same string rather than inventing new prose of its own.
#[derive(Debug, Clone)]
pub struct ReplacementSubDAG {
    pub replacement: Replacement,
    pub rationale: String,
}

/// A replacement strategy: given a [`TargetSubDAG`], does this strategy have
/// an opinion on it at all (`matches`), and if so, every semantically valid
/// replacement (`replacements`)?
///
/// The extension point this module exists for — the same shape
/// [`CostModel`] and [`Matcher`] already use elsewhere in this crate: a new
/// replacement source is a new `impl ReplacementStrategy`, no restructuring
/// of this trait or any existing strategy required.
///
/// `replacements` is only meaningful when `matches` would return `true` for
/// the same target; both [`SketchAlgorithmStrategy`] and [`SharedSubtreeStrategy`]
/// return an empty `Vec` rather than panicking when called on a target they
/// don't match, so a caller that skips the `matches` check first still gets a
/// safe (merely uninformative) answer instead of a crash.
pub trait ReplacementStrategy {
    /// Does this strategy have any replacement to offer for `target`?
    fn matches(&self, target: &TargetSubDAG<'_>) -> bool;

    /// Every valid replacement for `target` — not ranked, not filtered.
    /// Reporting "every valid candidate" is this method's whole job; picking
    /// the best one is a [`CostModel`]'s job, out of scope here.
    fn replacements(&self, target: &TargetSubDAG<'_>) -> Vec<ReplacementSubDAG>;
}

// ── Implementation: how one AggIntent may be realised ───────────────────────

/// How an [`AggIntent`] may be realised at post-ASAP binding time (issue
/// #98): by an approximate summary (sketch, sample, wavelet, statistical
/// model, …), by an exact mergeable accumulator, or by an ordinary exact
/// operator (pass-through). This is a post-ASAP concern — the pre-ASAP IR
/// carries only the intent + accuracy target, never the realization — and
/// it's a per-node decision, made once per `AggIntent`, not a plan-wide one.
///
/// [`implementations_for_with`] is where every valid realization gets
/// enumerated, exhaustive and ranked (most-preferred first) — this crate has
/// no separate function that computes just "the one" `Implementation`
/// independently of that list. [`SketchAlgorithmStrategy`] is the sole
/// consumer: it wraps every entry of this list into its own bound
/// [`SummaryNode`] and returns all of them, ranked — a caller wanting a
/// single answer keeps the first one itself (see the module docs above).
#[derive(Debug, Clone, PartialEq)]
pub enum Implementation {
    /// An exact **mergeable** accumulator (partial state ≡ the value
    /// itself: `Sum` / `Count` / `MinMax` / `Rate` / `Increase`). The
    /// built state *is* the answer already — no `SummaryEstimate` readout
    /// step.
    ExactAggregate {
        kind: ExactKind,
        params: ExactParams,
    },
    /// An approximate sketch sized to the intent's [`AccuracyTarget`].
    /// Needs a `SummaryEstimate` readout to recover a value. Already
    /// classified into its [`SketchKind`] category (`SketchKind::new`
    /// having been called) — construction always goes through that
    /// classifier, never this variant directly.
    Sketch(SketchKind),
    /// A sampling-based summary (a retained row subset). Needs a
    /// `SummaryEstimate` readout. Not chosen by any core `AggIntent`
    /// dispatch today — see the module docs.
    Sample {
        kind: SamplingKind,
        params: SamplingParams,
    },
    /// A wavelet-transform summary. Needs a `SummaryEstimate` readout. Not
    /// chosen by any core `AggIntent` dispatch today — see the module docs.
    Wavelet {
        kind: WaveletKind,
        params: WaveletParams,
    },
    /// A fitted statistical/parametric-model summary. Needs a
    /// `SummaryEstimate` readout. Not chosen by any core `AggIntent`
    /// dispatch today — see the module docs.
    StatModel {
        kind: StatModelKind,
        params: StatModelParams,
    },
    /// No summary form — the node stays a logical pre-ASAP operator and is
    /// executed exactly (per-series transforms, non-mergeable reducers, exact
    /// quantile/top-k/cardinality, classic-bucket `HistogramQuantile`, …).
    PassThrough,
}

/// Does an already-**available** [`Implementation`] — e.g. a summary
/// instance a downstream deployment already materialized somewhere, found
/// via whatever inventory/index that deployment keeps — satisfy a
/// **required** [`Implementation`] (one of the candidates
/// [`implementations_for_with`] produced for some [`AggIntent`])?
///
/// This is the query-optimization-literature "materialized view matching"
/// / "answering queries using views" question, narrowed to this crate's
/// summary vocabulary: not "can I build this from scratch" (that's what
/// [`implementations_for_with`] answers) but "does something that already
/// exists answer this".
///
/// `asap-plan` deliberately ships no implementation of this trait and no
/// default method body — unlike [`implementations_for_with`], which decision
/// an available `Implementation` satisfies a required one is not a fact this
/// crate can settle on its own. Two real, reasonable answers already
/// diverge outside this crate:
///
/// - A **pure sketch-algebra** answer would say a `Sketch{kind: Kll, ..}`
///   requirement is satisfied by an available `DDSketch` (both quantile
///   sketches), and that a heap-bearing top-k sketch also answers a bare
///   frequency point-query (the heap is additional info on the same
///   underlying matrix) — but not the reverse.
/// - A **deployment with its own storage-layout rules** may need more:
///   e.g. whether a multi-population accumulator can serve a
///   single-population query via re-aggregation is a fact about that
///   deployment's storage layout, not about any summary family's kind at
///   all — a family's own kind doesn't encode grouping (grouping lives on
///   the post-ASAP node's `by` instead), so there is nothing in this
///   crate's own vocabulary to subsume.
///
/// Implementations are expected to consult `required`/`available`'s
/// `kind` (and whatever grouping/placement context the deployment tracks
/// alongside `Implementation`, which this trait's signature doesn't carry
/// because this crate has no inventory concept to carry it in).
pub trait Matcher {
    fn is_satisfied_by(&self, required: &Implementation, available: &Implementation) -> bool;
}

/// Confidence δ assumed when the target carries only an ε
/// (`AccuracyTarget::Epsilon`): the (ε, δ)-parameterised sketches (CMS) need
/// one. `ln(1/0.01) → depth 5`, matching the conventional CMS sizing.
pub const DEFAULT_DELTA: f64 = 0.01;

/// The sketch kinds that can serve an intent, most-preferred first.
/// This is the `AggIntent → SketchAlgorithm` map of issue #98;
/// [`implementations_for_with`] sizes and ranks every entry via `cost_model`.
/// Listed here so the candidate set has one home.
pub fn summary_candidates(intent: &AggIntent) -> &'static [SketchAlgorithm] {
    match intent {
        AggIntent::Quantile { .. } => &[SketchAlgorithm::Kll, SketchAlgorithm::DDSketch],
        AggIntent::Cardinality { .. } => &[
            SketchAlgorithm::Hll,
            SketchAlgorithm::Theta,
            SketchAlgorithm::Kmv,
        ],
        // Count-Sketch-with-heap is CMS-with-heap's balanced/zero-mean-error
        // alternative for the same heavy-hitter shape.
        AggIntent::TopK { .. } => &[
            SketchAlgorithm::CmsWithHeap,
            SketchAlgorithm::CountSketchWithHeap,
        ],
        AggIntent::Count { .. } => &[SketchAlgorithm::Cms, SketchAlgorithm::CountSketch],
        _ => &[],
    }
}

/// The [`AccuracyTarget`] threaded onto an approximate-capable intent
/// (`Quantile`/`Cardinality`/`Count`/`TopK`), or `None` for every other
/// intent (no sketch candidate applies — [`implementations_for_with`]'s own
/// match routes those elsewhere). Exposed so callers resolve the exact same
/// accuracy target [`implementations_for_with`] does, without re-deriving it
/// from scratch.
pub fn accuracy_target(intent: &AggIntent) -> Option<&AccuracyTarget> {
    match intent {
        AggIntent::Quantile { accuracy, .. }
        | AggIntent::Cardinality { accuracy, .. }
        | AggIntent::Count { accuracy }
        | AggIntent::TopK { accuracy, .. } => Some(accuracy),
        _ => None,
    }
}

/// Every valid [`Implementation`] for `intent`, exhaustive and ranked
/// (most-preferred first via `cost_model`) — the *only* place this crate
/// decides what an `AggIntent` may become. Nothing in this crate computes
/// "the one" `Implementation` independently of this list:
/// [`SketchAlgorithmStrategy`] keeps every entry as a candidate, and a caller
/// that wants a single executable answer takes the head of *that* strategy's
/// output itself.
///
/// Exhaustive over the [`AggIntent`] vocabulary — adding a variant without an
/// explicit realization is a compile error, and the coverage-matrix test pins
/// each variant's category.
///
/// Module-private: [`SketchAlgorithmStrategy::replacements`] is the only
/// caller — a caller outside this module has no use for the bare
/// `Implementation` list on its own, only for the bound
/// [`ReplacementSubDAG`]s that strategy produces from it.
fn implementations_for_with(intent: &AggIntent, cost_model: &dyn CostModel) -> Vec<Implementation> {
    match intent {
        // ── Approximate-capable intents — the AccuracyTarget decides ────────
        AggIntent::Quantile { accuracy, .. }
        | AggIntent::Cardinality { accuracy, .. }
        | AggIntent::Count { accuracy }
        | AggIntent::TopK { accuracy, .. } => match accuracy {
            AccuracyTarget::Exact => vec![exact_realization(intent)],
            _ => sketch_implementations(intent, accuracy, cost_model),
        },

        // ── Exact mergeable accumulators ─────────────────────────────────────
        AggIntent::Sum { .. } => vec![exact_accumulator(intent, ExactKind::Sum, ExactParams::Sum)],
        AggIntent::Min { .. } | AggIntent::Max { .. } => {
            vec![exact_accumulator(
                intent,
                ExactKind::MinMax,
                ExactParams::MinMax,
            )]
        }
        AggIntent::Rate => vec![exact_accumulator(
            intent,
            ExactKind::Rate,
            ExactParams::Rate,
        )],
        AggIntent::Increase => {
            vec![exact_accumulator(
                intent,
                ExactKind::Increase,
                ExactParams::Increase,
            )]
        }

        // ── Exact, non-mergeable reducers — richer partial state than a
        //    single value (see `agg_is_mergeable`), so no accumulator form.
        AggIntent::Avg { .. } | AggIntent::StdDev { .. } | AggIntent::Variance { .. } => {
            vec![Implementation::PassThrough]
        }

        // ── Classic-bucket histogram_quantile (#79): exact `le`-bucket
        //    interpolation over pre-aggregated counts — NOT re-sketchable.
        //    (The native/raw form lowers to the generic `Quantile` above.)
        AggIntent::HistogramQuantile { .. } => vec![Implementation::PassThrough],

        // ── Per-series transforms and reductions with no sketch realization:
        //    counter-derivatives (#44), math (#45), time/calendar (#46),
        //    presence (#47), native-histogram accessors (#43), and the
        //    `*OverTime` reducers (#51). All exact by construction.
        AggIntent::Changes
        | AggIntent::Delta
        | AggIntent::IDelta
        | AggIntent::Deriv
        | AggIntent::Resets
        | AggIntent::PredictLinear { .. }
        | AggIntent::DoubleExpSmoothing { .. }
        | AggIntent::HistogramCount
        | AggIntent::HistogramSum
        | AggIntent::HistogramAvg
        | AggIntent::HistogramStdDev
        | AggIntent::HistogramStdVar
        | AggIntent::HistogramFraction { .. }
        | AggIntent::Math(_)
        | AggIntent::Absent
        | AggIntent::AbsentOverTime
        | AggIntent::PresentOverTime
        | AggIntent::TimeFn(_)
        | AggIntent::LastOverTime
        | AggIntent::FirstOverTime
        | AggIntent::MadOverTime
        | AggIntent::TsOfMinOverTime
        | AggIntent::TsOfMaxOverTime
        | AggIntent::TsOfFirstOverTime
        | AggIntent::TsOfLastOverTime => vec![Implementation::PassThrough],

        // ── Group / count_values (#49): exact per `agg_is_exact`, but their
        //    output is structural (constant-1 / a synthesized label column),
        //    not a value a summary accumulator carries.
        AggIntent::Group | AggIntent::CountValues { .. } => vec![Implementation::PassThrough],

        // ── Extension (deployment-model-specific, issue #131) — core has no
        //    realization opinion for a shape it doesn't know, so it defers
        //    entirely to the `CostModel` (issue #150): `realize_extension`
        //    defaults to `PassThrough`, preserving today's behavior for
        //    every deployment that doesn't override it. Core has no way to
        //    enumerate alternatives for an opaque deployment-defined shape,
        //    so this is always exactly one candidate. This is also the only
        //    path that can currently produce `Implementation::Sample`/
        //    `Wavelet`/`StatModel` — see the module docs.
        AggIntent::Extension { ext_kind, payload } => {
            vec![cost_model.realize_extension(ext_kind, payload)]
        }
    }
}

/// Exact realization of an approximate-capable intent whose target is
/// `AccuracyTarget::Exact`. `Count` has a mergeable exact accumulator; exact
/// quantile / top-k / cardinality have no single-value summary form (they
/// need the full multiset / heap / set) and pass through.
fn exact_realization(intent: &AggIntent) -> Implementation {
    match intent {
        AggIntent::Count { .. } => exact_accumulator(intent, ExactKind::Count, ExactParams::Count),
        _ => Implementation::PassThrough,
    }
}

fn exact_accumulator(intent: &AggIntent, kind: ExactKind, params: ExactParams) -> Implementation {
    // An exact accumulator is only sound when partial states merge
    // (`agg(A ∪ B) = combine(agg(A), agg(B))`).
    debug_assert!(
        agg_is_mergeable(intent),
        "accumulator for non-mergeable {intent:?}"
    );
    Implementation::ExactAggregate { kind, params }
}

/// Resolve an [`AccuracyTarget`] into the `(eps, delta)` budget
/// [`CostModel::size_params`] needs. Shared by [`sketch_implementations`] and
/// this crate's own sizing — one place this resolution happens, so nothing
/// can drift apart on it.
///
/// `Exact` is unreachable via [`implementations_for_with`] (which routes
/// `Exact` to [`exact_realization`] instead); degrades to the tightest
/// parameters for a caller that resolves it directly anyway.
pub fn accuracy_budget(accuracy: &AccuracyTarget) -> (f64, f64) {
    match accuracy {
        AccuracyTarget::Exact => (f64::MIN_POSITIVE, DEFAULT_DELTA),
        AccuracyTarget::Epsilon(e) => (*e, DEFAULT_DELTA),
        AccuracyTarget::EpsilonDelta { epsilon, delta } => (*epsilon, *delta),
    }
}

/// Every candidate sketch [`Implementation`] for an approximate-capable
/// intent, sized to `accuracy` and ranked via `cost_model.rank_candidates`
/// (most-preferred first) — [`implementations_for_with`]'s Sketch branch.
fn sketch_implementations(
    intent: &AggIntent,
    accuracy: &AccuracyTarget,
    cost_model: &dyn CostModel,
) -> Vec<Implementation> {
    let (eps, delta) = accuracy_budget(accuracy);
    let ranked = cost_model.rank_candidates(intent, summary_candidates(intent));
    ranked
        .into_iter()
        .map(|algorithm| {
            let params = cost_model.size_params(algorithm.clone(), intent, eps, delta);
            Implementation::Sketch(SketchKind::new(algorithm, params))
        })
        .collect()
}

/// `asap-plan`'s built-in `SketchParams` sizing, keyed off the resolved
/// `(eps, delta)` accuracy budget. [`CostModel::size_params`]'s default
/// body — factored out to a free function so a deployment's own
/// `CostModel` impl can still delegate to it for the candidates it
/// doesn't want to resize itself.
///
/// Each formula inverts the sketch family's standard error bound to the
/// smallest parameter satisfying the target, clamped to the family's sane
/// range. A non-positive ε saturates to the clamp maximum (tightest
/// allowed).
pub fn default_size_params(
    kind: SketchAlgorithm,
    intent: &AggIntent,
    eps: f64,
    delta: f64,
) -> SketchParams {
    match kind {
        SketchAlgorithm::Kll => SketchParams::Kll { k: kll_k(eps) },
        SketchAlgorithm::Cms => SketchParams::Cms {
            width: cms_width(eps),
            depth: cms_depth(delta),
        },
        SketchAlgorithm::Hll => SketchParams::Hll {
            precision: hll_precision(eps),
        },
        SketchAlgorithm::CmsWithHeap => {
            let k = match intent {
                AggIntent::TopK { k, .. } => *k,
                _ => unreachable!("CmsWithHeap is only a TopK candidate"),
            };
            SketchParams::CmsWithHeap {
                width: cms_width(eps),
                depth: cms_depth(delta),
                heap_size: k as u32,
            }
        }
        // Non-preferred candidates (DDSketch / Theta / Kmv / CountSketch /
        // CountSketchWithHeap) are only reachable once a cost model picks
        // them; sized here so that wiring is local.
        SketchAlgorithm::DDSketch => SketchParams::DDSketch { alpha: eps },
        SketchAlgorithm::Theta => SketchParams::Theta { k: kmv_k(eps) },
        SketchAlgorithm::Kmv => SketchParams::Kmv { k: kmv_k(eps) },
        // Count-Sketch is CMS's balanced/zero-mean-error alternative —
        // same (width, depth) shape, sized the same way for now (a
        // Count-Sketch-specific bound uses an L2-norm error guarantee
        // rather than CMS's L1-norm one; this is a placeholder pending
        // that refinement, same status as the other non-preferred
        // candidates above).
        SketchAlgorithm::CountSketch => SketchParams::CountSketch {
            width: cms_width(eps),
            depth: cms_depth(delta),
        },
        SketchAlgorithm::CountSketchWithHeap => {
            let k = match intent {
                AggIntent::TopK { k, .. } => *k,
                _ => unreachable!("CountSketchWithHeap is only a TopK candidate"),
            };
            SketchParams::CountSketchWithHeap {
                width: cms_width(eps),
                depth: cms_depth(delta),
                heap_size: k as u32,
            }
        }
    }
}

/// A deployment's explicit bet about how "typical" (non-adversarial) its
/// workload's collision pattern is expected to be, consumed only by
/// [`posterior_aware_size_params`].
///
/// This is **not** derived from Chen et al.'s posterior-error-estimation
/// technique (issue #239, `asap_types::post_asap::query_time::error_estimation`)
/// — that technique computes a tighter bound *at query time* from a
/// sketch's real counter values, and this repo has no sketch runtime yet
/// for a real counter array to size against (see that module's docs, and
/// `asap_types::post_asap::query_time`'s module doc for why it's a
/// deliberately separate folder from this crate's own *plan-time* code).
/// This struct is this crate's own *plan-time* analogue of the same
/// underlying intuition — an expected-case (skewed / non-adversarial)
/// workload needs a smaller sketch than the adversarial worst case —
/// expressed as an explicit, caller-supplied assumption rather than
/// anything observed or proven. Issue #250 tracks actually connecting the
/// two: feeding query-time-observed posterior error back into a future
/// replan's `width_relaxation` instead of a bare caller guess.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ExpectedCaseSizing {
    /// Fraction, in `(0, 1]`, of the traditional worst-case width
    /// ([`cms_width`]) the caller is betting is enough. `1.0` (or any
    /// value outside `(0, 1)`) reproduces the worst-case width exactly —
    /// no risk taken. A smaller value shrinks the sketch proportionally,
    /// at the cost documented on [`posterior_aware_size_params`].
    pub width_relaxation: f64,
}

/// Opt-in alternative to [`default_size_params`] for the CMS-family kinds
/// (`Cms` / `CmsWithHeap` / `CountSketch` / `CountSketchWithHeap`): sizes
/// width to `assumption.width_relaxation` of the worst-case [`cms_width`],
/// trading the unconditional worst-case `(ε,δ)` guarantee for a smaller
/// sketch under an explicit, caller-stated non-adversarial-workload bet —
/// see [`ExpectedCaseSizing`].
///
/// **The tradeoff, spelled out:** [`default_size_params`]'s width guarantees
/// `Pr[error > ε·|F|₁] < δ` for *any* input, including an adversarial one
/// built to maximize collisions (§3.3 of the posterior-error-estimation
/// paper this issue is about — see
/// `asap_types::post_asap::query_time::error_estimation`'s module docs).
/// Shrinking
/// width below that only keeps the same `(ε,δ)` guarantee if the real
/// workload's collision load stays within `width_relaxation` of the
/// worst-case assumption — this function does not check that, cannot check
/// it (no data exists at plan time), and does not change the formal
/// guarantee's statement; it only changes how much hardware is spent
/// chasing it. Callers accept that gap explicitly by choosing
/// `width_relaxation < 1.0`.
///
/// Depth ([`cms_depth`]) is left unchanged from [`default_size_params`]:
/// depth trades away confidence *exponentially* (`Pr[all r rows bad] =
/// p^r` — each extra row multiplies the failure probability down), a
/// differently-shaped and materially riskier tradeoff than width's linear
/// relaxation. Issue #239 asks for *a* tighter-sizing option under a
/// stated assumption, not a full redesign of the depth/width tradeoff
/// space, so depth relaxation is left as explicit future scope.
///
/// For every `SketchAlgorithm` outside the CMS family, this is identical to
/// [`default_size_params`] — `width_relaxation` only ever touches the
/// [`cms_width`]-sized formulas this issue is about.
///
/// [`default_size_params`]'s own behavior is completely unchanged by this
/// function's existence — this is a separate, additive entry point, never
/// called from [`default_size_params`] or [`implementations_for_with`].
pub fn posterior_aware_size_params(
    kind: SketchAlgorithm,
    intent: &AggIntent,
    eps: f64,
    delta: f64,
    assumption: ExpectedCaseSizing,
) -> SketchParams {
    let relaxed_width = |eps: f64| -> u32 {
        let base = cms_width(eps);
        let f = assumption.width_relaxation;
        if !(f.is_finite() && f > 0.0 && f < 1.0) {
            return base; // out-of-range bet: no relaxation, fall back to worst case
        }
        saturating_ceil(base as f64 * f, 2, base)
    };
    match kind {
        SketchAlgorithm::Cms => SketchParams::Cms {
            width: relaxed_width(eps),
            depth: cms_depth(delta),
        },
        SketchAlgorithm::CmsWithHeap => {
            let k = match intent {
                AggIntent::TopK { k, .. } => *k,
                _ => unreachable!("CmsWithHeap is only a TopK candidate"),
            };
            SketchParams::CmsWithHeap {
                width: relaxed_width(eps),
                depth: cms_depth(delta),
                heap_size: k as u32,
            }
        }
        SketchAlgorithm::CountSketch => SketchParams::CountSketch {
            width: relaxed_width(eps),
            depth: cms_depth(delta),
        },
        SketchAlgorithm::CountSketchWithHeap => {
            let k = match intent {
                AggIntent::TopK { k, .. } => *k,
                _ => unreachable!("CountSketchWithHeap is only a TopK candidate"),
            };
            SketchParams::CountSketchWithHeap {
                width: relaxed_width(eps),
                depth: cms_depth(delta),
                heap_size: k as u32,
            }
        }
        // Every other kind is untouched by this issue's CMS-specific
        // relaxation — defer to the existing formula verbatim. Spelled out
        // exhaustively, matching `default_size_params`'s own match, rather
        // than a wildcard arm: a future `SketchAlgorithm` variant then fails to
        // compile *here* too, instead of silently inheriting worst-case
        // sizing with no signal that this function never considered it.
        SketchAlgorithm::Kll => default_size_params(kind, intent, eps, delta),
        SketchAlgorithm::Hll => default_size_params(kind, intent, eps, delta),
        SketchAlgorithm::DDSketch => default_size_params(kind, intent, eps, delta),
        SketchAlgorithm::Theta => default_size_params(kind, intent, eps, delta),
        SketchAlgorithm::Kmv => default_size_params(kind, intent, eps, delta),
    }
}

// ── Parameter sizing ──────────────────────────────────────────────────────────
//
// Each function inverts the sketch family's standard error bound to the
// smallest parameter satisfying the target, clamped to the family's sane
// range. A non-positive ε saturates to the clamp maximum (tightest allowed).

/// KLL: rank error ε ≈ 2/k ⇒ `k = ⌈2/ε⌉`. ε = 0.01 → k = 200, matching the
/// design doc's worked example (`KLL{k=200}` satisfies ε=0.01).
fn kll_k(eps: f64) -> u32 {
    saturating_ceil(2.0 / eps, 8, 65_535)
}

/// HLL: standard error ≈ 1.04/√(2^p) ⇒ `p = ⌈log2((1.04/ε)²)⌉`. The default
/// `Cardinality` target (`asap-ir::default_cardinality`) inverts to p = 14.
fn hll_precision(eps: f64) -> u8 {
    saturating_ceil((1.04 / eps).powi(2).log2(), 4, 18) as u8
}

/// CMS: over-count ≤ ε·N with width `w = ⌈e/ε⌉` columns.
fn cms_width(eps: f64) -> u32 {
    saturating_ceil(std::f64::consts::E / eps, 2, 1 << 26)
}

/// CMS: failure probability ≤ δ with depth `d = ⌈ln(1/δ)⌉` rows.
/// δ = 0.01 → depth 5.
fn cms_depth(delta: f64) -> u32 {
    saturating_ceil((1.0 / delta).ln(), 1, 32)
}

/// KMV / theta: relative error ≈ 1/√k ⇒ `k = ⌈1/ε²⌉`.
fn kmv_k(eps: f64) -> u32 {
    saturating_ceil(1.0 / (eps * eps), 16, 1 << 26)
}

/// `⌈x⌉` clamped to `[lo, hi]`; NaN / non-positive x saturate to `hi`
/// (a degenerate ε means "as accurate as this family goes").
fn saturating_ceil(x: f64, lo: u32, hi: u32) -> u32 {
    if !x.is_finite() || x <= 0.0 {
        return hi;
    }
    (x.ceil() as u32).clamp(lo, hi)
}

// ── SketchAlgorithmStrategy ─────────────────────────────────────────────────

/// A single static instance so [`SketchAlgorithmStrategy::default_cost_model`]
/// can hand out a `&'static dyn CostModel` without heap-allocating one —
/// `DefaultCostModel` is a unit struct with no state, so one instance serves
/// every caller.
static DEFAULT_COST_MODEL: DefaultCostModel = DefaultCostModel;

/// Wraps [`implementations_for_with`]'s exhaustive, ranked list directly: for
/// a bindable `Aggregate`, every valid candidate summary realization as its
/// own [`ReplacementSubDAG`].
///
/// Ranked (only to *order the enumeration*, never to drop a candidate) via a
/// [`CostModel`] — [`DefaultCostModel`] unless constructed with
/// [`SketchAlgorithmStrategy::new`] — so a deployment-specific cost model's
/// other hooks (`size_params`, `realize_extension`, `readout_extension`) are
/// still consulted while binding each candidate.
pub struct SketchAlgorithmStrategy<'a> {
    cost_model: &'a dyn CostModel,
}

impl SketchAlgorithmStrategy<'static> {
    /// A strategy that ranks/binds via the built-in [`DefaultCostModel`] —
    /// what a deployment gets with no custom cost model plugged in.
    pub fn default_cost_model() -> Self {
        Self {
            cost_model: &DEFAULT_COST_MODEL,
        }
    }
}

impl<'a> SketchAlgorithmStrategy<'a> {
    /// A strategy that ranks/binds via `cost_model` instead of the built-in
    /// static preference order — the same customization point
    /// [`implementations_for_with`] already offers.
    pub fn new(cost_model: &'a dyn CostModel) -> Self {
        Self { cost_model }
    }
}

impl ReplacementStrategy for SketchAlgorithmStrategy<'_> {
    fn matches(&self, target: &TargetSubDAG<'_>) -> bool {
        bindable_intent(target.root).is_some()
    }

    fn replacements(&self, target: &TargetSubDAG<'_>) -> Vec<ReplacementSubDAG> {
        let Some(intent) = bindable_intent(target.root) else {
            return Vec::new();
        };
        // `implementations_for_with` is already exhaustive and ranked — no
        // separate dispatch needed here. Only `Sketch` has more than one
        // candidate in practice (every other variant's own dispatch produces
        // exactly one `Implementation`), but this loop doesn't need to know
        // that; it just constructs whatever the list contains.
        implementations_for_with(intent, self.cost_model)
            .into_iter()
            .filter_map(|implementation| {
                let rationale = describe_implementation(intent, &implementation);
                let node = construct_summary(target.root, implementation, self.cost_model).ok()?;
                Some(ReplacementSubDAG {
                    replacement: Replacement::Summary(node),
                    rationale,
                })
            })
            .collect()
    }
}

/// A human-readable rationale for one candidate `Implementation`, for
/// [`ReplacementSubDAG::rationale`] text.
fn describe_implementation(intent: &AggIntent, implementation: &Implementation) -> String {
    match implementation {
        Implementation::Sketch(kind) => format!(
            "{} realizes as a {:?} sketch — one of summary_candidates' \
             alternatives for this intent (asap_aware_mapping::replacement::implementations_for_with)",
            describe_intent(intent),
            kind.algorithm()
        ),
        Implementation::ExactAggregate { kind, .. } => format!(
            "{} realizes as an exact {kind:?} accumulator — the only realization \
             implementations_for_with produces for this intent (no approximate \
             candidate applies)",
            describe_intent(intent)
        ),
        Implementation::PassThrough => format!(
            "{} has no summary realization and stays a logical pass-through — the \
             only realization implementations_for_with produces for this intent",
            describe_intent(intent)
        ),
        Implementation::Sample { kind, .. } => format!(
            "{} realizes as a {kind:?} sample — the only realization the plugged-in \
             CostModel produced for this intent",
            describe_intent(intent)
        ),
        Implementation::Wavelet { kind, .. } => format!(
            "{} realizes as a {kind:?} wavelet transform — the only realization the \
             plugged-in CostModel produced for this intent",
            describe_intent(intent)
        ),
        Implementation::StatModel { kind, .. } => format!(
            "{} realizes as a {kind:?} statistical model — the only realization the \
             plugged-in CostModel produced for this intent",
            describe_intent(intent)
        ),
    }
}

/// A short human-readable label for an `AggIntent`, for
/// [`ReplacementSubDAG::rationale`] text. Not exhaustive by design (unlike
/// this crate's other `AggIntent` matches, e.g. [`implementations_for_with`]'s)
/// — this is prose for a rationale string, not a decision, so an unlisted
/// variant just falls back to its `Debug` tag rather than forcing every
/// future intent to be named here too. [`crate::applicability`] needs no
/// counterpart of its own: it reads a candidate's `rationale` — built from
/// this text — straight off [`ReplacementSubDAG`], rather than re-describing
/// the same intent a second time.
fn describe_intent(intent: &AggIntent) -> String {
    match intent {
        AggIntent::Quantile { q, .. } => format!("quantile(q={q})"),
        AggIntent::Cardinality { .. } => "cardinality (distinct count)".to_string(),
        AggIntent::TopK { k, .. } => format!("top-{k} heavy-hitters"),
        AggIntent::Count { .. } => "count".to_string(),
        other => format!("{other:?}"),
    }
}

// ── realize_child / keep_pre_asap: rank-and-take-first, and its fallback ──

/// Rank-and-take-first selector for a single [`QueryExpr`] node: enumerate
/// every candidate via [`SketchAlgorithmStrategy::replacements`], keep the
/// `cost_model`-preferred (first) one, and fall back to [`keep_pre_asap`]
/// when there's no candidate at all — **not** a general single-answer API
/// for a whole workload (that "commit to one final answer" step is a
/// downstream deployment's job, out of this crate's scope — see the crate
/// doc's `## Status` section). `root` must already be the caller's own
/// `Rc`, never fabricated per call, so this never allocates beyond what the
/// caller already held.
///
/// `pub(crate)`: reachable from this module's own construction helper
/// ([`construct_summary_agg`], so a nested aggregate gets its own
/// independent enumeration instead of inheriting the parent's forced
/// candidate), from this module's own [`realize_one`] (the representative
/// bound `SummaryNode` [`cse_preference`] needs for a
/// [`CostModel::cse_share_decision`] comparison), and from
/// [`crate::cost_model::DefaultCostModel::estimate_cost`] (the same
/// representative-node need, for a [`Replacement::Rewrite`] candidate's own
/// cost estimate). Every other caller goes through
/// [`SketchAlgorithmStrategy::replacements`] directly and decides for itself.
pub(crate) fn realize_child(
    root: &Rc<QueryExpr>,
    cost_model: &dyn CostModel,
) -> Result<Rc<SummaryNode>, ImplementError> {
    let target = TargetSubDAG::new(root);
    match SketchAlgorithmStrategy::new(cost_model)
        .replacements(&target)
        .into_iter()
        .next()
    {
        Some(ReplacementSubDAG {
            replacement: Replacement::Summary(node),
            ..
        }) => Ok(node),
        Some(ReplacementSubDAG {
            replacement: Replacement::Rewrite(_),
            ..
        }) => {
            unreachable!("SketchAlgorithmStrategy never returns a Rewrite candidate")
        }
        // No candidate at all: `root` isn't `bindable_intent` shape (or its
        // intent has no realization `implementations_for_with` can't
        // produce — never happens, that match is exhaustive) — the same
        // conservative fallback `SketchAlgorithmStrategy::matches` uses.
        None => keep_pre_asap(root),
    }
}

/// Wrap an unrewritten pre-ASAP subtree, lifting its schema with every column
/// `SummaryFamilyType::Plain`. `pub` so a caller can fall back to this
/// explicitly — e.g. when `SketchAlgorithmStrategy::replacements()` returns no
/// candidate for a target, or a deployment wants to force a node its own
/// runtime can't actually implement — through the same fallback this
/// crate's own dispatch uses, without duplicating the schema-lift logic.
pub fn keep_pre_asap(expr: &QueryExpr) -> Result<Rc<SummaryNode>, ImplementError> {
    let schema = expr.output_schema()?;
    Ok(Rc::new(SummaryNode {
        expr: SummaryExpr::KeepPreAsap(Box::new(expr.clone())),
        schema: lift(&schema),
    }))
}

// ── Construction: turn one already-decided Implementation into a SummaryNode ─

/// The bindable shape [`SketchAlgorithmStrategy`] targets: a single intent, no
/// `HAVING`. A multi-intent node (SQL `SELECT SUM(a), AVG(b)`), or one with a
/// `HAVING` predicate (the filter would need the estimate first), stays
/// logical — conservative fallbacks: [`SummaryExpr::KeepPreAsap`] boxes a
/// whole pre-ASAP subtree with no post-ASAP children, so a *logical*
/// operator above a bindable aggregate (`Filter`/`BinaryOp`/… over a
/// quantile) subsumes the aggregate into the logical wrapper unbound too —
/// rewriting through logical parents is the post-ASAP rule engine's job
/// (#6/#33), not this pass's.
pub fn bindable_intent(node: &QueryExpr) -> Option<&AggIntent> {
    if let QueryExpr::Aggregate {
        measures, having, ..
    } = node
    {
        if let ([intent], None) = (measures.as_slice(), having) {
            return Some(intent);
        }
    }
    None
}

/// Construct `expr`'s [`ReplacementSubDAG`] payload for one already-decided
/// [`Implementation`] of its top intent — the mechanical half of
/// [`SketchAlgorithmStrategy::replacements`], called once per candidate that
/// method enumerates.
///
/// `expr` must still be the [`bindable_intent`] shape for `implementation` to
/// have any effect; anything else falls back to [`keep_pre_asap`].
/// Only `expr`'s own top-level decision is forced — recursion into `expr`'s
/// child goes back through [`realize_child`] (fresh candidate
/// enumeration, not a forced pick), so choosing one candidate for a target
/// never leaks into that target's own nested aggregates.
fn construct_summary(
    expr: &QueryExpr,
    implementation: Implementation,
    cost_model: &dyn CostModel,
) -> Result<Rc<SummaryNode>, ImplementError> {
    if let QueryExpr::Aggregate {
        reduction,
        measures,
        having,
        child,
        ..
    } = expr
    {
        // The bindable shape: exactly one intent, no HAVING. (Multi-intent
        // nodes and HAVING stay logical — see `bindable_intent`.)
        if let ([intent], None) = (measures.as_slice(), having) {
            if let Some((family, estimate)) = summary_family(implementation) {
                return construct_summary_agg(
                    expr, reduction, intent, child, family, estimate, cost_model,
                );
            }
        }
    }
    keep_pre_asap(expr)
}

/// Translate an [`Implementation`] into the `(family, needs a
/// SummaryEstimate readout)` pair [`construct_summary_agg`] needs, or `None`
/// for `PassThrough` (the caller falls back to [`keep_pre_asap`]).
///
/// Every family's partial state needs a readout to recover a value, except
/// `ExactAggregate` — its partial state *is* the value already, so no
/// estimate step follows it.
fn summary_family(implementation: Implementation) -> Option<(SummaryFamilyType, bool)> {
    Some(match implementation {
        Implementation::ExactAggregate { kind, params } => {
            (SummaryFamilyType::ExactAggregate(kind, params), false)
        }
        Implementation::Sketch(kind) => (SummaryFamilyType::Sketch(kind), true),
        Implementation::Sample { kind, params } => (SummaryFamilyType::Sample(kind, params), true),
        Implementation::Wavelet { kind, params } => {
            (SummaryFamilyType::Wavelet(kind, params), true)
        }
        Implementation::StatModel { kind, params } => {
            (SummaryFamilyType::StatModel(kind, params), true)
        }
        Implementation::PassThrough => return None,
    })
}

/// Emit `SummaryAgg` (recursively binding the child), plus the
/// `SummaryEstimate` readout when `estimate` is set.
#[allow(clippy::too_many_arguments)]
fn construct_summary_agg(
    node: &QueryExpr,
    reduction: &Reduction,
    intent: &AggIntent,
    child: &Rc<QueryExpr>,
    family: SummaryFamilyType,
    estimate: bool,
    cost_model: &dyn CostModel,
) -> Result<Rc<SummaryNode>, ImplementError> {
    let child_schema = child.output_schema()?;
    // The single canonical pre-ASAP derivation (per-series vs cross-series,
    // name overrides) already computes the row shape; binding only retypes
    // the summary state column.
    let per_series = matches!(reduction, Reduction::PerEntity);
    let by: Vec<usize> = reduction
        .group_keys()
        .map(|g| g.to_vec())
        .unwrap_or_default();
    let out_schema = node.output_schema()?;
    let state_idx = summary_col_index(&out_schema, &by, per_series);

    let col = summarised_column(intent, &child_schema);
    let query = estimate.then(|| readout(intent, &col, cost_model));

    let mut state_schema = lift(&out_schema);
    if let Some(field) = state_schema.fields.get_mut(state_idx) {
        field.dtype = family.clone();
    }

    // `reduction` is carried onto `SummaryAgg` verbatim — not flattened to a
    // bare `Vec<ColumnId>` — so `SummaryExecutor::find_candidates` can tell
    // a genuine empty-`by` reduction apart from a per-entity shape with no
    // grouping concept at all (issue #163). `construct_summary_agg` is the
    // single place that decides this; nothing downstream re-derives it.
    let agg = Rc::new(SummaryNode {
        expr: SummaryExpr::SummaryAgg {
            child: realize_child(child, cost_model)?,
            family,
            col,
            reduction: reduction.clone(),
        },
        schema: state_schema,
    });
    match query {
        // The readout: downstream of the estimate the schema is the plain
        // pre-ASAP row shape again (the summary-state type does not
        // propagate).
        Some(query) => Ok(Rc::new(SummaryNode {
            expr: SummaryExpr::SummaryEstimate {
                summary_input: agg,
                query,
            },
            schema: lift(&out_schema),
        })),
        None => Ok(agg),
    }
}

/// Index of the summary-state column in the aggregate's output schema:
/// cross-series output is `by ++ [agg]` (the column after the keys);
/// a per-series reduction keeps every label and replaces the sample value
/// (named `value` — mirror `per_series_reduction_schema`'s fallback).
/// `per_series` is the caller's already-read `Reduction` (issue #165) —
/// this never re-derives it, so it can't disagree with the caller.
fn summary_col_index(out_schema: &Schema, by: &[usize], per_series: bool) -> usize {
    if per_series {
        out_schema
            .column_id("value")
            .or_else(|| (0..out_schema.columns.len()).find(|&i| Some(i) != out_schema.time_index))
            .unwrap_or(0)
    } else {
        by.len()
    }
}

/// The column fed into the summary: the intent's positional input column
/// resolved to a name against the child schema, or the PromQL sample value.
fn summarised_column(intent: &AggIntent, child_schema: &Schema) -> ColumnRef {
    match intent
        .input_col()
        .and_then(|id| child_schema.columns.get(id))
    {
        Some(c) => match &c.table {
            Some(t) => ColumnRef::Qualified {
                table: t.clone(),
                name: c.name.clone(),
            },
            None => ColumnRef::Named(c.name.clone()),
        },
        None => ColumnRef::SampleValue,
    }
}

/// The `SummaryEstimate` readout for a summary-bound intent.
fn readout(intent: &AggIntent, col: &ColumnRef, cost_model: &dyn CostModel) -> PostAsapSketchQuery {
    match intent {
        AggIntent::Quantile { q, .. } => PostAsapSketchQuery::Quantile { q: *q },
        AggIntent::Cardinality { .. } => PostAsapSketchQuery::Cardinality,
        AggIntent::TopK { k, .. } => PostAsapSketchQuery::TopK { k: *k },
        AggIntent::Count { .. } => PostAsapSketchQuery::PointCount {
            key: col.clone(),
            value: None,
        },
        // Core doesn't know the shape of a deployment-specific `Extension`
        // intent, so it can't build its readout either — delegate to the
        // same `CostModel` that decided (via `realize_extension`) this
        // intent gets a summary realization at all. See `readout_extension`'s
        // doc for the invariant this depends on.
        AggIntent::Extension { ext_kind, payload } => {
            cost_model.readout_extension(ext_kind, payload, col)
        }
        other => {
            unreachable!("no summary realization for {other:?} (implementations_for_with)")
        }
    }
}

/// Lift a pre-ASAP [`Schema`] to a [`SummarySchema`] with every column
/// `SummaryFamilyType::Plain` — shared by [`construct_summary_agg`] and
/// [`keep_pre_asap`], both in this module.
fn lift(schema: &Schema) -> SummarySchema {
    SummarySchema {
        fields: schema
            .columns
            .iter()
            .map(|c| SummaryField {
                name: c.name.clone(),
                dtype: SummaryFamilyType::Plain(c.dtype.clone()),
                nullable: c.nullable,
            })
            .collect(),
        time_index: schema.time_index,
    }
}

// ── SharedSubtreeStrategy ────────────────────────────────────────────────

/// Wraps `asap_types::pre_asap::cse::share_common_subtrees`'s sharing
/// decision as an explicit candidate pair, wherever a [`TargetSubDAG`]
/// already has two or more consumers.
///
/// This strategy does not decide sharing itself, nor does it discover which
/// nodes are shared — by the time a caller builds a `TargetSubDAG` with
/// `consumer_count >= 2`, `share_common_subtrees` has already made that
/// (legality-gated, `PartialEq`-checked) call; [`discover_targets`] below
/// discovers real consumer counts across a workload the same way for
/// [`search_workload_with`] (this module's own tests reuse the identical
/// dedup logic to build realistic fixtures — see the module docs'
/// "Non-goals" on why that traversal isn't itself part of this strategy).
/// This strategy only reframes "two or more consumers already share this
/// `Rc`" as the two-way choice a downstream cost model (today,
/// [`CostModel::cse_share_decision`]) picks between: build once and share, or
/// build independently at each consumer.
pub struct SharedSubtreeStrategy;

impl ReplacementStrategy for SharedSubtreeStrategy {
    fn matches(&self, target: &TargetSubDAG<'_>) -> bool {
        target.consumer_count >= 2
    }

    fn replacements(&self, target: &TargetSubDAG<'_>) -> Vec<ReplacementSubDAG> {
        if target.consumer_count < 2 {
            return Vec::new();
        }
        let count = target.consumer_count;
        vec![
            ReplacementSubDAG {
                // The already-interned `Rc` itself: reusing it verbatim *is*
                // "build once and share" — no new node to construct.
                replacement: Replacement::Rewrite(Rc::clone(target.root)),
                rationale: format!(
                    "build once and share: share_common_subtrees already interned this \
                     subtree once and reused it across {count} consumers — one build can \
                     answer all of them instead of computing it {count} times"
                ),
            },
            ReplacementSubDAG {
                // A structurally-identical but freshly-allocated `Rc`: same
                // value (`PartialEq`), deliberately *not* the same pointer,
                // representing "undo the sharing and recompute independently".
                replacement: Replacement::Rewrite(Rc::new((**target.root).clone())),
                rationale: format!(
                    "build independently: undo the sharing share_common_subtrees found and \
                     recompute this subtree separately at each of its {count} consumers — \
                     worth it only when independence outweighs the shared-maintenance cost, \
                     a CostModel's call (e.g. CostModel::cse_share_decision) and not this \
                     strategy's"
                ),
            },
        ]
    }
}

// ── Workload-wide search: MemoGroup / PlanSpace / search_workload ──────────
//
// Merged in from the former `search.rs` (issue #252, part of #33) — see this
// file's own top-level "Workload-wide search" doc section for the full
// design rationale.

/// A generous, documented backstop against a hypothetically ill-behaved
/// future [`ReplacementStrategy`] (see the module docs' "Termination"
/// section) — not a bound either shipped strategy could ever approach.
/// [`SketchAlgorithmStrategy`] and [`SharedSubtreeStrategy`] both converge in
/// exactly 2 passes over a fixed target set, regardless of workload size.
pub const MAX_SEARCH_ITERATIONS: usize = 1_000;

// ── MemoGroup ────────────────────────────────────────────────────────────

/// One Cascades-style MEMO group: a single distinct [`TargetSubDAG`] (its
/// own `target` `Rc<QueryExpr>`, keyed by pointer identity in
/// [`PlanSpace`]'s internal map — never re-derived by value) plus every
/// [`ReplacementSubDAG`] alternative any registered [`ReplacementStrategy`]
/// proposed for it.
///
/// `candidates` is deliberately *not* required to be non-empty — a
/// `TargetSubDAG` no registered strategy has an opinion on still gets a
/// group (with an empty candidate list), so [`PlanSpace`] always has
/// exactly one group per discovered `TargetSubDAG`, not "one group per
/// `TargetSubDAG` something matched".
#[derive(Debug, Clone)]
pub struct MemoGroup {
    /// The target sub-DAG this group is for.
    pub target: Rc<QueryExpr>,
    /// How many operator-child positions across the whole workload
    /// reference this exact `Rc` — see [`discover_targets`].
    pub consumer_count: usize,
    /// Every distinct alternative discovered for `target`, in discovery
    /// order (not ranked — see [`PlanSpace::cost_sorted`] for the ranked
    /// view).
    pub candidates: Vec<ReplacementSubDAG>,
}

impl MemoGroup {
    fn new(target: Rc<QueryExpr>, consumer_count: usize) -> Self {
        Self {
            target,
            consumer_count,
            candidates: Vec::new(),
        }
    }

    /// Add `candidate` unless it's already present (see
    /// [`is_duplicate_rewrite`]/[`is_duplicate_summary`] for what "already
    /// present" means for each [`Replacement`] variant). Returns whether it
    /// was actually added — [`search_workload_with`]'s fixpoint loop uses
    /// this to detect when a pass made no progress.
    fn add_candidate(&mut self, candidate: ReplacementSubDAG) -> bool {
        let is_duplicate = self.candidates.iter().any(|existing| {
            match (&existing.replacement, &candidate.replacement) {
                (Replacement::Rewrite(existing_rc), Replacement::Rewrite(rc)) => {
                    is_duplicate_rewrite(existing_rc, rc, &self.target)
                }
                (Replacement::Summary(existing_node), Replacement::Summary(node)) => {
                    is_duplicate_summary(existing_node, node)
                }
                // A `Rewrite` and a `Summary` are never the same candidate —
                // they're different `Replacement` variants entirely.
                (Replacement::Rewrite(_), Replacement::Summary(_))
                | (Replacement::Summary(_), Replacement::Rewrite(_)) => false,
            }
        });
        if is_duplicate {
            false
        } else {
            self.candidates.push(candidate);
            true
        }
    }
}

/// Are `existing` and `candidate` the same [`Replacement::Rewrite`]
/// candidate for a group targeting `target`?
///
/// Structural (`QueryExpr`) value equality alone is *not* enough here: this
/// module's one shipped multi-candidate `Replacement::Rewrite` source,
/// [`SharedSubtreeStrategy`], deliberately returns **two** candidates that
/// are value-equal to each other (`build once and share` vs. `build
/// independently` — see that strategy's own doc) but represent genuinely
/// different physical choices, distinguished *only* by whether the
/// candidate's `Rc` is the group's own `target` `Rc` (share) or a freshly
/// allocated one (recompute independently) — this IR has no field that
/// records "materialized once and shared", so `Rc` identity against
/// `target` is the only signal that distinction exists in at all. Treating
/// those two as duplicates of each other via pure value equality would
/// silently collapse a real choice into one candidate — the "false-positive
/// dedup is a wrong answer, not a missed optimization" failure mode
/// `cse.rs`'s own "Correctness" section warns about, just one level up from
/// where that module states it.
///
/// So: two candidates whose "is this the target's own `Rc`?" bit disagrees
/// are never duplicates of each other, full stop. Only when that bit
/// *agrees* does this fall through to the real dedup discipline —
/// [`structural_hash`] as a candidate-narrowing filter, `QueryExpr`'s
/// derived `PartialEq` as the actual decision — protecting against the
/// (currently hypothetical, since neither shipped strategy causes it)
/// case of the exact same alternative being proposed twice. A fresh
/// [`HashCache`] per call: this is a pairwise check between two candidates
/// for one group, not a bottom-up pass over a whole tree, so there is no
/// wider traversal to amortize the cache across the way `InternTable`'s own
/// use of `structural_hash` does.
fn is_duplicate_rewrite(
    existing: &Rc<QueryExpr>,
    candidate: &Rc<QueryExpr>,
    target: &Rc<QueryExpr>,
) -> bool {
    let existing_is_target = Rc::ptr_eq(existing, target);
    let candidate_is_target = Rc::ptr_eq(candidate, target);
    if existing_is_target != candidate_is_target {
        return false;
    }
    let mut cache = HashCache::new();
    structural_hash(existing, &mut cache) == structural_hash(candidate, &mut cache)
        && existing == candidate
}

/// Are `existing` and `candidate` the same [`Replacement::Summary`]
/// candidate?
///
/// [`SummaryNode`] derives neither `PartialEq` nor `Hash` (it embeds
/// `SketchParams`/`f64`-bearing accuracy targets deep inside `SummaryExpr`,
/// the same reason `QueryExpr` can't derive `Hash` either — see
/// [`structural_hash`]'s own doc). Per this module's inherited "hash is a
/// filter, `PartialEq` is the decision, no exceptions" rule, there is no
/// real equality check to back a dedup *decision* here — and skipping the
/// check is the only choice that rule permits: never merging two candidates
/// is harmless (at worst, a redundant entry in a group's candidate list),
/// while comparing by some proxy this module can't actually verify (e.g.
/// `Debug` text, or `ReplacementSubDAG::rationale` — documented elsewhere in
/// this crate as prose for a report, "not machine parsing") risks exactly
/// the false-positive merge the rule exists to prevent. Both strategies
/// shipped today already return a structurally distinct candidate for every
/// entry of one `replacements()` call, so this is future-proofing against a
/// hypothetical repeat call, not a gap either strategy's own tests exercise.
fn is_duplicate_summary(_existing: &Rc<SummaryNode>, _candidate: &Rc<SummaryNode>) -> bool {
    false
}

// ── PlanSpace ────────────────────────────────────────────────────────────

/// The deduped candidate space [`search_workload`]/[`search_workload_with`]
/// discover: one [`MemoGroup`] per distinct `TargetSubDAG` in the
/// (already-CSE'd) workload, plus the workload's own post-CSE roots so a
/// caller can still map a `Root`'s `Id` back to the `Rc<QueryExpr>` whose
/// group holds its alternatives.
pub struct PlanSpace<Id> {
    /// The workload's roots, after the one `share_common_subtrees` pass
    /// [`search_workload_with`] runs up front — the same post-CSE roots
    /// every `TargetSubDAG` in `groups` was discovered from.
    pub roots: Vec<(Id, Rc<QueryExpr>)>,
    groups: HashMap<*const QueryExpr, MemoGroup>,
    /// Discovery order — stable iteration for [`PlanSpace::groups`]/
    /// [`PlanSpace::cost_sorted`], since `HashMap` iteration order isn't.
    order: Vec<*const QueryExpr>,
}

impl<Id> PlanSpace<Id> {
    /// Every discovered group, in discovery order.
    pub fn groups(&self) -> impl Iterator<Item = &MemoGroup> {
        self.order.iter().map(move |ptr| &self.groups[ptr])
    }

    /// How many distinct targets were discovered.
    pub fn len(&self) -> usize {
        self.groups.len()
    }

    /// Whether no targets were discovered at all (an empty workload, or one
    /// with no `QueryExpr` nodes reachable from any root — never true for a
    /// non-empty `roots`, since every root is itself a target).
    pub fn is_empty(&self) -> bool {
        self.groups.is_empty()
    }

    /// The group for `target`, if `target`'s own `Rc` is a discovered
    /// `TargetSubDAG` (i.e. `Rc::ptr_eq` to some node reachable from
    /// `roots`).
    pub fn group_for(&self, target: &Rc<QueryExpr>) -> Option<&MemoGroup> {
        self.groups.get(&Rc::as_ptr(target))
    }

    /// The `sorted_by(cost_model)` step: every group, each with its own
    /// candidates ranked best-first under `cost_model` where this module
    /// knows how (see the module docs' "Cost-based final selection"
    /// section) — groups themselves stay in discovery order, since targets
    /// are independent decision points, not alternatives competing with
    /// each other.
    ///
    /// Ranking itself is decided entirely by [`rank_group`] before
    /// [`RankedGroup::costs`] is ever computed — pairing each candidate with
    /// [`CostModel::estimate_cost`]'s own number is an additive annotation
    /// for a caller that wants to *display* a cost (e.g. a
    /// DAG-visualization view), not a second ranking signal, so plugging in
    /// a `CostModel` whose `estimate_cost` disagrees with its own
    /// `rank_candidates`/`cse_share_decision` (a deployment bug, not
    /// something this method tries to protect against) would show a
    /// `RankedGroup` whose `costs` aren't monotonically non-decreasing —
    /// `cost_sorted`'s own ordering guarantee is unaffected either way.
    pub fn cost_sorted(&self, cost_model: &dyn CostModel) -> Vec<RankedGroup<'_>> {
        self.order
            .iter()
            .map(|ptr| {
                let group = &self.groups[ptr];
                let candidates = rank_group(group, cost_model);
                let target = TargetSubDAG::with_consumer_count(&group.target, group.consumer_count);
                let costs = candidates
                    .iter()
                    .map(|c| cost_model.estimate_cost(c, &target))
                    .collect();
                RankedGroup {
                    target: &group.target,
                    consumer_count: group.consumer_count,
                    candidates,
                    costs,
                }
            })
            .collect()
    }
}

/// One [`MemoGroup`]'s candidates, ranked best-first by
/// [`PlanSpace::cost_sorted`].
#[derive(Debug)]
pub struct RankedGroup<'a> {
    pub target: &'a Rc<QueryExpr>,
    pub consumer_count: usize,
    pub candidates: Vec<&'a ReplacementSubDAG>,
    /// `costs[i]` is `candidates[i]`'s own [`CostModel::estimate_cost`]
    /// estimate — aligned index-for-index with `candidates`, one number per
    /// candidate, for a caller that wants an actual `f64` next to each
    /// candidate (e.g. "candidate A costs ≈ X, candidate B costs ≈ Y") and
    /// not just `candidates`' own relative order. `f64::NAN` throughout
    /// unless `cost_model` overrides `estimate_cost` — see that method's own
    /// doc.
    pub costs: Vec<f64>,
}

/// Rank `group`'s candidates best-first under `cost_model`, per the module
/// docs' "Cost-based final selection" section. Falls back to discovery
/// order whenever there's nothing to rank (0 or 1 candidates) or this
/// module doesn't have a defined `CostModel` comparison for the shape it
/// sees — it never invents one.
fn rank_group<'a>(group: &'a MemoGroup, cost_model: &dyn CostModel) -> Vec<&'a ReplacementSubDAG> {
    let mut ranked: Vec<&ReplacementSubDAG> = group.candidates.iter().collect();
    if ranked.len() <= 1 {
        return ranked;
    }

    // Shape 1: a `SharedSubtreeStrategy` share-vs-recompute pair (every
    // candidate is a `Rewrite`) — rank via `CostModel::cse_share_decision`
    // (see `cse_preference` below).
    if ranked
        .iter()
        .all(|c| matches!(c.replacement, Replacement::Rewrite(_)))
    {
        if let Some(prefer_target) = cse_preference(group, cost_model) {
            ranked.sort_by_key(|c| {
                let is_target = matches!(
                    &c.replacement,
                    Replacement::Rewrite(rc) if Rc::ptr_eq(rc, &group.target)
                );
                u8::from(is_target != prefer_target)
            });
        }
        return ranked;
    }

    // Shape 2: `SketchAlgorithmStrategy`'s sketch-family candidates (every
    // candidate is a `Summary` that realizes a `SketchAlgorithm`) — rank via
    // `CostModel::rank_candidates`, the same hook `implementations_for_with`
    // itself consults.
    if let Some(intent) = bindable_intent(&group.target) {
        let kinds: Option<Vec<SketchAlgorithm>> = ranked
            .iter()
            .map(|c| match &c.replacement {
                Replacement::Summary(node) => sketch_kind_of(node),
                Replacement::Rewrite(_) => None,
            })
            .collect();
        if let Some(kinds) = kinds {
            let order = cost_model.rank_candidates(intent, &kinds);
            ranked.sort_by_key(|c| {
                let kind = match &c.replacement {
                    Replacement::Summary(node) => sketch_kind_of(node),
                    Replacement::Rewrite(_) => None,
                };
                kind.and_then(|k| order.iter().position(|o| *o == k))
                    .unwrap_or(usize::MAX)
            });
        }
    }
    ranked
}

/// For a group whose candidates are all [`Replacement::Rewrite`] (the
/// [`SharedSubtreeStrategy`] shape): does [`CostModel::cse_share_decision`]
/// prefer the candidate that shares `group.target`'s own `Rc` (`true`), or
/// the one that recomputes independently (`false`)? `None` when there's no
/// real comparison to make — fewer than 2 consumers (mirrors
/// [`SharedSubtreeStrategy::matches`]'s own gate), or `group.target` can't
/// actually be bound at all (no candidate and no logical fallback — never
/// expected in practice for a target that's already part of a legitimate
/// workload tree, but this degrades to "keep discovery order" rather than
/// panicking).
fn cse_preference(group: &MemoGroup, cost_model: &dyn CostModel) -> Option<bool> {
    if group.consumer_count < 2 {
        return None;
    }
    let bound = realize_one(&group.target, cost_model)?;
    let candidate = CseCandidate {
        subtree: &group.target,
        bound_summary: &bound,
        consumer_count: group.consumer_count,
    };
    Some(match cost_model.cse_share_decision(&candidate) {
        ShareDecision::Share => true,
        ShareDecision::RecomputeIndependently => false,
    })
}

/// [`cse_preference`] only needs one representative bound [`SummaryNode`]
/// for `target` (to build a [`CseCandidate`] for
/// [`CostModel::cse_share_decision`]), not the full ranked candidate list
/// [`SketchAlgorithmStrategy::replacements`] returns — so this just reuses
/// [`realize_child`], the same rank-and-take-first helper
/// `construct_summary_agg`'s own recursion and
/// [`crate::cost_model::DefaultCostModel::estimate_cost`] already use,
/// wrapped to swallow the (here, uninteresting) error into `None`.
fn realize_one(target: &Rc<QueryExpr>, cost_model: &dyn CostModel) -> Option<Rc<SummaryNode>> {
    realize_child(target, cost_model).ok()
}

/// The `SketchAlgorithm` a bound [`Replacement::Summary`] candidate ultimately
/// realizes, if any (`None` for an `ExactAggregate`/pass-through
/// `Summary` — nothing to rank against another `SketchAlgorithm`).
///
/// Mirrors this module's own `#[cfg(test)]`-only `summary_family_algorithm`
/// helper (in the test module below), which does the identical
/// `SummaryEstimate`-unwrap-then-match for that module's own tests; that
/// copy is test-only, so this needs its own for real (non-test) ranking
/// code — the same "duplicate a small, self-contained traversal rather than
/// restructure a test helper" call this file's own top doc already makes
/// for [`discover_targets`].
fn sketch_kind_of(node: &SummaryNode) -> Option<SketchAlgorithm> {
    match &node.expr {
        SummaryExpr::SummaryEstimate { summary_input, .. } => sketch_kind_of(summary_input),
        SummaryExpr::SummaryAgg {
            family: SummaryFamilyType::Sketch(kind),
            ..
        } => Some(kind.algorithm().clone()),
        _ => None,
    }
}

// ── default_strategies ──────────────────────────────────────────────────

/// The strategies [`search_workload`] runs, in the built-in
/// [`DefaultCostModel`] configuration — mirrors this module's own two
/// shipped [`ReplacementStrategy`] impls.
/// [`crate::applicability::find_applicable_optimizations`] (issue #257) uses
/// this same set (via [`search_workload`]) rather than keeping a second,
/// applicability-specific list to stay in sync with. Use
/// [`default_strategies_with`] to plug in a deployment-specific
/// [`CostModel`] instead.
pub fn default_strategies() -> Vec<Box<dyn ReplacementStrategy>> {
    vec![
        Box::new(SketchAlgorithmStrategy::default_cost_model()),
        Box::new(SharedSubtreeStrategy),
    ]
}

/// Like [`default_strategies`], but [`SketchAlgorithmStrategy`] ranks/binds via
/// `cost_model` instead of the built-in [`DefaultCostModel`] — the same
/// customization point [`SketchAlgorithmStrategy::new`] itself offers.
pub fn default_strategies_with<'a>(
    cost_model: &'a dyn CostModel,
) -> Vec<Box<dyn ReplacementStrategy + 'a>> {
    vec![
        Box::new(SketchAlgorithmStrategy::new(cost_model)),
        Box::new(SharedSubtreeStrategy),
    ]
}

// ── search_workload ──────────────────────────────────────────────────────

/// Search a whole workload's pre-ASAP roots for every candidate replacement
/// [`default_strategies`] can find, deduped into a [`PlanSpace`]. Candidate
/// *generation* uses the built-in [`DefaultCostModel`] (via
/// [`default_strategies`], the same way [`SketchAlgorithmStrategy::default_cost_model`]
/// does); call [`PlanSpace::cost_sorted`] on the result for the final
/// `sorted_by(cost_model)` step. Use [`search_workload_with`] to plug in a
/// custom strategy set (e.g. built via [`default_strategies_with`] for a
/// deployment-specific [`CostModel`]).
pub fn search_workload<Id>(roots: Vec<(Id, Rc<QueryExpr>)>) -> PlanSpace<Id> {
    search_workload_with(roots, &default_strategies())
}

/// Like [`search_workload`], but with an explicit `strategies` set (see
/// [`default_strategies_with`] to plug in a deployment-specific
/// [`CostModel`] for candidate generation).
///
/// Runs [`share_common_subtrees`] once over `roots` first — so every
/// strategy (and, transitively, every
/// [`crate::applicability::ApplicabilityFinding`] a caller reads off the
/// result) sees the same already-deduplicated tree — then discovers every
/// `TargetSubDAG` (see [`discover_targets`]) and runs the
/// fixpoint loop the module docs describe, capped at
/// [`MAX_SEARCH_ITERATIONS`] passes (see the module docs' "Termination"
/// section). Deduping candidate plans this way needs no
/// [`CostModel`] at all — that only enters at two well-defined points: each
/// [`ReplacementStrategy`] in `strategies` may already carry its own (e.g.
/// [`SketchAlgorithmStrategy::new`]'s), and [`PlanSpace::cost_sorted`]'s final
/// ranking step takes one explicitly.
pub fn search_workload_with<'s, Id>(
    roots: Vec<(Id, Rc<QueryExpr>)>,
    strategies: &[Box<dyn ReplacementStrategy + 's>],
) -> PlanSpace<Id> {
    // `share_common_subtrees` wants owned `QueryExpr`s, not already-`Rc`
    // roots — the same `Rc::try_unwrap`-with-clone-fallback pattern
    // `asap_types::pre_asap::cse::intern_child` itself uses to recover an
    // owned node without cloning in the common (uniquely-owned) case.
    let owned_roots: Vec<(Id, QueryExpr)> = roots
        .into_iter()
        .map(|(id, rc)| {
            let expr = Rc::try_unwrap(rc).unwrap_or_else(|shared| (*shared).clone());
            (id, expr)
        })
        .collect();
    let cse_roots = share_common_subtrees(owned_roots);

    let mut order = Vec::new();
    let mut nodes = HashMap::new();
    let mut counts: HashMap<*const QueryExpr, usize> = HashMap::new();
    discover_targets(&cse_roots, &mut order, &mut nodes, &mut counts);

    let mut groups: HashMap<*const QueryExpr, MemoGroup> = HashMap::new();
    for ptr in &order {
        groups.insert(*ptr, MemoGroup::new(Rc::clone(&nodes[ptr]), counts[ptr]));
    }

    // Round-based frontier: every target is asked exactly once per strategy
    // (never re-asked — see the module docs' "Termination" section on why
    // that matters for `Replacement::Summary` dedup specifically). A round
    // can grow the *next* round's frontier only by a candidate's own
    // reachable children exposing a genuinely new, not-yet-known `Rc` — see
    // `discover_new_descendant_targets`.
    let mut frontier = order.clone();
    let mut rounds = 0usize;
    while !frontier.is_empty() {
        rounds += 1;
        assert!(
            rounds <= MAX_SEARCH_ITERATIONS,
            "search_workload: fixpoint search did not converge within {MAX_SEARCH_ITERATIONS} \
             rounds — a registered ReplacementStrategy's Replacement::Rewrite candidates keep \
             exposing new, never-before-seen descendant structure every round. \
             SketchAlgorithmStrategy/SharedSubtreeStrategy never do this (see replacement.rs's \
             module docs' \"Termination\" section); check any custom strategies passed to \
             search_workload_with.",
        );

        let targets_before = order.len();
        for ptr in &frontier {
            let (root, consumer_count) = {
                let group = &groups[ptr];
                (Rc::clone(&group.target), group.consumer_count)
            };
            let target = TargetSubDAG::with_consumer_count(&root, consumer_count);

            let mut proposed = Vec::new();
            for strategy in strategies {
                if strategy.matches(&target) {
                    proposed.extend(strategy.replacements(&target));
                }
            }

            for candidate in &proposed {
                if let Replacement::Rewrite(rc) = &candidate.replacement {
                    discover_new_descendant_targets(rc, &mut order, &mut nodes, &mut counts);
                }
            }

            let group = groups
                .get_mut(ptr)
                .expect("every discovered target has a group");
            for candidate in proposed {
                group.add_candidate(candidate);
            }
        }

        // Any pointer `discover_new_descendant_targets` appended to `order`
        // this round is a genuinely new target — give it a group and process
        // it next round. Targets already in `groups` are never revisited.
        let new_targets = &order[targets_before..];
        for ptr in new_targets {
            groups
                .entry(*ptr)
                .or_insert_with(|| MemoGroup::new(Rc::clone(&nodes[ptr]), counts[ptr]));
        }
        frontier = new_targets.to_vec();
    }

    PlanSpace {
        roots: cse_roots,
        groups,
        order,
    }
}

// ── target discovery ─────────────────────────────────────────────────────

/// Walk every root's whole DAG, discovering one `TargetSubDAG` per distinct
/// `Rc` and its real `consumer_count` — see the module docs' "Where
/// `TargetSubDAG` discovery comes from" section for the full rationale.
fn discover_targets<Id>(
    roots: &[(Id, Rc<QueryExpr>)],
    order: &mut Vec<*const QueryExpr>,
    nodes: &mut HashMap<*const QueryExpr, Rc<QueryExpr>>,
    counts: &mut HashMap<*const QueryExpr, usize>,
) {
    for (_, root) in roots {
        walk(root, order, nodes, counts);
    }
}

/// Scan `candidate`'s **children** (deliberately never `candidate`'s own
/// top-level pointer — see the module docs' "Termination" section: a
/// [`Replacement::Rewrite`]'s value is an alternative *for* the target that
/// proposed it, never a new target of its own) for any `Rc` not already
/// known, appending each to `order`/`nodes`/`counts` so
/// [`search_workload_with`]'s next round processes it. A no-op when every
/// child is already known — the case both shipped strategies always produce
/// (see that section).
fn discover_new_descendant_targets(
    candidate: &Rc<QueryExpr>,
    order: &mut Vec<*const QueryExpr>,
    nodes: &mut HashMap<*const QueryExpr, Rc<QueryExpr>>,
    counts: &mut HashMap<*const QueryExpr, usize>,
) {
    walk_children(candidate, order, nodes, counts);
}

/// Visit `node`: count this occurrence, and — the first time this exact
/// `Rc` is seen — record it as a target and recurse into its children.
fn walk(
    node: &Rc<QueryExpr>,
    order: &mut Vec<*const QueryExpr>,
    nodes: &mut HashMap<*const QueryExpr, Rc<QueryExpr>>,
    counts: &mut HashMap<*const QueryExpr, usize>,
) {
    let ptr = Rc::as_ptr(node);
    let already_visited = counts.contains_key(&ptr);
    *counts.entry(ptr).or_insert(0) += 1;
    if !already_visited {
        order.push(ptr);
        nodes.insert(ptr, Rc::clone(node));
        walk_children(node, order, nodes, counts);
    }
}

/// `node`'s own **relational-skeleton** operator children — the same scope
/// `asap_types::pre_asap::cse::share_common_subtrees`/`rebuild_children`
/// itself uses (see that module's "Algorithm" section) and
/// `tests::count_consumers` mirrors for its own fixtures. Exhaustive over
/// every `QueryExpr` variant: a new variant fails to compile here until this
/// match is extended too.
fn walk_children(
    node: &QueryExpr,
    order: &mut Vec<*const QueryExpr>,
    nodes: &mut HashMap<*const QueryExpr, Rc<QueryExpr>>,
    counts: &mut HashMap<*const QueryExpr, usize>,
) {
    use QueryExpr::*;
    match node {
        Scan { .. } | PromqlScalarBridge(_) | QueryTimestamp => {}
        PromqlVectorFromScalar(c) | PromqlScalarFromVector(c) => walk(c, order, nodes, counts),
        PromqlRelabel { child, .. }
        | PromqlInfoEnrich { child, .. }
        | PromqlSeriesSample { child, .. }
        | Filter { child, .. }
        | Project { child, .. }
        | Aggregate { child, .. }
        | Dedup { child, .. }
        | PromqlSubquery { child, .. }
        | TimeRange { child, .. }
        | TimeShift { child, .. }
        | SQLWindowFunc { child, .. }
        | Sort { child, .. }
        | Limit { child, .. } => walk(child, order, nodes, counts),
        Concat { children } => {
            for c in children {
                walk_children(c, order, nodes, counts);
            }
        }
        Join { left, right, .. } | SetOp { left, right, .. } => {
            walk(left, order, nodes, counts);
            walk(right, order, nodes, counts);
        }
        BinaryOp { lhs, rhs, .. } => {
            walk(lhs, order, nodes, counts);
            walk(rhs, order, nodes, counts);
        }
        Column(_)
        | Literal(_)
        | Compare { .. }
        | BoolAnd(_)
        | BoolOr(_)
        | Not(_)
        | IsNull(_)
        | IsNotNull(_)
        | Cast { .. }
        | InList { .. }
        | FunctionCall { .. }
        | Arithmetic { .. }
        | Case { .. } => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use asap_types::pre_asap::agg_intent::{
        agg_is_exact, default_cardinality, default_quantile, MathFunc, TimeFunc,
    };
    use asap_types::pre_asap::query_expr::{Reduction as ReductionTy, Source};
    use asap_types::pre_asap::schema::{Column, DataType, Schema as SchemaTy};
    use asap_types::types::AccuracyTarget;
    use std::collections::HashMap;

    fn eps(e: f64) -> AccuracyTarget {
        AccuracyTarget::Epsilon(e)
    }

    // ── implementations_for_with / sizing ───────────────────────────────

    /// The most-preferred `Implementation` — `implementations_for_with(intent,
    /// &DefaultCostModel)`'s head — for tests that only care about the
    /// default pick, not the full candidate list.
    fn preferred(intent: &AggIntent) -> Implementation {
        implementations_for_with(intent, &DefaultCostModel)
            .into_iter()
            .next()
            .expect("every intent has at least one Implementation")
    }

    /// Shorthand for asserting the realization *category*.
    #[derive(Debug, PartialEq)]
    enum Cat {
        Sketch(SketchAlgorithm),
        Acc(ExactKind),
        Pass,
    }

    fn cat(intent: &AggIntent) -> Cat {
        match preferred(intent) {
            Implementation::ExactAggregate { kind, .. } => Cat::Acc(kind),
            Implementation::Sketch(kind) => Cat::Sketch(kind.algorithm().clone()),
            Implementation::PassThrough => Cat::Pass,
            other => {
                panic!("this coverage matrix expects only Exact/Sketch/PassThrough, got {other:?}")
            }
        }
    }

    /// The `AggIntent → SummaryKind` coverage matrix (issue #98): every intent
    /// variant maps to a sketch, an exact accumulator, or an explicit
    /// pass-through. `implementations_for_with`'s match is exhaustive, so a
    /// new variant cannot compile without a decision; this matrix pins what
    /// each decision *is* (its preferred/first candidate).
    #[test]
    fn agg_intent_to_summary_kind_coverage_matrix() {
        use AggIntent as A;
        use Cat::*;
        use ExactKind as E;
        use SketchAlgorithm as K;
        let matrix: Vec<(A, Cat)> = vec![
            // approximate-capable, at an ε target → sketch
            (default_quantile(0.99), Sketch(K::Kll)),
            (default_cardinality(), Sketch(K::Hll)),
            (
                A::Count {
                    accuracy: eps(0.01),
                },
                Sketch(K::Cms),
            ),
            (
                A::TopK {
                    k: 10,
                    accuracy: eps(0.01),
                },
                Sketch(K::CmsWithHeap),
            ),
            // the same intents at Exact → exact realization
            (
                A::Quantile {
                    col: None,
                    q: 0.5,
                    accuracy: AccuracyTarget::Exact,
                },
                Pass,
            ),
            (
                A::Cardinality {
                    col: None,
                    accuracy: AccuracyTarget::Exact,
                },
                Pass,
            ),
            (
                A::Count {
                    accuracy: AccuracyTarget::Exact,
                },
                Acc(E::Count),
            ),
            (
                A::TopK {
                    k: 10,
                    accuracy: AccuracyTarget::Exact,
                },
                Pass,
            ),
            // exact mergeable accumulators
            (A::Sum { col: None }, Acc(E::Sum)),
            (A::Min { col: None }, Acc(E::MinMax)),
            (A::Max { col: None }, Acc(E::MinMax)),
            (A::Rate, Acc(E::Rate)),
            (A::Increase, Acc(E::Increase)),
            // exact but non-mergeable → pass-through
            (A::Avg { col: None }, Pass),
            (
                A::StdDev {
                    col: None,
                    population: false,
                },
                Pass,
            ),
            (
                A::Variance {
                    col: None,
                    population: true,
                },
                Pass,
            ),
            // classic-bucket histogram_quantile is not re-sketchable (#79)
            (A::HistogramQuantile { q: 0.99 }, Pass),
            // counter-derivative / range-vector functions (#44)
            (A::Changes, Pass),
            (A::Delta, Pass),
            (A::IDelta, Pass),
            (A::Deriv, Pass),
            (A::Resets, Pass),
            (A::PredictLinear { seconds: 60.0 }, Pass),
            (
                A::DoubleExpSmoothing {
                    smoothing: 0.5,
                    trend: 0.5,
                },
                Pass,
            ),
            // native-histogram accessors (#43)
            (A::HistogramCount, Pass),
            (A::HistogramSum, Pass),
            (A::HistogramAvg, Pass),
            (A::HistogramStdDev, Pass),
            (A::HistogramStdVar, Pass),
            (
                A::HistogramFraction {
                    lower: 0.0,
                    upper: 1.0,
                },
                Pass,
            ),
            // per-sample transforms (#45, #46) + presence (#47)
            (A::Math(MathFunc::Abs), Pass),
            (A::TimeFn(TimeFunc::Hour), Pass),
            (A::Absent, Pass),
            (A::AbsentOverTime, Pass),
            (A::PresentOverTime, Pass),
            // extended aggregations (#49)
            (A::Group, Pass),
            (A::CountValues { label: "v".into() }, Pass),
            // additional range reducers (#51)
            (A::LastOverTime, Pass),
            (A::FirstOverTime, Pass),
            (A::MadOverTime, Pass),
            (A::TsOfMinOverTime, Pass),
            (A::TsOfMaxOverTime, Pass),
            (A::TsOfFirstOverTime, Pass),
            (A::TsOfLastOverTime, Pass),
        ];
        for (intent, expected) in &matrix {
            assert_eq!(&cat(intent), expected, "realization for {intent:?}");
        }
        // Every accumulator pick is mergeable; every sketch pick is on a
        // genuinely approximate target (the `agg_is_*` helpers stay truthful).
        for (intent, expected) in &matrix {
            if let Cat::Acc(_) = expected {
                assert!(agg_is_mergeable(intent), "{intent:?}");
            }
            if let Cat::Sketch(_) = expected {
                assert!(
                    !agg_is_exact(intent) || matches!(intent, AggIntent::Count { .. }),
                    "{intent:?} sketches only under an approximate target"
                );
            }
        }
    }

    #[test]
    fn accuracy_target_drives_the_boundary() {
        // Same intent, three targets → three different decisions.
        let exact = AggIntent::Quantile {
            col: None,
            q: 0.99,
            accuracy: AccuracyTarget::Exact,
        };
        assert_eq!(preferred(&exact), Implementation::PassThrough);

        let approx = default_quantile(0.99); // ε = 0.01
        assert_eq!(
            preferred(&approx),
            Implementation::Sketch(SketchKind::Quantile(
                SketchAlgorithm::Kll,
                SketchParams::Kll { k: 200 }, // design.md worked example
            ))
        );

        let looser = AggIntent::Quantile {
            col: None,
            q: 0.99,
            accuracy: eps(0.05),
        };
        assert_eq!(
            preferred(&looser),
            Implementation::Sketch(SketchKind::Quantile(
                SketchAlgorithm::Kll,
                SketchParams::Kll { k: 40 }, // ⌈2/0.05⌉
            ))
        );
    }

    #[test]
    fn default_cardinality_inverts_to_hll_precision_14() {
        // `default_cardinality` encodes HLL's standard error at p=14; the
        // sizing must invert it back exactly.
        assert_eq!(
            preferred(&default_cardinality()),
            Implementation::Sketch(SketchKind::Cardinality(
                SketchAlgorithm::Hll,
                SketchParams::Hll { precision: 14 },
            ))
        );
    }

    #[test]
    fn epsilon_delta_sizes_cms_depth() {
        let intent = AggIntent::Count {
            accuracy: AccuracyTarget::EpsilonDelta {
                epsilon: 0.001,
                delta: 0.001,
            },
        };
        assert_eq!(
            preferred(&intent),
            Implementation::Sketch(SketchKind::Frequency(
                SketchAlgorithm::Cms,
                SketchParams::Cms {
                    width: 2719,
                    depth: 7
                }, // ⌈e/0.001⌉, ⌈ln 1000⌉
            ))
        );
        // Epsilon-only falls back to DEFAULT_DELTA → depth 5.
        let intent = AggIntent::Count {
            accuracy: eps(0.001),
        };
        assert_eq!(
            preferred(&intent),
            Implementation::Sketch(SketchKind::Frequency(
                SketchAlgorithm::Cms,
                SketchParams::Cms {
                    width: 2719,
                    depth: 5
                },
            ))
        );
    }

    #[test]
    fn topk_heap_size_tracks_k() {
        let intent = AggIntent::TopK {
            k: 25,
            accuracy: eps(0.01),
        };
        match preferred(&intent) {
            Implementation::Sketch(SketchKind::TopK(
                SketchAlgorithm::CmsWithHeap,
                SketchParams::CmsWithHeap {
                    width,
                    depth,
                    heap_size,
                },
            )) => {
                assert_eq!(heap_size, 25);
                assert_eq!(width, 272); // ⌈e/0.01⌉
                assert_eq!(depth, 5);
            }
            other => panic!("expected CmsWithHeap, got {other:?}"),
        }
    }

    #[test]
    fn candidate_lists_match_the_issue_map() {
        assert_eq!(
            summary_candidates(&default_quantile(0.5)),
            &[SketchAlgorithm::Kll, SketchAlgorithm::DDSketch]
        );
        assert_eq!(
            summary_candidates(&default_cardinality()),
            &[
                SketchAlgorithm::Hll,
                SketchAlgorithm::Theta,
                SketchAlgorithm::Kmv
            ]
        );
        assert_eq!(
            summary_candidates(&AggIntent::TopK {
                k: 5,
                accuracy: eps(0.01)
            }),
            &[
                SketchAlgorithm::CmsWithHeap,
                SketchAlgorithm::CountSketchWithHeap
            ]
        );
        assert_eq!(
            summary_candidates(&AggIntent::Count {
                accuracy: eps(0.01)
            }),
            &[SketchAlgorithm::Cms, SketchAlgorithm::CountSketch]
        );
        assert!(summary_candidates(&AggIntent::Rate).is_empty());
    }

    #[test]
    fn implementations_for_with_enumerates_every_candidate_ranked() {
        // Quantile's candidate list is [Kll, DDSketch] — implementations_for_with
        // must return both, ranked with the DefaultCostModel's preferred
        // (Kll) first.
        let kinds: Vec<SketchAlgorithm> =
            implementations_for_with(&default_quantile(0.99), &DefaultCostModel)
                .into_iter()
                .map(|implementation| match implementation {
                    Implementation::Sketch(kind) => kind.algorithm().clone(),
                    other => panic!("expected Sketch, got {other:?}"),
                })
                .collect();
        assert_eq!(kinds, vec![SketchAlgorithm::Kll, SketchAlgorithm::DDSketch]);
    }

    #[test]
    fn degenerate_epsilon_saturates_to_tightest_params() {
        let intent = AggIntent::Quantile {
            col: None,
            q: 0.99,
            accuracy: eps(0.0),
        };
        assert_eq!(
            preferred(&intent),
            Implementation::Sketch(SketchKind::Quantile(
                SketchAlgorithm::Kll,
                SketchParams::Kll { k: 65_535 },
            ))
        );
    }

    // ── posterior_aware_size_params (issue #239, integration point 2) ──────

    fn count_intent(e: f64) -> AggIntent {
        AggIntent::Count { accuracy: eps(e) }
    }

    #[test]
    fn posterior_aware_sizing_shrinks_width_under_stated_assumption() {
        let intent = count_intent(0.01);
        let worst_case = default_size_params(SketchAlgorithm::Cms, &intent, 0.01, 0.01);
        let relaxed = posterior_aware_size_params(
            SketchAlgorithm::Cms,
            &intent,
            0.01,
            0.01,
            ExpectedCaseSizing {
                width_relaxation: 0.5,
            },
        );
        match (worst_case, relaxed) {
            (
                SketchParams::Cms {
                    width: w0,
                    depth: d0,
                },
                SketchParams::Cms {
                    width: w1,
                    depth: d1,
                },
            ) => {
                assert!(
                    w1 < w0,
                    "expected relaxed width {w1} to be strictly smaller than worst-case {w0}"
                );
                assert_eq!(d0, d1, "depth must be unaffected by width_relaxation");
            }
            other => panic!("expected Cms/Cms pair, got {other:?}"),
        }
    }

    #[test]
    fn posterior_aware_sizing_at_full_relaxation_matches_worst_case() {
        // width_relaxation = 1.0 must reproduce default_size_params exactly
        // — the "no risk taken" boundary.
        let intent = count_intent(0.01);
        let worst_case = default_size_params(SketchAlgorithm::Cms, &intent, 0.01, 0.01);
        let relaxed = posterior_aware_size_params(
            SketchAlgorithm::Cms,
            &intent,
            0.01,
            0.01,
            ExpectedCaseSizing {
                width_relaxation: 1.0,
            },
        );
        assert_eq!(worst_case, relaxed);
    }

    #[test]
    fn posterior_aware_sizing_invalid_relaxation_falls_back_to_worst_case() {
        let intent = count_intent(0.01);
        let worst_case = default_size_params(SketchAlgorithm::Cms, &intent, 0.01, 0.01);
        for bad in [0.0, -0.5, 1.5, f64::NAN, f64::INFINITY] {
            let relaxed = posterior_aware_size_params(
                SketchAlgorithm::Cms,
                &intent,
                0.01,
                0.01,
                ExpectedCaseSizing {
                    width_relaxation: bad,
                },
            );
            assert_eq!(
                worst_case, relaxed,
                "width_relaxation={bad} should fall back to the worst-case width"
            );
        }
    }

    #[test]
    fn posterior_aware_sizing_applies_to_every_cms_family_kind() {
        let cms_heap_intent = AggIntent::TopK {
            k: 7,
            accuracy: eps(0.01),
        };
        let assumption = ExpectedCaseSizing {
            width_relaxation: 0.25,
        };
        // CountSketch
        assert_eq!(
            posterior_aware_size_params(
                SketchAlgorithm::CountSketch,
                &count_intent(0.01),
                0.01,
                0.01,
                assumption
            ),
            SketchParams::CountSketch {
                width: 68,
                depth: 5
            }, // ceil(272 * 0.25)
        );
        // CmsWithHeap / CountSketchWithHeap carry k through untouched.
        match posterior_aware_size_params(
            SketchAlgorithm::CmsWithHeap,
            &cms_heap_intent,
            0.01,
            0.01,
            assumption,
        ) {
            SketchParams::CmsWithHeap {
                width,
                depth,
                heap_size,
            } => {
                assert_eq!(width, 68);
                assert_eq!(depth, 5);
                assert_eq!(heap_size, 7);
            }
            other => panic!("expected CmsWithHeap, got {other:?}"),
        }
    }

    #[test]
    fn posterior_aware_sizing_leaves_non_cms_kinds_unchanged() {
        // Kll/Hll/etc. have no width_relaxation concept — must be byte-for-
        // byte identical to default_size_params.
        let intent = default_quantile(0.99);
        let assumption = ExpectedCaseSizing {
            width_relaxation: 0.1,
        };
        assert_eq!(
            posterior_aware_size_params(SketchAlgorithm::Kll, &intent, 0.01, 0.01, assumption),
            default_size_params(SketchAlgorithm::Kll, &intent, 0.01, 0.01),
        );
    }

    #[test]
    fn default_size_params_unchanged_by_new_function_existing() {
        // Regression pin: default_size_params's own worst-case behavior for
        // existing callers must be untouched by adding
        // posterior_aware_size_params alongside it.
        assert_eq!(
            default_size_params(SketchAlgorithm::Cms, &count_intent(0.001), 0.001, 0.001),
            SketchParams::Cms {
                width: 2719,
                depth: 7
            },
        );
    }

    // ── SketchAlgorithmStrategy / SharedSubtreeStrategy fixtures ───────────

    fn metric_scan(labels: &[&str]) -> QueryExpr {
        let mut columns = vec![
            Column::new("ts", DataType::Timestamp, false),
            Column::new("value", DataType::Float64, false),
        ];
        columns.extend(labels.iter().map(|n| Column::new(*n, DataType::Utf8, true)));
        QueryExpr::Scan {
            source: Source::TimeSeries { metric: "m".into() },
            predicates: vec![],
            schema: SchemaTy::with_time_index(columns, 0, vec![]),
        }
    }

    fn agg(by: Vec<usize>, intent: AggIntent, child: QueryExpr) -> QueryExpr {
        QueryExpr::Aggregate {
            reduction: ReductionTy::by(by),
            measures: vec![intent],
            output_names: vec![],
            having: None,
            child: Rc::new(child),
        }
    }

    // ── SketchAlgorithmStrategy ─────────────────────────────────────────────

    #[test]
    fn matches_a_bindable_aggregate() {
        let q = Rc::new(agg(vec![2], default_quantile(0.99), metric_scan(&["job"])));
        let target = TargetSubDAG::new(&q);
        assert!(SketchAlgorithmStrategy::default_cost_model().matches(&target));
    }

    #[test]
    fn does_not_match_a_multi_intent_or_having_aggregate() {
        let strategy = SketchAlgorithmStrategy::default_cost_model();

        let multi = Rc::new(QueryExpr::Aggregate {
            reduction: ReductionTy::by(vec![2]),
            measures: vec![AggIntent::Sum { col: None }, AggIntent::Avg { col: None }],
            output_names: vec![],
            having: None,
            child: Rc::new(metric_scan(&["job"])),
        });
        let target = TargetSubDAG::new(&multi);
        assert!(!strategy.matches(&target));
        assert!(strategy.replacements(&target).is_empty());

        let mut having_q = agg(vec![2], default_quantile(0.99), metric_scan(&["job"]));
        if let QueryExpr::Aggregate { having, .. } = &mut having_q {
            *having = Some(asap_types::pre_asap::query_expr::Predicate(Rc::new(
                QueryExpr::Literal(asap_types::pre_asap::expr_ir::ScalarValue::Boolean(true)),
            )));
        }
        let having_q = Rc::new(having_q);
        let target = TargetSubDAG::new(&having_q);
        assert!(!strategy.matches(&target));
        assert!(strategy.replacements(&target).is_empty());
    }

    #[test]
    fn does_not_match_a_non_aggregate_node() {
        let scan = Rc::new(metric_scan(&["job"]));
        let target = TargetSubDAG::new(&scan);
        assert!(!SketchAlgorithmStrategy::default_cost_model().matches(&target));
        assert!(SketchAlgorithmStrategy::default_cost_model()
            .replacements(&target)
            .is_empty());
    }

    #[test]
    fn approximate_quantile_enumerates_every_summary_candidate() {
        // Quantile's candidate list is [Kll, DDSketch] (summary_candidates) —
        // every entry must come back as its own bound SummaryNode candidate,
        // not just Kll (the CostModel-ranked head implementations_for_with commits to).
        let q = Rc::new(agg(vec![2], default_quantile(0.99), metric_scan(&["job"])));
        let target = TargetSubDAG::new(&q);
        let replacements = SketchAlgorithmStrategy::default_cost_model().replacements(&target);
        assert_eq!(
            replacements.len(),
            2,
            "expected 2 candidates, got {replacements:?}"
        );

        let kinds: Vec<SketchAlgorithm> = replacements
            .iter()
            .map(|r| match &r.replacement {
                Replacement::Summary(node) => summary_family_algorithm(node),
                Replacement::Rewrite(_) => panic!("expected a Summary replacement"),
            })
            .collect();
        assert!(kinds.contains(&SketchAlgorithm::Kll), "{kinds:?}");
        assert!(kinds.contains(&SketchAlgorithm::DDSketch), "{kinds:?}");
        assert!(
            replacements.iter().all(|r| !r.rationale.is_empty()),
            "every candidate must carry a rationale"
        );
    }

    #[test]
    fn cardinality_enumerates_all_three_summary_candidates() {
        let q = Rc::new(agg(vec![2], default_cardinality(), metric_scan(&["job"])));
        let target = TargetSubDAG::new(&q);
        let replacements = SketchAlgorithmStrategy::default_cost_model().replacements(&target);
        let kinds: Vec<SketchAlgorithm> = replacements
            .iter()
            .map(|r| match &r.replacement {
                Replacement::Summary(node) => summary_family_algorithm(node),
                Replacement::Rewrite(_) => panic!("expected a Summary replacement"),
            })
            .collect();
        assert_eq!(
            kinds,
            vec![
                SketchAlgorithm::Hll,
                SketchAlgorithm::Theta,
                SketchAlgorithm::Kmv
            ],
            "expected every summary_candidates entry for Cardinality"
        );
    }

    #[test]
    fn exact_accuracy_target_yields_exactly_one_pass_through_candidate() {
        // Exact quantile has no sketch candidate at all — implementations_for_with
        // produces PassThrough, the only option, so exactly one candidate.
        let intent = AggIntent::Quantile {
            col: None,
            q: 0.99,
            accuracy: AccuracyTarget::Exact,
        };
        let q = Rc::new(agg(vec![2], intent, metric_scan(&["job"])));
        let target = TargetSubDAG::new(&q);
        let replacements = SketchAlgorithmStrategy::default_cost_model().replacements(&target);
        assert_eq!(replacements.len(), 1, "{replacements:?}");
        assert!(matches!(
            &replacements[0].replacement,
            Replacement::Summary(node) if matches!(
                node.expr,
                asap_types::post_asap::SummaryExpr::KeepPreAsap(_)
            )
        ));
        assert!(replacements[0].rationale.contains("only realization"));
    }

    #[test]
    fn exact_mergeable_intent_yields_exactly_one_accumulator_candidate() {
        let q = Rc::new(agg(
            vec![2],
            AggIntent::Sum { col: None },
            metric_scan(&["job"]),
        ));
        let target = TargetSubDAG::new(&q);
        let replacements = SketchAlgorithmStrategy::default_cost_model().replacements(&target);
        assert_eq!(replacements.len(), 1, "{replacements:?}");
        assert!(matches!(
            &replacements[0].replacement,
            Replacement::Summary(node) if matches!(
                node.expr,
                asap_types::post_asap::SummaryExpr::SummaryAgg { .. }
            )
        ));
    }

    /// A custom `CostModel` doesn't change *which* candidates are enumerated
    /// (still every `summary_candidates` entry) — only which one
    /// `implementations_for_with` itself would prefer first, and how each
    /// candidate's own params are sized.
    struct PreferDDSketch;
    impl CostModel for PreferDDSketch {
        fn rank_candidates(
            &self,
            _intent: &AggIntent,
            candidates: &[SketchAlgorithm],
        ) -> Vec<SketchAlgorithm> {
            let mut v = candidates.to_vec();
            if let Some(pos) = v.iter().position(|k| *k == SketchAlgorithm::DDSketch) {
                let dd = v.remove(pos);
                v.insert(0, dd);
            }
            v
        }
    }

    #[test]
    fn custom_cost_model_still_enumerates_every_candidate_not_just_its_own_pick() {
        let q = Rc::new(agg(vec![2], default_quantile(0.99), metric_scan(&["job"])));
        let target = TargetSubDAG::new(&q);
        let custom = PreferDDSketch;
        let replacements = SketchAlgorithmStrategy::new(&custom).replacements(&target);
        let kinds: Vec<SketchAlgorithm> = replacements
            .iter()
            .map(|r| match &r.replacement {
                Replacement::Summary(node) => summary_family_algorithm(node),
                Replacement::Rewrite(_) => panic!("expected a Summary replacement"),
            })
            .collect();
        assert!(kinds.contains(&SketchAlgorithm::Kll));
        assert!(kinds.contains(&SketchAlgorithm::DDSketch));
        assert_eq!(kinds.len(), 2);
    }

    /// Enumerating candidates for the *target* node must only steer that
    /// node's own decision — a nested aggregate underneath it still gets its
    /// own independent (`cost_model`-ranked) enumeration, not whatever the
    /// caller happened to pick for the outer target. This is the behavior
    /// [`construct_summary`]'s recursion (via [`realize_child`])
    /// gets for free: only the top node's `Implementation` is ever forced
    /// from outside; the child is always re-enumerated fresh.
    #[test]
    fn enumerating_the_targets_candidates_does_not_leak_into_a_nested_aggregate() {
        // outer: quantile(0.99, ...) over inner: quantile(0.5, m) — both
        // Quantile, so both share the [Kll, DDSketch] candidate list.
        let inner = agg(vec![2], default_quantile(0.5), metric_scan(&["job"]));
        let outer = Rc::new(agg(vec![], default_quantile(0.99), inner));
        let target = TargetSubDAG::new(&outer);
        let replacements = SketchAlgorithmStrategy::default_cost_model().replacements(&target);

        let ddsketch = replacements
            .iter()
            .find(|r| {
                matches!(&r.replacement, Replacement::Summary(node)
                    if summary_family_algorithm(node) == SketchAlgorithm::DDSketch)
            })
            .expect("the outer target's DDSketch candidate must be present");
        let Replacement::Summary(node) = &ddsketch.replacement else {
            unreachable!("filtered on Replacement::Summary above");
        };
        assert_eq!(
            summary_family_algorithm(node),
            SketchAlgorithm::DDSketch,
            "the outer (target) node must be the DDSketch candidate"
        );

        let asap_types::post_asap::SummaryExpr::SummaryEstimate { summary_input, .. } = &node.expr
        else {
            panic!("expected SummaryEstimate root, got {:?}", node.expr);
        };
        let asap_types::post_asap::SummaryExpr::SummaryAgg { child, .. } = &summary_input.expr
        else {
            panic!("expected SummaryAgg, got {:?}", summary_input.expr);
        };
        assert_eq!(
            summary_family_algorithm(child),
            SketchAlgorithm::Kll,
            "the nested inner aggregate must still get the cost-model-ranked \
             default (Kll), not inherit the outer target's DDSketch candidate"
        );
    }

    /// The `SummaryFamilyType`'s committed `SketchAlgorithm`, from the top
    /// `SummaryAgg` reachable under a (possibly `SummaryEstimate`-wrapped)
    /// bound root.
    fn summary_family_algorithm(node: &SummaryNode) -> SketchAlgorithm {
        match &node.expr {
            asap_types::post_asap::SummaryExpr::SummaryEstimate { summary_input, .. } => {
                summary_family_algorithm(summary_input)
            }
            asap_types::post_asap::SummaryExpr::SummaryAgg { family, .. } => match family {
                asap_types::post_asap::SummaryFamilyType::Sketch(kind) => kind.algorithm().clone(),
                other => panic!("expected a Sketch family, got {other:?}"),
            },
            other => panic!("expected SummaryAgg/SummaryEstimate, got {other:?}"),
        }
    }

    // ── SharedSubtreeStrategy ────────────────────────────────────────────

    #[test]
    fn does_not_match_a_single_consumer_target() {
        let q = Rc::new(agg(
            vec![2],
            AggIntent::Sum { col: None },
            metric_scan(&["job"]),
        ));
        let target = TargetSubDAG::new(&q);
        assert_eq!(target.consumer_count, 1);
        assert!(!SharedSubtreeStrategy.matches(&target));
        assert!(SharedSubtreeStrategy.replacements(&target).is_empty());
    }

    #[test]
    fn two_or_more_consumers_yields_the_share_vs_independent_pair() {
        let q = Rc::new(agg(
            vec![2],
            AggIntent::Sum { col: None },
            metric_scan(&["job"]),
        ));
        let target = TargetSubDAG::with_consumer_count(&q, 2);
        assert!(SharedSubtreeStrategy.matches(&target));

        let replacements = SharedSubtreeStrategy.replacements(&target);
        assert_eq!(replacements.len(), 2, "{replacements:?}");

        let shared = match &replacements[0].replacement {
            Replacement::Rewrite(rc) => rc,
            other => panic!("expected a Rewrite replacement, got {other:?}"),
        };
        assert!(
            Rc::ptr_eq(shared, &q),
            "the 'build once and share' candidate must be the same Rc as the target"
        );
        assert!(replacements[0].rationale.contains("build once and share"));

        let independent = match &replacements[1].replacement {
            Replacement::Rewrite(rc) => rc,
            other => panic!("expected a Rewrite replacement, got {other:?}"),
        };
        assert!(
            !Rc::ptr_eq(independent, &q),
            "the 'build independently' candidate must be a distinct Rc from the target"
        );
        assert_eq!(
            **independent, *q,
            "the 'build independently' candidate must still be structurally identical"
        );
        assert!(replacements[1].rationale.contains("build independently"));
    }

    #[test]
    fn three_consumers_are_reported_verbatim_in_both_rationales() {
        let q = Rc::new(agg(
            vec![2],
            AggIntent::Sum { col: None },
            metric_scan(&["job"]),
        ));
        let target = TargetSubDAG::with_consumer_count(&q, 3);
        let replacements = SharedSubtreeStrategy.replacements(&target);
        assert!(replacements[0].rationale.contains('3'));
        assert!(replacements[1].rationale.contains('3'));
    }

    /// Builds realistic multi-consumer `TargetSubDAG`s the same way this
    /// module's own [`discover_targets`]/`walk` does: dedup by `Rc::as_ptr`,
    /// walking only the relational-skeleton operator children
    /// `asap_types::pre_asap::cse::share_common_subtrees` itself scopes to,
    /// so a shared node nested below another shared node is only ever
    /// counted at the highest (maximal) point sharing starts. Test-only:
    /// this module deliberately does not ship a workload-wide discovery
    /// pass of its own (see the module docs' "Non-goals").
    fn count_consumers(roots: &[Rc<QueryExpr>]) -> HashMap<*const QueryExpr, usize> {
        fn walk(node: &Rc<QueryExpr>, counts: &mut HashMap<*const QueryExpr, usize>) {
            let ptr = Rc::as_ptr(node);
            let already_visited = counts.contains_key(&ptr);
            *counts.entry(ptr).or_insert(0) += 1;
            if !already_visited {
                walk_children(node, counts);
            }
        }
        fn walk_children(node: &QueryExpr, counts: &mut HashMap<*const QueryExpr, usize>) {
            use QueryExpr::*;
            match node {
                Scan { .. } | PromqlScalarBridge(_) | QueryTimestamp => {}
                PromqlVectorFromScalar(c) | PromqlScalarFromVector(c) => walk(c, counts),
                PromqlRelabel { child, .. }
                | PromqlInfoEnrich { child, .. }
                | PromqlSeriesSample { child, .. }
                | Filter { child, .. }
                | Project { child, .. }
                | Aggregate { child, .. }
                | Dedup { child, .. }
                | PromqlSubquery { child, .. }
                | TimeRange { child, .. }
                | TimeShift { child, .. }
                | SQLWindowFunc { child, .. }
                | Sort { child, .. }
                | Limit { child, .. } => walk(child, counts),
                Concat { children } => {
                    for c in children {
                        walk_children(c, counts);
                    }
                }
                Join { left, right, .. } | SetOp { left, right, .. } => {
                    walk(left, counts);
                    walk(right, counts);
                }
                BinaryOp { lhs, rhs, .. } => {
                    walk(lhs, counts);
                    walk(rhs, counts);
                }
                Column(_)
                | Literal(_)
                | Compare { .. }
                | BoolAnd(_)
                | BoolOr(_)
                | Not(_)
                | IsNull(_)
                | IsNotNull(_)
                | Cast { .. }
                | InList { .. }
                | FunctionCall { .. }
                | Arithmetic { .. }
                | Case { .. } => {}
            }
        }

        let mut counts = HashMap::new();
        for root in roots {
            walk(root, &mut counts);
        }
        counts
    }

    #[test]
    fn realistic_cse_output_produces_a_two_consumer_target() {
        // Two workload roots that `share_common_subtrees` collapses onto one
        // Rc (mirrors `applicability`'s and `cse`'s own fixtures): a grouped
        // Sum aggregate over the same scan, built independently at each root.
        let a = agg(vec![2], AggIntent::Sum { col: None }, metric_scan(&["job"]));
        let b = agg(vec![2], AggIntent::Sum { col: None }, metric_scan(&["job"]));
        let shared = asap_types::pre_asap::cse::share_common_subtrees(vec![("a", a), ("b", b)]);
        let [(_, ra), (_, rb)] = shared.as_slice() else {
            panic!("expected 2 roots");
        };
        assert!(Rc::ptr_eq(ra, rb), "fixture sanity: the two roots merged");

        let roots: Vec<Rc<QueryExpr>> = shared.into_iter().map(|(_, rc)| rc).collect();
        let counts = count_consumers(&roots);
        let count = counts[&Rc::as_ptr(&roots[0])];
        assert_eq!(count, 2);

        let target = TargetSubDAG::with_consumer_count(&roots[0], count);
        assert!(SharedSubtreeStrategy.matches(&target));
        assert_eq!(SharedSubtreeStrategy.replacements(&target).len(), 2);
    }

    // ── search_workload / PlanSpace / MemoGroup (merged from search.rs) ──
    //
    // Reuses this test module's own `metric_scan`/`agg` fixture helpers
    // above (identical to `search.rs`'s own copies, which are dropped here
    // to avoid a duplicate-definition collision now that both test modules
    // share one file) and `count_consumers` above (which mirrors
    // `discover_targets`' own real, non-test traversal for these fixtures).

    // ── discovery + MEMO shape ───────────────────────────────────────────

    #[test]
    fn single_bindable_aggregate_gets_a_group_with_every_sketch_candidate() {
        let root = Rc::new(agg(vec![2], default_quantile(0.99), metric_scan(&["job"])));
        let space = search_workload(vec![("q", root)]);

        // One group for the Aggregate, one for its Scan child.
        assert_eq!(space.len(), 2);

        let agg_group = space
            .groups()
            .find(|g| matches!(g.target.as_ref(), QueryExpr::Aggregate { .. }))
            .expect("an Aggregate group must be discovered");
        assert_eq!(agg_group.consumer_count, 1);
        assert_eq!(
            agg_group.candidates.len(),
            2,
            "quantile has 2 summary_candidates entries: {:?}",
            agg_group.candidates
        );
        assert!(agg_group
            .candidates
            .iter()
            .all(|c| matches!(c.replacement, Replacement::Summary(_))));

        let scan_group = space
            .groups()
            .find(|g| matches!(g.target.as_ref(), QueryExpr::Scan { .. }))
            .expect("a Scan group must be discovered");
        assert_eq!(scan_group.consumer_count, 1);
        assert!(
            scan_group.candidates.is_empty(),
            "no strategy matches a bare Scan"
        );
    }

    #[test]
    fn cardinality_group_gets_all_three_candidates() {
        let root = Rc::new(agg(vec![2], default_cardinality(), metric_scan(&["job"])));
        let space = search_workload(vec![("q", root)]);
        let agg_group = space
            .groups()
            .find(|g| matches!(g.target.as_ref(), QueryExpr::Aggregate { .. }))
            .unwrap();
        assert_eq!(agg_group.candidates.len(), 3);
    }

    #[test]
    fn shared_aggregate_across_two_roots_gets_both_strategies_candidates() {
        // Two independently-built, structurally identical Sum aggregates:
        // share_common_subtrees (run inside search_workload) collapses them
        // onto one Rc with consumer_count 2, so this single group should
        // carry SketchAlgorithmStrategy's one ExactAggregate candidate *and*
        // SharedSubtreeStrategy's share-vs-recompute pair.
        let a = agg(vec![2], AggIntent::Sum { col: None }, metric_scan(&["job"]));
        let b = agg(vec![2], AggIntent::Sum { col: None }, metric_scan(&["job"]));
        let space = search_workload(vec![("a", Rc::new(a)), ("b", Rc::new(b))]);

        // roots[0] and roots[1] must have merged onto the same Rc.
        assert!(Rc::ptr_eq(&space.roots[0].1, &space.roots[1].1));

        let group = space.group_for(&space.roots[0].1).unwrap();
        assert_eq!(group.consumer_count, 2);
        assert_eq!(
            group.candidates.len(),
            3,
            "1 ExactAggregate Summary + 2 Rewrite (share/recompute): {:?}",
            group.candidates
        );

        let summary_count = group
            .candidates
            .iter()
            .filter(|c| matches!(c.replacement, Replacement::Summary(_)))
            .count();
        let rewrite_count = group
            .candidates
            .iter()
            .filter(|c| matches!(c.replacement, Replacement::Rewrite(_)))
            .count();
        assert_eq!(summary_count, 1);
        assert_eq!(rewrite_count, 2);

        // The two Rewrite candidates must NOT have collapsed into one
        // (the "false-positive dedup" failure mode `is_duplicate_rewrite`
        // exists to prevent).
        let one_is_the_target = group.candidates.iter().any(
            |c| matches!(&c.replacement, Replacement::Rewrite(rc) if Rc::ptr_eq(rc, &group.target)),
        );
        let one_is_not = group.candidates.iter().any(|c| {
            matches!(&c.replacement, Replacement::Rewrite(rc) if !Rc::ptr_eq(rc, &group.target))
        });
        assert!(one_is_the_target && one_is_not);
    }

    #[test]
    fn nested_shared_subtree_below_an_unshared_parent_is_still_discovered() {
        // A shared grouped Aggregate nested under two *different*,
        // unshared Filter parents — real consumer_count must come from
        // walking the whole DAG, not just root-level pointer identity
        // (a naive whole-root-only consumer-count pass would miss this;
        // this module's discover_targets must not).
        use asap_types::pre_asap::expr_ir::ScalarValue;
        use asap_types::pre_asap::query_expr::Predicate;

        let shared = Rc::new(agg(
            vec![2],
            AggIntent::Sum { col: None },
            metric_scan(&["job"]),
        ));
        // Different predicates so the two Filter *parents* stay distinct
        // (don't themselves merge under CSE) — only their shared `child`
        // should collapse onto one `Rc`.
        let root_a = QueryExpr::Filter {
            pred: Predicate(Rc::new(QueryExpr::Literal(ScalarValue::Int64(1)))),
            child: Rc::clone(&shared),
        };
        let root_b = QueryExpr::Filter {
            pred: Predicate(Rc::new(QueryExpr::Literal(ScalarValue::Int64(2)))),
            child: Rc::clone(&shared),
        };

        let space = search_workload(vec![("a", Rc::new(root_a)), ("b", Rc::new(root_b))]);
        assert_eq!(
            space.len(),
            4,
            "2 distinct Filters + 1 shared Aggregate + 1 shared Scan"
        );

        // `share_common_subtrees` re-clones+re-interns anything that already
        // had more than one owner going in (see `cse.rs`'s own doc on
        // `intern_child`'s clone-fallback path) — so the post-CSE shared
        // node is a *fresh* Rc, structurally equal to (but not the same
        // pointer as) the pre-search `shared` variable. Recover it from the
        // post-CSE root's own `child` field instead of the stale `shared`
        // handle.
        let QueryExpr::Filter {
            child: post_cse_shared_a,
            ..
        } = space.roots[0].1.as_ref()
        else {
            panic!("expected a Filter root");
        };
        let QueryExpr::Filter {
            child: post_cse_shared_b,
            ..
        } = space.roots[1].1.as_ref()
        else {
            panic!("expected a Filter root");
        };
        assert!(
            Rc::ptr_eq(post_cse_shared_a, post_cse_shared_b),
            "fixture sanity: the two Filters' children must still merge"
        );
        let post_cse_shared = post_cse_shared_a;
        let group = space
            .group_for(post_cse_shared)
            .expect("shared node must be a discovered target");
        assert_eq!(group.consumer_count, 2);
        assert!(
            SharedSubtreeStrategy.matches(&TargetSubDAG::with_consumer_count(
                post_cse_shared,
                group.consumer_count
            ))
        );
    }

    // ── dedup ────────────────────────────────────────────────────────────

    #[test]
    fn add_candidate_rejects_a_true_rewrite_duplicate() {
        // SharedSubtreeStrategy's `Replacement::Rewrite` candidates are
        // real `QueryExpr` values with `PartialEq`, so `add_candidate` can
        // (and must) actually reject a genuine repeat — unlike the
        // `Replacement::Summary` case (see the test below).
        let root = Rc::new(agg(
            vec![2],
            AggIntent::Sum { col: None },
            metric_scan(&["job"]),
        ));
        let mut group = MemoGroup::new(Rc::clone(&root), 2);
        let target = TargetSubDAG::with_consumer_count(&root, 2);
        let mut inserted = 0;
        for candidate in SharedSubtreeStrategy.replacements(&target) {
            if group.add_candidate(candidate) {
                inserted += 1;
            }
        }
        assert_eq!(inserted, 2, "share + recompute-independently candidates");

        // Re-adding the identical candidate list must add nothing new: the
        // "share" candidate is literally the same Rc as before, and the
        // "recompute independently" candidate is a fresh Rc but
        // structurally identical value, both already covered by
        // `is_duplicate_rewrite`.
        let mut re_inserted = 0;
        for candidate in SharedSubtreeStrategy.replacements(&target) {
            if group.add_candidate(candidate) {
                re_inserted += 1;
            }
        }
        assert_eq!(
            re_inserted, 0,
            "re-proposing the same Rewrite candidates must not grow the group"
        );
        assert_eq!(group.candidates.len(), 2);
    }

    #[test]
    fn add_candidate_never_dedups_summary_candidates() {
        // Documented, deliberate consequence of `SummaryNode` deriving no
        // `PartialEq` (see `is_duplicate_summary`'s own doc): re-proposing
        // the same `Replacement::Summary` candidates DOES grow the group —
        // this module refuses to guess at an equality check it can't back
        // with a real `PartialEq`. `search_workload_with` never actually
        // does this in practice (every target is asked exactly once — see the
        // module docs' "Termination" section), so this test exists to pin
        // the documented behavior, not to endorse calling `replacements`
        // twice for the same target.
        let root = Rc::new(agg(vec![2], default_quantile(0.99), metric_scan(&["job"])));
        let mut group = MemoGroup::new(Rc::clone(&root), 1);
        let strategy = SketchAlgorithmStrategy::default_cost_model();
        let target = TargetSubDAG::new(&root);
        for candidate in strategy.replacements(&target) {
            group.add_candidate(candidate);
        }
        assert_eq!(group.candidates.len(), 2);

        for candidate in strategy.replacements(&target) {
            group.add_candidate(candidate);
        }
        assert_eq!(
            group.candidates.len(),
            4,
            "Summary candidates are never deduped by this module — see is_duplicate_summary"
        );
    }

    #[test]
    fn is_duplicate_rewrite_never_merges_share_with_recompute() {
        let target = Rc::new(agg(
            vec![2],
            AggIntent::Sum { col: None },
            metric_scan(&["job"]),
        ));
        let share = Rc::clone(&target);
        let recompute = Rc::new((*target).clone());
        assert!(!Rc::ptr_eq(&share, &recompute));
        assert_eq!(
            *share, *recompute,
            "fixture sanity: same value, different Rc"
        );
        assert!(!is_duplicate_rewrite(&share, &recompute, &target));
        assert!(!is_duplicate_rewrite(&recompute, &share, &target));
    }

    #[test]
    fn is_duplicate_rewrite_catches_a_real_repeat() {
        let target = Rc::new(agg(
            vec![2],
            AggIntent::Sum { col: None },
            metric_scan(&["job"]),
        ));
        let first_recompute = Rc::new((*target).clone());
        let second_recompute = Rc::new((*target).clone());
        assert!(!Rc::ptr_eq(&first_recompute, &second_recompute));
        assert!(is_duplicate_rewrite(
            &first_recompute,
            &second_recompute,
            &target
        ));
    }

    // ── cost-based ranking ───────────────────────────────────────────────

    #[test]
    fn cost_sorted_orders_shared_subtree_candidates_by_cse_share_decision() {
        // Many consumers of a cheap-to-recompute, cheap-to-maintain exact
        // accumulator: cse_share_decision should prefer Share (see
        // cost_model.rs's own `cse_share_decision_shares_when_recompute_dominates_maintenance`).
        let mut roots = Vec::new();
        let shared = agg(vec![2], AggIntent::Sum { col: None }, metric_scan(&["job"]));
        for i in 0..20 {
            roots.push((i, Rc::new(shared.clone())));
        }
        let space = search_workload(roots);
        let group = space.group_for(&space.roots[0].1).unwrap();
        assert_eq!(group.consumer_count, 20);

        let ranked = space.cost_sorted(&DefaultCostModel);
        let ranked_group = ranked
            .iter()
            .find(|g| Rc::ptr_eq(g.target, &space.roots[0].1))
            .unwrap();
        let rewrites: Vec<&ReplacementSubDAG> = ranked_group
            .candidates
            .iter()
            .filter(|c| matches!(c.replacement, Replacement::Rewrite(_)))
            .copied()
            .collect();
        assert_eq!(rewrites.len(), 2);
        let first_shares_target = match &rewrites[0].replacement {
            Replacement::Rewrite(rc) => Rc::ptr_eq(rc, &group.target),
            Replacement::Summary(_) => false,
        };
        assert!(
            first_shares_target,
            "with 20 cheap consumers, Share should rank first: {rewrites:?}"
        );
    }

    #[test]
    fn cost_sorted_orders_sketch_candidates_by_rank_candidates() {
        struct PreferDDSketch;
        impl CostModel for PreferDDSketch {
            fn rank_candidates(
                &self,
                _intent: &AggIntent,
                candidates: &[SketchAlgorithm],
            ) -> Vec<SketchAlgorithm> {
                let mut v = candidates.to_vec();
                if let Some(pos) = v.iter().position(|k| *k == SketchAlgorithm::DDSketch) {
                    let dd = v.remove(pos);
                    v.insert(0, dd);
                }
                v
            }
        }

        let root = Rc::new(agg(vec![2], default_quantile(0.99), metric_scan(&["job"])));
        let space = search_workload(vec![("q", root)]);
        let ranked = space.cost_sorted(&PreferDDSketch);
        let agg_group = ranked
            .iter()
            .find(|g| matches!(g.target.as_ref(), QueryExpr::Aggregate { .. }))
            .unwrap();
        assert_eq!(agg_group.candidates.len(), 2);
        let first_kind = match &agg_group.candidates[0].replacement {
            Replacement::Summary(node) => sketch_kind_of(node),
            Replacement::Rewrite(_) => None,
        };
        assert_eq!(first_kind, Some(SketchAlgorithm::DDSketch));
    }

    /// [`RankedGroup::costs`] is a per-candidate annotation, aligned
    /// index-for-index with `candidates` — each entry must equal what
    /// calling [`CostModel::estimate_cost`] directly on that same candidate
    /// and target produces, not some other (or stale) number.
    #[test]
    fn cost_sorted_pairs_each_candidate_with_its_own_estimate_cost() {
        let root = Rc::new(agg(vec![2], default_quantile(0.99), metric_scan(&["job"])));
        let space = search_workload(vec![("q", root)]);
        let ranked = space.cost_sorted(&DefaultCostModel);
        let agg_group = ranked
            .iter()
            .find(|g| matches!(g.target.as_ref(), QueryExpr::Aggregate { .. }))
            .unwrap();
        assert_eq!(
            agg_group.costs.len(),
            agg_group.candidates.len(),
            "costs must be aligned 1:1 with candidates"
        );
        assert!(!agg_group.costs.is_empty());

        let target = TargetSubDAG::with_consumer_count(agg_group.target, agg_group.consumer_count);
        for (candidate, &cost) in agg_group.candidates.iter().zip(&agg_group.costs) {
            assert_eq!(
                cost,
                DefaultCostModel.estimate_cost(candidate, &target),
                "RankedGroup::costs must match calling CostModel::estimate_cost directly \
                 for the same candidate/target"
            );
        }
    }

    // ── termination ──────────────────────────────────────────────────────

    #[test]
    fn default_strategies_converge_without_hitting_the_iteration_cap() {
        // A workload exercising both strategies at once; if this test
        // completes at all, the fixpoint converged well under
        // MAX_SEARCH_ITERATIONS (both strategies are idempotent — see the
        // module docs — so this always converges in exactly 2 passes).
        let a = agg(vec![2], default_quantile(0.99), metric_scan(&["job"]));
        let b = agg(vec![2], default_quantile(0.99), metric_scan(&["job"]));
        let space = search_workload(vec![("a", Rc::new(a)), ("b", Rc::new(b))]);
        assert!(!space.is_empty());
    }

    /// A deliberately ill-behaved [`ReplacementStrategy`]: every call to
    /// `replacements` wraps `target` in two `Filter` layers — the outer one
    /// (ignored by target discovery — see [`discover_new_descendant_targets`])
    /// and an inner one carrying a monotonically-increasing counter, so the
    /// inner layer is a **brand-new, never-before-seen `Rc` every call**.
    /// Each round, `search_workload_with` discovers that inner layer as a
    /// new target, processes it next round (this strategy matches
    /// everything), and gets handed *another* fresh inner layer — the
    /// frontier never empties, exactly the failure mode
    /// [`MAX_SEARCH_ITERATIONS`] exists to catch.
    struct AlwaysGrowingStrategy {
        next: std::cell::Cell<i64>,
    }

    impl ReplacementStrategy for AlwaysGrowingStrategy {
        fn matches(&self, _target: &TargetSubDAG<'_>) -> bool {
            true
        }

        fn replacements(&self, target: &TargetSubDAG<'_>) -> Vec<ReplacementSubDAG> {
            let n = self.next.get();
            self.next.set(n + 1);
            use asap_types::pre_asap::expr_ir::ScalarValue;
            use asap_types::pre_asap::query_expr::Predicate;
            let fresh_inner_layer = QueryExpr::Filter {
                pred: Predicate(Rc::new(QueryExpr::Literal(ScalarValue::Int64(n)))),
                child: Rc::clone(target.root),
            };
            let outer_wrapper = QueryExpr::Filter {
                pred: Predicate(Rc::new(QueryExpr::Literal(ScalarValue::Boolean(true)))),
                child: Rc::new(fresh_inner_layer),
            };
            vec![ReplacementSubDAG {
                replacement: Replacement::Rewrite(Rc::new(outer_wrapper)),
                rationale: format!("pathological candidate #{n}"),
            }]
        }
    }

    #[test]
    #[should_panic(expected = "did not converge")]
    fn a_pathologically_growing_strategy_trips_the_iteration_cap() {
        let root = Rc::new(metric_scan(&["job"]));
        let strategies: Vec<Box<dyn ReplacementStrategy>> = vec![Box::new(AlwaysGrowingStrategy {
            next: std::cell::Cell::new(0),
        })];
        let _ = search_workload_with(vec![("q", root)], &strategies);
    }
    // ── realize_child / keep_pre_asap: end-to-end single-target realization ──
    //
    // Moved from the former `bind.rs` (issue #251): `bind.rs`'s own
    // workload-wide orchestration (`implement_workload`/
    // `implement_workload_with`) was deleted as out of this crate's scope
    // (see the crate doc's `## Status` section), but these tests exercise
    // `construct_summary_agg`'s schema derivation end to end through
    // `realize_child` — production logic that still lives in this module —
    // so they move here rather than disappear. Unlike `bind.rs` (an
    // external caller that had to reconstruct the rank-and-take-first
    // pattern by hand since `realize_child` is `pub(crate)`), these tests
    // call `realize_child` directly.

    fn agg_per_entity(intent: AggIntent, child: QueryExpr) -> QueryExpr {
        QueryExpr::Aggregate {
            reduction: ReductionTy::PerEntity,
            measures: vec![intent],
            output_names: vec![],
            having: None,
            child: Rc::new(child),
        }
    }

    fn field<'a>(schema: &'a SummarySchema, name: &str) -> &'a SummaryField {
        schema
            .fields
            .iter()
            .find(|f| f.name == name)
            .unwrap_or_else(|| panic!("no field {name:?} in {schema:?}"))
    }

    fn realize_first(
        expr: &QueryExpr,
        cost_model: &dyn CostModel,
    ) -> Result<Rc<SummaryNode>, ImplementError> {
        realize_child(&Rc::new(expr.clone()), cost_model)
    }

    fn realize(expr: &QueryExpr) -> Result<Rc<SummaryNode>, ImplementError> {
        realize_first(expr, &DefaultCostModel)
    }

    #[test]
    fn quantile_realizes_kll_wrapped_in_estimate() {
        // quantile by (job) (m) at ε=0.01 → Estimate(Quantile) over
        // SummaryAgg(Kll{k:200}) over KeepPreAsap(Scan). job = col 2.
        let q = agg(vec![2], default_quantile(0.99), metric_scan(&["job"]));
        let root = realize(&q).unwrap();

        let SummaryExpr::SummaryEstimate {
            summary_input,
            query,
        } = &root.expr
        else {
            panic!("expected SummaryEstimate root, got {:?}", root.expr);
        };
        assert!(matches!(query, PostAsapSketchQuery::Quantile { q } if *q == 0.99));
        // Estimate edge: plain row shape — group key + Float64 answer.
        assert_eq!(
            field(&root.schema, "quantile_0_99").dtype,
            SummaryFamilyType::Plain(DataType::Float64)
        );
        assert_eq!(
            field(&root.schema, "job").dtype,
            SummaryFamilyType::Plain(DataType::Utf8)
        );

        let SummaryExpr::SummaryAgg {
            child,
            family,
            col,
            reduction,
        } = &summary_input.expr
        else {
            panic!("expected SummaryAgg, got {:?}", summary_input.expr);
        };
        assert_eq!(
            family,
            &SummaryFamilyType::Sketch(SketchKind::Quantile(
                SketchAlgorithm::Kll,
                SketchParams::Kll { k: 200 }
            ))
        );
        assert_eq!(col, &ColumnRef::SampleValue);
        assert_eq!(reduction, &ReductionTy::by(vec![2]));
        // SummaryAgg edge: the state column carries the committed family.
        assert_eq!(
            field(&summary_input.schema, "quantile_0_99").dtype,
            SummaryFamilyType::Sketch(SketchKind::Quantile(
                SketchAlgorithm::Kll,
                SketchParams::Kll { k: 200 }
            ))
        );
        assert!(matches!(child.expr, SummaryExpr::KeepPreAsap(ref e)
            if matches!(**e, QueryExpr::Scan { .. })));
    }

    /// A deployment-supplied [`CostModel`] can override the default KLL
    /// choice — `realize_first` (via `realize_child`) must actually consult
    /// it, not just accept and ignore it (issue: cost model interface, see
    /// `crate::cost_model`).
    struct PreferDDSketchViaCostModel;

    impl CostModel for PreferDDSketchViaCostModel {
        fn rank_candidates(
            &self,
            _intent: &AggIntent,
            candidates: &[SketchAlgorithm],
        ) -> Vec<SketchAlgorithm> {
            let mut v = candidates.to_vec();
            if let Some(pos) = v.iter().position(|k| *k == SketchAlgorithm::DDSketch) {
                let ddsketch = v.remove(pos);
                v.insert(0, ddsketch);
            }
            v
        }
    }

    #[test]
    fn realize_with_custom_cost_model_overrides_default_summary_choice() {
        let q = agg(vec![2], default_quantile(0.99), metric_scan(&["job"]));

        // Default: KLL (see `quantile_realizes_kll_wrapped_in_estimate` above).
        let default_root = realize(&q).unwrap();
        let SummaryExpr::SummaryEstimate { summary_input, .. } = &default_root.expr else {
            panic!("expected SummaryEstimate root, got {:?}", default_root.expr);
        };
        let SummaryExpr::SummaryAgg { family, .. } = &summary_input.expr else {
            panic!("expected SummaryAgg, got {:?}", summary_input.expr);
        };
        assert!(matches!(
            family,
            SummaryFamilyType::Sketch(SketchKind::Quantile(SketchAlgorithm::Kll, _))
        ));

        // With `PreferDDSketchViaCostModel`: DDSketch instead, same query.
        let custom_root = realize_first(&q, &PreferDDSketchViaCostModel).unwrap();
        let SummaryExpr::SummaryEstimate { summary_input, .. } = &custom_root.expr else {
            panic!("expected SummaryEstimate root, got {:?}", custom_root.expr);
        };
        let SummaryExpr::SummaryAgg { family, .. } = &summary_input.expr else {
            panic!("expected SummaryAgg, got {:?}", summary_input.expr);
        };
        assert_eq!(
            family,
            &SummaryFamilyType::Sketch(SketchKind::Quantile(
                SketchAlgorithm::DDSketch,
                SketchParams::DDSketch { alpha: 0.01 }
            ))
        );
    }

    /// A deployment-supplied `CostModel` can realize an `AggIntent::Extension`
    /// intent as a real sketch instead of the default `PassThrough` (issue
    /// #150) — `implementations_for_with` must consult `realize_extension`
    /// for the `Extension` arm, and `readout` must consult
    /// `readout_extension` to build its `SketchQuery` without panicking.
    struct FrequencyCostModel;

    impl CostModel for FrequencyCostModel {
        fn rank_candidates(
            &self,
            _intent: &AggIntent,
            candidates: &[SketchAlgorithm],
        ) -> Vec<SketchAlgorithm> {
            candidates.to_vec()
        }

        fn realize_extension(
            &self,
            ext_kind: &str,
            _payload: &serde_json::Value,
        ) -> Implementation {
            if ext_kind == "frequency" {
                Implementation::Sketch(SketchKind::Frequency(
                    SketchAlgorithm::CountSketch,
                    SketchParams::CountSketch {
                        width: 256,
                        depth: 4,
                    },
                ))
            } else {
                Implementation::PassThrough
            }
        }

        fn readout_extension(
            &self,
            ext_kind: &str,
            payload: &serde_json::Value,
            _col: &ColumnRef,
        ) -> PostAsapSketchQuery {
            assert_eq!(ext_kind, "frequency");
            let value = payload["item"].as_str().map(str::to_string);
            PostAsapSketchQuery::PointCount {
                key: ColumnRef::Named("item".into()),
                value,
            }
        }
    }

    #[test]
    fn extension_intent_stays_logical_by_default() {
        // Without a CostModel overriding `realize_extension`, an
        // `Extension` intent must stay `PassThrough` -- today's behavior,
        // unchanged.
        let intent = AggIntent::Extension {
            ext_kind: "frequency".to_string(),
            payload: serde_json::json!({ "item": "checkout" }),
        };
        let q = agg(vec![], intent, metric_scan(&[]));
        let root = realize(&q).unwrap();
        assert!(matches!(root.expr, SummaryExpr::KeepPreAsap(_)));
    }

    #[test]
    fn extension_intent_realizes_via_custom_cost_model() {
        let intent = AggIntent::Extension {
            ext_kind: "frequency".to_string(),
            payload: serde_json::json!({ "item": "checkout" }),
        };
        let q = agg(vec![], intent, metric_scan(&[]));
        let root = realize_first(&q, &FrequencyCostModel).unwrap();

        let SummaryExpr::SummaryEstimate {
            summary_input,
            query,
        } = &root.expr
        else {
            panic!("expected SummaryEstimate root, got {:?}", root.expr);
        };
        assert!(matches!(
            query,
            PostAsapSketchQuery::PointCount { key: ColumnRef::Named(k), value: Some(v) }
                if k == "item" && v == "checkout"
        ));

        let SummaryExpr::SummaryAgg { family, .. } = &summary_input.expr else {
            panic!("expected SummaryAgg, got {:?}", summary_input.expr);
        };
        assert_eq!(
            family,
            &SummaryFamilyType::Sketch(SketchKind::Frequency(
                SketchAlgorithm::CountSketch,
                SketchParams::CountSketch {
                    width: 256,
                    depth: 4
                }
            ))
        );
    }

    #[test]
    fn exact_sum_realizes_accumulator_without_estimate() {
        let q = agg(vec![2], AggIntent::Sum { col: None }, metric_scan(&["job"]));
        let root = realize(&q).unwrap();
        let SummaryExpr::SummaryAgg { family, .. } = &root.expr else {
            panic!(
                "expected bare SummaryAgg (no estimate), got {:?}",
                root.expr
            );
        };
        assert_eq!(
            family,
            &SummaryFamilyType::ExactAggregate(ExactKind::Sum, ExactParams::Sum)
        );
        assert_eq!(
            field(&root.schema, "sum").dtype,
            SummaryFamilyType::ExactAggregate(ExactKind::Sum, ExactParams::Sum)
        );
    }

    #[test]
    fn per_series_rate_keeps_labels_and_retypes_value() {
        // rate(m[5m]) — per-series: every label survives; the sample value
        // column becomes the Rate accumulator state.
        use std::time::Duration;
        let q = agg_per_entity(
            AggIntent::Rate,
            QueryExpr::TimeRange {
                range: Duration::from_secs(300),
                child: Rc::new(metric_scan(&["job"])),
            },
        );
        let root = realize(&q).unwrap();
        let SummaryExpr::SummaryAgg { family, .. } = &root.expr else {
            panic!("expected SummaryAgg, got {:?}", root.expr);
        };
        assert_eq!(
            family,
            &SummaryFamilyType::ExactAggregate(ExactKind::Rate, ExactParams::Rate)
        );
        assert_eq!(
            root.schema
                .fields
                .iter()
                .map(|f| f.name.as_str())
                .collect::<Vec<_>>(),
            vec!["ts", "value", "job"],
        );
        assert_eq!(
            field(&root.schema, "value").dtype,
            SummaryFamilyType::ExactAggregate(ExactKind::Rate, ExactParams::Rate)
        );
        assert_eq!(root.schema.time_index, Some(0));
    }

    /// Issue #163, case 1: a bare per-series range function (e.g.
    /// `quantile_over_time(...)`) realizes to `SummaryAgg { reduction:
    /// PerEntity, .. }` — proving the pre-ASAP `Reduction` this crate
    /// already computes (issue #165) is carried onto the post-ASAP node
    /// verbatim, not flattened back into an ambiguous bare `Vec<ColumnId>`.
    #[test]
    fn bare_per_series_aggregate_realizes_summary_agg_with_per_entity_reduction() {
        use std::time::Duration;
        let q = agg_per_entity(
            default_quantile(0.99),
            QueryExpr::TimeRange {
                range: Duration::from_secs(10),
                child: Rc::new(metric_scan(&["job"])),
            },
        );
        let root = realize(&q).unwrap();
        let SummaryExpr::SummaryEstimate { summary_input, .. } = &root.expr else {
            panic!("expected estimate root, got {:?}", root.expr);
        };
        let SummaryExpr::SummaryAgg { reduction, .. } = &summary_input.expr else {
            panic!("expected SummaryAgg, got {:?}", summary_input.expr);
        };
        assert_eq!(reduction, &ReductionTy::PerEntity);
    }

    /// Issue #163, case 2: an aggregation operator explicitly invoked with
    /// no `by(...)` (e.g. `count(hll_metric)`) realizes to `SummaryAgg {
    /// reduction: Reduce(vec![]), .. }` — byte-identical `by: []` to the
    /// previous test at the old `Vec<ColumnId>` shape; `reduction` is what
    /// tells them apart now.
    #[test]
    fn explicit_empty_by_aggregate_realizes_summary_agg_with_reduce_reduction() {
        let intent = AggIntent::Cardinality {
            col: None,
            accuracy: AccuracyTarget::Epsilon(0.01),
        };
        let q = agg(vec![], intent, metric_scan(&["job"]));
        let root = realize(&q).unwrap();
        let SummaryExpr::SummaryEstimate { summary_input, .. } = &root.expr else {
            panic!("expected estimate root, got {:?}", root.expr);
        };
        let SummaryExpr::SummaryAgg { reduction, .. } = &summary_input.expr else {
            panic!("expected SummaryAgg, got {:?}", summary_input.expr);
        };
        assert_eq!(reduction, &ReductionTy::by(vec![]));
    }

    #[test]
    fn nested_aggregates_realize_per_node() {
        // quantile(0.9, sum by (job) (m)) — the implementation decision
        // fires per node over the nested tree: KLL over an exact Sum
        // accumulator.
        let inner = agg(vec![2], AggIntent::Sum { col: None }, metric_scan(&["job"]));
        let outer = agg(vec![], default_quantile(0.9), inner);
        let root = realize(&outer).unwrap();

        let SummaryExpr::SummaryEstimate { summary_input, .. } = &root.expr else {
            panic!("expected estimate root, got {:?}", root.expr);
        };
        let SummaryExpr::SummaryAgg { child, family, .. } = &summary_input.expr else {
            panic!("expected outer SummaryAgg, got {:?}", summary_input.expr);
        };
        assert!(matches!(
            family,
            SummaryFamilyType::Sketch(SketchKind::Quantile(SketchAlgorithm::Kll, _))
        ));
        let SummaryExpr::SummaryAgg {
            family: inner_family,
            child: leaf,
            ..
        } = &child.expr
        else {
            panic!("expected inner SummaryAgg, got {:?}", child.expr);
        };
        assert_eq!(
            inner_family,
            &SummaryFamilyType::ExactAggregate(ExactKind::Sum, ExactParams::Sum)
        );
        assert!(matches!(leaf.expr, SummaryExpr::KeepPreAsap(_)));
    }

    /// Issue #115: the summary is built over the intent's own input column.
    /// Before `Cardinality`/`Quantile` carried `col`, `summarised_column` always
    /// fell through to `ColumnRef::SampleValue`, so an HLL was built over the
    /// wrong column for every SQL `COUNT(DISTINCT c)`.
    #[test]
    fn sketch_realizes_over_the_intents_input_column() {
        // `metric_scan(&["job"])` → columns [ts=0, value=1, job=2].
        let cases = [
            (Some(2), ColumnRef::Named("job".into())),
            (Some(1), ColumnRef::Named("value".into())),
            // PromQL convention: no column ⇒ the synthetic sample value.
            (None, ColumnRef::SampleValue),
        ];
        for (col, want) in cases {
            let intent = AggIntent::Cardinality {
                col,
                accuracy: AccuracyTarget::Epsilon(0.01),
            };
            let root = realize(&agg(vec![0], intent, metric_scan(&["job"]))).unwrap();
            let bound = find_summary_col(&root)
                .unwrap_or_else(|| panic!("expected a SummaryAgg for col={col:?}"));
            assert_eq!(bound, want, "wrong summarised column for col={col:?}");
        }
    }

    /// The `col` of the first `SummaryAgg` in the tree.
    fn find_summary_col(node: &SummaryNode) -> Option<ColumnRef> {
        match &node.expr {
            SummaryExpr::SummaryAgg { col, .. } => Some(col.clone()),
            SummaryExpr::SummaryEstimate { summary_input, .. } => find_summary_col(summary_input),
            _ => None,
        }
    }

    #[test]
    fn pass_through_intents_stay_logical() {
        // avg is exact but non-mergeable; histogram_quantile (classic
        // buckets, #79) is never sketchable; exact quantile is exact by
        // decree. All three stay whole logical subtrees.
        for intent in [
            AggIntent::Avg { col: None },
            AggIntent::HistogramQuantile { q: 0.99 },
            AggIntent::Quantile {
                col: None,
                q: 0.99,
                accuracy: AccuracyTarget::Exact,
            },
        ] {
            let q = agg(vec![2], intent.clone(), metric_scan(&["job"]));
            let root = realize(&q).unwrap();
            assert!(
                matches!(root.expr, SummaryExpr::KeepPreAsap(ref e) if **e == q),
                "expected KeepPreAsap passthrough for {intent:?}"
            );
        }
    }

    #[test]
    fn logical_parent_subsumes_bindable_child() {
        // Filter over a bindable quantile: `KeepPreAsap` has no post-ASAP
        // children, so the conservative fallback keeps the whole subtree
        // logical.
        use asap_types::pre_asap::expr_ir::{CompareOpKind, ScalarValue};
        use asap_types::pre_asap::query_expr::Predicate;
        let q = QueryExpr::Filter {
            pred: Predicate(Rc::new(QueryExpr::Compare {
                left: Rc::new(QueryExpr::Column(0)),
                op: CompareOpKind::Gt,
                right: Rc::new(QueryExpr::Literal(ScalarValue::Float64(0.5))),
            })),
            child: Rc::new(agg(vec![], default_quantile(0.99), metric_scan(&[]))),
        };
        let root = realize(&q).unwrap();
        assert!(matches!(root.expr, SummaryExpr::KeepPreAsap(ref e) if **e == q));
    }

    #[test]
    fn having_and_multi_intent_stay_logical() {
        use asap_types::pre_asap::expr_ir::ScalarValue;
        use asap_types::pre_asap::query_expr::Predicate;
        let mut q = agg(vec![2], default_quantile(0.99), metric_scan(&["job"]));
        if let QueryExpr::Aggregate { having, .. } = &mut q {
            *having = Some(Predicate(Rc::new(QueryExpr::Literal(
                ScalarValue::Boolean(true),
            ))));
        }
        assert!(matches!(
            realize(&q).unwrap().expr,
            SummaryExpr::KeepPreAsap(_)
        ));

        let multi = QueryExpr::Aggregate {
            reduction: ReductionTy::by(vec![2]),
            measures: vec![AggIntent::Sum { col: None }, AggIntent::Avg { col: None }],
            output_names: vec![],
            having: None,
            child: Rc::new(metric_scan(&["job"])),
        };
        assert!(matches!(
            realize(&multi).unwrap().expr,
            SummaryExpr::KeepPreAsap(_)
        ));
    }

    #[test]
    fn topk_realizes_cms_with_heap_and_topk_readout() {
        let q = agg(
            vec![2],
            AggIntent::TopK {
                k: 5,
                accuracy: AccuracyTarget::Epsilon(0.01),
            },
            metric_scan(&["job"]),
        );
        let root = realize(&q).unwrap();
        let SummaryExpr::SummaryEstimate {
            summary_input,
            query,
        } = &root.expr
        else {
            panic!("expected estimate root, got {:?}", root.expr);
        };
        assert!(matches!(query, PostAsapSketchQuery::TopK { k: 5 }));
        assert!(matches!(
            &summary_input.expr,
            SummaryExpr::SummaryAgg {
                family: SummaryFamilyType::Sketch(SketchKind::TopK(
                    SketchAlgorithm::CmsWithHeap,
                    _
                )),
                ..
            }
        ));
    }

    #[test]
    fn sql_reducer_resolves_named_input_column() {
        // SUM(bytes) over a tabular scan: `col` resolves positionally to the
        // named column, not the PromQL sample value.
        let scan = QueryExpr::Scan {
            source: Source::Table {
                table_ref: "t".into(),
            },
            predicates: vec![],
            schema: SchemaTy {
                columns: vec![
                    Column::new("host", DataType::Utf8, false),
                    Column::new("bytes", DataType::Int64, false),
                ],
                time_index: None,
                unique_keys: vec![],
                closed: true,
            },
        };
        let q = agg(vec![0], AggIntent::Sum { col: Some(1) }, scan);
        let root = realize(&q).unwrap();
        let SummaryExpr::SummaryAgg { col, .. } = &root.expr else {
            panic!("expected SummaryAgg, got {:?}", root.expr);
        };
        assert_eq!(col, &ColumnRef::Named("bytes".into()));
    }
}

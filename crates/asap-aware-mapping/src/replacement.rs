//! `TargetSubDAG` / `ReplacementSubDAG` / `ReplacementStrategy` — the
//! candidate-replacement vocabulary `docs/design_docs/asap_aware_mapping.md` stubs out
//! under "Key concepts (not yet implemented)", implemented for real (issue
//! #251, part of #33).
//!
//! ## One step, not two: `SketchFamilyStrategy::replacements()` decides *and* builds
//!
//! For a bindable `Aggregate`, `SketchFamilyStrategy::replacements()` is the
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
//!    [`crate::bind::select_and_bind`], so a nested aggregate gets its own
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
//!   crate (and the same shape issue #33's applicability-rule framework, PR
//!   #247, uses for its own `ApplicabilityRule`): a new replacement source is
//!   a new `impl ReplacementStrategy`, not a restructuring of this trait or of
//!   any existing strategy. `replacements` is **exhaustive, not ranked, not
//!   filtered** — reporting "every valid candidate" is core's job; picking
//!   the best one is left to the caller.
//!
//! A caller that wants one executable answer takes the first
//! (`cost_model`-preferred) entry off `replacements()` itself
//! (`.into_iter().next()`) — that "keep the head" step lives entirely on the
//! calling side, not behind a second module-level entry point. `bind.rs`'s
//! own [`crate::bind::implement_workload`]/[`crate::bind::implement_workload_with`]
//! are the one place inside this crate that still perform that take-first
//! step internally, because workload-wide CSE memoization needs one
//! canonical decision per shared root to key sharing on (see `bind.rs`'s own
//! module docs). Every other caller goes through
//! `SketchFamilyStrategy::replacements` directly and decides for itself.
//!
//! This means an ordinary single-target bind sizes and fully constructs
//! *every* sketch candidate at every sketch-capable node (not just the one a
//! caller keeps) — a deliberate tradeoff, made so there is exactly one place
//! in this crate that decides what an `AggIntent` may become, at the cost of
//! extra work per bind proportional to each node's own candidate count.
//!
//! ## The two strategies, and why these two
//!
//! - [`SketchFamilyStrategy`] wraps [`implementations_for_with`]'s exhaustive,
//!   ranked list directly: for the same bindable-`Aggregate` shape this crate
//!   binds (single intent, no `HAVING`), every entry becomes its own bound
//!   candidate.
//! - [`SharedSubtreeStrategy`] wraps
//!   `asap_types::pre_asap::cse::share_common_subtrees`'s sharing decision.
//!   Wherever a [`TargetSubDAG`] already has two or more consumers (i.e.
//!   `share_common_subtrees` already collapsed two or more workload
//!   locations onto the same `Rc<QueryExpr>` — PR #247's own
//!   `SharedSubexpressionRule` traversal/dedup logic, over in the
//!   applicability-rule framework, does the identical discovery; this
//!   module's own tests reuse the same dedup logic to build realistic
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
//! - **No search/selection-across-a-whole-plan logic.** `docs/design_docs/asap_aware_mapping.md`'s
//!   replacement-plan-searching pseudocode — trying every `ReplacementStrategy`
//!   against every candidate plan, deduplicating, iterating to a fixpoint,
//!   then ranking by a `CostModel` — is a Cascades/Volcano-style search
//!   engine, tracked as a separate follow-up. Taking the first candidate off
//!   [`SketchFamilyStrategy::replacements`] is a single-node stand-in for
//!   that, not the real thing: it never compares whole candidate *plans*,
//!   only one node's own candidates against each other via
//!   `cost_model.rank_candidates`.
//! - **No workload-wide `TargetSubDAG` discovery pass.** Finding every
//!   candidate node in a whole workload (walking every root, deduplicating by
//!   `Rc` identity, computing real consumer counts) is exactly what PR #247's
//!   `SharedSubexpressionRule`/`SketchApplicabilityRule` traversals already
//!   do — reusable, but wiring it up to feed this module's strategies
//!   automatically is part of the same future search engine, not this issue.
//!   This module's own tests build `TargetSubDAG`s directly, the same
//!   hand-rolled-fixture style `bind.rs`/`cost_model.rs`'s own tests already
//!   use.
//! - **[`implementations_for_with`]'s own outward-facing behavior is
//!   unchanged.** Same inputs still produce the same exhaustive, ranked
//!   list — only its home moved (from a separate `implementation` module
//!   into this one) and its own visibility dropped to module-private, since
//!   [`SketchFamilyStrategy`] is now its only caller.

use asap_types::post_asap::{
    ExactKind, ExactParams, SamplingKind, SamplingParams, SketchKind, SketchParams,
    SketchQuery as PostAsapSketchQuery, StatModelKind, StatModelParams, SummaryExpr,
    SummaryFamilyType, SummaryField, SummaryNode, SummarySchema, WaveletKind, WaveletParams,
};
use asap_types::pre_asap::agg_intent::{agg_is_mergeable, AggIntent};
use asap_types::pre_asap::expr_ir::ColumnRef;
use asap_types::pre_asap::query_expr::{QueryExpr, Reduction};
use asap_types::pre_asap::schema::Schema;
use asap_types::types::AccuracyTarget;
use std::rc::Rc;

use crate::bind::{select_and_bind, ImplementError};
use crate::cost_model::{CostModel, DefaultCostModel};

/// A pre-ASAP sub-DAG a [`ReplacementStrategy`] knows how to replace.
///
/// `root` is a reference into the workload's own [`QueryExpr`] tree (an
/// `Rc<QueryExpr>`, the same currency [`crate::bind::implement_workload`] and
/// `asap_types::pre_asap::cse::share_common_subtrees` already thread through
/// this crate's public API — not a bare `&QueryExpr` — so a strategy that
/// needs the node's own `Rc` identity, not just its shape, has it available
/// without the caller re-deriving it).
///
/// `consumer_count` is how many locations across the workload already
/// reference this exact `Rc` — 1 for an ordinary single-use node, 2+ when the
/// caller already ran `share_common_subtrees` and found this subtree shared.
/// A strategy that only cares about `root`'s own shape (e.g.
/// [`SketchFamilyStrategy`]) can ignore it entirely; [`SharedSubtreeStrategy`]
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
/// the same role an `ApplicabilityFinding`'s `reason` field plays for
/// applicability findings, over in PR #247's applicability-rule framework).
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
/// the same target; both [`SketchFamilyStrategy`] and [`SharedSubtreeStrategy`]
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
/// independently of that list. [`SketchFamilyStrategy`] is the sole
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
    /// Needs a `SummaryEstimate` readout to recover a value.
    Sketch {
        kind: SketchKind,
        params: SketchParams,
    },
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
/// This is the `AggIntent → SketchKind` map of issue #98;
/// [`implementations_for_with`] sizes and ranks every entry via `cost_model`.
/// Listed here so the candidate set has one home.
pub fn summary_candidates(intent: &AggIntent) -> &'static [SketchKind] {
    match intent {
        AggIntent::Quantile { .. } => &[SketchKind::Kll, SketchKind::DDSketch],
        AggIntent::Cardinality { .. } => &[SketchKind::Hll, SketchKind::Theta, SketchKind::Kmv],
        // Count-Sketch-with-heap is CMS-with-heap's balanced/zero-mean-error
        // alternative for the same heavy-hitter shape.
        AggIntent::TopK { .. } => &[SketchKind::CmsWithHeap, SketchKind::CountSketchWithHeap],
        AggIntent::Count { .. } => &[SketchKind::Cms, SketchKind::CountSketch],
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
/// [`SketchFamilyStrategy`] keeps every entry as a candidate, and a caller
/// that wants a single executable answer takes the head of *that* strategy's
/// output itself.
///
/// Exhaustive over the [`AggIntent`] vocabulary — adding a variant without an
/// explicit realization is a compile error, and the coverage-matrix test pins
/// each variant's category.
///
/// Module-private: [`SketchFamilyStrategy::replacements`] is the only
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
        .map(|kind| {
            let params = cost_model.size_params(kind.clone(), intent, eps, delta);
            Implementation::Sketch { kind, params }
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
    kind: SketchKind,
    intent: &AggIntent,
    eps: f64,
    delta: f64,
) -> SketchParams {
    match kind {
        SketchKind::Kll => SketchParams::Kll { k: kll_k(eps) },
        SketchKind::Cms => SketchParams::Cms {
            width: cms_width(eps),
            depth: cms_depth(delta),
        },
        SketchKind::Hll => SketchParams::Hll {
            precision: hll_precision(eps),
        },
        SketchKind::CmsWithHeap => {
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
        SketchKind::DDSketch => SketchParams::DDSketch { alpha: eps },
        SketchKind::Theta => SketchParams::Theta { k: kmv_k(eps) },
        SketchKind::Kmv => SketchParams::Kmv { k: kmv_k(eps) },
        // Count-Sketch is CMS's balanced/zero-mean-error alternative —
        // same (width, depth) shape, sized the same way for now (a
        // Count-Sketch-specific bound uses an L2-norm error guarantee
        // rather than CMS's L1-norm one; this is a placeholder pending
        // that refinement, same status as the other non-preferred
        // candidates above).
        SketchKind::CountSketch => SketchParams::CountSketch {
            width: cms_width(eps),
            depth: cms_depth(delta),
        },
        SketchKind::CountSketchWithHeap => {
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
/// For every `SketchKind` outside the CMS family, this is identical to
/// [`default_size_params`] — `width_relaxation` only ever touches the
/// [`cms_width`]-sized formulas this issue is about.
///
/// [`default_size_params`]'s own behavior is completely unchanged by this
/// function's existence — this is a separate, additive entry point, never
/// called from [`default_size_params`] or [`implementations_for_with`].
pub fn posterior_aware_size_params(
    kind: SketchKind,
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
        SketchKind::Cms => SketchParams::Cms {
            width: relaxed_width(eps),
            depth: cms_depth(delta),
        },
        SketchKind::CmsWithHeap => {
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
        SketchKind::CountSketch => SketchParams::CountSketch {
            width: relaxed_width(eps),
            depth: cms_depth(delta),
        },
        SketchKind::CountSketchWithHeap => {
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
        // than a wildcard arm: a future `SketchKind` variant then fails to
        // compile *here* too, instead of silently inheriting worst-case
        // sizing with no signal that this function never considered it.
        SketchKind::Kll => default_size_params(kind, intent, eps, delta),
        SketchKind::Hll => default_size_params(kind, intent, eps, delta),
        SketchKind::DDSketch => default_size_params(kind, intent, eps, delta),
        SketchKind::Theta => default_size_params(kind, intent, eps, delta),
        SketchKind::Kmv => default_size_params(kind, intent, eps, delta),
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

// ── SketchFamilyStrategy ─────────────────────────────────────────────────

/// A single static instance so [`SketchFamilyStrategy::default_cost_model`]
/// can hand out a `&'static dyn CostModel` without heap-allocating one —
/// `DefaultCostModel` is a unit struct with no state, so one instance serves
/// every caller (same pattern `applicability::SketchApplicabilityRule` uses).
static DEFAULT_COST_MODEL: DefaultCostModel = DefaultCostModel;

/// Wraps [`implementations_for_with`]'s exhaustive, ranked list directly: for
/// a bindable `Aggregate`, every valid candidate summary realization as its
/// own [`ReplacementSubDAG`].
///
/// Ranked (only to *order the enumeration*, never to drop a candidate) via a
/// [`CostModel`] — [`DefaultCostModel`] unless constructed with
/// [`SketchFamilyStrategy::new`] — so a deployment-specific cost model's
/// other hooks (`size_params`, `realize_extension`, `readout_extension`) are
/// still consulted while binding each candidate.
pub struct SketchFamilyStrategy<'a> {
    cost_model: &'a dyn CostModel,
}

impl SketchFamilyStrategy<'static> {
    /// A strategy that ranks/binds via the built-in [`DefaultCostModel`] —
    /// what a deployment gets with no custom cost model plugged in.
    pub fn default_cost_model() -> Self {
        Self {
            cost_model: &DEFAULT_COST_MODEL,
        }
    }
}

impl<'a> SketchFamilyStrategy<'a> {
    /// A strategy that ranks/binds via `cost_model` instead of the built-in
    /// static preference order — the same customization point
    /// [`implementations_for_with`] already offers.
    pub fn new(cost_model: &'a dyn CostModel) -> Self {
        Self { cost_model }
    }
}

impl ReplacementStrategy for SketchFamilyStrategy<'_> {
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
        Implementation::Sketch { kind, .. } => format!(
            "{} realizes as a {kind:?} sketch — one of summary_candidates' \
             alternatives for this intent (asap_aware_mapping::replacement::implementations_for_with)",
            describe_intent(intent)
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
/// future intent to be named here too (same rationale, and same shape, as
/// `applicability`'s own private `describe_intent`).
fn describe_intent(intent: &AggIntent) -> String {
    match intent {
        AggIntent::Quantile { q, .. } => format!("quantile(q={q})"),
        AggIntent::Cardinality { .. } => "cardinality (distinct count)".to_string(),
        AggIntent::TopK { k, .. } => format!("top-{k} heavy-hitters"),
        AggIntent::Count { .. } => "count".to_string(),
        other => format!("{other:?}"),
    }
}

// ── Construction: turn one already-decided Implementation into a SummaryNode ─

/// The bindable shape [`SketchFamilyStrategy`] targets: a single intent, no
/// `HAVING`. A multi-intent node (SQL `SELECT SUM(a), AVG(b)`), or one with a
/// `HAVING` predicate (the filter would need the estimate first), stays
/// logical — see the module docs' "Conservative fallbacks" in `bind.rs`.
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
/// [`SketchFamilyStrategy::replacements`], called once per candidate that
/// method enumerates.
///
/// `expr` must still be the [`bindable_intent`] shape for `implementation` to
/// have any effect; anything else falls back to [`crate::bind::keep_pre_asap`].
/// Only `expr`'s own top-level decision is forced — recursion into `expr`'s
/// child goes back through [`crate::bind::select_and_bind`] (fresh candidate
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
    crate::bind::keep_pre_asap(expr)
}

/// Translate an [`Implementation`] into the `(family, needs a
/// SummaryEstimate readout)` pair [`construct_summary_agg`] needs, or `None`
/// for `PassThrough` (the caller falls back to [`crate::bind::keep_pre_asap`]).
///
/// Every family's partial state needs a readout to recover a value, except
/// `ExactAggregate` — its partial state *is* the value already, so no
/// estimate step follows it.
fn summary_family(implementation: Implementation) -> Option<(SummaryFamilyType, bool)> {
    Some(match implementation {
        Implementation::ExactAggregate { kind, params } => {
            (SummaryFamilyType::ExactAggregate(kind, params), false)
        }
        Implementation::Sketch { kind, params } => (SummaryFamilyType::Sketch(kind, params), true),
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
            child: select_and_bind(child, cost_model)?,
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
/// [`crate::bind::keep_pre_asap`] (`pub(crate)` for that cross-module reuse).
pub(crate) fn lift(schema: &Schema) -> SummarySchema {
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
/// (legality-gated, `PartialEq`-checked) call; PR #247's
/// `SharedSubexpressionRule` traversal discovers real consumer counts across
/// a workload the same way (this module's own tests reuse the identical
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
        Sketch(SketchKind),
        Acc(ExactKind),
        Pass,
    }

    fn cat(intent: &AggIntent) -> Cat {
        match preferred(intent) {
            Implementation::ExactAggregate { kind, .. } => Cat::Acc(kind),
            Implementation::Sketch { kind, .. } => Cat::Sketch(kind),
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
        use SketchKind as K;
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
            Implementation::Sketch {
                kind: SketchKind::Kll,
                params: SketchParams::Kll { k: 200 }, // design.md worked example
            }
        );

        let looser = AggIntent::Quantile {
            col: None,
            q: 0.99,
            accuracy: eps(0.05),
        };
        assert_eq!(
            preferred(&looser),
            Implementation::Sketch {
                kind: SketchKind::Kll,
                params: SketchParams::Kll { k: 40 }, // ⌈2/0.05⌉
            }
        );
    }

    #[test]
    fn default_cardinality_inverts_to_hll_precision_14() {
        // `default_cardinality` encodes HLL's standard error at p=14; the
        // sizing must invert it back exactly.
        assert_eq!(
            preferred(&default_cardinality()),
            Implementation::Sketch {
                kind: SketchKind::Hll,
                params: SketchParams::Hll { precision: 14 },
            }
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
            Implementation::Sketch {
                kind: SketchKind::Cms,
                params: SketchParams::Cms {
                    width: 2719,
                    depth: 7
                }, // ⌈e/0.001⌉, ⌈ln 1000⌉
            }
        );
        // Epsilon-only falls back to DEFAULT_DELTA → depth 5.
        let intent = AggIntent::Count {
            accuracy: eps(0.001),
        };
        assert_eq!(
            preferred(&intent),
            Implementation::Sketch {
                kind: SketchKind::Cms,
                params: SketchParams::Cms {
                    width: 2719,
                    depth: 5
                },
            }
        );
    }

    #[test]
    fn topk_heap_size_tracks_k() {
        let intent = AggIntent::TopK {
            k: 25,
            accuracy: eps(0.01),
        };
        match preferred(&intent) {
            Implementation::Sketch {
                kind: SketchKind::CmsWithHeap,
                params:
                    SketchParams::CmsWithHeap {
                        width,
                        depth,
                        heap_size,
                    },
            } => {
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
            &[SketchKind::Kll, SketchKind::DDSketch]
        );
        assert_eq!(
            summary_candidates(&default_cardinality()),
            &[SketchKind::Hll, SketchKind::Theta, SketchKind::Kmv]
        );
        assert_eq!(
            summary_candidates(&AggIntent::TopK {
                k: 5,
                accuracy: eps(0.01)
            }),
            &[SketchKind::CmsWithHeap, SketchKind::CountSketchWithHeap]
        );
        assert_eq!(
            summary_candidates(&AggIntent::Count {
                accuracy: eps(0.01)
            }),
            &[SketchKind::Cms, SketchKind::CountSketch]
        );
        assert!(summary_candidates(&AggIntent::Rate).is_empty());
    }

    #[test]
    fn implementations_for_with_enumerates_every_candidate_ranked() {
        // Quantile's candidate list is [Kll, DDSketch] — implementations_for_with
        // must return both, ranked with the DefaultCostModel's preferred
        // (Kll) first.
        let kinds: Vec<SketchKind> =
            implementations_for_with(&default_quantile(0.99), &DefaultCostModel)
                .into_iter()
                .map(|implementation| match implementation {
                    Implementation::Sketch { kind, .. } => kind,
                    other => panic!("expected Sketch, got {other:?}"),
                })
                .collect();
        assert_eq!(kinds, vec![SketchKind::Kll, SketchKind::DDSketch]);
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
            Implementation::Sketch {
                kind: SketchKind::Kll,
                params: SketchParams::Kll { k: 65_535 },
            }
        );
    }

    // ── posterior_aware_size_params (issue #239, integration point 2) ──────

    fn count_intent(e: f64) -> AggIntent {
        AggIntent::Count { accuracy: eps(e) }
    }

    #[test]
    fn posterior_aware_sizing_shrinks_width_under_stated_assumption() {
        let intent = count_intent(0.01);
        let worst_case = default_size_params(SketchKind::Cms, &intent, 0.01, 0.01);
        let relaxed = posterior_aware_size_params(
            SketchKind::Cms,
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
        let worst_case = default_size_params(SketchKind::Cms, &intent, 0.01, 0.01);
        let relaxed = posterior_aware_size_params(
            SketchKind::Cms,
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
        let worst_case = default_size_params(SketchKind::Cms, &intent, 0.01, 0.01);
        for bad in [0.0, -0.5, 1.5, f64::NAN, f64::INFINITY] {
            let relaxed = posterior_aware_size_params(
                SketchKind::Cms,
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
                SketchKind::CountSketch,
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
            SketchKind::CmsWithHeap,
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
            posterior_aware_size_params(SketchKind::Kll, &intent, 0.01, 0.01, assumption),
            default_size_params(SketchKind::Kll, &intent, 0.01, 0.01),
        );
    }

    #[test]
    fn default_size_params_unchanged_by_new_function_existing() {
        // Regression pin: default_size_params's own worst-case behavior for
        // existing callers must be untouched by adding
        // posterior_aware_size_params alongside it.
        assert_eq!(
            default_size_params(SketchKind::Cms, &count_intent(0.001), 0.001, 0.001),
            SketchParams::Cms {
                width: 2719,
                depth: 7
            },
        );
    }

    // ── SketchFamilyStrategy / SharedSubtreeStrategy fixtures ───────────

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

    // ── SketchFamilyStrategy ─────────────────────────────────────────────

    #[test]
    fn matches_a_bindable_aggregate() {
        let q = Rc::new(agg(vec![2], default_quantile(0.99), metric_scan(&["job"])));
        let target = TargetSubDAG::new(&q);
        assert!(SketchFamilyStrategy::default_cost_model().matches(&target));
    }

    #[test]
    fn does_not_match_a_multi_intent_or_having_aggregate() {
        let strategy = SketchFamilyStrategy::default_cost_model();

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
        assert!(!SketchFamilyStrategy::default_cost_model().matches(&target));
        assert!(SketchFamilyStrategy::default_cost_model()
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
        let replacements = SketchFamilyStrategy::default_cost_model().replacements(&target);
        assert_eq!(
            replacements.len(),
            2,
            "expected 2 candidates, got {replacements:?}"
        );

        let kinds: Vec<SketchKind> = replacements
            .iter()
            .map(|r| match &r.replacement {
                Replacement::Summary(node) => summary_family_kind(node),
                Replacement::Rewrite(_) => panic!("expected a Summary replacement"),
            })
            .collect();
        assert!(kinds.contains(&SketchKind::Kll), "{kinds:?}");
        assert!(kinds.contains(&SketchKind::DDSketch), "{kinds:?}");
        assert!(
            replacements.iter().all(|r| !r.rationale.is_empty()),
            "every candidate must carry a rationale"
        );
    }

    #[test]
    fn cardinality_enumerates_all_three_summary_candidates() {
        let q = Rc::new(agg(vec![2], default_cardinality(), metric_scan(&["job"])));
        let target = TargetSubDAG::new(&q);
        let replacements = SketchFamilyStrategy::default_cost_model().replacements(&target);
        let kinds: Vec<SketchKind> = replacements
            .iter()
            .map(|r| match &r.replacement {
                Replacement::Summary(node) => summary_family_kind(node),
                Replacement::Rewrite(_) => panic!("expected a Summary replacement"),
            })
            .collect();
        assert_eq!(
            kinds,
            vec![SketchKind::Hll, SketchKind::Theta, SketchKind::Kmv],
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
        let replacements = SketchFamilyStrategy::default_cost_model().replacements(&target);
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
        let replacements = SketchFamilyStrategy::default_cost_model().replacements(&target);
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
            candidates: &[SketchKind],
        ) -> Vec<SketchKind> {
            let mut v = candidates.to_vec();
            if let Some(pos) = v.iter().position(|k| *k == SketchKind::DDSketch) {
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
        let replacements = SketchFamilyStrategy::new(&custom).replacements(&target);
        let kinds: Vec<SketchKind> = replacements
            .iter()
            .map(|r| match &r.replacement {
                Replacement::Summary(node) => summary_family_kind(node),
                Replacement::Rewrite(_) => panic!("expected a Summary replacement"),
            })
            .collect();
        assert!(kinds.contains(&SketchKind::Kll));
        assert!(kinds.contains(&SketchKind::DDSketch));
        assert_eq!(kinds.len(), 2);
    }

    /// Enumerating candidates for the *target* node must only steer that
    /// node's own decision — a nested aggregate underneath it still gets its
    /// own independent (`cost_model`-ranked) enumeration, not whatever the
    /// caller happened to pick for the outer target. This is the behavior
    /// [`construct_summary`]'s recursion (via `crate::bind::select_and_bind`)
    /// gets for free: only the top node's `Implementation` is ever forced
    /// from outside; the child is always re-enumerated fresh.
    #[test]
    fn enumerating_the_targets_candidates_does_not_leak_into_a_nested_aggregate() {
        // outer: quantile(0.99, ...) over inner: quantile(0.5, m) — both
        // Quantile, so both share the [Kll, DDSketch] candidate list.
        let inner = agg(vec![2], default_quantile(0.5), metric_scan(&["job"]));
        let outer = Rc::new(agg(vec![], default_quantile(0.99), inner));
        let target = TargetSubDAG::new(&outer);
        let replacements = SketchFamilyStrategy::default_cost_model().replacements(&target);

        let ddsketch = replacements
            .iter()
            .find(|r| {
                matches!(&r.replacement, Replacement::Summary(node)
                    if summary_family_kind(node) == SketchKind::DDSketch)
            })
            .expect("the outer target's DDSketch candidate must be present");
        let Replacement::Summary(node) = &ddsketch.replacement else {
            unreachable!("filtered on Replacement::Summary above");
        };
        assert_eq!(
            summary_family_kind(node),
            SketchKind::DDSketch,
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
            summary_family_kind(child),
            SketchKind::Kll,
            "the nested inner aggregate must still get the cost-model-ranked \
             default (Kll), not inherit the outer target's DDSketch candidate"
        );
    }

    /// The `SummaryFamilyType`'s `SketchKind`, from the top `SummaryAgg`
    /// reachable under a (possibly `SummaryEstimate`-wrapped) bound root.
    fn summary_family_kind(node: &SummaryNode) -> SketchKind {
        match &node.expr {
            asap_types::post_asap::SummaryExpr::SummaryEstimate { summary_input, .. } => {
                summary_family_kind(summary_input)
            }
            asap_types::post_asap::SummaryExpr::SummaryAgg { family, .. } => match family {
                asap_types::post_asap::SummaryFamilyType::Sketch(kind, _) => kind.clone(),
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

    /// Builds realistic multi-consumer `TargetSubDAG`s the same way
    /// `applicability::SharedSubexpressionRule`'s `register_site`/
    /// `walk_rc_children` traversal does: dedup by `Rc::as_ptr`, walking
    /// only the relational-skeleton operator children
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
}

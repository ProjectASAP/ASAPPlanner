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
//!   [`crate::explanation`] (issue #257) is this trait's own downstream
//!   consumer, not a second extension point: it explains why a replacement
//!   exists as a pure view over the candidates strategies registered here
//!   already produced, rather than re-deriving that explanation with a rule
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
//! no principled bound on strategy-generated descendant sites,
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
//!
//! ## Whole-plan (cross-group) selection — issue #271
//!
//! [`PlanSpace::cost_sorted`] above ranks every group's candidates
//! independently: it never lets one group's choice influence how another
//! group is costed. That's the right behavior when groups genuinely don't
//! interact — which both shipped strategies' one-round convergence (see
//! "Termination" above) makes the common case — but it's the wrong answer
//! whenever they do. Concretely: [`CostModel::cse_share_decision`] costs a
//! [`SharedSubtreeStrategy`] group by comparing a `consumer_count`-scaled
//! recompute cost against a fixed maintenance cost — but a **nested**
//! `SharedSubtreeStrategy` group's *true* recompute burden isn't its own
//! raw [`MemoGroup::consumer_count`] (how many operator-child positions
//! directly reference it) whenever an ancestor on the path to it is
//! *itself* being recomputed independently rather than shared: recomputing
//! that ancestor independently at each of *its own* uses recomputes
//! everything underneath it that many times too, even though nothing
//! underneath gained a single new direct reference. `cost_sorted`'s
//! per-group ranking has no way to see this — it only ever looks at one
//! group's own `candidates`, in isolation.
//!
//! [`PlanSpace::global_selection`] is that missing step: a single
//! **top-down dynamic-programming pass** over the discovered sites,
//! processed in the topological order [`topological_order`] computes over a
//! small [`ReferenceGraph`] built for exactly this purpose (parent before
//! every child, so a site's `effective_consumer_count` is always computed
//! from *already-decided* ancestors). For every site it computes the
//! **effective consumer count** — how many times that site actually runs
//! once every ancestor's own selected candidate is accounted for — and, for
//! every [`SharedSubtreeStrategy`]-shaped group, re-decides
//! [`CostModel::cse_share_decision`] against *that* corrected count instead
//! of the group's raw structural one. When that group also contains a
//! non-CSE alternative such as a semantic rewrite, the chosen CSE candidate
//! and the cheapest non-CSE candidate additionally compete through
//! [`CostModel::estimate_cost`]; the CSE pair is no longer allowed to hide an
//! otherwise valid logical alternative. See [`multiplier`]'s doc for the
//! exact recurrence: a group that chooses `Share` collapses its own
//! multiplicity to exactly `1` for everything beneath it (one shared
//! execution backs every use of it); a group that chooses
//! `RecomputeIndependently` — or has no Share/Recompute decision of its own
//! at all, i.e. isn't itself a `SharedSubtreeStrategy` shape — passes its
//! *own* effective count straight through to whatever it references,
//! transitively composing contributions from every ancestor on the path,
//! not just the immediate parent.
//!
//! This is genuine dynamic programming in the classical sense: overlapping
//! subproblems (a site reachable through more than one parent path is
//! solved once, memoized in `effective_uses`, and reused for every path
//! into it) combined via a real recurrence — not just the MEMO-group
//! sharing [`PlanSpace`] itself already does for *storing* candidates. That
//! distinction is exactly what issue #271 raised: this module already looks
//! like a Cascades/Volcano MEMO, but [`PlanSpace::cost_sorted`] alone never
//! actually performed this composition step; `global_selection` is that
//! step, added alongside `cost_sorted` rather than replacing it (both stay
//! available — see [`RankedGroup`] vs. [`SelectedGroup`]'s own docs for when
//! to reach for which).
//!
//! Two things this deliberately does **not** attempt, both left as
//! documented follow-up rather than silently overclaimed:
//!
//! - [`CostModel::rank_candidates`]/[`CostModel::size_params`] — the hooks
//!   [`SketchAlgorithmStrategy`] groups rank by — take no `consumer_count`
//!   parameter at all today, so a `SketchAlgorithmStrategy` group's selection
//!   here still falls back to [`rank_group`]'s ordinary (consumer-count-
//!   blind) local ranking, even though its own
//!   [`SelectedGroup::effective_consumer_count`] is computed and exposed
//!   correctly regardless. Wiring sketch sizing/ranking to actually consume
//!   it needs a `CostModel` interface change — out of scope here per this
//!   issue's own "reuse `CostModel`, don't invent a new interface" ask; a
//!   correct `effective_consumer_count` is the input such a future hook
//!   would need, and this module now computes it for every group, sketch
//!   groups included.
//! - This is not an exhaustive search over combinations of choices for a
//!   provably-global optimum in every case. [`CostModel::cse_share_decision`]
//!   is still a *local*, pairwise comparison at each `SharedSubtreeStrategy`
//!   site (recompute-total vs. one fixed maintenance cost) — this module
//!   just now feeds it a *correct* input instead of an *incorrect* one. Two
//!   sibling `SharedSubtreeStrategy` groups that could trade off against
//!   each other under some shared resource budget (memory, say) still
//!   aren't jointly optimized here — this crate has no
//!   cardinality/statistics estimation to bound a combinatorial search like
//!   that with (the same constraint #237/#263 already navigated), so real
//!   multi-group joint optimization beyond this per-site recurrence is left
//!   for whenever that changes.

use std::collections::{HashMap, HashSet, VecDeque};

use asap_types::post_asap::{AccuracyError, CompositionOperator, GuaranteeSource, ResultGuarantee};
use asap_types::post_asap::{
    ExactKind, ExactParams, GroupingStrategy, SamplingKind, SamplingParams, SketchAlgorithm,
    SketchKind, SketchParams, SketchQuery as PostAsapSketchQuery, StatModelKind, StatModelParams,
    SummaryExpr, SummaryFamilyType, SummaryField, SummaryNode, SummarySchema, WaveletKind,
    WaveletParams,
};
use asap_types::pre_asap::agg_intent::{agg_is_mergeable, AggIntent};
use asap_types::pre_asap::cse::{share_common_subtrees, structural_hash, HashCache};
use asap_types::pre_asap::expr_ir::ColumnRef;
use asap_types::pre_asap::query_expr::{QueryExpr, QueryExprError, Reduction};
use asap_types::pre_asap::schema::Schema;
use asap_types::types::AccuracyTarget;
use asap_types::workload::{
    ExpectedDemand, QueryRecurrence, QueryWorkload, RepeatedDemand, RepetitionInterval,
};
use std::rc::Rc;
use thiserror::Error;

use crate::accuracy::{
    AccuracyBudgetAllocator, AccuracyEvidenceProvider, AccuracyModel, CompositionShape,
    DefaultAccuracyModel, EqualSplitAllocator, NoAccuracyEvidence, KLL_RANK_ERROR_COEFFICIENT_99,
    KLL_RANK_ERROR_EXPONENT_99,
};
use crate::accuracy_reconciliation::AccuracyReconciliationStrategy;
use crate::cost_model::{CostModel, CseCandidate, DefaultCostModel, ShareDecision};
use crate::grouping::HydraGroupingStrategy;
use crate::recurrence::{
    evaluation_rate_of, Horizon, RecurrenceError, RecurrenceProfile, RootRecurrence, UpdateRate,
};
use crate::rollup::RollupStrategy;
use crate::topk_reuse::TopKLimitReuseStrategy;

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
    /// The candidate is accuracy-illegal (issue #172): its composed
    /// guarantee has no sound propagation rule, or misses the applicable
    /// `AccuracyTarget`. Fail-closed — the candidate is never constructed
    /// with the child "treated as exact". [`SketchAlgorithmStrategy::propose`]
    /// records it as a [`RejectedCandidate`] instead of a candidate.
    #[error("accuracy-illegal candidate: {0}")]
    Accuracy(#[from] AccuracyError),
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
/// `consumer_count` is how many locations across the workload reference this
/// exact `Rc` — 1 for an ordinary single-use node and 2+ for a shared subtree.
/// [`search_workload_with`] computes the workload-wide value during target
/// discovery. [`TargetSubDAG::new`] defaults it to `1` for callers invoking a
/// strategy against one node in isolation. A strategy that only cares about
/// `root`'s shape (for example, [`SketchAlgorithmStrategy`]) can ignore the
/// count; [`SharedSubtreeStrategy`] consults it directly.
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

    /// A target with an explicit `consumer_count`, used by workload discovery
    /// and by callers that already know how many locations reference `root`.
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
/// [`crate::explanation::ReplacementExplanation::reason`] literally reuses
/// this same string rather than inventing new prose of its own.
#[derive(Debug, Clone)]
pub struct ReplacementSubDAG {
    pub replacement: Replacement,
    /// Name of the [`ReplacementStrategy`] that proposed this candidate.
    /// Search fills this from `ReplacementStrategy::name`; consumers must not
    /// infer it from the replacement's shape or provenance.
    pub strategy: &'static str,
    /// Machine-readable origin/role of this alternative. Selection uses this
    /// instead of inferring strategy semantics from replacement shape or
    /// pointer identity when several strategies contribute to one memo group.
    pub provenance: ReplacementProvenance,
    pub rationale: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReplacementProvenance {
    SummaryImplementation,
    CseShare,
    CseRecompute,
    LogicalRewrite,
    /// [`crate::accuracy_reconciliation::AccuracyReconciliationStrategy`]'s
    /// "read a strictly-tighter sibling instead of building an independent,
    /// looser copy" candidate (issue #273). Kept distinct from
    /// `LogicalRewrite` — even though both are structurally-different,
    /// semantically-equivalent rewrites — because
    /// [`crate::cost_model::DefaultCostModel::estimate_cost`] needs to price
    /// it differently: `LogicalRewrite` candidates (`RollupStrategy`,
    /// `TopKLimitReuseStrategy`) still rebuild `target` itself from a
    /// different source, so pricing them like an independent rebuild is
    /// correct; this candidate never rebuilds `target` at all; it reads a
    /// sibling that (per this strategy's own safety argument) is built
    /// regardless, so pricing it like a full independent rebuild would be
    /// the wrong shape of cost, not just the wrong number.
    AccuracyReconciliation,
}

/// A candidate a strategy considered for a target but refused to propose on
/// accuracy-legality grounds (issue #172) — kept alongside the group's
/// legal candidates in [`MemoGroup::rejected`] so a rejection is as
/// inspectable (and exportable) as a selection. Never ranked: a
/// [`CostModel`] only ever sees [`MemoGroup::candidates`].
#[derive(Debug, Clone)]
pub struct RejectedCandidate {
    /// Name of the [`ReplacementStrategy`] that considered it.
    pub strategy: &'static str,
    /// What the candidate would have been (the same prose a
    /// [`ReplacementSubDAG::rationale`] would have carried).
    pub description: String,
    /// The typed reason it is illegal.
    pub error: AccuracyError,
}

/// Everything one [`ReplacementStrategy`] has to say about one target: the
/// legal candidates it proposes plus the accuracy-illegal ones it refused —
/// the output of [`ReplacementStrategy::propose`].
#[derive(Debug, Clone, Default)]
pub struct Proposals {
    pub candidates: Vec<ReplacementSubDAG>,
    pub rejected: Vec<RejectedCandidate>,
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
    /// Stable, human-readable strategy name carried into every proposed
    /// candidate and ultimately into planner diagnostics/visualizations.
    fn name(&self) -> &'static str {
        let short = std::any::type_name::<Self>()
            .rsplit("::")
            .next()
            .expect("a Rust type name always has a final segment");
        short.split_once('<').map_or(short, |(base, _)| base)
    }

    /// Does this strategy have any replacement to offer for `target`?
    fn matches(&self, target: &TargetSubDAG<'_>) -> bool;

    /// Every valid replacement for `target` — not ranked, not filtered.
    /// Reporting "every valid candidate" is this method's whole job; picking
    /// the best one is a [`CostModel`]'s job, out of scope here.
    fn replacements(&self, target: &TargetSubDAG<'_>) -> Vec<ReplacementSubDAG>;

    /// [`replacements`](Self::replacements) plus the accuracy-illegal
    /// candidates this strategy refused to propose (issue #172). Default:
    /// every candidate from `replacements`, no rejections — a strategy that
    /// never performs an accuracy check need not override this.
    /// [`search_workload_with`] calls this (not `replacements`) so the
    /// rejections land in [`MemoGroup::rejected`].
    fn propose(&self, target: &TargetSubDAG<'_>) -> Proposals {
        Proposals {
            candidates: self.replacements(target),
            rejected: Vec::new(),
        }
    }
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
/// `pub(crate)`: [`SketchAlgorithmStrategy::replacements`] is this module's
/// own caller; `grouping::HydraGroupingStrategy` (issue #256) is the one
/// caller outside it, needing the exact same already-ranked candidate list
/// to find the `Implementation::Sketch` matching the Hydra-eligible kind it
/// is building a candidate for.
pub(crate) fn implementations_for_with(
    intent: &AggIntent,
    cost_model: &dyn CostModel,
) -> Vec<Implementation> {
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
    let ranked = crate::cost_model::validated_candidate_ranking(
        cost_model,
        intent,
        summary_candidates(intent),
    );
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
        SketchAlgorithm::Theta => SketchParams::Theta { k: kmv_k_99(eps) },
        SketchAlgorithm::Kmv => SketchParams::Kmv { k: kmv_k_99(eps) },
        SketchAlgorithm::CountSketch => SketchParams::CountSketch {
            width: count_sketch_width(eps),
            depth: count_sketch_depth(delta),
        },
        SketchAlgorithm::CountSketchWithHeap => {
            let k = match intent {
                AggIntent::TopK { k, .. } => *k,
                _ => unreachable!("CountSketchWithHeap is only a TopK candidate"),
            };
            SketchParams::CountSketchWithHeap {
                width: count_sketch_width(eps),
                depth: count_sketch_depth(delta),
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
        SketchAlgorithm::CountSketch => {
            // CMS's expected-L1 collision relaxation is not a CountSketch
            // L2 theorem; retain the formal CountSketch sizing unchanged.
            default_size_params(kind, intent, eps, delta)
        }
        SketchAlgorithm::CountSketchWithHeap => {
            // As above, do not apply CMS's L1 relaxation to CountSketch.
            default_size_params(kind, intent, eps, delta)
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

/// Invert Apache DataSketches' empirical 99th-percentile, single-sided KLL
/// normalized rank-error fit: `epsilon = 2.296 / k^0.9723`.
fn kll_k(eps: f64) -> u32 {
    saturating_ceil(
        (KLL_RANK_ERROR_COEFFICIENT_99 / eps).powf(1.0 / KLL_RANK_ERROR_EXPONENT_99),
        8,
        65_535,
    )
}

/// HLL RSE-magnitude inversion. Generic HLL has no modeled confidence target.
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

/// 99%-confidence KMV/Theta relative bound via Chebyshev, using
/// `RSE <= 1/sqrt(k-2)` and a ten-standard-deviation interval.
fn kmv_k_99(eps: f64) -> u32 {
    saturating_ceil(100.0 / (eps * eps) + 2.0, 16, 1 << 26)
}

/// CountSketch `L2` point-query width: ε = sqrt(3/w).
fn count_sketch_width(eps: f64) -> u32 {
    saturating_ceil(3.0 / (eps * eps), 2, 1 << 26)
}

/// Positive odd depth satisfying Hoeffding's median failure bound
/// `exp(-depth/18) <= delta` for per-row failure at most 1/3.
fn count_sketch_depth(delta: f64) -> u32 {
    if !(delta.is_finite() && delta > 0.0 && delta < 1.0) {
        return 255;
    }
    let depth = saturating_ceil(18.0 * (1.0 / delta).ln(), 1, 255);
    if depth.is_multiple_of(2) {
        (depth + 1).min(255)
    } else {
        depth
    }
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
static DEFAULT_ACCURACY_MODEL: DefaultAccuracyModel = DefaultAccuracyModel;
static DEFAULT_ALLOCATOR: EqualSplitAllocator = EqualSplitAllocator;
static NO_ACCURACY_EVIDENCE: NoAccuracyEvidence = NoAccuracyEvidence;

/// The three deployment-pluggable models one candidate construction
/// consults, bundled so the construction path threads one argument rather
/// than three. `cost` ranks and sizes; `accuracy` and `allocator` decide
/// legality (issue #172) — see [`crate::accuracy`]'s module docs for why
/// those are separate from `cost` and run before it.
#[derive(Clone, Copy)]
pub(crate) struct Models<'a> {
    pub cost: &'a dyn CostModel,
    pub accuracy: &'a dyn AccuracyModel,
    pub allocator: &'a dyn AccuracyBudgetAllocator,
    pub evidence: &'a dyn AccuracyEvidenceProvider,
}

impl<'a> Models<'a> {
    /// `cost` with the built-in [`DefaultAccuracyModel`]/
    /// [`EqualSplitAllocator`] — what every entry point that only takes a
    /// `CostModel` uses.
    pub(crate) fn with_default_accuracy(cost: &'a dyn CostModel) -> Self {
        Self {
            cost,
            accuracy: &DEFAULT_ACCURACY_MODEL,
            allocator: &DEFAULT_ALLOCATOR,
            evidence: &NO_ACCURACY_EVIDENCE,
        }
    }
}

/// Wraps [`implementations_for_with`]'s exhaustive, ranked list directly: for
/// a bindable `Aggregate`, every valid candidate summary realization as its
/// own [`ReplacementSubDAG`].
///
/// Ranked (only to *order the enumeration*, never to drop a candidate) via a
/// [`CostModel`] — [`DefaultCostModel`] unless constructed with
/// [`SketchAlgorithmStrategy::new`] — so a deployment-specific cost model's
/// other hooks (`size_params`, `realize_extension`, `readout_extension`) are
/// still consulted while binding each candidate.
///
/// The one thing that *does* drop a candidate is accuracy legality (issue
/// #172), decided by the [`AccuracyModel`] — never by the cost model: a
/// sketch over an approximate child is proposed only if its composed
/// guarantee has a sound propagation rule and satisfies the node's own
/// `AccuracyTarget`; otherwise it is reported through
/// [`ReplacementStrategy::propose`] as a [`RejectedCandidate`]. See
/// [`crate::accuracy`]'s module docs for the rules and the precedence
/// between root and per-node targets.
pub struct SketchAlgorithmStrategy<'a> {
    models: Models<'a>,
}

impl SketchAlgorithmStrategy<'static> {
    /// A strategy that ranks/binds via the built-in [`DefaultCostModel`] —
    /// what a deployment gets with no custom cost model plugged in.
    pub fn default_cost_model() -> Self {
        Self {
            models: Models::with_default_accuracy(&DEFAULT_COST_MODEL),
        }
    }
}

impl<'a> SketchAlgorithmStrategy<'a> {
    /// A strategy that ranks/binds via `cost_model` instead of the built-in
    /// static preference order — the same customization point
    /// [`implementations_for_with`] already offers. Accuracy legality stays
    /// with the built-in [`DefaultAccuracyModel`]/[`EqualSplitAllocator`].
    pub fn new(cost_model: &'a dyn CostModel) -> Self {
        Self {
            models: Models::with_default_accuracy(cost_model),
        }
    }

    /// A strategy with every model plugged in explicitly: `cost_model` for
    /// ranking/sizing, `accuracy_model` for guarantee derivation/propagation/
    /// satisfaction, `allocator` for end-to-end budget splits. One model
    /// never overrides another: legality is settled by `accuracy_model`
    /// before `cost_model` ranks what is left.
    pub fn with_models(
        cost_model: &'a dyn CostModel,
        accuracy_model: &'a dyn AccuracyModel,
        allocator: &'a dyn AccuracyBudgetAllocator,
    ) -> Self {
        Self {
            models: Models {
                cost: cost_model,
                accuracy: accuracy_model,
                allocator,
                evidence: &NO_ACCURACY_EVIDENCE,
            },
        }
    }

    /// Like [`Self::with_models`], with typed planning-time evidence for
    /// rules such as TopK membership and Hydra shared-grid composition.
    pub fn with_models_and_evidence(
        cost_model: &'a dyn CostModel,
        accuracy_model: &'a dyn AccuracyModel,
        allocator: &'a dyn AccuracyBudgetAllocator,
        evidence: &'a dyn AccuracyEvidenceProvider,
    ) -> Self {
        Self {
            models: Models {
                cost: cost_model,
                accuracy: accuracy_model,
                allocator,
                evidence,
            },
        }
    }

    pub(crate) fn from_models(models: Models<'a>) -> Self {
        Self { models }
    }

    /// The whole enumeration for one target, with `intent_override`
    /// substituting the target's own intent (only ever its `AccuracyTarget`
    /// differs — see [`realize_child_with`]).
    fn propose_with(&self, root: &Rc<QueryExpr>, intent_override: Option<&AggIntent>) -> Proposals {
        let mut proposals = Proposals::default();
        let Some(declared) = bindable_intent(root) else {
            return proposals;
        };
        let intent = intent_override.unwrap_or(declared);
        let models = self.models;

        // Is the child approximate? Probed once, up front: a candidate over
        // an approximate child needs the end-to-end budget split across both
        // layers, which changes which candidates exist at all.
        let child_layers = aggregate_child(root)
            .and_then(|child| realize_child_with(child, models, None).ok())
            .and_then(|child| {
                child
                    .guarantee
                    .as_ref()
                    .filter(|g| !g.is_exact())
                    .map(ResultGuarantee::approximate_layer_count)
            });

        // `implementations_for_with` is already exhaustive and ranked — no
        // separate dispatch needed here. Only `Sketch` has more than one
        // candidate in practice (every other variant's own dispatch produces
        // exactly one `Implementation`), but this loop doesn't need to know
        // that; it just constructs whatever the list contains.
        for implementation in implementations_for_with(intent, models.cost) {
            let rationale = describe_implementation(intent, &implementation);
            // The as-declared composition: every layer sized to its own
            // declared `AccuracyTarget`. Legal iff the composed guarantee
            // satisfies this node's target — a front end copying one target
            // onto every node does not make that so.
            proposals.record(
                rationale.clone(),
                construct_summary_with(root, intent, implementation.clone(), models, None, None),
            );

            // Budget-split alternatives (issue #172, PR 2): re-size this
            // layer and the approximate child under each allocation of this
            // node's target across every approximate layer.
            let (Some(child_layers), Implementation::Sketch(kind), Some(target)) =
                (child_layers, &implementation, accuracy_target(intent))
            else {
                continue;
            };
            let Some(readout_query) = aggregate_child(root)
                .and_then(|child| child.output_schema().ok())
                .map(|schema| readout(intent, &summarised_column(intent, &schema), models.cost))
            else {
                continue;
            };
            let family = SummaryFamilyType::Sketch(kind.clone(), GroupingStrategy::default());
            let Some(local) = models.accuracy.local_guarantee(&family, &readout_query) else {
                continue;
            };
            let shape = CompositionShape {
                metric: local.metric,
                approximate_layer_count: 1 + child_layers,
            };
            let allocations = models.allocator.allocations(target, &shape);
            if allocations.is_empty() {
                proposals.rejected.push(RejectedCandidate {
                    strategy: "SketchAlgorithmStrategy",
                    description: rationale.clone(),
                    error: AccuracyError::NoLegalAllocation {
                        target: target.clone(),
                        layer_count: shape.approximate_layer_count,
                    },
                });
                continue;
            }
            let declared_child_target = aggregate_child(root)
                .and_then(|child| bindable_intent(child))
                .and_then(accuracy_target);
            for allocation in allocations {
                let outer_target = &allocation.layers[0];
                let inner_target = allocation.inner_target(&shape);
                let (eps, delta) = accuracy_budget(outer_target);
                let resized = Implementation::Sketch(SketchKind::new(
                    kind.algorithm().clone(),
                    models
                        .cost
                        .size_params(kind.algorithm().clone(), intent, eps, delta),
                ));
                // Identical to the as-declared composition already recorded
                // above — nothing new to propose.
                if resized == implementation && inner_target.as_ref() == declared_child_target {
                    continue;
                }
                let note = GuaranteeSource::BudgetAllocation {
                    allocator: allocation.allocator.to_string(),
                    layer: 0,
                    layer_count: shape.approximate_layer_count,
                    local_target: outer_target.clone(),
                    end_to_end_target: target.clone(),
                };
                proposals.record(
                    format!(
                        "{rationale}; sized under {} budget split of {target:?} across \
                         {} approximate layers (this layer {outer_target:?}, child subtree \
                         {inner_target:?})",
                        allocation.allocator, shape.approximate_layer_count
                    ),
                    construct_summary_with(
                        root,
                        intent,
                        resized,
                        models,
                        inner_target.as_ref(),
                        Some(note),
                    ),
                );
            }
        }
        proposals
    }
}

impl Proposals {
    /// File one construction attempt: a legal node becomes a candidate, an
    /// [`ImplementError::Accuracy`] becomes a [`RejectedCandidate`], and a
    /// schema-derivation failure is skipped exactly as it always was.
    fn record(&mut self, rationale: String, built: Result<Rc<SummaryNode>, ImplementError>) {
        match built {
            Ok(node) => self.candidates.push(ReplacementSubDAG {
                strategy: "SketchAlgorithmStrategy",
                replacement: Replacement::Summary(node),
                provenance: ReplacementProvenance::SummaryImplementation,
                rationale,
            }),
            Err(ImplementError::Accuracy(error)) => self.rejected.push(RejectedCandidate {
                strategy: "SketchAlgorithmStrategy",
                description: rationale,
                error,
            }),
            Err(ImplementError::Schema(_)) => {}
        }
    }
}

/// The `child` of a [`bindable_intent`]-shaped `Aggregate`.
fn aggregate_child(node: &QueryExpr) -> Option<&Rc<QueryExpr>> {
    match node {
        QueryExpr::Aggregate { child, .. } => Some(child),
        _ => None,
    }
}

impl ReplacementStrategy for SketchAlgorithmStrategy<'_> {
    fn matches(&self, target: &TargetSubDAG<'_>) -> bool {
        bindable_intent(target.root).is_some()
    }

    fn replacements(&self, target: &TargetSubDAG<'_>) -> Vec<ReplacementSubDAG> {
        self.propose(target).candidates
    }

    fn propose(&self, target: &TargetSubDAG<'_>) -> Proposals {
        self.propose_with(target.root, None)
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
/// future intent to be named here too. [`crate::explanation`] needs no
/// counterpart of its own: it reads a candidate's `rationale` — built from
/// this text — straight off [`ReplacementSubDAG`], rather than re-describing
/// the same intent a second time.
///
/// `pub(crate)`: `grouping::HydraGroupingStrategy` (issue #256) reuses this
/// for its own rationale strings, for the same reason.
pub(crate) fn describe_intent(intent: &AggIntent) -> String {
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
    realize_child_with(root, Models::with_default_accuracy(cost_model), None)
}

/// [`realize_child`] with every model explicit, plus an optional
/// `end_to_end_target` for `root`'s own value (issue #172): when an
/// [`AccuracyBudgetAllocator`] hands an approximate child a share of its
/// parent's budget, the child is re-enumerated with that share substituted
/// for its declared `AccuracyTarget` — sizing its sketch (and, recursively,
/// re-splitting for its own approximate children) under the allocated
/// budget. A child whose declared target is `Exact` keeps it: an allocation
/// never approximates something the caller declared exact.
pub(crate) fn realize_child_with(
    root: &Rc<QueryExpr>,
    models: Models<'_>,
    end_to_end_target: Option<&AccuracyTarget>,
) -> Result<Rc<SummaryNode>, ImplementError> {
    let overridden = end_to_end_target.and_then(|target| {
        let declared = bindable_intent(root)?;
        match accuracy_target(declared) {
            Some(AccuracyTarget::Exact) | None => None,
            Some(_) => Some(override_accuracy(declared, target)),
        }
    });
    match SketchAlgorithmStrategy::from_models(models)
        .propose_with(root, overridden.as_ref())
        .candidates
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
        // produce — never happens, that match is exhaustive), or every
        // candidate was accuracy-illegal — either way the same conservative
        // fallback `SketchAlgorithmStrategy::matches` uses: keep the
        // pre-ASAP subtree, executed exactly.
        None => keep_pre_asap(root),
    }
}

/// `intent` with its `AccuracyTarget` replaced by `target` — a no-op for an
/// intent that carries none (see [`accuracy_target`]).
fn override_accuracy(intent: &AggIntent, target: &AccuracyTarget) -> AggIntent {
    let mut out = intent.clone();
    match &mut out {
        AggIntent::Quantile { accuracy, .. }
        | AggIntent::Cardinality { accuracy, .. }
        | AggIntent::Count { accuracy }
        | AggIntent::TopK { accuracy, .. } => *accuracy = target.clone(),
        _ => {}
    }
    out
}

/// Wrap an unrewritten pre-ASAP subtree, lifting its schema with every column
/// `SummaryFamilyType::Plain`. `pub` so a caller can fall back to this
/// explicitly — e.g. when `SketchAlgorithmStrategy::replacements()` returns no
/// candidate for a target, or a deployment wants to force a node its own
/// runtime can't actually implement — through the same fallback this
/// crate's own dispatch uses, without duplicating the schema-lift logic.
pub fn keep_pre_asap(expr: &Rc<QueryExpr>) -> Result<Rc<SummaryNode>, ImplementError> {
    keep_pre_asap_rc(Rc::clone(expr))
}

fn keep_pre_asap_rc(expr: Rc<QueryExpr>) -> Result<Rc<SummaryNode>, ImplementError> {
    let schema = expr.output_schema()?;
    Ok(Rc::new(SummaryNode {
        expr: SummaryExpr::KeepPreAsap(expr),
        schema: lift(&schema),
        // A kept pre-ASAP subtree is executed exactly by the runtime
        // (`Implementation::PassThrough`'s contract) — zero error.
        guarantee: Some(ResultGuarantee::exact("KeepPreAsap")),
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

/// `expr` must still be the [`bindable_intent`] shape for `implementation` to
/// have any effect; anything else falls back to [`keep_pre_asap`].
/// Only `expr`'s own top-level decision is forced — recursion into `expr`'s
/// child goes back through [`realize_child`] (fresh candidate
/// enumeration, not a forced pick), so choosing one candidate for a target
/// never leaks into that target's own nested aggregates.
///
/// `pub(crate)`: `grouping::HydraGroupingStrategy` (issue #256) is the one
/// caller outside this module — the same first-class,
/// one-candidate-at-a-time primitive [`SketchAlgorithmStrategy`] itself
/// calls once per candidate, reused rather than duplicated so a Hydra
/// candidate gets exactly the same schema derivation/column
/// resolution/readout construction as every other candidate, patching only
/// the `grouping` field this axis owns.
/// Construct a summary with every model explicit (issue #172). `intent`
/// is `expr`'s own [`bindable_intent`], or a copy of it with an allocated
/// `AccuracyTarget` substituted (see [`realize_child_with`]).
/// `child_target`, when set, is the end-to-end budget the child subtree is
/// re-enumerated under; `allocation` is the provenance note recording the
/// split that produced both. `Err(ImplementError::Accuracy)` is the
/// fail-closed answer for a composition with no sound rule or one that
/// misses `intent`'s target.
pub(crate) fn construct_summary_with(
    expr: &QueryExpr,
    intent: &AggIntent,
    implementation: Implementation,
    models: Models<'_>,
    child_target: Option<&AccuracyTarget>,
    allocation: Option<GuaranteeSource>,
) -> Result<Rc<SummaryNode>, ImplementError> {
    if let QueryExpr::Aggregate {
        reduction, child, ..
    } = expr
    {
        // `bindable_intent` already established the shape: exactly one
        // intent, no HAVING. (Multi-intent nodes and HAVING stay logical.)
        if bindable_intent(expr).is_some() {
            if let Some((family, estimate)) = summary_family(implementation) {
                return construct_summary_agg(
                    expr,
                    reduction,
                    intent,
                    child,
                    family,
                    estimate,
                    models,
                    child_target,
                    allocation,
                );
            }
        }
    }
    keep_pre_asap_rc(Rc::new(expr.clone()))
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
        Implementation::Sketch(kind) => (
            SummaryFamilyType::Sketch(kind, GroupingStrategy::default()),
            true,
        ),
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
    models: Models<'_>,
    child_target: Option<&AccuracyTarget>,
    allocation: Option<GuaranteeSource>,
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
    let query = estimate.then(|| readout(intent, &col, models.cost));

    let mut state_schema = lift(&out_schema);
    if let Some(field) = state_schema.fields.get_mut(state_idx) {
        field.dtype = family.clone();
    }

    let bound_child = realize_child_with(child, models, child_target)?;

    // ── Guarantee (issue #172) ──────────────────────────────────────────
    // Derived *before* the node exists, so an illegal composition is never
    // materialized: the local guarantee of this family's readout (or exact
    // accumulator) composed over the child's, under the operator this
    // family applies to the child's values.
    let guarantee = compose_guarantee(
        &family,
        query.as_ref(),
        &bound_child,
        intent,
        models.accuracy,
        models.evidence,
        allocation,
    )?;

    // `reduction` is carried onto `SummaryAgg` verbatim — not flattened to a
    // bare `Vec<ColumnId>` — so `SummaryExecutor::find_candidates` can tell
    // a genuine empty-`by` reduction apart from a per-entity shape with no
    // grouping concept at all (issue #163). `construct_summary_agg` is the
    // single place that decides this; nothing downstream re-derives it.
    let agg = Rc::new(SummaryNode {
        expr: SummaryExpr::SummaryAgg {
            child: bound_child,
            family,
            col,
            reduction: reduction.clone(),
            grouping: GroupingStrategy::default(),
        },
        schema: state_schema,
        // Summary *state* carries no caller-visible guarantee; only a
        // finalized value does. An exact accumulator's state is its value.
        guarantee: if estimate { None } else { guarantee.clone() },
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
            guarantee,
        })),
        None => Ok(agg),
    }
}

/// The guarantee of the value a `family` node produces over `child` —
/// [`AccuracyModel::propagate`] under the [`CompositionOperator`] this family
/// applies to its child's values — checked against `intent`'s own
/// `AccuracyTarget` whenever the child is approximate (an approximate
/// parent over an exact child is sized to that target by construction and
/// is not re-checked here, so single-layer behavior is unchanged; see
/// [`crate::accuracy`]'s precedence rules). `Ok(None)` is "no error model"
/// (an approximate family the model has no local guarantee for, over an
/// exact child) — unknown, never exact.
fn compose_guarantee(
    family: &SummaryFamilyType,
    query: Option<&PostAsapSketchQuery>,
    child: &SummaryNode,
    intent: &AggIntent,
    accuracy: &dyn AccuracyModel,
    evidence: &dyn AccuracyEvidenceProvider,
    allocation: Option<GuaranteeSource>,
) -> Result<Option<ResultGuarantee>, AccuracyError> {
    let (op, local) = match (family, query) {
        (SummaryFamilyType::ExactAggregate(kind, _), _) => {
            let op = match kind {
                ExactKind::Sum => CompositionOperator::ExactSum,
                ExactKind::MinMax => CompositionOperator::ExactExtremum,
                // A row count does not depend on the rows' values: exact
                // regardless of the child's own error.
                ExactKind::Count => {
                    return Ok(Some(ResultGuarantee::exact(
                        "ExactAggregate(Count): row count is independent of input values",
                    )))
                }
                // Counter-reset detection over perturbed values has no finite
                // Lipschitz constant — over an approximate child this is a
                // deterministic transform with no registered rule.
                ExactKind::Increase | ExactKind::Rate => CompositionOperator::Lipschitz {
                    constant: f64::INFINITY,
                },
            };
            (
                op,
                Some(ResultGuarantee::exact(format!("ExactAggregate({kind:?})"))),
            )
        }
        (_, Some(query)) => (
            if matches!(query, PostAsapSketchQuery::TopK { .. }) {
                CompositionOperator::TopKSelection
            } else {
                CompositionOperator::ApproximateAggregate
            },
            accuracy.local_guarantee(family, query),
        ),
        (_, None) => (CompositionOperator::ApproximateAggregate, None),
    };
    let Some(input) = child.guarantee.clone() else {
        // A child with no guarantee at all is an unknown quantity, which
        // nothing can be composed over (a `Sample` readout, say) — unless
        // this node is itself the unknown family, in which case it inherits
        // "unknown" rather than fabricating a guarantee for its child.
        return match local {
            Some(_) => Err(AccuracyError::MissingInputGuarantee {
                operator: op,
                input_index: 0,
            }),
            None => Ok(None),
        };
    };
    if local.is_none() && input.is_exact() {
        return Ok(None);
    }
    let stats = evidence.propagation_stats(&op, family, query);
    let mut guarantee =
        accuracy.propagate(&op, std::slice::from_ref(&input), local.as_ref(), &stats)?;
    if let Some(note) = allocation {
        guarantee.provenance.push(note);
    }
    if let Some(target) = accuracy_target(intent) {
        guarantee.provenance.push(GuaranteeSource::AccuracyTarget {
            target: target.clone(),
        });
        // Check even over an exact input: parameter clamps or a conservative
        // confidence conversion can make the tightest available sketch miss
        // its requested target.
        if !accuracy.satisfies(&guarantee, target) {
            return Err(AccuracyError::TargetNotSatisfied {
                metric: guarantee.metric,
                bound: guarantee.bound.evaluate(),
                failure_probability: guarantee.failure_probability.evaluate(),
                target: target.clone(),
            });
        }
    }
    Ok(Some(guarantee))
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
                strategy: "SharedSubtreeStrategy",
                // The already-interned `Rc` itself: reusing it verbatim *is*
                // "build once and share" — no new node to construct.
                replacement: Replacement::Rewrite(Rc::clone(target.root)),
                provenance: ReplacementProvenance::CseShare,
                rationale: format!(
                    "build once and share: share_common_subtrees already interned this \
                     subtree once and reused it across {count} consumers — one build can \
                     answer all of them instead of computing it {count} times"
                ),
            },
            ReplacementSubDAG {
                strategy: "SharedSubtreeStrategy",
                // A structurally-identical but freshly-allocated `Rc`: same
                // value (`PartialEq`), deliberately *not* the same pointer,
                // representing "undo the sharing and recompute independently".
                replacement: Replacement::Rewrite(Rc::new((**target.root).clone())),
                provenance: ReplacementProvenance::CseRecompute,
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
    /// Every candidate a strategy considered for `target` but refused on
    /// accuracy-legality grounds (issue #172), plus any `candidates` entry
    /// the root-target check ([`search_workload_with_targets`]) moved here.
    /// Never ranked — [`PlanSpace::cost_sorted`]/[`PlanSpace::global_selection`]
    /// read only `candidates`, so a [`CostModel`] cannot resurrect one.
    pub rejected: Vec<RejectedCandidate>,
}

impl MemoGroup {
    fn new(target: Rc<QueryExpr>, consumer_count: usize) -> Self {
        Self {
            target,
            consumer_count,
            candidates: Vec::new(),
            rejected: Vec::new(),
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
    /// [`CostModel::grouping_state_cost`] for grouping alternatives, or
    /// [`CostModel::estimate_cost`] otherwise, is an additive annotation
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
                    .map(|c| {
                        cost_model
                            .grouping_state_cost(c, &target)
                            .map_or_else(|| cost_model.estimate_cost(c, &target), |cost| cost.0)
                    })
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

    /// Recurrence-aware counterpart to [`Self::cost_sorted`]. CSE
    /// share/recompute pairs are ordered with the target's recurrence
    /// profile; all other candidate shapes retain their existing ranking.
    pub fn cost_sorted_with_recurrence(
        &self,
        cost_model: &dyn CostModel,
        profiles: &RecurrenceProfileMap,
        horizon: Option<Horizon>,
    ) -> Result<Vec<RankedGroup<'_>>, RecurrenceError> {
        self.order
            .iter()
            .map(|ptr| {
                let group = &self.groups[ptr];
                let mut candidates = rank_group(group, cost_model);
                if cse_candidate_pair(group).is_some() {
                    if let Some(decision) = decide_group_with_recurrence(
                        group,
                        group.consumer_count,
                        profiles.for_target(&group.target),
                        horizon,
                        cost_model,
                    )? {
                        candidates.sort_by_key(|candidate| match candidate.provenance {
                            ReplacementProvenance::CseShare if decision == ShareDecision::Share => {
                                0
                            }
                            ReplacementProvenance::CseRecompute
                                if decision == ShareDecision::RecomputeIndependently =>
                            {
                                0
                            }
                            ReplacementProvenance::CseShare
                            | ReplacementProvenance::CseRecompute => 2,
                            _ => 1,
                        });
                    }
                }
                let target = TargetSubDAG::with_consumer_count(&group.target, group.consumer_count);
                let costs = candidates
                    .iter()
                    .map(|candidate| {
                        cost_model
                            .grouping_state_cost(candidate, &target)
                            .map_or_else(
                                || cost_model.estimate_cost(candidate, &target),
                                |cost| cost.0,
                            )
                    })
                    .collect();
                Ok(RankedGroup {
                    target: &group.target,
                    consumer_count: group.consumer_count,
                    candidates,
                    costs,
                })
            })
            .collect()
    }
}

// ── Recurrence-aware cost context (issue #287) ──────────────────────────

/// One [`RecurrenceProfile`] per discovered [`MemoGroup`] target, built by
/// [`PlanSpace::recurrence_profiles`] — the "carry `RepeatingEntry.demand`
/// and relevant `DataWorkload` into ASAP-aware search/cost context"
/// half of issue #287. Looked up by `Rc` pointer identity, the same
/// currency [`PlanSpace::group_for`]/[`GlobalSelection::for_target`] already
/// use.
/// Holds an owned `Rc<QueryExpr>` clone alongside each profile (not just its
/// raw pointer) so this map keeps every node it describes alive for as long
/// as the map itself lives — a `RecurrenceProfileMap` is safe to outlive the
/// `PlanSpace` it was built from. Without this, a raw `*const QueryExpr` key
/// could, after the originating `PlanSpace` (the only other owner of those
/// `Rc`s) is dropped, collide with an unrelated, later allocation that
/// happens to reuse the same freed address — silently returning a stale
/// profile for the wrong node (issue #287 review, bug 4).
#[derive(Debug, Clone)]
pub struct RecurrenceProfileMap {
    profiles: HashMap<*const QueryExpr, (Rc<QueryExpr>, RecurrenceProfile)>,
}

impl RecurrenceProfileMap {
    /// The [`RecurrenceProfile`] for `target`, or
    /// [`RecurrenceProfile::EMPTY`] when `target` wasn't a discovered site
    /// in the [`PlanSpace`] this map was built from (or carried no
    /// recurring/one-shot/update-rate metadata at all) — always a valid,
    /// "no metadata" answer, never a panic.
    pub fn for_target(&self, target: &Rc<QueryExpr>) -> RecurrenceProfile {
        self.profiles
            .get(&Rc::as_ptr(target))
            .map(|(_, profile)| *profile)
            .unwrap_or(RecurrenceProfile::EMPTY)
    }
}

impl<Id> PlanSpace<Id> {
    /// Build one [`RecurrenceProfile`] per discovered site, by walking every
    /// root's whole reachable sub-DAG (the same relational-skeleton
    /// traversal [`discover_targets`] itself used to discover those sites)
    /// and folding each root's own recurrence tag
    /// ([`RootRecurrence::Repeating`]'s interval, or
    /// [`RootRecurrence::OneShot`]) into every site reachable from it.
    ///
    /// `root_recurrence` is positional: `root_recurrence[i]` describes
    /// `self.roots[i]` — the same order [`search_workload`]/
    /// [`search_workload_with`] were originally called with (post-CSE
    /// dedup preserves both root count and order — see
    /// `asap_types::pre_asap::cse::share_common_subtrees`'s own
    /// `.map(...).collect()` body). This keeps `Id` fully opaque (no `Eq`/
    /// `Hash`/`Clone` bound needed on it at all — issue #287's "keep
    /// caller/query identifiers opaque" requirement) at the cost of the
    /// caller keeping the two slices in step; `root_recurrence.len()` must
    /// equal `self.roots.len()`.
    ///
    /// A shared sub-DAG reachable from more than one root aggregates every
    /// reaching root's contribution — repeating roots' intervals combine via
    /// [`evaluation_rate_of`]'s `sum(1 / interval_i)`, one-shot roots
    /// increment [`RecurrenceProfile::one_shot_consumers`] — so a summary
    /// consumed by queries with different intervals gets one profile
    /// reflecting all of them, per issue #287's "support a shared sub-DAG
    /// consumed by queries with different intervals".
    ///
    /// `update_rate` is applied uniformly to every discovered site *that
    /// this walk actually reached from some root* (see the "unreachable
    /// sites" note below): today's
    /// [`asap_types::workload::DataWorkload`] is a single
    /// workload-level value (applies to every query in a `QueryWorkload`),
    /// not per-target, so there is no finer-grained source to attach
    /// instead. `None` when no `DataWorkload` evidence was available —
    /// preserves "missing metadata" behavior for the update-rate term alone
    /// even when repeating/one-shot consumer information is present.
    ///
    /// A parent that structurally references the same child more than once
    /// (e.g. `BinaryOp{lhs: X, rhs: X}`) credits that child with one
    /// contribution per reference, not one contribution per distinct node —
    /// matching how [`MemoGroup::consumer_count`] counts that occurrence.
    /// Multiplicity is propagated through the full descendant path: if the
    /// repeated parent is independently evaluated twice, its child is also
    /// evaluated twice. This supplies recurrence-aware selection with the
    /// effective structural execution rate rather than mere reachability.
    ///
    /// **Unreachable sites**: [`PlanSpace`] can contain a site no root's own
    /// structural tree actually reaches — e.g. one only ever produced by a
    /// [`Replacement::Rewrite`] candidate a [`ReplacementStrategy`] invented
    /// (this walk only follows [`MemoGroup::target`]'s own structural
    /// children, the same scope [`discover_targets`] uses for the original
    /// roots, never a candidate's rewritten value). Such a site gets
    /// [`RecurrenceProfile::EMPTY`] — in particular, `update_rate` is
    /// **not** stamped onto it — so it falls back to the ordinary
    /// structural decision instead of being charged an ingest-driven
    /// maintenance cost against a real evaluation/one-shot signal of
    /// exactly zero, which previously made `RecomputeIndependently` win
    /// there unconditionally, regardless of the site's actual
    /// `consumer_count` (issue #287 review, bug 2).
    ///
    /// Returns [`RecurrenceError::InvalidInterval`] if any
    /// `RootRecurrence::Repeating` interval is zero,
    /// [`RecurrenceError::InvalidUpdateRate`] if `update_rate` is non-finite
    /// or negative, or [`RecurrenceError::RootCountMismatch`] if
    /// `root_recurrence.len() != self.roots.len()`.
    pub fn recurrence_profiles(
        &self,
        root_recurrence: &[RootRecurrence],
        update_rate: Option<UpdateRate>,
    ) -> Result<RecurrenceProfileMap, crate::recurrence::RecurrenceError> {
        if root_recurrence.len() != self.roots.len() {
            return Err(crate::recurrence::RecurrenceError::RootCountMismatch {
                expected: self.roots.len(),
                got: root_recurrence.len(),
            });
        }
        if let Some(rate) = update_rate {
            crate::recurrence::validate_update_rate(rate)?;
        }
        for recurrence in root_recurrence {
            if let RootRecurrence::RepeatingRate(rate) = recurrence {
                if !rate.0.is_finite() || rate.0 < 0.0 {
                    return Err(crate::recurrence::RecurrenceError::InvalidEvaluationRate(
                        *rate,
                    ));
                }
            }
        }

        let mut intervals: HashMap<*const QueryExpr, Vec<RepetitionInterval>> = HashMap::new();
        let mut rates: HashMap<*const QueryExpr, f64> = HashMap::new();
        let mut one_shot_counts: HashMap<*const QueryExpr, usize> = HashMap::new();
        // Sites actually reached by at least one root's own recurrence tag
        // during the walk below — see this method's own "Unreachable
        // sites" doc.
        let mut reached: HashSet<*const QueryExpr> = HashSet::new();

        for ((_, root), recurrence) in self.roots.iter().zip(root_recurrence) {
            let recurrence = *recurrence;
            let root_ptr = Rc::as_ptr(root);
            // Carry path multiplicity transitively. If a shared ancestor is
            // referenced twice, every descendant below an independently
            // recomputed occurrence is evaluated twice as well; stopping
            // expansion after the first pointer visit undercounts exactly
            // the effective-consumer rate recurrence-aware costing needs.
            let mut queue: VecDeque<(*const QueryExpr, usize)> = VecDeque::new();
            queue.push_back((root_ptr, 1));

            while let Some((ptr, path_count)) = queue.pop_front() {
                contribute(
                    ptr,
                    path_count,
                    recurrence,
                    &mut intervals,
                    &mut rates,
                    &mut one_shot_counts,
                    &mut reached,
                );
                // Every reachable node was itself discovered as its own
                // `MemoGroup` (`discover_targets` walks the identical
                // relational-skeleton scope) — its own `target` is the
                // canonical `Rc` to read children off.
                if let Some(group) = self.groups.get(&ptr) {
                    for (child, edge_count) in direct_child_counts(&group.target) {
                        queue.push_back((
                            child,
                            path_count
                                .checked_mul(edge_count)
                                .expect("query DAG path multiplicity overflowed usize"),
                        ));
                    }
                }
            }
        }

        let empty_intervals: Vec<RepetitionInterval> = Vec::new();
        let mut profiles = HashMap::with_capacity(self.order.len());
        for ptr in &self.order {
            let site_intervals = intervals.get(ptr).unwrap_or(&empty_intervals);
            let interval_rate =
                evaluation_rate_of(site_intervals.iter().copied())?.map_or(0.0, |rate| rate.0);
            let direct_rate = rates.get(ptr).copied().unwrap_or(0.0);
            let evaluation_rate = ((interval_rate + direct_rate) > 0.0).then_some(
                crate::recurrence::EvaluationRate(interval_rate + direct_rate),
            );
            let one_shot_consumers = one_shot_counts.get(ptr).copied().unwrap_or(0);
            // Bug 2 fix (see "Unreachable sites" above): only a reached
            // site carries the caller-supplied `update_rate`.
            let site_update_rate = if reached.contains(ptr) {
                update_rate
            } else {
                None
            };
            let node = Rc::clone(&self.groups[ptr].target);
            profiles.insert(
                *ptr,
                (
                    node,
                    RecurrenceProfile {
                        evaluation_rate,
                        one_shot_consumers,
                        update_rate: site_update_rate,
                    },
                ),
            );
        }

        Ok(RecurrenceProfileMap { profiles })
    }

    /// Derive per-target recurrence profiles directly from the normalized
    /// query and data workloads. This is the authoritative bridge from the
    /// public workload model into recurrence-aware candidate costing.
    /// `root_workload_entries[i]` explicitly identifies the normalized
    /// workload entry for `self.roots[i]`; callers need not arrange roots in
    /// the batch-then-repeating storage order.
    pub fn recurrence_profiles_from_workload(
        &self,
        workload: &QueryWorkload,
        // For each `PlanSpace::roots[i]`, the explicit index of its
        // corresponding normalized workload entry.
        root_workload_entries: &[usize],
        now_ms: u64,
        horizon: Option<Horizon>,
    ) -> Result<RecurrenceProfileMap, crate::recurrence::RecurrenceError> {
        workload.validate()?;
        if let Some(horizon) = horizon {
            if !horizon.0.is_finite() || horizon.0 <= 0.0 {
                return Err(crate::recurrence::RecurrenceError::InvalidHorizon(horizon));
            }
        }
        if root_workload_entries.len() != self.roots.len() {
            return Err(crate::recurrence::RecurrenceError::RootCountMismatch {
                expected: self.roots.len(),
                got: root_workload_entries.len(),
            });
        }
        let entries: Vec<_> = workload.entries().collect();
        let mut recurrences = Vec::with_capacity(root_workload_entries.len());
        for &index in root_workload_entries {
            let entry = entries.get(index).ok_or(
                crate::recurrence::RecurrenceError::InvalidWorkloadEntry {
                    index,
                    entry_count: entries.len(),
                },
            )?;
            let recurrence = match &entry.recurrence {
                QueryRecurrence::OneTime { invocations, .. } => RootRecurrence::OneShotCount(
                    usize::try_from(*invocations).unwrap_or(usize::MAX),
                ),
                QueryRecurrence::Repeated(RepeatedDemand::FixedInterval(interval)) => {
                    RootRecurrence::Repeating(*interval)
                }
                QueryRecurrence::Repeated(RepeatedDemand::Scheduled(schedule)) => {
                    let Some(horizon) = horizon else {
                        return Err(crate::recurrence::RecurrenceError::MissingHorizon);
                    };
                    let end_ms = now_ms.saturating_add((horizon.0 * 1000.0) as u64);
                    let count = schedule
                        .iter()
                        .filter(|at| at.0 >= now_ms && at.0 <= end_ms)
                        .count();
                    RootRecurrence::RepeatingRate(crate::recurrence::EvaluationRate(
                        count as f64 / horizon.0,
                    ))
                }
                QueryRecurrence::Repeated(RepeatedDemand::EstimatedRate(estimate)) => {
                    if !estimate.is_fresh_at(now_ms) {
                        RootRecurrence::Unknown
                    } else {
                        let rate = match estimate.expected {
                            ExpectedDemand::AverageRate(rate) => rate.0,
                            ExpectedDemand::InvocationCount(count) => {
                                let millis = estimate
                                    .observation_window
                                    .end
                                    .0
                                    .saturating_sub(estimate.observation_window.start.0);
                                count as f64 / (millis as f64 / 1000.0)
                            }
                        };
                        RootRecurrence::RepeatingRate(crate::recurrence::EvaluationRate(rate))
                    }
                }
                QueryRecurrence::Unknown => RootRecurrence::Unknown,
            };
            recurrences.push(recurrence);
        }
        let update_rate = workload
            .data_workload
            .as_ref()
            .and_then(|data| data.ingestion_rate.value_at(now_ms))
            .map(|rate| UpdateRate(rate.0));
        self.recurrence_profiles(&recurrences, update_rate)
    }
}

/// Record `times` occurrences of `recurrence` against `ptr` — `times > 1`
/// when a single parent structurally references `ptr` more than once (see
/// [`PlanSpace::recurrence_profiles`]'s own doc on edge multiplicity).
/// A no-op for `times == 0` (an `Rc` returned as a `direct_child_counts`
/// child always has `edge_count >= 1` in practice, but this keeps the
/// helper correct regardless).
fn contribute(
    ptr: *const QueryExpr,
    times: usize,
    recurrence: RootRecurrence,
    intervals: &mut HashMap<*const QueryExpr, Vec<RepetitionInterval>>,
    rates: &mut HashMap<*const QueryExpr, f64>,
    one_shot_counts: &mut HashMap<*const QueryExpr, usize>,
    reached: &mut HashSet<*const QueryExpr>,
) {
    if times == 0 {
        return;
    }
    reached.insert(ptr);
    match recurrence {
        RootRecurrence::Repeating(interval) => {
            intervals
                .entry(ptr)
                .or_default()
                .extend(std::iter::repeat_n(interval, times));
        }
        RootRecurrence::RepeatingRate(rate) => {
            *rates.entry(ptr).or_insert(0.0) += rate.0 * times as f64;
        }
        RootRecurrence::OneShot => {
            *one_shot_counts.entry(ptr).or_insert(0) += times;
        }
        RootRecurrence::OneShotCount(count) => {
            *one_shot_counts.entry(ptr).or_insert(0) += count.saturating_mul(times);
        }
        RootRecurrence::Unknown => {}
    }
}

/// One [`MemoGroup`]'s candidates, ranked best-first by
/// [`PlanSpace::cost_sorted`].
#[derive(Debug)]
pub struct RankedGroup<'a> {
    pub target: &'a Rc<QueryExpr>,
    pub consumer_count: usize,
    pub candidates: Vec<&'a ReplacementSubDAG>,
    /// `costs[i]` is `candidates[i]`'s own grouping-state cost when available,
    /// and its [`CostModel::estimate_cost`] otherwise
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

    // Shape 1: the exact `SharedSubtreeStrategy` share-vs-recompute pair —
    // rank via `CostModel::cse_share_decision`, the same comparison
    // the local CSE ranking path already uses.
    if cse_candidate_pair(group).is_some() {
        if let Some(prefer_target) = cse_preference(group, cost_model) {
            ranked.sort_by_key(|c| match c.provenance {
                ReplacementProvenance::CseShare if prefer_target => 0,
                ReplacementProvenance::CseRecompute if !prefer_target => 0,
                ReplacementProvenance::CseShare | ReplacementProvenance::CseRecompute => 2,
                _ => 1,
            });
        }
        return ranked;
    }

    // Shape 2: independent and Hydra grouping alternatives for the same
    // sketch algorithms. When deployment statistics provide a subpopulation
    // estimate, compare N independent states with the shared grid directly.
    let target = TargetSubDAG::with_consumer_count(&group.target, group.consumer_count);
    let has_hydra = ranked.iter().any(|candidate| {
        let Replacement::Summary(node) = &candidate.replacement else {
            return false;
        };
        summary_grouping(node).is_some_and(|grouping| {
            matches!(grouping, GroupingStrategy::SharedMultiSubpopulation { .. })
        })
    });
    let grouping_costs: Option<Vec<f64>> = if has_hydra {
        ranked
            .iter()
            .map(|candidate| {
                cost_model
                    .grouping_state_cost(candidate, &target)
                    .map(|cost| cost.0)
            })
            .collect()
    } else {
        None
    };
    if let Some(costs) = grouping_costs {
        let by_ptr: HashMap<*const ReplacementSubDAG, f64> = ranked
            .iter()
            .zip(costs)
            .map(|(candidate, cost)| (*candidate as *const ReplacementSubDAG, cost))
            .collect();
        ranked.sort_by(|a, b| {
            by_ptr[&(*a as *const ReplacementSubDAG)]
                .total_cmp(&by_ptr[&(*b as *const ReplacementSubDAG)])
        });
        return ranked;
    }

    // Shape 3: `SketchAlgorithmStrategy`'s sketch-family candidates (every
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
            let order = crate::cost_model::validated_candidate_ranking(cost_model, intent, &kinds);
            ranked.sort_by_key(|c| {
                let kind = match &c.replacement {
                    Replacement::Summary(node) => sketch_kind_of(node),
                    Replacement::Rewrite(_) => None,
                };
                kind.and_then(|k| order.iter().position(|o| *o == k))
                    .unwrap_or(usize::MAX)
            });
            return ranked;
        }
    }

    // A target may be handled by more than one strategy (for example, a
    // shared aggregate has both bound-summary and share/recompute rewrite
    // candidates). No shape-specific hook spans those different candidate
    // types, so compare the numeric estimates the CostModel exposes for that
    // purpose. `total_cmp` gives deterministic placement to a model's NaN
    // placeholders without dropping any candidate.
    ranked.sort_by(|a, b| {
        cost_model
            .estimate_cost(a, &target)
            .total_cmp(&cost_model.estimate_cost(b, &target))
    });
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
            family: SummaryFamilyType::Sketch(kind, _),
            ..
        } => Some(kind.algorithm().clone()),
        _ => None,
    }
}

/// The grouping strategy used by a bound summary candidate, unwrapping its
/// readout node when necessary.
fn summary_grouping(node: &SummaryNode) -> Option<&GroupingStrategy> {
    match &node.expr {
        SummaryExpr::SummaryEstimate { summary_input, .. } => summary_grouping(summary_input),
        SummaryExpr::SummaryAgg { grouping, .. } => Some(grouping),
        _ => None,
    }
}

// ── global_selection ─────────────────────────────────────────────────────

/// One group's globally-selected candidate — the answer
/// [`PlanSpace::global_selection`] commits to for one site, after folding in
/// every ancestor [`SharedSubtreeStrategy`] decision on the path from a
/// workload root to this site. See the module docs' "Whole-plan
/// (cross-group) selection" section for the full recurrence.
///
/// Contrast with [`RankedGroup`] ([`PlanSpace::cost_sorted`]'s output):
/// that ranks every candidate for one group in isolation and never commits
/// to just one; this commits to exactly one (or none), and the count it
/// ranks against — [`Self::effective_consumer_count`] — can differ from the
/// group's own raw structural [`MemoGroup::consumer_count`] whenever an
/// ancestor's choice changes how many times this site truly runs. Use
/// `cost_sorted` to inspect every alternative for a site; use
/// `global_selection` when you need this module's best single answer,
/// accounting for cross-group interaction where it knows how to.
#[derive(Debug)]
pub struct SelectedGroup<'a> {
    /// The target sub-DAG this selection is for.
    pub target: &'a Rc<QueryExpr>,
    /// [`MemoGroup::consumer_count`] — how many operator-child positions
    /// directly reference `target`, ignoring every ancestor's own choice.
    pub consumer_count: usize,
    /// How many times `target`'s computation actually runs once every
    /// ancestor's own selected candidate is accounted for — see
    /// [`multiplier`]'s doc for the exact recurrence. Equal to
    /// `consumer_count` unless some ancestor on a path from a root to this
    /// site is itself a [`SharedSubtreeStrategy`] group that chose
    /// [`ShareDecision::RecomputeIndependently`].
    pub effective_consumer_count: usize,
    /// The candidate this selection committed to, or `None` for a group no
    /// registered strategy proposed anything for (mirrors
    /// [`MemoGroup::candidates`] being possibly empty).
    pub chosen: Option<&'a ReplacementSubDAG>,
}

/// [`PlanSpace::global_selection`]'s result: one [`SelectedGroup`] per
/// discovered site, in the same discovery order [`PlanSpace::groups`]/
/// [`PlanSpace::cost_sorted`] use.
#[derive(Debug)]
pub struct GlobalSelection<'a> {
    order: Vec<*const QueryExpr>,
    groups: HashMap<*const QueryExpr, SelectedGroup<'a>>,
}

impl<'a> GlobalSelection<'a> {
    /// Every selected group, in discovery order.
    pub fn groups(&self) -> impl Iterator<Item = &SelectedGroup<'a>> {
        self.order.iter().map(move |ptr| &self.groups[ptr])
    }

    /// The selection for `target`, if `target`'s own `Rc` is a discovered
    /// site (i.e. `Rc::ptr_eq` to some node reachable from the workload's
    /// roots).
    pub fn for_target(&self, target: &Rc<QueryExpr>) -> Option<&SelectedGroup<'a>> {
        self.groups.get(&Rc::as_ptr(target))
    }
}

impl<Id> PlanSpace<Id> {
    /// The whole-plan (cross-group) selection step the module docs'
    /// "Whole-plan (cross-group) selection" section describes: one
    /// [`SelectedGroup`] per discovered site, each ranked against an
    /// `effective_consumer_count` that accounts for every ancestor
    /// [`SharedSubtreeStrategy`] decision on the path to it — unlike
    /// [`Self::cost_sorted`], whose per-group ranking only ever sees a
    /// group's own raw [`MemoGroup::consumer_count`].
    pub fn global_selection(&self, cost_model: &dyn CostModel) -> GlobalSelection<'_> {
        self.global_selection_impl(cost_model, None, None)
            .expect("structural global selection cannot produce a recurrence error")
    }

    /// Recurrence-aware counterpart to [`Self::global_selection`]. The same
    /// whole-plan traversal and effective structural consumer counts are
    /// retained, while every CSE share/recompute choice is made from the
    /// corresponding recurrence profile.
    pub fn global_selection_with_recurrence(
        &self,
        cost_model: &dyn CostModel,
        profiles: &RecurrenceProfileMap,
        horizon: Option<Horizon>,
    ) -> Result<GlobalSelection<'_>, RecurrenceError> {
        self.global_selection_impl(cost_model, Some(profiles), horizon)
    }

    fn global_selection_impl(
        &self,
        cost_model: &dyn CostModel,
        profiles: Option<&RecurrenceProfileMap>,
        horizon: Option<Horizon>,
    ) -> Result<GlobalSelection<'_>, RecurrenceError> {
        let graph = reference_graph(self);
        let topo = topological_order(&self.order, &graph);

        let mut effective_uses = graph.external_root_uses.clone();
        let mut chosen_share: HashMap<*const QueryExpr, ShareDecision> = HashMap::new();
        let mut groups: HashMap<*const QueryExpr, SelectedGroup<'_>> = HashMap::new();

        for ptr in &topo {
            let group = &self.groups[ptr];

            let effective = effective_uses.get(ptr).copied().unwrap_or(0);
            effective_uses.insert(*ptr, effective);

            let chosen = if effective >= 2 && cse_candidate_pair(group).is_some() {
                let decision = if let Some(profiles) = profiles {
                    decide_group_with_recurrence(
                        group,
                        effective,
                        profiles.for_target(&group.target),
                        horizon,
                        cost_model,
                    )?
                } else {
                    decide_with_effective_count(group, effective, cost_model)
                };
                match decision {
                    Some(decision) => {
                        let cse = pick_shared_subtree_candidate(group, decision);
                        let effective_target =
                            TargetSubDAG::with_consumer_count(&group.target, effective);
                        let logical = group
                            .candidates
                            .iter()
                            .filter(|candidate| !is_cse_candidate(candidate))
                            .min_by(|a, b| {
                                cost_model
                                    .estimate_cost(a, &effective_target)
                                    .total_cmp(&cost_model.estimate_cost(b, &effective_target))
                            });
                        match (cse, logical) {
                            (Some(cse), Some(logical))
                                if cost_model
                                    .estimate_cost(logical, &effective_target)
                                    .total_cmp(&cost_model.estimate_cost(cse, &effective_target))
                                    .is_lt() =>
                            {
                                Some(logical)
                            }
                            (cse, _) => {
                                if cse.is_some() {
                                    chosen_share.insert(*ptr, decision);
                                }
                                cse
                            }
                        }
                    }
                    // `realize_child` couldn't produce even a logical fallback —
                    // not expected in practice for a target that's already
                    // part of a legitimate workload tree (mirrors
                    // `cse_preference`'s own doc on this same degrade).
                    // Falling back to ordinary local ranking is still a
                    // valid answer, just not a cross-group-aware one; this
                    // group also contributes no Share collapse to its own
                    // children (see `multiplier`'s `_ => effective` arm).
                    None => rank_group(group, cost_model).into_iter().next(),
                }
            } else {
                rank_group(group, cost_model)
                    .into_iter()
                    .find(|candidate| !is_cse_candidate(candidate))
                    .or_else(|| cse_candidate_pair(group).map(|(share, _)| share))
            };

            let outgoing_multiplier = multiplier(*ptr, &effective_uses, &chosen_share);
            match chosen {
                Some(ReplacementSubDAG {
                    replacement: Replacement::Rewrite(source),
                    provenance: ReplacementProvenance::AccuracyReconciliation,
                    ..
                }) => {
                    // Accuracy reconciliation reads another discovered memo
                    // group, rather than inlining that group's children. Let
                    // the source group receive the uses and propagate them
                    // through its own selected implementation when its turn
                    // arrives in topological order.
                    *effective_uses.entry(Rc::as_ptr(source)).or_insert(0) += outgoing_multiplier;
                }
                _ => {
                    let selected_rewrite = match chosen.map(|candidate| &candidate.replacement) {
                        Some(Replacement::Rewrite(rewrite)) => rewrite,
                        Some(Replacement::Summary(_)) | None => &group.target,
                    };
                    for (child, edge_count) in direct_child_counts(selected_rewrite) {
                        *effective_uses.entry(child).or_insert(0) +=
                            edge_count * outgoing_multiplier;
                    }
                }
            }

            groups.insert(
                *ptr,
                SelectedGroup {
                    target: &group.target,
                    consumer_count: group.consumer_count,
                    effective_consumer_count: effective,
                    chosen,
                },
            );
        }

        Ok(GlobalSelection {
            order: self.order.clone(),
            groups,
        })
    }
}

fn is_cse_candidate(candidate: &ReplacementSubDAG) -> bool {
    matches!(
        candidate.provenance,
        ReplacementProvenance::CseShare | ReplacementProvenance::CseRecompute
    )
}

/// How much one direct reference to `parent_ptr` actually costs, once
/// `parent_ptr`'s own chosen candidate (if it has a Share/Recompute pair at
/// all) is taken into account:
///
/// - `1`, if `parent_ptr` chose [`ShareDecision::Share`] — one shared
///   execution backs every reference to it, so referencing it costs no more
///   than referencing it once.
/// - `parent_ptr`'s own `effective_consumer_count` otherwise — either it
///   chose [`ShareDecision::RecomputeIndependently`] (each of its own uses
///   gets its own independent execution, so referencing it costs as much as
///   its *own* full multiplicity), or it has no Share/Recompute decision at
///   all (not a [`SharedSubtreeStrategy`] shape — nothing here collapses
///   its multiplicity to one, so whatever multiplicity *its* ancestors
///   established simply passes through).
///
/// Composing this recurrence transitively up the whole ancestor chain (not
/// just the immediate parent) is exactly what makes
/// [`PlanSpace::global_selection`]'s `effective_consumer_count` differ from
/// [`MemoGroup::consumer_count`] whenever a `RecomputeIndependently`
/// ancestor sits anywhere on the path from a root to a site — see the
/// module docs' "Whole-plan (cross-group) selection" section.
fn multiplier(
    parent_ptr: *const QueryExpr,
    effective_uses: &HashMap<*const QueryExpr, usize>,
    chosen_share: &HashMap<*const QueryExpr, ShareDecision>,
) -> usize {
    let effective = *effective_uses.get(&parent_ptr).expect(
        "topological_order guarantees a parent is processed (and its effective_consumer_count \
         recorded) before any of its children",
    );
    match chosen_share.get(&parent_ptr) {
        Some(ShareDecision::Share) => 1,
        _ => effective,
    }
}

/// Find the explicitly-tagged CSE share/recompute pair inside `group`, even
/// when other strategies contributed additional alternatives to the same
/// memo group. Provenance makes these two orthogonal choices identifiable
/// without inferring semantics from pointer or expression shape.
fn cse_candidate_pair(group: &MemoGroup) -> Option<(&ReplacementSubDAG, &ReplacementSubDAG)> {
    let mut share = None;
    let mut recompute = None;
    for candidate in &group.candidates {
        match candidate.provenance {
            ReplacementProvenance::CseShare => {
                let Replacement::Rewrite(rc) = &candidate.replacement else {
                    return None;
                };
                if !Rc::ptr_eq(rc, &group.target) || share.replace(candidate).is_some() {
                    return None;
                }
            }
            ReplacementProvenance::CseRecompute => {
                let Replacement::Rewrite(rc) = &candidate.replacement else {
                    return None;
                };
                if Rc::ptr_eq(rc, &group.target)
                    || rc.as_ref() != group.target.as_ref()
                    || recompute.replace(candidate).is_some()
                {
                    return None;
                }
            }
            _ => {}
        }
    }
    Some((share?, recompute?))
}

/// [`CostModel::cse_share_decision`] for `group`, against an explicit
/// `effective_consumer_count` instead of `group.consumer_count` — the
/// cross-group-aware counterpart to [`cse_preference`], which uses the raw
/// structural count. `None` only when [`realize_child`] can't produce even a
/// logical fallback for `group.target` (see that function's own doc).
fn decide_with_effective_count(
    group: &MemoGroup,
    effective_consumer_count: usize,
    cost_model: &dyn CostModel,
) -> Option<ShareDecision> {
    let bound = realize_child(&group.target, cost_model).ok()?;
    let candidate = CseCandidate {
        subtree: &group.target,
        bound_summary: &bound,
        consumer_count: effective_consumer_count,
    };
    Some(cost_model.cse_share_decision(&candidate))
}

fn decide_group_with_recurrence(
    group: &MemoGroup,
    effective_consumer_count: usize,
    recurrence: RecurrenceProfile,
    horizon: Option<Horizon>,
    cost_model: &dyn CostModel,
) -> Result<Option<ShareDecision>, RecurrenceError> {
    let Some(bound) = realize_child(&group.target, cost_model).ok() else {
        return Ok(None);
    };
    let candidate = CseCandidate {
        subtree: &group.target,
        bound_summary: &bound,
        consumer_count: effective_consumer_count,
    };
    Ok(Some(
        cost_model
            .cse_share_decision_with_recurrence(&candidate, &recurrence, horizon)?
            .decision,
    ))
}

/// The [`SharedSubtreeStrategy`] candidate matching `decision`: the one
/// that shares `group.target`'s own `Rc` for [`ShareDecision::Share`], the
/// freshly-allocated one for [`ShareDecision::RecomputeIndependently`] —
/// the same `Rc`-identity distinction [`is_duplicate_rewrite`]'s own doc
/// explains is the *only* signal this IR carries for that choice.
fn pick_shared_subtree_candidate(
    group: &MemoGroup,
    decision: ShareDecision,
) -> Option<&ReplacementSubDAG> {
    let (share, recompute) = cse_candidate_pair(group)?;
    Some(match decision {
        ShareDecision::Share => share,
        ShareDecision::RecomputeIndependently => recompute,
    })
}

// ── reference graph + topological order ─────────────────────────────────

/// The parent/child structure [`PlanSpace::global_selection`]'s DP walks —
/// built separately from [`discover_targets`]'s own `order`/`nodes`/`counts`
/// maps (which only track *aggregate* reference counts, not per-parent
/// breakdown or direction) rather than extending that already-reviewed,
/// already-tested pass. Same "small duplicated traversal over reshaping
/// proven code" call as [`is_shared_subtree_group`].
struct ReferenceGraph {
    /// child ptr -> `(parent ptr, edge count from that one parent)`, for
    /// every direct operator-child edge in the relational-skeleton scope
    /// [`walk_children`] itself uses (an edge count above 1 happens when
    /// one parent references the same child from two different fields,
    /// e.g. a `Join`'s `left`/`right` both being the same `Rc`).
    parents_of: HashMap<*const QueryExpr, Vec<(*const QueryExpr, usize)>>,
    /// parent ptr -> every distinct child ptr it directly references — the
    /// reverse of `parents_of`, for [`topological_order`]'s Kahn's-algorithm
    /// traversal.
    children_of: HashMap<*const QueryExpr, Vec<*const QueryExpr>>,
    /// How many of the workload's own `roots` point directly at each node —
    /// a node's "external" use. Nothing inside the tree decides this (it
    /// isn't a reference from another discovered site), so it's never
    /// subject to any ancestor's Share/Recompute choice — it's the base
    /// case [`PlanSpace::global_selection`]'s recurrence starts from.
    external_root_uses: HashMap<*const QueryExpr, usize>,
}

/// Build an ordering graph containing every edge that could be selected:
/// the original target's edges plus every rewrite candidate's edges. An
/// accuracy-reconciliation rewrite points at another discovered memo group,
/// so it contributes an edge to that group itself; other rewrites contribute
/// their relational children as before. The
/// graph is deliberately only used for topological ordering; effective-use
/// counts are propagated through the one candidate actually selected.
fn reference_graph<Id>(space: &PlanSpace<Id>) -> ReferenceGraph {
    let mut graph = ReferenceGraph {
        parents_of: HashMap::new(),
        children_of: HashMap::new(),
        external_root_uses: HashMap::new(),
    };
    for (_, root) in &space.roots {
        *graph
            .external_root_uses
            .entry(Rc::as_ptr(root))
            .or_insert(0) += 1;
    }
    for ptr in &space.order {
        let group = &space.groups[ptr];
        record_possible_edges(*ptr, &group.target, &mut graph);
        for candidate in &group.candidates {
            if let Replacement::Rewrite(rewrite) = &candidate.replacement {
                if candidate.provenance == ReplacementProvenance::AccuracyReconciliation {
                    add_edge(*ptr, Rc::as_ptr(rewrite), 1, &mut graph);
                } else {
                    record_possible_edges(*ptr, rewrite, &mut graph);
                }
            }
        }
    }
    graph
}

/// Record one `parent_ptr -> child` edge (both directions — see
/// [`ReferenceGraph`]'s fields), retaining the greatest multiplicity seen
/// when the target and alternative rewrites expose the same edge.
fn add_edge(
    parent_ptr: *const QueryExpr,
    child_ptr: *const QueryExpr,
    edge_count: usize,
    graph: &mut ReferenceGraph,
) {
    let siblings = graph.parents_of.entry(child_ptr).or_default();
    match siblings.iter_mut().find(|(p, _)| *p == parent_ptr) {
        Some((_, count)) => *count = (*count).max(edge_count),
        None => siblings.push((parent_ptr, edge_count)),
    }
    let kids = graph.children_of.entry(parent_ptr).or_default();
    if !kids.contains(&child_ptr) {
        kids.push(child_ptr);
    }
}

fn record_possible_edges(
    parent_ptr: *const QueryExpr,
    node: &QueryExpr,
    graph: &mut ReferenceGraph,
) {
    for (child_ptr, edge_count) in direct_child_counts(node) {
        add_edge(parent_ptr, child_ptr, edge_count, graph);
    }
}

/// Direct relational-skeleton children and their edge multiplicities.
/// `Concat` is transparent, matching [`walk_children`]'s site scope.
fn direct_child_counts(node: &QueryExpr) -> Vec<(*const QueryExpr, usize)> {
    fn push(children: &mut Vec<(*const QueryExpr, usize)>, child: &Rc<QueryExpr>) {
        let ptr = Rc::as_ptr(child);
        match children.iter_mut().find(|(existing, _)| *existing == ptr) {
            Some((_, count)) => *count += 1,
            None => children.push((ptr, 1)),
        }
    }

    fn collect(node: &QueryExpr, children: &mut Vec<(*const QueryExpr, usize)>) {
        use QueryExpr::*;
        match node {
            Scan { .. } | PromqlScalarBridge(_) | EvalTimestamp | CurrentTimestamp => {}
            PromqlVectorFromScalar(c) | PromqlScalarFromVector(c) => {
                push(children, c);
            }
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
            | Limit { child, .. } => {
                push(children, child);
            }
            Concat {
                children: concat_children,
            } => {
                for c in concat_children {
                    collect(c, children);
                }
            }
            Join { left, right, .. } | SetOp { left, right, .. } => {
                push(children, left);
                push(children, right);
            }
            BinaryOp { lhs, rhs, .. } => {
                push(children, lhs);
                push(children, rhs);
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

    let mut children = Vec::new();
    collect(node, &mut children);
    children
}

/// A topological order over `order` (parent before every child) via Kahn's
/// algorithm on `graph`'s reverse adjacency — needed because
/// [`discover_targets`]'s own `order` is only a valid *discovery* order
/// (first-seen-first), not a valid topological one: a node reached via two
/// different root paths can have a parent that's discovered *after* it (see
/// this function's own test for a worked diamond example), which is exactly
/// backwards for [`PlanSpace::global_selection`]'s recurrence.
fn topological_order(order: &[*const QueryExpr], graph: &ReferenceGraph) -> Vec<*const QueryExpr> {
    let mut in_degree: HashMap<*const QueryExpr, usize> = HashMap::new();
    for ptr in order {
        let degree = graph.parents_of.get(ptr).map(Vec::len).unwrap_or(0);
        in_degree.insert(*ptr, degree);
    }

    let mut queue: VecDeque<*const QueryExpr> = order
        .iter()
        .copied()
        .filter(|ptr| in_degree[ptr] == 0)
        .collect();

    let mut topo = Vec::with_capacity(order.len());
    while let Some(ptr) = queue.pop_front() {
        topo.push(ptr);
        if let Some(children) = graph.children_of.get(&ptr) {
            for child in children {
                if let Some(degree) = in_degree.get_mut(child) {
                    *degree -= 1;
                    if *degree == 0 {
                        queue.push_back(*child);
                    }
                }
            }
        }
    }

    assert_eq!(
        topo.len(),
        order.len(),
        "topological_order: the discovered-site reference graph has a cycle — every QueryExpr \
         node is built from Rc children, which can't form one, so this indicates a bug in \
         reference_graph rather than a real cyclic workload",
    );
    topo
}

// ── default_strategies ──────────────────────────────────────────────────

/// The context-free strategies [`search_workload`] runs in the built-in
/// [`DefaultCostModel`] configuration. Workload-dependent strategies such as
/// [`RollupStrategy`] and [`AccuracyReconciliationStrategy`] (issue #273,
/// cross-consumer accuracy reconciliation for CSE sharing — see that
/// module's own docs) are added by [`search_workload`] after CSE and target
/// discovery, when their sibling context exists.
/// [`crate::explanation::explain_replacements`] (issue #257) uses
/// this same set (via [`search_workload`]) rather than keeping a second,
/// explanation-specific list to stay in sync with. Use
/// [`default_strategies_with`] to plug in a deployment-specific
/// [`CostModel`] instead.
///
/// [`AvgToSumOverCountStrategy`](crate::rewrite::AvgToSumOverCountStrategy) is
/// included here (issue #253) even though it's a
/// [`Replacement::Rewrite`]-only strategy with no [`CostModel`] of its own to
/// plug in — it's context-free (`matches`/`replacements` need nothing beyond
/// the target itself) exactly like [`SharedSubtreeStrategy`], so it belongs
/// in this list rather than being derived per-workload the way
/// [`RollupStrategy`] is. Rewriting `avg` into `sum`/`count` upfront is what
/// lets [`SketchAlgorithmStrategy`] and [`SharedSubtreeStrategy`] see a
/// mergeable accumulator to sketch or share at all — see that module's own
/// doc comment for why a bare `avg` node otherwise never becomes a
/// [`ReplacementStrategy`] target for anything.
pub fn default_strategies() -> Vec<Box<dyn ReplacementStrategy>> {
    vec![
        Box::new(SketchAlgorithmStrategy::default_cost_model()),
        Box::new(HydraGroupingStrategy::default_cost_model()),
        Box::new(SharedSubtreeStrategy),
        Box::new(crate::rewrite::AvgToSumOverCountStrategy),
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
        Box::new(HydraGroupingStrategy::new(cost_model)),
        Box::new(SharedSubtreeStrategy),
        Box::new(crate::rewrite::AvgToSumOverCountStrategy),
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

/// Like [`search_workload`], but with an explicit set of context-free
/// `strategies` (see [`default_strategies_with`] to plug in a
/// deployment-specific [`CostModel`]). The workload-dependent
/// [`RollupStrategy`] is derived and added automatically after CSE for both
/// entry points, because only this function owns the post-CSE sibling set.
///
/// Runs [`share_common_subtrees`] once over `roots` first — so every
/// strategy (and, transitively, every
/// [`crate::explanation::ReplacementExplanation`] a caller reads off the
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
    search_cse_workload_with(cse_workload(roots), strategies)
}

/// [`search_workload_with`] plus a per-root end-to-end `AccuracyTarget`
/// (issue #172) — the workload's `QueryRequirements.accuracy`, threaded
/// alongside each root. After the search, every root that carries a target
/// has its group's bound [`Replacement::Summary`] candidates checked with
/// `accuracy_model`'s [`AccuracyModel::satisfies`]: a candidate whose
/// guarantee is absent (unknown) or misses the target is moved from
/// [`MemoGroup::candidates`] to [`MemoGroup::rejected`] *before*
/// [`PlanSpace::cost_sorted`]/[`PlanSpace::global_selection`] ever rank the
/// group, so a `CostModel` cannot pick it. A `KeepPreAsap` candidate is
/// exact and always survives — the raw/pre-ASAP alternative is what an
/// unsatisfiable root keeps. Logical [`Replacement::Rewrite`] candidates
/// are not bound values and are left alone; the targets *inside* a rewrite
/// are their own groups.
///
/// Precedence against per-node `AggIntent.accuracy` is documented in
/// [`crate::accuracy`]'s module docs.
pub fn search_workload_with_targets<'s, Id>(
    roots: Vec<(Id, Rc<QueryExpr>, Option<AccuracyTarget>)>,
    strategies: &[Box<dyn ReplacementStrategy + 's>],
    accuracy_model: &dyn AccuracyModel,
) -> PlanSpace<Id> {
    let mut targets = Vec::with_capacity(roots.len());
    let roots = roots
        .into_iter()
        .map(|(id, root, target)| {
            targets.push(target);
            (id, root)
        })
        .collect();
    let mut space = search_workload_with(roots, strategies);
    // `cse_workload` preserves root order, so targets zip by position.
    let root_ptrs: Vec<(*const QueryExpr, AccuracyTarget)> = space
        .roots
        .iter()
        .zip(targets)
        .filter_map(|((_, root), target)| target.map(|t| (Rc::as_ptr(root), t)))
        .collect();
    for (ptr, target) in root_ptrs {
        let Some(group) = space.groups.get_mut(&ptr) else {
            continue;
        };
        let (legal, illegal): (Vec<_>, Vec<_>) =
            group
                .candidates
                .drain(..)
                .partition(|candidate| match &candidate.replacement {
                    Replacement::Summary(node) => node
                        .guarantee
                        .as_ref()
                        .is_some_and(|g| accuracy_model.satisfies(g, &target)),
                    Replacement::Rewrite(_) => true,
                });
        group.candidates = legal;
        group.rejected.extend(illegal.into_iter().map(|candidate| {
            let (metric, bound, failure_probability) = match &candidate.replacement {
                Replacement::Summary(node) => node
                    .guarantee
                    .as_ref()
                    .map(|g| {
                        (
                            g.metric,
                            g.bound.evaluate(),
                            g.failure_probability.evaluate(),
                        )
                    })
                    .unwrap_or((
                        asap_types::post_asap::ErrorMetric::AbsoluteValue,
                        None,
                        None,
                    )),
                Replacement::Rewrite(_) => unreachable!("rewrites are never rejected here"),
            };
            RejectedCandidate {
                strategy: candidate.strategy,
                description: format!("{} (root end-to-end target check)", candidate.rationale),
                error: AccuracyError::TargetNotSatisfied {
                    metric,
                    bound,
                    failure_probability,
                    target: target.clone(),
                },
            }
        }));
    }
    space
}

fn cse_workload<Id>(roots: Vec<(Id, Rc<QueryExpr>)>) -> Vec<(Id, Rc<QueryExpr>)> {
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
    share_common_subtrees(owned_roots)
}

fn search_cse_workload_with<'s, Id>(
    cse_roots: Vec<(Id, Rc<QueryExpr>)>,
    strategies: &[Box<dyn ReplacementStrategy + 's>],
) -> PlanSpace<Id> {
    let mut order = Vec::new();
    let mut nodes = HashMap::new();
    let mut counts: HashMap<*const QueryExpr, usize> = HashMap::new();
    discover_targets(&cse_roots, &mut order, &mut nodes, &mut counts);
    let siblings: Vec<Rc<QueryExpr>> = order
        .iter()
        .filter_map(|ptr| {
            let node = &nodes[ptr];
            matches!(node.as_ref(), QueryExpr::Aggregate { .. }).then(|| Rc::clone(node))
        })
        .collect();
    let rollup_strategy = RollupStrategy::new(&siblings);
    let accuracy_reconciliation_strategy = AccuracyReconciliationStrategy::new(&siblings);
    let limits: Vec<Rc<QueryExpr>> = order
        .iter()
        .filter_map(|ptr| {
            let node = &nodes[ptr];
            matches!(node.as_ref(), QueryExpr::Limit { .. }).then(|| Rc::clone(node))
        })
        .collect();
    let topk_reuse_strategy = TopKLimitReuseStrategy::new(&limits);

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
            let mut rejected = Vec::new();
            for strategy in strategies {
                if strategy.matches(&target) {
                    let name = strategy.name();
                    let proposals = strategy.propose(&target);
                    proposed.extend(proposals.candidates.into_iter().map(|mut candidate| {
                        candidate.strategy = name;
                        candidate
                    }));
                    rejected.extend(proposals.rejected.into_iter().map(|mut rejection| {
                        rejection.strategy = name;
                        rejection
                    }));
                }
            }
            if rollup_strategy.matches(&target) {
                let name = rollup_strategy.name();
                proposed.extend(rollup_strategy.replacements(&target).into_iter().map(
                    |mut candidate| {
                        candidate.strategy = name;
                        candidate
                    },
                ));
            }
            if accuracy_reconciliation_strategy.matches(&target) {
                let name = accuracy_reconciliation_strategy.name();
                proposed.extend(
                    accuracy_reconciliation_strategy
                        .replacements(&target)
                        .into_iter()
                        .map(|mut candidate| {
                            candidate.strategy = name;
                            candidate
                        }),
                );
            }
            if topk_reuse_strategy.matches(&target) {
                let name = topk_reuse_strategy.name();
                proposed.extend(topk_reuse_strategy.replacements(&target).into_iter().map(
                    |mut candidate| {
                        candidate.strategy = name;
                        candidate
                    },
                ));
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
            group.rejected.extend(rejected);
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

    add_effective_count_cse_candidates(&order, &mut groups);

    PlanSpace {
        roots: cse_roots,
        groups,
        order,
    }
}

/// Materialize share/recompute alternatives for descendants whose raw edge
/// count is one but whose effective count can exceed one when a repeated
/// ancestor is recomputed. We only do this when an ordinary repeated group
/// proves that `SharedSubtreeStrategy` is part of this search's strategy set.
fn add_effective_count_cse_candidates(
    order: &[*const QueryExpr],
    groups: &mut HashMap<*const QueryExpr, MemoGroup>,
) {
    let mut possible_children: HashMap<*const QueryExpr, Vec<*const QueryExpr>> = HashMap::new();
    for ptr in order {
        let group = &groups[ptr];
        let children = possible_children.entry(*ptr).or_default();
        for (child, _) in direct_child_counts(&group.target) {
            if !children.contains(&child) {
                children.push(child);
            }
        }
        for candidate in &group.candidates {
            if let Replacement::Rewrite(rewrite) = &candidate.replacement {
                for (child, _) in direct_child_counts(rewrite) {
                    if !children.contains(&child) {
                        children.push(child);
                    }
                }
            }
        }
    }

    let mut potentially_repeated = HashSet::new();
    let mut queue = VecDeque::new();
    for ptr in order {
        let group = &groups[ptr];
        if group.consumer_count >= 2 && cse_candidate_pair(group).is_some() {
            potentially_repeated.insert(*ptr);
            queue.push_back(*ptr);
        }
    }
    while let Some(parent) = queue.pop_front() {
        if let Some(children) = possible_children.get(&parent) {
            for child in children {
                if groups.contains_key(child) && potentially_repeated.insert(*child) {
                    queue.push_back(*child);
                }
            }
        }
    }

    for ptr in order {
        let group = groups
            .get_mut(ptr)
            .expect("every discovered site has a group");
        if potentially_repeated.contains(ptr) && cse_candidate_pair(group).is_none() {
            let target = Rc::clone(&group.target);
            let site = TargetSubDAG::with_consumer_count(&target, 2);
            for mut candidate in SharedSubtreeStrategy.replacements(&site) {
                candidate.rationale = format!(
                    "{}: this subtree can become repeated when a repeated ancestor is recomputed; \
                     global_selection decides using its effective consumer count",
                    match candidate.provenance {
                        ReplacementProvenance::CseShare => "build once and share",
                        ReplacementProvenance::CseRecompute => "recompute independently",
                        _ => unreachable!("SharedSubtreeStrategy only emits CSE candidates"),
                    }
                );
                group.add_candidate(candidate);
            }
        }
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
        Scan { .. } | PromqlScalarBridge(_) | EvalTimestamp | CurrentTimestamp => {}
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
    use crate::accuracy::PropagationStats;
    use crate::cost_model::Cost;
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
            Implementation::Sketch(SketchKind::new(
                SketchAlgorithm::Kll,
                SketchParams::Kll { k: 269 },
            ))
        );

        let looser = AggIntent::Quantile {
            col: None,
            q: 0.99,
            accuracy: eps(0.05),
        };
        assert_eq!(
            preferred(&looser),
            Implementation::Sketch(SketchKind::new(
                SketchAlgorithm::Kll,
                SketchParams::Kll { k: 52 },
            ))
        );
    }

    #[test]
    fn default_cardinality_sizes_hll_to_its_rse_magnitude() {
        assert_eq!(
            preferred(&default_cardinality()),
            Implementation::Sketch(SketchKind::new(
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
            Implementation::Sketch(SketchKind::new(
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
            Implementation::Sketch(SketchKind::new(
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
            Implementation::Sketch(kind) if kind.algorithm() == &SketchAlgorithm::CmsWithHeap => {
                let SketchParams::CmsWithHeap {
                    width,
                    depth,
                    heap_size,
                } = kind.params()
                else {
                    unreachable!("SketchKind validates CmsWithHeap params")
                };
                assert_eq!(*heap_size, 25);
                assert_eq!(*width, 272); // ⌈e/0.01⌉
                assert_eq!(*depth, 5);
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
            Implementation::Sketch(SketchKind::new(
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
    fn posterior_aware_sizing_does_not_apply_cms_l1_relaxation_to_count_sketch() {
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
            default_size_params(
                SketchAlgorithm::CountSketch,
                &count_intent(0.01),
                0.01,
                0.01
            ),
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
    fn cardinality_epsilon_keeps_hll_but_epsilon_delta_rejects_unknown_confidence() {
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
            ]
        );

        let q = Rc::new(agg(
            vec![2],
            AggIntent::Cardinality {
                col: None,
                accuracy: AccuracyTarget::EpsilonDelta {
                    epsilon: 0.01,
                    delta: 0.01,
                },
            },
            metric_scan(&["job"]),
        ));
        let kinds: Vec<_> = SketchAlgorithmStrategy::default_cost_model()
            .replacements(&TargetSubDAG::new(&q))
            .iter()
            .map(|r| match &r.replacement {
                Replacement::Summary(node) => summary_family_algorithm(node),
                Replacement::Rewrite(_) => panic!("expected a Summary replacement"),
            })
            .collect();
        assert_eq!(kinds, vec![SketchAlgorithm::Theta, SketchAlgorithm::Kmv]);
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
        //
        // Rank-over-rank has no registered rule in `DefaultAccuracyModel`
        // (issue #172 — see `approximate_over_approximate_is_rejected_by_default`),
        // so this test injects `RankAdditiveModel` to admit the composition
        // and keep exercising the per-node enumeration property it is about.
        let inner = agg(vec![2], default_quantile(0.5), metric_scan(&["job"]));
        let outer = Rc::new(agg(vec![], default_quantile(0.99), inner));
        let target = TargetSubDAG::new(&outer);
        let replacements = SketchAlgorithmStrategy::with_models(
            &DefaultCostModel,
            &RankAdditiveModel,
            &EqualSplitAllocator,
        )
        .replacements(&target);

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
                asap_types::post_asap::SummaryFamilyType::Sketch(kind, _) => {
                    kind.algorithm().clone()
                }
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
                Scan { .. } | PromqlScalarBridge(_) | EvalTimestamp | CurrentTimestamp => {}
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
        // Rc (mirrors `explanation`'s and `cse`'s own fixtures): a grouped
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
    fn single_bindable_aggregate_excludes_unprovable_hydra_candidates() {
        let intent = AggIntent::Count {
            accuracy: AccuracyTarget::EpsilonDelta {
                epsilon: 0.01,
                delta: 0.01,
            },
        };
        let root = Rc::new(agg(vec![2], intent, metric_scan(&["job"])));
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
            "only independent CMS/CountSketch candidates have modeled guarantees: {:?}",
            agg_group.candidates
        );
        assert!(agg_group
            .candidates
            .iter()
            .all(|c| matches!(c.replacement, Replacement::Summary(_))));
        assert_eq!(
            agg_group
                .candidates
                .iter()
                .filter(|candidate| {
                    let Replacement::Summary(node) = &candidate.replacement else {
                        return false;
                    };
                    let SummaryExpr::SummaryEstimate { summary_input, .. } = &node.expr else {
                        return false;
                    };
                    matches!(
                        &summary_input.expr,
                        SummaryExpr::SummaryAgg {
                            grouping: GroupingStrategy::SharedMultiSubpopulation { .. },
                            ..
                        }
                    )
                })
                .count(),
            0,
            "Hydra shared-grid error is unmodeled, so accuracy-targeted candidates must be absent"
        );

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
        assert!(matches!(
            &ranked_group.candidates[0].replacement,
            Replacement::Rewrite(rc) if Rc::ptr_eq(rc, &group.target)
        ));
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

    #[test]
    fn grouping_cost_cannot_resurrect_unprovable_hydra_candidates() {
        struct EstimatedSubpopulations(usize);

        impl CostModel for EstimatedSubpopulations {
            fn rank_candidates(
                &self,
                _intent: &AggIntent,
                candidates: &[SketchAlgorithm],
            ) -> Vec<SketchAlgorithm> {
                candidates.to_vec()
            }

            fn estimated_subpopulation_count(&self, _target: &QueryExpr) -> Option<usize> {
                Some(self.0)
            }
        }

        fn first_grouping(estimated_count: usize) -> GroupingStrategy {
            let model = EstimatedSubpopulations(estimated_count);
            let intent = AggIntent::Count {
                accuracy: AccuracyTarget::EpsilonDelta {
                    epsilon: 0.01,
                    delta: 0.01,
                },
            };
            let root = Rc::new(agg(
                vec![2, 3],
                intent,
                metric_scan(&["tenant_id", "endpoint"]),
            ));
            let strategies = default_strategies_with(&model);
            let space = search_workload_with(vec![("tenant_endpoint_count", root)], &strategies);
            let ranked = space.cost_sorted(&model);
            let aggregate = ranked
                .iter()
                .find(|group| matches!(group.target.as_ref(), QueryExpr::Aggregate { .. }))
                .expect("aggregate group");
            let Replacement::Summary(node) = &aggregate.candidates[0].replacement else {
                panic!("grouping candidate must be a summary")
            };
            summary_grouping(node)
                .expect("bound summary grouping")
                .clone()
        }

        assert_eq!(
            first_grouping(10_000),
            GroupingStrategy::PerSubpopulationInstance
        );
        assert_eq!(
            first_grouping(10),
            GroupingStrategy::PerSubpopulationInstance
        );
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

    // ── global_selection (issue #271) ───────────────────────────────────

    /// A `CostModel` with a constant, `subtree`-independent recompute cost
    /// and shared-maintenance cost, chosen (40 recompute-per-use, 100
    /// maintenance) so that a `SharedSubtreeStrategy` group's
    /// `cse_share_decision` flips exactly between a consumer count of 2
    /// (recompute total 80, below maintenance: `RecomputeIndependently`)
    /// and a consumer count of 3 (recompute total 120, above
    /// maintenance: `Share`) — the precise threshold
    /// `effective_consumer_count_corrects_a_nested_groups_share_decision`
    /// needs to cross.
    struct ConstantCseCost;
    impl CostModel for ConstantCseCost {
        fn rank_candidates(
            &self,
            _intent: &AggIntent,
            candidates: &[SketchAlgorithm],
        ) -> Vec<SketchAlgorithm> {
            candidates.to_vec()
        }
        fn cse_recompute_cost(&self, _candidate: &CseCandidate) -> Cost {
            Cost(40.0)
        }
        fn cse_shared_maintenance_cost(&self, _candidate: &CseCandidate) -> Cost {
            Cost(100.0)
        }
    }

    #[test]
    fn global_selection_matches_cost_sorted_for_a_non_interacting_workload() {
        // No nested sharing at all — global_selection's effective_consumer_count
        // must equal the group's own raw consumer_count, and its `chosen`
        // candidate must be cost_sorted's top pick, for both the sketch
        // group and its child Scan.
        let root = Rc::new(agg(vec![2], default_quantile(0.99), metric_scan(&["job"])));
        let space = search_workload(vec![("q", root)]);

        let ranked = space.cost_sorted(&DefaultCostModel);
        let selected = space.global_selection(&DefaultCostModel);
        assert_eq!(ranked.len(), selected.groups().count());

        for ranked_group in &ranked {
            let selected_group = selected.for_target(ranked_group.target).unwrap();
            assert_eq!(
                selected_group.effective_consumer_count, ranked_group.consumer_count,
                "no ancestor is ever RecomputeIndependently here, so effective must equal raw"
            );
            assert_eq!(
                selected_group.chosen.map(|c| &c.rationale),
                ranked_group.candidates.first().map(|c| &c.rationale),
                "with no cross-group interaction, global_selection's pick must match \
                 cost_sorted's top-ranked candidate"
            );
        }
    }

    #[test]
    fn global_selection_leaves_an_unmatched_group_as_none() {
        // A bare Scan: no registered strategy has an opinion on it, so it
        // gets a group with an empty candidate list (see MemoGroup's own
        // doc) — global_selection must not invent a candidate for it.
        let root = Rc::new(metric_scan(&["job"]));
        let space = search_workload(vec![("q", root)]);
        let selected = space.global_selection(&DefaultCostModel);
        let scan_group = selected
            .groups()
            .find(|g| matches!(g.target.as_ref(), QueryExpr::Scan { .. }))
            .unwrap();
        assert!(scan_group.chosen.is_none());
        assert_eq!(scan_group.effective_consumer_count, 1);
    }

    #[test]
    fn global_selection_falls_back_to_local_ranking_for_sketch_family_groups() {
        // SketchAlgorithmStrategy groups have no cross-group-aware cost hook
        // (rank_candidates takes no consumer_count) — global_selection must
        // still return cost_sorted's own top pick for them (documented in
        // the module docs' "Whole-plan (cross-group) selection" section),
        // not silently drop the candidate or fall back to discovery order.
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
        let selected = space.global_selection(&PreferDDSketch);
        let agg_group = selected
            .groups()
            .find(|g| matches!(g.target.as_ref(), QueryExpr::Aggregate { .. }))
            .unwrap();
        let kind = match &agg_group.chosen.unwrap().replacement {
            Replacement::Summary(node) => sketch_kind_of(node),
            Replacement::Rewrite(_) => None,
        };
        assert_eq!(kind, Some(SketchAlgorithm::DDSketch));
    }

    #[test]
    fn mixed_rewrite_group_keeps_and_selects_its_explicit_cse_pair() {
        let target = Rc::new(metric_scan(&["job"]));
        let mut group = MemoGroup::new(Rc::clone(&target), 2);
        group.candidates = vec![
            ReplacementSubDAG {
                strategy: "TestStrategy",
                replacement: Replacement::Rewrite(Rc::clone(&target)),
                provenance: ReplacementProvenance::CseShare,
                rationale: "share".into(),
            },
            ReplacementSubDAG {
                strategy: "TestStrategy",
                replacement: Replacement::Rewrite(Rc::new(target.as_ref().clone())),
                provenance: ReplacementProvenance::CseRecompute,
                rationale: "recompute".into(),
            },
            ReplacementSubDAG {
                strategy: "TestStrategy",
                replacement: Replacement::Rewrite(Rc::new(QueryExpr::CurrentTimestamp)),
                provenance: ReplacementProvenance::LogicalRewrite,
                rationale: "different rewrite strategy".into(),
            },
        ];

        assert!(cse_candidate_pair(&group).is_some());
        let ranked = rank_group(&group, &ConstantCseCost);
        assert_eq!(
            ranked
                .iter()
                .map(|c| c.rationale.as_str())
                .collect::<Vec<_>>(),
            vec!["recompute", "different rewrite strategy", "share"],
            "the preferred CSE choice must be ranked without losing the unrelated rewrite"
        );
        let chosen = pick_shared_subtree_candidate(
            &group,
            decide_with_effective_count(&group, 2, &ConstantCseCost).unwrap(),
        )
        .unwrap();
        assert_eq!(chosen.provenance, ReplacementProvenance::CseRecompute);
    }

    #[test]
    fn effective_consumer_count_corrects_a_nested_groups_share_decision() {
        // The interaction issue #271 describes: an outer shared subtree `a`
        // (referenced by 2 roots, so consumer_count == 2) wraps an inner
        // shared subtree `c` (referenced once through `a`'s own child edge,
        // plus once more directly by a third, separate root — so `c`'s own
        // *raw* structural consumer_count is also 2, independent of `a`).
        //
        //   root1 ─┐
        //          ├─▶ a = Filter(child = c) ─▶ c = Dedup(job)
        //   root2 ─┘
        //   root3 ───────────────────────────▶ c  (same shared Rc)
        //
        // `a` and `c` are both non-`Aggregate` nodes (`Filter`/`Dedup`) so
        // neither is bindable — each group is a *clean* two-candidate
        // SharedSubtreeStrategy share-vs-recompute pair, with no
        // SketchAlgorithmStrategy `Summary` candidate mixed in to complicate
        // ranking (see `shared_aggregate_across_two_roots_gets_both_strategies_candidates`
        // for what a *mixed*-shape group looks like — deliberately avoided
        // here to isolate the SharedSubtreeStrategy-only interaction).
        //
        // Under ConstantCseCost, consumer_count == 2 loses to maintenance
        // (2 * 40 = 80 < 100 ⇒ RecomputeIndependently); consumer_count == 3 wins
        // (3 * 40 = 120 > 100 ⇒ Share). `cost_sorted` only ever sees `c`'s raw
        // count (2) and picks RecomputeIndependently for it — the WRONG
        // answer once `a` itself is accounted for: `a`'s own decision is
        // also RecomputeIndependently (same 80-vs-100 threshold), so `a`
        // actually runs twice, and each run recomputes `c` once more —
        // `c`'s *true* effective count is 2 (via `a`) + 1 (via root3) = 3,
        // which flips its own decision to Share. Only global_selection,
        // which folds `a`'s decision into `c`'s effective_consumer_count
        // before deciding `c`, gets this right.
        use asap_types::pre_asap::expr_ir::ScalarValue;
        use asap_types::pre_asap::query_expr::Predicate;

        let c = || QueryExpr::Dedup {
            cols: vec![0],
            child: Rc::new(metric_scan(&["job"])),
        };
        let a = || QueryExpr::Filter {
            pred: Predicate(Rc::new(QueryExpr::Literal(ScalarValue::Boolean(true)))),
            child: Rc::new(c()),
        };

        let space = search_workload(vec![
            ("root1", Rc::new(a())),
            ("root2", Rc::new(a())),
            ("root3", Rc::new(c())),
        ]);

        // Fixture sanity: root1/root2 merged onto one shared `a`, and `c`
        // (root1/root2's shared child, and root3 itself) merged onto one
        // shared `c` with raw consumer_count 2, and both groups are clean
        // (non-mixed) two-candidate SharedSubtreeStrategy pairs.
        assert!(Rc::ptr_eq(&space.roots[0].1, &space.roots[1].1));
        let a_rc = &space.roots[0].1;
        let QueryExpr::Filter { child: c_via_a, .. } = a_rc.as_ref() else {
            panic!("expected root1/root2 to still be a Filter");
        };
        assert!(Rc::ptr_eq(c_via_a, &space.roots[2].1));
        let a_group = space.group_for(a_rc).unwrap();
        let c_group = space.group_for(c_via_a).unwrap();
        assert_eq!(
            a_group.consumer_count, 2,
            "fixture sanity: a has 2 consumers"
        );
        assert_eq!(
            c_group.consumer_count, 2,
            "fixture sanity: c has 2 raw consumers (via a's child edge, and via root3)"
        );
        assert_eq!(
            a_group.candidates.len(),
            2,
            "fixture sanity: a is a clean Rewrite pair"
        );
        assert_eq!(
            c_group.candidates.len(),
            2,
            "fixture sanity: c is a clean Rewrite pair"
        );

        // The naive/local answer: cost_sorted ranks c using its raw count
        // (2) alone and prefers RecomputeIndependently.
        let ranked = space.cost_sorted(&ConstantCseCost);
        let c_ranked = ranked
            .iter()
            .find(|g| Rc::ptr_eq(g.target, c_via_a))
            .unwrap();
        let c_top_shares = matches!(
            &c_ranked.candidates[0].replacement,
            Replacement::Rewrite(rc) if Rc::ptr_eq(rc, c_via_a)
        );
        assert!(
            !c_top_shares,
            "cost_sorted, blind to a's own decision, must (wrongly) prefer \
             RecomputeIndependently for c using its raw consumer_count of 2"
        );

        // The corrected, cross-group-aware answer: global_selection folds
        // a's own RecomputeIndependently choice into c's effective count
        // (2 from a + 1 from root3 = 3) and flips to Share.
        let selected = space.global_selection(&ConstantCseCost);
        let a_selected = selected.for_target(a_rc).unwrap();
        let c_selected = selected.for_target(c_via_a).unwrap();

        assert_eq!(
            a_selected.effective_consumer_count, 2,
            "a has no interacting ancestor"
        );
        let a_shares = matches!(
            &a_selected.chosen.unwrap().replacement,
            Replacement::Rewrite(rc) if Rc::ptr_eq(rc, a_rc)
        );
        assert!(
            !a_shares,
            "fixture sanity: a itself must also choose RecomputeIndependently"
        );

        assert_eq!(
            c_selected.effective_consumer_count, 3,
            "c's effective count must be 2 (a, itself recomputed twice) + 1 (root3)"
        );
        let c_shares = matches!(
            &c_selected.chosen.unwrap().replacement,
            Replacement::Rewrite(rc) if Rc::ptr_eq(rc, c_via_a)
        );
        assert!(
            c_shares,
            "global_selection must flip c to Share once a's own recomputation is accounted for"
        );
    }

    #[test]
    fn effective_repetition_materializes_a_cse_choice_for_a_single_edge_child() {
        use asap_types::pre_asap::expr_ir::ScalarValue;
        use asap_types::pre_asap::query_expr::Predicate;

        let c = || QueryExpr::Dedup {
            cols: vec![0],
            child: Rc::new(metric_scan(&["job"])),
        };
        let a = || QueryExpr::Filter {
            pred: Predicate(Rc::new(QueryExpr::Literal(ScalarValue::Boolean(true)))),
            child: Rc::new(c()),
        };
        let space = search_workload(vec![("root1", Rc::new(a())), ("root2", Rc::new(a()))]);
        let a_rc = &space.roots[0].1;
        let QueryExpr::Filter { child: c_rc, .. } = a_rc.as_ref() else {
            panic!("expected Filter root");
        };

        assert_eq!(space.group_for(c_rc).unwrap().consumer_count, 1);
        assert!(cse_candidate_pair(space.group_for(c_rc).unwrap()).is_some());

        let selected = space.global_selection(&ConstantCseCost);
        let child = selected.for_target(c_rc).unwrap();
        assert_eq!(child.effective_consumer_count, 2);
        assert!(child.chosen.is_some());
    }

    #[test]
    fn shared_ancestor_keeps_a_single_use_cse_descendant_selected() {
        use asap_types::pre_asap::expr_ir::ScalarValue;
        use asap_types::pre_asap::query_expr::Predicate;

        struct AlwaysShare;
        impl CostModel for AlwaysShare {
            fn rank_candidates(
                &self,
                _intent: &AggIntent,
                candidates: &[SketchAlgorithm],
            ) -> Vec<SketchAlgorithm> {
                candidates.to_vec()
            }

            fn cse_share_decision(&self, _candidate: &CseCandidate) -> ShareDecision {
                ShareDecision::Share
            }
        }

        let child = || QueryExpr::Dedup {
            cols: vec![0],
            child: Rc::new(metric_scan(&["job"])),
        };
        let parent = || QueryExpr::Filter {
            pred: Predicate(Rc::new(QueryExpr::Literal(ScalarValue::Boolean(true)))),
            child: Rc::new(child()),
        };
        let space = search_workload(vec![
            ("root1", Rc::new(parent())),
            ("root2", Rc::new(parent())),
        ]);
        let parent_rc = &space.roots[0].1;
        let QueryExpr::Filter {
            child: child_rc, ..
        } = parent_rc.as_ref()
        else {
            panic!("expected Filter root");
        };

        let selected = space.global_selection(&AlwaysShare);
        assert_eq!(
            selected
                .for_target(parent_rc)
                .unwrap()
                .effective_consumer_count,
            2
        );
        let child_selection = selected.for_target(child_rc).unwrap();
        assert_eq!(child_selection.effective_consumer_count, 1);
        assert_eq!(
            child_selection.chosen.map(|candidate| candidate.provenance),
            Some(ReplacementProvenance::CseShare),
            "a descendant collapsed to one execution still needs a selected plan"
        );
    }

    #[test]
    fn global_selection_propagates_uses_through_the_selected_rewrite() {
        use asap_types::pre_asap::expr_ir::ScalarValue;
        use asap_types::pre_asap::query_expr::Predicate;

        struct ReplaceFilterChild;
        impl ReplacementStrategy for ReplaceFilterChild {
            fn matches(&self, target: &TargetSubDAG<'_>) -> bool {
                matches!(target.root.as_ref(), QueryExpr::Filter { .. })
            }

            fn replacements(&self, _target: &TargetSubDAG<'_>) -> Vec<ReplacementSubDAG> {
                vec![ReplacementSubDAG {
                    strategy: "ReplaceFilterChild",
                    replacement: Replacement::Rewrite(Rc::new(QueryExpr::Dedup {
                        cols: vec![0],
                        child: Rc::new(metric_scan(&["replacement"])),
                    })),
                    provenance: ReplacementProvenance::LogicalRewrite,
                    rationale: "replace the Filter and its input".into(),
                }]
            }
        }

        let original_child = Rc::new(metric_scan(&["original"]));
        let root = Rc::new(QueryExpr::Filter {
            pred: Predicate(Rc::new(QueryExpr::Literal(ScalarValue::Boolean(true)))),
            child: Rc::clone(&original_child),
        });
        let strategies: Vec<Box<dyn ReplacementStrategy>> = vec![Box::new(ReplaceFilterChild)];
        let space = search_workload_with(vec![("q", root)], &strategies);
        let root = &space.roots[0].1;
        let selected = space.global_selection(&DefaultCostModel);
        let Replacement::Rewrite(rewrite) = &selected
            .for_target(root)
            .unwrap()
            .chosen
            .unwrap()
            .replacement
        else {
            panic!("expected logical rewrite");
        };
        let QueryExpr::Dedup {
            child: replacement_child,
            ..
        } = rewrite.as_ref()
        else {
            panic!("expected Dedup rewrite");
        };
        let QueryExpr::Filter {
            child: original_child,
            ..
        } = root.as_ref()
        else {
            panic!("expected Filter root");
        };

        assert_eq!(
            selected
                .for_target(original_child)
                .unwrap()
                .effective_consumer_count,
            0
        );
        assert_eq!(
            selected
                .for_target(replacement_child)
                .unwrap()
                .effective_consumer_count,
            1
        );
    }

    #[test]
    fn global_selection_compares_a_logical_rewrite_with_the_cse_choice() {
        struct PreferLogicalRewrite;

        impl CostModel for PreferLogicalRewrite {
            fn rank_candidates(
                &self,
                _intent: &AggIntent,
                candidates: &[SketchAlgorithm],
            ) -> Vec<SketchAlgorithm> {
                candidates.to_vec()
            }

            fn estimate_cost(
                &self,
                candidate: &ReplacementSubDAG,
                _target: &TargetSubDAG<'_>,
            ) -> f64 {
                match candidate.provenance {
                    ReplacementProvenance::LogicalRewrite => 0.0,
                    _ => 100.0,
                }
            }
        }

        let a = Rc::new(agg(
            vec![2],
            AggIntent::Avg { col: None },
            metric_scan(&["job"]),
        ));
        let b = Rc::new(agg(
            vec![2],
            AggIntent::Avg { col: None },
            metric_scan(&["job"]),
        ));
        let space = search_workload(vec![("a", a), ("b", b)]);
        let root = &space.roots[0].1;
        let selected = space.global_selection(&PreferLogicalRewrite);

        assert_eq!(
            selected
                .for_target(root)
                .and_then(|group| group.chosen)
                .map(|candidate| candidate.provenance),
            Some(ReplacementProvenance::LogicalRewrite)
        );
    }

    #[test]
    fn topological_order_puts_a_later_discovered_parent_before_its_child() {
        // Mirrors nested_shared_subtree_below_an_unshared_parent_is_still_discovered's
        // diamond fixture: discover_targets's own `order` visits root_b (a
        // parent of `shared`) *after* `shared` itself, because `shared` was
        // already fully walked via root_a first. A naive "process
        // discover_targets's own order" DP would see root_b's child edge
        // after already processing `shared` — topological_order must not
        // make that mistake.
        use asap_types::pre_asap::expr_ir::ScalarValue;
        use asap_types::pre_asap::query_expr::Predicate;

        let shared = agg(vec![2], AggIntent::Sum { col: None }, metric_scan(&["job"]));
        let root_a = QueryExpr::Filter {
            pred: Predicate(Rc::new(QueryExpr::Literal(ScalarValue::Int64(1)))),
            child: Rc::new(shared.clone()),
        };
        let root_b = QueryExpr::Filter {
            pred: Predicate(Rc::new(QueryExpr::Literal(ScalarValue::Int64(2)))),
            child: Rc::new(shared),
        };
        let roots = vec![("a", Rc::new(root_a)), ("b", Rc::new(root_b))];

        let mut order = Vec::new();
        let mut nodes = HashMap::new();
        let mut counts = HashMap::new();
        discover_targets(&roots, &mut order, &mut nodes, &mut counts);
        let groups = order
            .iter()
            .map(|ptr| (*ptr, MemoGroup::new(Rc::clone(&nodes[ptr]), counts[ptr])))
            .collect();
        let space = PlanSpace {
            roots,
            groups,
            order: order.clone(),
        };
        let graph = reference_graph(&space);

        // Discovery-order sanity: root_b comes after the shared child in
        // discover_targets's own order (the exact non-topological case this
        // test exists to cover).
        let QueryExpr::Filter {
            child: shared_via_a,
            ..
        } = space.roots[0].1.as_ref()
        else {
            panic!("expected a Filter root");
        };
        let shared_ptr = Rc::as_ptr(shared_via_a);
        let root_b_ptr = Rc::as_ptr(&space.roots[1].1);
        let shared_discovery_pos = order.iter().position(|p| *p == shared_ptr).unwrap();
        let root_b_discovery_pos = order.iter().position(|p| *p == root_b_ptr).unwrap();
        assert!(
            root_b_discovery_pos > shared_discovery_pos,
            "fixture sanity: discover_targets's own order must NOT already be topological here"
        );

        let topo = topological_order(&order, &graph);
        let shared_topo_pos = topo.iter().position(|p| *p == shared_ptr).unwrap();
        let root_b_topo_pos = topo.iter().position(|p| *p == root_b_ptr).unwrap();
        assert!(
            root_b_topo_pos < shared_topo_pos,
            "topological_order must place root_b (a parent of the shared node) before it, \
             unlike discover_targets's own discovery order"
        );
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
                strategy: "AlwaysGrowingStrategy",
                replacement: Replacement::Rewrite(Rc::new(outer_wrapper)),
                provenance: ReplacementProvenance::LogicalRewrite,
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
        // SummaryAgg(Kll{k:269}) over KeepPreAsap(Scan). job = col 2.
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
            ..
        } = &summary_input.expr
        else {
            panic!("expected SummaryAgg, got {:?}", summary_input.expr);
        };
        assert_eq!(
            family,
            &SummaryFamilyType::Sketch(
                SketchKind::new(SketchAlgorithm::Kll, SketchParams::Kll { k: 269 }),
                GroupingStrategy::default()
            )
        );
        assert_eq!(col, &ColumnRef::SampleValue);
        assert_eq!(reduction, &ReductionTy::by(vec![2]));
        // SummaryAgg edge: the state column carries the committed family.
        assert_eq!(
            field(&summary_input.schema, "quantile_0_99").dtype,
            SummaryFamilyType::Sketch(
                SketchKind::new(SketchAlgorithm::Kll, SketchParams::Kll { k: 269 }),
                GroupingStrategy::default()
            )
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
            SummaryFamilyType::Sketch(kind, _) if kind.algorithm() == &SketchAlgorithm::Kll
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
            &SummaryFamilyType::Sketch(
                SketchKind::new(
                    SketchAlgorithm::DDSketch,
                    SketchParams::DDSketch { alpha: 0.01 }
                ),
                GroupingStrategy::default()
            )
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
                Implementation::Sketch(SketchKind::new(
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
            &SummaryFamilyType::Sketch(
                SketchKind::new(
                    SketchAlgorithm::CountSketch,
                    SketchParams::CountSketch {
                        width: 256,
                        depth: 4
                    }
                ),
                GroupingStrategy::default()
            )
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
            SummaryFamilyType::Sketch(kind, _) if kind.algorithm() == &SketchAlgorithm::Kll
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
    fn topk_without_margin_evidence_falls_back_to_pre_asap() {
        let q = agg(
            vec![2],
            AggIntent::TopK {
                k: 5,
                accuracy: AccuracyTarget::Epsilon(0.01),
            },
            metric_scan(&["job"]),
        );
        let root = realize(&q).unwrap();
        assert!(matches!(root.expr, SummaryExpr::KeepPreAsap(_)));
    }

    struct SeparatedTopKEvidence;

    impl AccuracyEvidenceProvider for SeparatedTopKEvidence {
        fn propagation_stats(
            &self,
            op: &CompositionOperator,
            _family: &SummaryFamilyType,
            _query: Option<&PostAsapSketchQuery>,
        ) -> PropagationStats {
            if matches!(op, CompositionOperator::TopKSelection) {
                PropagationStats {
                    topk_selected_lower_bound: Some(101.0),
                    topk_excluded_upper_bound: Some(100.0),
                    topk_interval_failure_probability: Some(0.005),
                    ..Default::default()
                }
            } else {
                PropagationStats::default()
            }
        }
    }

    #[test]
    fn topk_margin_evidence_is_consumed_by_candidate_construction() {
        let q = Rc::new(agg(
            vec![2],
            AggIntent::TopK {
                k: 5,
                accuracy: AccuracyTarget::EpsilonDelta {
                    epsilon: 0.0,
                    delta: 0.01,
                },
            },
            metric_scan(&["job"]),
        ));
        let strategy = SketchAlgorithmStrategy::with_models_and_evidence(
            &DefaultCostModel,
            &DefaultAccuracyModel,
            &EqualSplitAllocator,
            &SeparatedTopKEvidence,
        );
        let replacements = strategy.replacements(&TargetSubDAG::new(&q));
        assert!(!replacements.is_empty());
        assert!(replacements.iter().all(|candidate| matches!(
            &candidate.replacement,
            Replacement::Summary(node)
                if node.guarantee.as_ref().is_some_and(|g|
                    g.metric == ErrorMetric::TopKMembership
                        && g.failure_probability.evaluate() == Some(0.005))
        )));
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

    // ── Accuracy guarantees and fail-closed composition (issue #172) ─────

    use asap_types::post_asap::ErrorMetric;

    /// A test-only `AccuracyModel` that *registers* a rule the default
    /// deliberately lacks — a sketch over rank-bounded inputs composes
    /// additively, keeping the outer sketch's own metric — so the
    /// composition/allocation machinery can be exercised end to end.
    /// Everything else delegates to `DefaultAccuracyModel`.
    struct RankAdditiveModel;

    impl AccuracyModel for RankAdditiveModel {
        fn local_guarantee(
            &self,
            family: &SummaryFamilyType,
            query: &PostAsapSketchQuery,
        ) -> Option<ResultGuarantee> {
            DefaultAccuracyModel.local_guarantee(family, query)
        }

        fn propagate(
            &self,
            op: &CompositionOperator,
            inputs: &[ResultGuarantee],
            local: Option<&ResultGuarantee>,
            stats: &PropagationStats,
        ) -> Result<ResultGuarantee, AccuracyError> {
            let rank = |g: &ResultGuarantee| g.is_exact() || g.metric == ErrorMetric::Rank;
            if let (CompositionOperator::ApproximateAggregate, true, Some(local)) =
                (op, inputs.iter().all(rank), local)
            {
                let relabel = |g: &ResultGuarantee| ResultGuarantee {
                    metric: ErrorMetric::AbsoluteValue,
                    ..g.clone()
                };
                let inputs: Vec<_> = inputs.iter().map(relabel).collect();
                let mut out =
                    DefaultAccuracyModel.propagate(op, &inputs, Some(&relabel(local)), stats)?;
                out.metric = local.metric;
                return Ok(out);
            }
            DefaultAccuracyModel.propagate(op, inputs, local, stats)
        }

        fn satisfies(&self, guarantee: &ResultGuarantee, target: &AccuracyTarget) -> bool {
            DefaultAccuracyModel.satisfies(guarantee, target)
        }
    }

    fn quantile_eps(q: f64, eps: f64) -> AggIntent {
        AggIntent::Quantile {
            col: None,
            q,
            accuracy: AccuracyTarget::Epsilon(eps),
        }
    }

    fn summary_child(node: &SummaryNode) -> &Rc<SummaryNode> {
        match &node.expr {
            SummaryExpr::SummaryEstimate { summary_input, .. } => summary_child(summary_input),
            SummaryExpr::SummaryAgg { child, .. } => child,
            other => panic!("expected a SummaryAgg, got {other:?}"),
        }
    }

    #[test]
    fn approximate_over_approximate_is_rejected_by_default_not_treated_as_exact() {
        // quantile(0.99, quantile by (job) (0.5, m)): rank over rank — no
        // registered rule, so every outer sketch candidate is refused with a
        // typed reason and the raw/pre-ASAP alternative is what remains.
        let inner = agg(vec![2], default_quantile(0.5), metric_scan(&["job"]));
        let outer = Rc::new(agg(vec![], default_quantile(0.99), inner));
        let proposals =
            SketchAlgorithmStrategy::default_cost_model().propose(&TargetSubDAG::new(&outer));
        assert!(
            proposals.candidates.is_empty(),
            "no outer sketch may be proposed over an approximate child without a rule: {:?}",
            proposals.candidates
        );
        // Every attempt — the as-declared composition and the equal-split
        // re-sizing, for each of KLL/DDSketch — is refused for the same
        // typed reason: no rule, whatever the budget.
        assert_eq!(proposals.rejected.len(), 4, "{:?}", proposals.rejected);
        for rejection in &proposals.rejected {
            assert!(
                matches!(
                    &rejection.error,
                    AccuracyError::UnsupportedComposition { input_metrics, .. }
                        if input_metrics == &vec![ErrorMetric::Rank]
                ),
                "{:?}",
                rejection.error
            );
        }
        // Fallback keeps the whole subtree pre-ASAP — executed exactly.
        let realized = realize_child(&outer, &DefaultCostModel).unwrap();
        assert!(matches!(realized.expr, SummaryExpr::KeepPreAsap(_)));
        assert!(realized
            .guarantee
            .as_ref()
            .is_some_and(ResultGuarantee::is_exact));

        // Cross-metric: a quantile over a cardinality estimate.
        let inner = agg(vec![2], default_cardinality(), metric_scan(&["job"]));
        let outer = Rc::new(agg(vec![], default_quantile(0.99), inner));
        let proposals =
            SketchAlgorithmStrategy::default_cost_model().propose(&TargetSubDAG::new(&outer));
        assert!(proposals.candidates.is_empty());
        assert!(proposals.rejected.iter().all(|r| matches!(
            &r.error,
            AccuracyError::UnsupportedComposition { input_metrics, .. }
                if input_metrics == &vec![ErrorMetric::Cardinality]
        )));
    }

    #[test]
    fn exact_child_contributes_zero_error() {
        // quantile(0.9, sum by (job) (m)): KLL over an exact Sum accumulator
        // — the readout's guarantee is exactly KLL's own local guarantee.
        let inner = agg(vec![2], AggIntent::Sum { col: None }, metric_scan(&["job"]));
        let outer = agg(vec![], default_quantile(0.9), inner);
        let root = realize(&outer).unwrap();
        let guarantee = root
            .guarantee
            .as_ref()
            .expect("a readout carries a guarantee");
        assert_eq!(guarantee.metric, ErrorMetric::Rank);
        assert_eq!(
            guarantee.bound.evaluate(),
            Some(crate::accuracy::kll_rank_error_99(269))
        );
        assert_eq!(guarantee.approximate_layer_count(), 1);
        assert!(guarantee.provenance.iter().any(|s| matches!(
            s,
            GuaranteeSource::ChildGuarantee { guarantee, .. } if guarantee.is_exact()
        )));
        assert!(guarantee.provenance.iter().any(|s| matches!(
            s,
            GuaranteeSource::CompositionStep { rule, .. } if rule == "exact_input"
        )));
        // The sketch *state* node carries no guarantee; the exact
        // accumulator's state is its value and does.
        let SummaryExpr::SummaryEstimate { summary_input, .. } = &root.expr else {
            panic!()
        };
        assert!(summary_input.guarantee.is_none());
        assert!(summary_child(&root)
            .guarantee
            .as_ref()
            .is_some_and(ResultGuarantee::is_exact));
    }

    #[test]
    fn exact_sum_can_consume_an_approximate_readout() {
        // sum(count_distinct by (job) (m)) is an outer exact summary over
        // the inner HLL readout. Both summary levels remain explicit.
        let inner = agg(vec![2], default_cardinality(), metric_scan(&["job"]));
        let outer = agg(vec![], AggIntent::Sum { col: None }, inner);
        let root = realize(&outer).unwrap();
        let SummaryExpr::SummaryAgg { child, .. } = &root.expr else {
            panic!("outer exact sum should remain a SummaryAgg")
        };
        assert!(matches!(child.expr, SummaryExpr::SummaryEstimate { .. }));
        assert!(root.guarantee.is_some());

        // count(...) over the same child is exact: a row count does not
        // depend on the rows' values.
        let inner = agg(vec![2], default_cardinality(), metric_scan(&["job"]));
        let outer = agg(
            vec![],
            AggIntent::Count {
                accuracy: AccuracyTarget::Exact,
            },
            inner,
        );
        let root = realize(&outer).unwrap();
        assert!(root
            .guarantee
            .as_ref()
            .is_some_and(ResultGuarantee::is_exact));
    }

    #[test]
    fn equal_split_allocation_supports_nested_summary_readouts() {
        // A registered rank-additive rule and valid budget split make both
        // summary levels explicit while preserving the composed guarantee.
        let inner = agg(vec![2], quantile_eps(0.5, 0.1), metric_scan(&["job"]));
        let outer = Rc::new(agg(vec![], quantile_eps(0.99, 0.1), inner));
        let strategy = SketchAlgorithmStrategy::with_models(
            &DefaultCostModel,
            &RankAdditiveModel,
            &EqualSplitAllocator,
        );
        let proposals = strategy.propose(&TargetSubDAG::new(&outer));

        assert!(!proposals.candidates.is_empty());
        assert!(proposals.candidates.iter().all(|candidate| {
            let Replacement::Summary(node) = &candidate.replacement else {
                return false;
            };
            matches!(node.expr, SummaryExpr::SummaryEstimate { .. })
                && node.guarantee.as_ref().is_some_and(|guarantee| {
                    DefaultAccuracyModel.satisfies(guarantee, &AccuracyTarget::Epsilon(0.1))
                })
        }));
    }

    #[test]
    fn global_selection_can_choose_nested_summaries() {
        // The same nested summary remains available through workload search
        // and global cost ranking.
        let inner = agg(vec![2], quantile_eps(0.5, 0.1), metric_scan(&["job"]));
        let outer = Rc::new(agg(vec![], quantile_eps(0.99, 0.1), inner));
        let strategies: Vec<Box<dyn ReplacementStrategy>> =
            vec![Box::new(SketchAlgorithmStrategy::with_models(
                &DefaultCostModel,
                &RankAdditiveModel,
                &EqualSplitAllocator,
            ))];
        let space = search_workload_with(vec![("q", Rc::clone(&outer))], &strategies);
        let root = &space.roots[0].1;
        let group = space.group_for(root).unwrap();
        assert!(!group.rejected.is_empty());
        assert!(group.candidates.iter().all(|c| match &c.replacement {
            Replacement::Summary(node) => node.guarantee.as_ref().is_some_and(|g| {
                DefaultAccuracyModel.satisfies(g, &AccuracyTarget::Epsilon(0.1))
            }),
            Replacement::Rewrite(_) => false,
        }));
        let ranked = space.cost_sorted(&DefaultCostModel);
        let root_ranked = ranked.iter().find(|g| Rc::ptr_eq(g.target, root)).unwrap();
        assert_eq!(root_ranked.candidates.len(), group.candidates.len());

        let selection = space.global_selection(&DefaultCostModel);
        let chosen = selection
            .for_target(root)
            .unwrap()
            .chosen
            .expect("a nested summary candidate wins");
        let Replacement::Summary(node) = &chosen.replacement else {
            panic!()
        };
        assert!(matches!(node.expr, SummaryExpr::SummaryEstimate { .. }));
    }

    #[test]
    fn root_target_check_removes_candidates_before_cost_ranking() {
        let q = Rc::new(agg(vec![2], default_quantile(0.99), metric_scan(&["job"])));
        // A root target tighter than the node's own ε=0.01: every sketch
        // candidate misses it and is moved to `rejected`; nothing is left
        // for the cost model to rank.
        let space = search_workload_with_targets(
            vec![("q", Rc::clone(&q), Some(AccuracyTarget::Epsilon(0.001)))],
            &default_strategies(),
            &DefaultAccuracyModel,
        );
        let root = &space.roots[0].1;
        let group = space.group_for(root).unwrap();
        assert!(group
            .candidates
            .iter()
            .all(|c| matches!(c.replacement, Replacement::Rewrite(_))));
        assert!(group.rejected.iter().all(|r| matches!(
            r.error,
            AccuracyError::TargetNotSatisfied { target: AccuracyTarget::Epsilon(e), .. } if e == 0.001
        )));
        assert!(group.rejected.len() >= 2);
        let selection = space.global_selection(&DefaultCostModel);
        assert!(selection.for_target(root).unwrap().chosen.is_none());

        // A root target the node's own sizing meets keeps every candidate.
        let space = search_workload_with_targets(
            vec![("q", Rc::clone(&q), Some(AccuracyTarget::Epsilon(0.01)))],
            &default_strategies(),
            &DefaultAccuracyModel,
        );
        let group = space.group_for(&space.roots[0].1).unwrap();
        assert!(group
            .candidates
            .iter()
            .any(|c| matches!(c.replacement, Replacement::Summary(_))));

        // An `Exact` root target admits only exact candidates.
        let space = search_workload_with_targets(
            vec![("q", Rc::clone(&q), Some(AccuracyTarget::Exact))],
            &default_strategies(),
            &DefaultAccuracyModel,
        );
        let group = space.group_for(&space.roots[0].1).unwrap();
        assert!(group.candidates.iter().all(|c| match &c.replacement {
            Replacement::Summary(node) => node
                .guarantee
                .as_ref()
                .is_some_and(ResultGuarantee::is_exact),
            Replacement::Rewrite(_) => true,
        }));
    }

    #[test]
    fn topk_accuracy_target_rejects_uncertified_membership() {
        let q = Rc::new(agg(
            vec![2],
            AggIntent::TopK {
                k: 10,
                accuracy: AccuracyTarget::Epsilon(0.01),
            },
            metric_scan(&["job"]),
        ));
        let space = search_workload_with_targets(
            vec![("q", Rc::clone(&q), Some(AccuracyTarget::Epsilon(0.01)))],
            &default_strategies(),
            &DefaultAccuracyModel,
        );
        let group = space.group_for(&space.roots[0].1).unwrap();

        assert!(group
            .candidates
            .iter()
            .all(|candidate| matches!(candidate.replacement, Replacement::Rewrite(_))));
        assert!(!group.rejected.is_empty());
        assert!(group.rejected.iter().all(|rejected| matches!(
            rejected.error,
            AccuracyError::TargetNotSatisfied { .. } | AccuracyError::UnsupportedComposition { .. }
        )));
    }
}

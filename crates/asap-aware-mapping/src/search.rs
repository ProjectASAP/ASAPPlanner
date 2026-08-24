//! The Cascades/Volcano-style candidate-plan search engine
//! `docs/asap_aware_mapping.md`'s "Pseudocode for Replacement Plan Searching"
//! section has stubbed out since #206/#211, implemented for real (issue
//! #252, part of #33) over the [`ReplacementStrategy`] extension point
//! (issue #251).
//!
//! ## The pseudocode, and the two things it deliberately leaves open
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
//! workload's tree, one per combination of per-site choices. A workload with
//! `N` independently-choosable sites would produce up to `2^N` flat plans,
//! each one duplicating every untouched sibling subtree. This module does
//! not do that:
//!
//! 1. **MEMO groups, not flat plans.** [`MemoGroup`] is this engine's
//!    Cascades-style "group": one distinct [`TargetSubDAG`] (identified by
//!    its own `Rc<QueryExpr>` pointer identity — the same currency
//!    [`asap_types::pre_asap::cse::share_common_subtrees`] already
//!    established across the workload) holding every
//!    [`ReplacementSubDAG`] alternative discovered for it. [`PlanSpace`] is
//!    a collection of these groups, keyed by site — a candidate "plan" is
//!    never materialized as a distinct top-level `Rc<QueryExpr>` at all;
//!    two logically-different overall choices at two different sites are
//!    just two different entries in two different groups, sharing every
//!    other node in the workload by construction (they *are* the same
//!    `Rc`s — nothing was copied to make a second "plan").
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
//! ## Where "for site in plan.bindable_sites()" comes from
//!
//! [`discover_sites`] is the workload-wide `TargetSubDAG` discovery pass
//! [`replacement`](crate::replacement)'s own module docs explicitly flagged
//! as *not* that module's job ("no workload-wide `TargetSubDAG` discovery
//! pass is shipped either... wiring it up automatically belongs to the same
//! future search engine, not this issue") — this is that future engine, so
//! it's this module's job. It walks every workload root's whole DAG (the
//! same **relational-skeleton** operator-child scope
//! `asap_types::pre_asap::cse::share_common_subtrees` itself uses — see that
//! module's "Algorithm" section), discovering one site per distinct `Rc`
//! and a *real* `consumer_count`: how many operator-child positions
//! anywhere in the workload reference that exact `Rc`, not just how many of
//! the workload's own top-level roots happen to be it. This deliberately
//! goes one step further than
//! [`bind::implement_workload_with`](crate::bind::implement_workload_with)'s
//! own consumer-count pass, which only counts whole-root sharing (that
//! function's own doc calls widening this "future work" for binding) — a
//! `SharedSubtreeStrategy` candidate three levels under an unshared `Filter`
//! is exactly as real a search-space site as a shared whole root, so this
//! module's discovery can't stop at the top level the way binding's does.
//!
//! `discover_sites` duplicates (rather than reuses)
//! [`replacement`](crate::replacement)'s own `#[cfg(test)]`-only
//! `count_consumers` traversal, which mirrors this exact shape for that
//! module's test fixtures — that copy is intentionally test-only (see the
//! same "non-goals" section quoted above), so it isn't reachable from this
//! module's production code without either moving it into shared,
//! non-test-gated code or duplicating the (small, self-contained) traversal
//! here. Duplicating was judged simpler than restructuring another module's
//! test helper into shared production code for one caller.
//!
//! ## Termination
//!
//! Every discovered site is asked *once* per registered strategy, never
//! re-asked — [`search_workload_with`]'s loop processes each round's
//! frontier of not-yet-visited sites exactly one time each, so there is no
//! scenario where the same `(site, strategy)` pair is queried twice (the
//! `new_plans -= candidate_plans` dedup step the module-level pseudocode
//! describes is therefore never asked to recognize "the same candidate,
//! proposed again" as a special case — see [`MemoGroup::add_candidate`]'s
//! own doc on why that distinction matters for [`Replacement::Summary`]
//! specifically, where no real equality check exists to make it safely).
//!
//! What *can* grow the frontier is a candidate's own reachable structure:
//! after a site is processed, every [`Replacement::Rewrite`] candidate's
//! **children** (never the candidate's own top-level node — that value is
//! an alternative *for* the site just processed, not a new site of its own;
//! see [`discover_new_descendant_sites`]) are scanned for pointers not
//! already known, and any found become next round's frontier. Both shipped
//! strategies are idempotent in exactly this sense:
//! [`SketchFamilyStrategy`](crate::replacement::SketchFamilyStrategy)
//! produces terminal [`Replacement::Summary`] candidates (no `QueryExpr`
//! children to scan at all), and
//! [`SharedSubtreeStrategy`](crate::replacement::SharedSubtreeStrategy)'s
//! two [`Replacement::Rewrite`] candidates both reuse the target's own
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
//! ## Cost-based final selection — reusing `CostModel`, not a second interface
//!
//! [`PlanSpace::cost_sorted`] is the `sorted_by(cost_model)` step, and it
//! reuses this crate's existing [`CostModel`] trait rather than inventing a
//! second cost interface (`docs/cse-cost-model-decision.md`, issue #237,
//! explicitly reasoned about *why* a narrow, direct cost comparison was
//! enough for the CSE share/recompute decision alone, and flagged that a
//! real search engine — this module — is where that stops being the whole
//! story; it isn't a contradiction of #237, it's the scope change #237
//! itself named). Concretely, per [`MemoGroup`]:
//!
//! - A group whose candidates are the
//!   [`SharedSubtreeStrategy`](crate::replacement::SharedSubtreeStrategy)
//!   share-vs-recompute pair is ranked by calling
//!   [`CostModel::cse_share_decision`] — the exact comparison
//!   `bind::implement_workload_with` already uses for this same decision —
//!   rather than re-deriving a competing comparison in this module.
//! - A group whose candidates are
//!   [`SketchFamilyStrategy`](crate::replacement::SketchFamilyStrategy)'s
//!   sketch-family candidates is ranked via
//!   [`CostModel::rank_candidates`] (the same hook
//!   [`implementation::implementations_for_with`](crate::implementation::implementations_for_with)
//!   itself consults), applied to the candidates' own [`SketchKind`]s.
//! - Any other shape (a single candidate, or a mix this module doesn't have
//!   a defined comparison for) keeps discovery order — there is nothing to
//!   rank, or no [`CostModel`] hook this module knows how to apply; it never
//!   invents a comparison `CostModel` doesn't already define.

use std::collections::HashMap;
use std::rc::Rc;

use asap_types::post_asap::{SketchKind, SummaryExpr, SummaryFamilyType, SummaryNode};
use asap_types::pre_asap::cse::{share_common_subtrees, structural_hash, HashCache};
use asap_types::pre_asap::query_expr::QueryExpr;

use crate::bind::bindable_intent;
use crate::cost_model::{CostModel, CseCandidate, ShareDecision};
use crate::replacement::{
    Replacement, ReplacementStrategy, ReplacementSubDAG, SharedSubtreeStrategy,
    SketchFamilyStrategy, TargetSubDAG,
};

/// A generous, documented backstop against a hypothetically ill-behaved
/// future [`ReplacementStrategy`] (see the module docs' "Termination"
/// section) — not a bound either shipped strategy could ever approach.
/// [`SketchFamilyStrategy`] and [`SharedSubtreeStrategy`] both converge in
/// exactly 2 passes over a fixed site set, regardless of workload size.
pub const MAX_SEARCH_ITERATIONS: usize = 1_000;

// ── MemoGroup ────────────────────────────────────────────────────────────

/// One Cascades-style MEMO group: a single distinct [`TargetSubDAG`] (its
/// own `target` `Rc<QueryExpr>`, keyed by pointer identity in
/// [`PlanSpace`]'s internal map — never re-derived by value) plus every
/// [`ReplacementSubDAG`] alternative any registered [`ReplacementStrategy`]
/// proposed for it.
///
/// `candidates` is deliberately *not* required to be non-empty — a site no
/// registered strategy has an opinion on still gets a group (with an empty
/// candidate list), so [`PlanSpace`] always has exactly one group per
/// discovered site, not "one group per site something matched".
#[derive(Debug, Clone)]
pub struct MemoGroup {
    /// The target sub-DAG this group is for.
    pub target: Rc<QueryExpr>,
    /// How many operator-child positions across the whole workload
    /// reference this exact `Rc` — see [`discover_sites`].
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
/// discover: one [`MemoGroup`] per distinct site in the (already-CSE'd)
/// workload, plus the workload's own post-CSE roots so a caller can still
/// map a `Root`'s `Id` back to the `Rc<QueryExpr>` whose group holds its
/// alternatives.
pub struct PlanSpace<Id> {
    /// The workload's roots, after the one `share_common_subtrees` pass
    /// [`search_workload_with`] runs up front — the same post-CSE roots
    /// every site in `groups` was discovered from.
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

    /// How many distinct sites were discovered.
    pub fn len(&self) -> usize {
        self.groups.len()
    }

    /// Whether no sites were discovered at all (an empty workload, or one
    /// with no `QueryExpr` nodes reachable from any root — never true for a
    /// non-empty `roots`, since every root is itself a site).
    pub fn is_empty(&self) -> bool {
        self.groups.is_empty()
    }

    /// The group for `target`, if `target`'s own `Rc` is a discovered site
    /// (i.e. `Rc::ptr_eq` to some node reachable from `roots`).
    pub fn group_for(&self, target: &Rc<QueryExpr>) -> Option<&MemoGroup> {
        self.groups.get(&Rc::as_ptr(target))
    }

    /// The `sorted_by(cost_model)` step: every group, each with its own
    /// candidates ranked best-first under `cost_model` where this module
    /// knows how (see the module docs' "Cost-based final selection"
    /// section) — groups themselves stay in discovery order, since sites
    /// are independent decision points, not alternatives competing with
    /// each other.
    pub fn cost_sorted(&self, cost_model: &dyn CostModel) -> Vec<RankedGroup<'_>> {
        self.order
            .iter()
            .map(|ptr| {
                let group = &self.groups[ptr];
                RankedGroup {
                    target: &group.target,
                    consumer_count: group.consumer_count,
                    candidates: rank_group(group, cost_model),
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
    // candidate is a `Rewrite`) — rank via `CostModel::cse_share_decision`,
    // the same comparison `bind::implement_workload_with` already uses.
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

    // Shape 2: `SketchFamilyStrategy`'s sketch-family candidates (every
    // candidate is a `Summary` that realizes a `SketchKind`) — rank via
    // `CostModel::rank_candidates`, the same hook
    // `implementation::implementations_for_with` itself consults.
    if let Some(intent) = bindable_intent(&group.target) {
        let kinds: Option<Vec<SketchKind>> = ranked
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
/// [`SharedSubtreeStrategy::matches`](crate::replacement::SharedSubtreeStrategy)'s
/// own gate), or `group.target` can't actually be bound at all (no
/// candidate and no logical fallback — never expected in practice for a
/// target that's already part of a legitimate workload tree, but this
/// degrades to "keep discovery order" rather than panicking).
fn cse_preference(group: &MemoGroup, cost_model: &dyn CostModel) -> Option<bool> {
    if group.consumer_count < 2 {
        return None;
    }
    let bound = bind_one(&group.target, cost_model)?;
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

/// This crate has no "bind me one tree" entry point any more —
/// `bind::implement_tree`/`implement_tree_with` were deleted once
/// [`SketchFamilyStrategy::replacements`] became the sole way to get bound
/// output for a target, always returning every candidate. [`cse_preference`]
/// only needs one representative bound `SummaryNode` for `target` (to build
/// a [`CseCandidate`] for [`CostModel::cse_share_decision`]), so this
/// reproduces the take-the-first-(`cost_model`-preferred)-candidate pattern
/// — the same one `bind.rs`'s own tests and every external caller of this
/// crate now use — falling back to [`crate::bind::logical`] when `target`
/// isn't a bindable `Aggregate` at all.
fn bind_one(target: &Rc<QueryExpr>, cost_model: &dyn CostModel) -> Option<Rc<SummaryNode>> {
    let site = TargetSubDAG::new(target);
    match SketchFamilyStrategy::new(cost_model)
        .replacements(&site)
        .into_iter()
        .next()
    {
        Some(ReplacementSubDAG {
            replacement: Replacement::Summary(node),
            ..
        }) => Some(node),
        _ => crate::bind::logical(target).ok(),
    }
}

/// The `SketchKind` a bound [`Replacement::Summary`] candidate ultimately
/// realizes, if any (`None` for an `ExactAggregate`/pass-through
/// `Summary` — nothing to rank against another `SketchKind`).
///
/// Mirrors [`replacement`](crate::replacement)'s own `#[cfg(test)]`-only
/// `summary_family_kind` helper, which does the identical
/// `SummaryEstimate`-unwrap-then-match for that module's own tests; that
/// copy is test-only, so this module needs its own for real (non-test)
/// ranking code — the same "duplicate a small, self-contained traversal
/// rather than restructure another module's test helper" call this
/// module's docs already make for [`discover_sites`].
fn sketch_kind_of(node: &SummaryNode) -> Option<SketchKind> {
    match &node.expr {
        SummaryExpr::SummaryEstimate { summary_input, .. } => sketch_kind_of(summary_input),
        SummaryExpr::SummaryAgg {
            family: SummaryFamilyType::Sketch(kind, _),
            ..
        } => Some(kind.clone()),
        _ => None,
    }
}

// ── default_strategies ──────────────────────────────────────────────────

/// The strategies [`search_workload`] runs, in the built-in
/// [`DefaultCostModel`] configuration — mirrors
/// [`replacement`](crate::replacement)'s own two shipped
/// [`ReplacementStrategy`] impls.
/// [`crate::applicability::find_applicable_optimizations`] (issue #257) uses
/// this same set (via [`search_workload`]) rather than keeping a second,
/// applicability-specific list to stay in sync with. Use
/// [`default_strategies_with`] to plug in a deployment-specific
/// [`CostModel`] instead.
pub fn default_strategies() -> Vec<Box<dyn ReplacementStrategy>> {
    vec![
        Box::new(SketchFamilyStrategy::default_cost_model()),
        Box::new(SharedSubtreeStrategy),
    ]
}

/// Like [`default_strategies`], but [`SketchFamilyStrategy`] ranks/binds via
/// `cost_model` instead of the built-in [`DefaultCostModel`] — the same
/// customization point [`SketchFamilyStrategy::new`] itself offers.
pub fn default_strategies_with<'a>(
    cost_model: &'a dyn CostModel,
) -> Vec<Box<dyn ReplacementStrategy + 'a>> {
    vec![
        Box::new(SketchFamilyStrategy::new(cost_model)),
        Box::new(SharedSubtreeStrategy),
    ]
}

// ── search_workload ──────────────────────────────────────────────────────

/// Search a whole workload's pre-ASAP roots for every candidate replacement
/// [`default_strategies`] can find, deduped into a [`PlanSpace`]. Candidate
/// *generation* uses the built-in [`DefaultCostModel`] (via
/// [`default_strategies`], the same way [`SketchFamilyStrategy::default_cost_model`]
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
/// [`crate::applicability::ApplicabilityFinding`] this module's caller reads
/// off the result) sees the same already-deduplicated tree
/// [`crate::bind::implement_workload`] would — then discovers every site
/// (see [`discover_sites`]) and runs the fixpoint loop the module docs
/// describe, capped at [`MAX_SEARCH_ITERATIONS`] passes (see the module
/// docs' "Termination" section). Deduping candidate plans this way needs no
/// [`CostModel`] at all — that only enters at two well-defined points: each
/// [`ReplacementStrategy`] in `strategies` may already carry its own (e.g.
/// [`SketchFamilyStrategy::new`]'s), and [`PlanSpace::cost_sorted`]'s final
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
    discover_sites(&cse_roots, &mut order, &mut nodes, &mut counts);

    let mut groups: HashMap<*const QueryExpr, MemoGroup> = HashMap::new();
    for ptr in &order {
        groups.insert(*ptr, MemoGroup::new(Rc::clone(&nodes[ptr]), counts[ptr]));
    }

    // Round-based frontier: every site is asked exactly once per strategy
    // (never re-asked — see the module docs' "Termination" section on why
    // that matters for `Replacement::Summary` dedup specifically). A round
    // can grow the *next* round's frontier only by a candidate's own
    // reachable children exposing a genuinely new, not-yet-known `Rc` — see
    // `discover_new_descendant_sites`.
    let mut frontier = order.clone();
    let mut rounds = 0usize;
    while !frontier.is_empty() {
        rounds += 1;
        assert!(
            rounds <= MAX_SEARCH_ITERATIONS,
            "search_workload: fixpoint search did not converge within {MAX_SEARCH_ITERATIONS} \
             rounds — a registered ReplacementStrategy's Replacement::Rewrite candidates keep \
             exposing new, never-before-seen descendant structure every round. \
             SketchFamilyStrategy/SharedSubtreeStrategy never do this (see search.rs's module \
             docs' \"Termination\" section); check any custom strategies passed to \
             search_workload_with.",
        );

        let sites_before = order.len();
        for ptr in &frontier {
            let (target, consumer_count) = {
                let group = &groups[ptr];
                (Rc::clone(&group.target), group.consumer_count)
            };
            let site = TargetSubDAG::with_consumer_count(&target, consumer_count);

            let mut proposed = Vec::new();
            for strategy in strategies {
                if strategy.matches(&site) {
                    proposed.extend(strategy.replacements(&site));
                }
            }

            for candidate in &proposed {
                if let Replacement::Rewrite(rc) = &candidate.replacement {
                    discover_new_descendant_sites(rc, &mut order, &mut nodes, &mut counts);
                }
            }

            let group = groups
                .get_mut(ptr)
                .expect("every discovered site has a group");
            for candidate in proposed {
                group.add_candidate(candidate);
            }
        }

        // Any pointer `discover_new_descendant_sites` appended to `order`
        // this round is a genuinely new site — give it a group and process
        // it next round. Sites already in `groups` are never revisited.
        let new_sites = &order[sites_before..];
        for ptr in new_sites {
            groups
                .entry(*ptr)
                .or_insert_with(|| MemoGroup::new(Rc::clone(&nodes[ptr]), counts[ptr]));
        }
        frontier = new_sites.to_vec();
    }

    PlanSpace {
        roots: cse_roots,
        groups,
        order,
    }
}

// ── site discovery ───────────────────────────────────────────────────────

/// Walk every root's whole DAG, discovering one site per distinct `Rc` and
/// its real `consumer_count` — see the module docs' "Where 'for site in
/// plan.bindable_sites()' comes from" section for the full rationale.
fn discover_sites<Id>(
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
/// [`Replacement::Rewrite`]'s value is an alternative *for* the site that
/// proposed it, never a new site of its own) for any `Rc` not already known,
/// appending each to `order`/`nodes`/`counts` so
/// [`search_workload_with`]'s next round processes it. A no-op when every
/// child is already known — the case both shipped strategies always produce
/// (see that section).
fn discover_new_descendant_sites(
    candidate: &Rc<QueryExpr>,
    order: &mut Vec<*const QueryExpr>,
    nodes: &mut HashMap<*const QueryExpr, Rc<QueryExpr>>,
    counts: &mut HashMap<*const QueryExpr, usize>,
) {
    walk_children(candidate, order, nodes, counts);
}

/// Visit `node`: count this occurrence, and — the first time this exact
/// `Rc` is seen — record it as a site and recurse into its children.
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
/// `replacement::tests::count_consumers` mirrors for its own fixtures.
/// Exhaustive over every `QueryExpr` variant: a new variant fails to
/// compile here until this match is extended too.
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
    use crate::cost_model::DefaultCostModel;
    use asap_types::pre_asap::agg_intent::{default_cardinality, default_quantile, AggIntent};
    use asap_types::pre_asap::query_expr::{Reduction, Source};
    use asap_types::pre_asap::schema::{Column, DataType, Schema};

    fn metric_scan(labels: &[&str]) -> QueryExpr {
        let mut columns = vec![
            Column::new("ts", DataType::Timestamp, false),
            Column::new("value", DataType::Float64, false),
        ];
        columns.extend(labels.iter().map(|n| Column::new(*n, DataType::Utf8, true)));
        QueryExpr::Scan {
            source: Source::TimeSeries { metric: "m".into() },
            predicates: vec![],
            schema: Schema::with_time_index(columns, 0, vec![]),
        }
    }

    fn agg(by: Vec<usize>, intent: AggIntent, child: QueryExpr) -> QueryExpr {
        QueryExpr::Aggregate {
            reduction: Reduction::by(by),
            measures: vec![intent],
            output_names: vec![],
            having: None,
            child: Rc::new(child),
        }
    }

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
        // carry SketchFamilyStrategy's one ExactAggregate candidate *and*
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
        // (bind::implement_workload_with's own consumer-count pass would
        // miss this; this module's discover_sites must not).
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
            .expect("shared node must be a discovered site");
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
        // does this in practice (every site is asked exactly once — see the
        // module docs' "Termination" section), so this test exists to pin
        // the documented behavior, not to endorse calling `replacements`
        // twice for the same site.
        let root = Rc::new(agg(vec![2], default_quantile(0.99), metric_scan(&["job"])));
        let mut group = MemoGroup::new(Rc::clone(&root), 1);
        let strategy = SketchFamilyStrategy::default_cost_model();
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
        assert_eq!(first_kind, Some(SketchKind::DDSketch));
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
    /// (ignored by site discovery — see [`discover_new_descendant_sites`])
    /// and an inner one carrying a monotonically-increasing counter, so the
    /// inner layer is a **brand-new, never-before-seen `Rc` every call**.
    /// Each round, `search_workload_with` discovers that inner layer as a
    /// new site, processes it next round (this strategy matches
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
}

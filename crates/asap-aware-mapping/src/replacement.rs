//! `TargetSubDAG` / `ReplacementSubDAG` / `ReplacementStrategy` — the
//! candidate-replacement vocabulary `docs/design_docs/asap_aware_mapping.md` stubs out
//! under "Key concepts (not yet implemented)", implemented for real (issue
//! #251, part of #33).
//!
//! ## One step, not two: `ASAPStrategies::replacements()` decides *and* builds
//!
//! For a bindable `Aggregate`, `ASAPStrategies::replacements()` is the
//! single place this crate both decides what an `AggIntent` may become and
//! turns each of those candidates into a real, executable
//! [`ReplacementSubDAG`]:
//!
//! 1. **Decide**: [`realizations_for_intent`] enumerates every valid
//!    [`Realization`] for the target's intent — exhaustive, and ranked
//!    most-preferred-first via a [`CostModel`] (candidate sketch family/kind,
//!    already sized to the target's own accuracy target: `Realization::Sketch`'s
//!    `params` are the output of inverting that accuracy target through
//!    `CostModel::size_params`, not a placeholder filled in later).
//! 2. **Build**: for each candidate in that list, [`construct_summary`]
//!    mechanically turns the already-decided `(kind, params)` into a real
//!    [`OperatorNode`] — derives the child schema, resolves the summarized
//!    column, builds the evaluation query, recurses into the child (via
//!    [`realize_child`], so a nested aggregate gets its own
//!    independent enumeration, never the outer target's forced choice), and
//!    assembles the `SummaryAgg`/`SummaryEstimate` node.
//!
//! There is no separate decision step and construction step living in
//! different modules bridged by a named "given a `Realization`, bind it"
//! function — step 2 is *not* a second decision (nothing about which
//! candidate to prefer happens there), it is mechanical construction that
//! has to run regardless of how `(kind, params)` were chosen, so it lives
//! directly inside the one method that needs it.
//!
//! - [`TargetSubDAG`] — a reference to a pre-ASAP [`OperatorNode`] that is a
//!   candidate for replacement, plus how many places in the workload already
//!   reference it (its `consumer_count`) — the one piece of cross-node
//!   context [`SharedSubDAGStrategy`] needs that a bare node reference alone
//!   doesn't carry.
//! - [`ReplacementSubDAG`] — one candidate replacement for a `TargetSubDAG`:
//!   either a fully bound summary sub-DAG or a pre-ASAP logical rewrite
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
//! A caller may inspect local replacements, but taking the first candidate
//! does not establish a compatible workload plan or physical deployability.
//! For Planner-owned logical selection, call [`CandidateLogicalASAPDAGs::global_selection`]
//! once and [`GlobalSelection::assemble_selected_dag`] for each wanted query
//! root. Physical binding, deployment, and execution remain downstream.
//!
//! Internally, [`realize_child`] and [`realize_one`] may take a preferred local
//! realization while constructing or costing a candidate. That local operation
//! is not the public workload-selection workflow and does not create runtime state.
//!
//! This means an ordinary single-target bind sizes and fully constructs
//! *every* sketch candidate at every sketch-capable node (not just the one a
//! caller keeps) — a deliberate tradeoff, made so there is exactly one place
//! in this crate that decides what an `AggIntent` may become, at the cost of
//! extra work per bind proportional to each node's own candidate count.
//!
//! ## The two strategies, and why these two
//!
//! - [`ASAPStrategies`] wraps [`realizations_for_intent`]'s exhaustive,
//!   ranked list directly: for the same bindable-`Aggregate` shape this crate
//!   binds (single intent, no `HAVING`), every entry becomes its own bound
//!   candidate.
//! - [`SharedSubDAGStrategy`] wraps
//!   `asap_types::ir::cse::share_common_sub_dags`'s sharing decision.
//!   Wherever a [`TargetSubDAG`] already has two or more consumers (i.e.
//!   `share_common_sub_dags` already collapsed two or more workload
//!   locations onto the same `Rc<OperatorNode>` — [`discover_targets`] below
//!   does the identical workload-wide discovery for [`search_workload_with`];
//!   this module's own tests reuse the same dedup logic to build realistic
//!   fixtures), it reports the two-way candidate CSE's own detection pass
//!   deliberately declines to pick between on its own: build once and share
//!   the already-interned sub-DAG, or build it independently at each
//!   consumer. [`crate::cost_model::CostModel::cse_share_decision`] is where
//!   that choice actually gets made *today* (a fixed comparison, not a
//!   search) — this strategy exposes the same two-way choice as an explicit,
//!   inspectable pair of candidates instead of a cost model's already-decided
//!   boolean.
//!
//! ## Non-goals (tracked separately, not attempted here)
//!
//! - **[`realizations_for_intent`]'s own outward-facing behavior is
//!   unchanged.** Same inputs still produce the same exhaustive, ranked
//!   list — only its home moved (from a separate `implementation` module
//!   into this one) and its own visibility dropped to module-private, since
//!   [`ASAPStrategies`] is now its only caller.
//!
//! ## Workload-wide search — merged in from the former `search.rs` (issue #252, part of #33)
//!
//! This section used to carry two more "non-goals" bullets here — "no
//! search/selection-across-a-whole-plan logic" and "no workload-wide
//! `TargetSubDAG` discovery pass" — describing work deliberately left for a
//! future Cascades/Volcano-style search engine (PR #263,
//! `feat/cascades-search-252`, over the [`ReplacementStrategy`] extension
//! point above). That engine is [`CandidateLogicalASAPDAGs`]/[`TargetSubDAGCandidates`]/
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
//! workload's DAG, one per combination of per-target choices. A workload
//! with `N` independently-choosable targets would produce up to `2^N` flat
//! plans, each one duplicating every untouched sibling sub-DAG. This module
//! does not do that:
//!
//! 1. **Per-target candidates, not flat plans.** [`TargetSubDAGCandidates`]
//!    stores the alternatives for one distinct [`TargetSubDAG`] (identified by
//!    its own `Rc<OperatorNode>` pointer identity — the same currency
//!    [`asap_types::ir::cse::share_common_sub_dags`] already
//!    established across the workload) holding every
//!    [`ReplacementSubDAG`] alternative discovered for it. [`CandidateLogicalASAPDAGs`] is
//!    a collection of these groups, keyed by `TargetSubDAG` — a candidate
//!    "plan" is never materialized as a distinct top-level `Rc<OperatorNode>`
//!    at all; two logically-different overall choices at two different
//!    targets are just two different entries in two different groups,
//!    sharing every other node in the workload by construction (they *are*
//!    the same `Rc`s — nothing was copied to make a second "plan").
//! 2. **Dedup by structural hash + `PartialEq`, reusing `pre_asap::cse`'s own
//!    discipline.** [`asap_types::ir::cse::structural_hash`] (made
//!    `pub` for exactly this reuse) is only ever a candidate-narrowing
//!    filter; [`TargetSubDAGCandidates::add_candidate`]'s actual duplicate check is
//!    `OperatorNode`'s derived `PartialEq` — the same "hash is a filter,
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
//! `asap_types::ir::cse::share_common_sub_dags` itself uses — see
//! that module's "Algorithm" section), discovering one `TargetSubDAG` per
//! distinct `Rc` and a *real* `consumer_count`: how many operator-child
//! positions anywhere in the workload reference that exact `Rc`, not just
//! how many of the workload's own top-level roots happen to be it — a
//! `SharedSubDAGStrategy` candidate three levels under an unshared
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
//! [`TargetSubDAGCandidates::add_candidate`]'s own doc on why that distinction matters
//! for [`Replacement::Summary`] specifically, where no real equality check
//! exists to make it safely).
//!
//! What *can* grow the frontier is a candidate's own reachable structure:
//! after a target is processed, every [`Replacement::Rewrite`] candidate's
//! **children** (never the candidate's own top-level node — that value is
//! an alternative *for* the target just processed, not a new target of its
//! own; see [`discover_new_descendant_targets`]) are scanned for pointers
//! not already known, and any found become next round's frontier. Both shipped
//! strategies are idempotent in exactly this sense: [`ASAPStrategies`]
//! produces terminal bound-summary [`Replacement::SubDAG`] candidates (no
//! logical-rewrite children to scan at all), and [`SharedSubDAGStrategy`]'s
//! two logical-rewrite [`Replacement::SubDAG`] candidates both reuse the target's own
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
//! [`CandidateLogicalASAPDAGs::cost_sorted`] is the `sorted_by(cost_model)` step, and it
//! reuses this crate's existing [`CostModel`] trait rather than inventing a
//! second cost interface (`docs/design_docs/cse-cost-model-decision.md`,
//! issue #237, explicitly reasoned about *why* a narrow, direct cost
//! comparison was enough for the CSE share/recompute decision alone, and
//! flagged that a real search engine — this module — is where that stops
//! being the whole story; it isn't a contradiction of #237, it's the scope
//! change #237 itself named). Concretely, per [`TargetSubDAGCandidates`]:
//!
//! - A group whose candidates are the [`SharedSubDAGStrategy`]
//!   share-vs-recompute pair is ranked by calling
//!   [`CostModel::cse_share_decision`] via this module's own
//!   [`cse_preference`] — rather than re-deriving a competing comparison.
//! - A group whose candidates are [`ASAPStrategies`]'s sketch-family
//!   candidates is ranked via [`CostModel::rank_candidates`] (the same hook
//!   `realizations_for_intent` itself consults), applied to the
//!   candidates' own [`SketchAlgorithm`]s.
//! - Any other shape (a single candidate, or a mix this module doesn't have
//!   a defined comparison for) keeps discovery order — there is nothing to
//!   rank, or no [`CostModel`] hook this module knows how to apply; it never
//!   invents a comparison `CostModel` doesn't already define.
//!
//! ## Whole-plan (cross-group) selection — issue #271
//!
//! [`CandidateLogicalASAPDAGs::cost_sorted`] above ranks every group's candidates
//! independently: it never lets one group's choice influence how another
//! group is costed. That's the right behavior when groups genuinely don't
//! interact — which both shipped strategies' one-round convergence (see
//! "Termination" above) makes the common case — but it's the wrong answer
//! whenever they do. Concretely: [`CostModel::cse_share_decision`] costs a
//! [`SharedSubDAGStrategy`] group by comparing a `consumer_count`-scaled
//! recompute cost against a fixed maintenance cost — but a **nested**
//! `SharedSubDAGStrategy` group's *true* recompute burden isn't its own
//! raw [`TargetSubDAGCandidates::consumer_count`] (how many operator-child positions
//! directly reference it) whenever an ancestor on the path to it is
//! *itself* being recomputed independently rather than shared: recomputing
//! that ancestor independently at each of *its own* uses recomputes
//! everything underneath it that many times too, even though nothing
//! underneath gained a single new direct reference. `cost_sorted`'s
//! per-group ranking has no way to see this — it only ever looks at one
//! group's own `candidates`, in isolation.
//!
//! [`CandidateLogicalASAPDAGs::global_selection`] is that missing step: a single
//! **top-down dynamic-programming pass** over the discovered sites,
//! processed in the topological order [`topological_order`] computes over a
//! small [`ReferenceDAG`] built for exactly this purpose (parent before
//! every child, so a site's `effective_consumer_count` is always computed
//! from *already-decided* ancestors). For every site it computes the
//! **effective consumer count** — how many times that site actually runs
//! once every ancestor's own selected candidate is accounted for — and, for
//! every [`SharedSubDAGStrategy`]-shaped group, re-decides
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
//! at all, i.e. isn't itself a `SharedSubDAGStrategy` shape — passes its
//! *own* effective count straight through to whatever it references,
//! transitively composing contributions from every ancestor on the path,
//! not just the immediate parent.
//!
//! This is genuine dynamic programming in the classical sense: overlapping
//! subproblems (a site reachable through more than one parent path is
//! solved once, memoized in `effective_uses`, and reused for every path
//! into it) combined via a real recurrence — not just the MEMO-group
//! sharing [`CandidateLogicalASAPDAGs`] itself already does for *storing* candidates. That
//! distinction is exactly what issue #271 raised: this module already looks
//! like a Cascades/Volcano MEMO, but [`CandidateLogicalASAPDAGs::cost_sorted`] alone never
//! actually performed this composition step; `global_selection` is that
//! step, added alongside `cost_sorted` rather than replacing it (both stay
//! available — see [`RankedTargetSubDAGCandidates`] vs. [`TargetSubDAGSelection`]'s own docs for when
//! to reach for which).
//!
//! Two things this deliberately does **not** attempt, both left as
//! documented follow-up rather than silently overclaimed:
//!
//! - [`CostModel::rank_candidates`]/[`CostModel::size_params`] — the hooks
//!   [`ASAPStrategies`] groups rank by — take no `consumer_count`
//!   parameter at all today, so a `ASAPStrategies` group's selection
//!   here still falls back to [`rank_group`]'s ordinary (consumer-count-
//!   blind) local ranking, even though its own
//!   [`TargetSubDAGSelection::effective_consumer_count`] is computed and exposed
//!   correctly regardless. Wiring sketch sizing/ranking to actually consume
//!   it needs a `CostModel` interface change — out of scope here per this
//!   issue's own "reuse `CostModel`, don't invent a new interface" ask; a
//!   correct `effective_consumer_count` is the input such a future hook
//!   would need, and this module now computes it for every group, sketch
//!   groups included.
//! - This is not an exhaustive search over combinations of choices for a
//!   provably-global optimum in every case. [`CostModel::cse_share_decision`]
//!   is still a *local*, pairwise comparison at each `SharedSubDAGStrategy`
//!   site (recompute-total vs. one fixed maintenance cost) — this module
//!   just now feeds it a *correct* input instead of an *incorrect* one. Two
//!   sibling `SharedSubDAGStrategy` groups that could trade off against
//!   each other under some shared resource budget (memory, say) still
//!   aren't jointly optimized here — this crate has no
//!   cardinality/statistics estimation to bound a combinatorial search like
//!   that with (the same constraint #237/#263 already navigated), so real
//!   multi-group joint optimization beyond this per-site recurrence is left
//!   for whenever that changes.

use crate::accuracy::estimators::{
    cms::{cms_depth, cms_width},
    saturating_ceil,
};
use asap_types::ir::operator::non_asap::any_measure_filtered;
use asap_types::ir::scalar::resolve_column_ref;
use std::cell::RefCell;
use std::collections::{HashMap, HashSet, VecDeque};

use asap_types::ir::cse::{share_common_sub_dags, structural_hash, HashCache};
use asap_types::ir::operator::agg_intent::{agg_is_mergeable, AggIntent};
use asap_types::ir::operator::operator_properties::{BinaryOpKind, Reduction};
use asap_types::ir::properties::summary_coverage::{CoverageRegion, SummaryCoverage};
use asap_types::ir::properties::timing::validate_maintained;
use asap_types::ir::properties::{
    AccuracyError, CompositionOperator, GuaranteeSource, ResultGuarantee,
};
use asap_types::ir::properties::{ExecutionDataStateError, ExecutionTiming};
use asap_types::ir::scalar::{ArithmeticOpKind, ColumnRef};
use asap_types::ir::schema::{
    EntityIdentity, ExactKind, ExactParams, Field, FieldDataType, GroupingStrategy,
    NonNegativeWeightProof, SamplingKind, SamplingParams, Schema, SketchAlgorithm, SketchKind,
    SketchParams, SketchStatistic as PostAsapSketchStatistic, StatModelKind, StatModelParams,
    SummaryInputExpr, SummaryUpdate, WaveletKind, WaveletParams, WeightDomain,
};
use asap_types::ir::validate_maintained;
use asap_types::ir::SchemaDerivationError;
use asap_types::ir::{
    ASAPOp, BinaryOperator, NonASAPOp, Operator, OperatorNode, ProjectItem, ScalarExpr, SortKey,
};
use asap_types::physical::ExactOperationSchemaError;
use asap_types::types::AccuracyTarget;
use std::rc::{Rc, Weak};
use thiserror::Error;

use crate::accuracy::reconciliation::AccuracyReconciliationStrategy;
use crate::accuracy::{
    AccuracyBudgetAllocator, AccuracyEvidenceProvider, AccuracyModel, CompositionShape,
    DefaultAccuracyModel, EqualSplitAllocator, NoAccuracyEvidence,
};
use crate::cost_model::{CostModel, DefaultCostModel};
use crate::exact_composition::{ExactComposition, ExactCompositionStrategy, OperationPlacement};
use crate::grouping::HydraGroupingStrategy;
use crate::plan_selection::candidate_selection::{GlobalSelection, TargetSubDAGSelection};
use crate::rollup::RollupStrategy;
use crate::topk_reuse::TopKLimitReuseStrategy;

/// Errors from the pre-ASAP → post-ASAP replacement/construction path
/// ([`realize_child`] and [`retain_exact`]). Moved here from the former
/// `bind.rs` (issue #251): this is what a [`ReplacementStrategy`]
/// implementor's own construction path can realistically fail with —
/// schema derivation over a pre-ASAP [`OperatorNode`] sub-DAG — not
/// something specific to workload-wide orchestration.
#[derive(Debug, Error)]
pub enum RealizationError {
    /// Schema derivation failed while lifting an edge to `Schema`.
    #[error("schema derivation failed during pre-ASAP → post-ASAP binding: {0}")]
    Schema(#[from] SchemaDerivationError),
    /// The candidate is accuracy-illegal (issue #172): its composed
    /// guarantee has no sound propagation rule, or misses the applicable
    /// `AccuracyTarget`. Fail-closed — the candidate is never constructed
    /// with the child "treated as exact". [`ASAPStrategies::propose`]
    /// records it as a [`RejectedCandidate`] instead of a candidate.
    #[error("accuracy-illegal candidate: {0}")]
    Accuracy(#[from] AccuracyError),
    /// The selected summary family has a physical realization rule for this
    /// logical shape, but the rule cannot represent the complete input. The
    /// candidate must not fall back to ordinary one-node binding because that
    /// would change its semantics.
    #[error("unsupported physical summary realization: {0}")]
    PhysicalRealization(&'static str),
    /// A constructed plan violates the update/evaluation phase contract
    /// (issue #171) — e.g. a summary evaluation placed beneath a maintained
    /// `SummaryAgg`. Detected at construction, never at runtime.
    #[error("execution-data_state violation in post-ASAP plan: {0}")]
    ExecutionDataState(#[from] ExecutionDataStateError),
    /// An `ExactOperator`'s output schema could not be derived over its
    /// child — the child carries summary state the operator can't read.
    #[error("exact operator schema derivation failed: {0}")]
    ExactOperationSchema(#[from] ExactOperationSchemaError),
}

/// A pre-ASAP sub-DAG a [`ReplacementStrategy`] knows how to replace.
///
/// `root` is a reference into the workload's own [`OperatorNode`] DAG (an
/// `Rc<OperatorNode>`, the same currency [`search_workload`] and
/// `asap_types::ir::cse::share_common_sub_dags` already thread through
/// this crate's public API — not a bare `&OperatorNode` — so a strategy that
/// needs the node's own `Rc` identity, not just its shape, has it available
/// without the caller re-deriving it).
///
/// `consumer_count` is how many locations across the workload reference this
/// exact `Rc` — 1 for an ordinary single-use node and 2+ for a shared sub-DAG.
/// [`search_workload_with`] computes the workload-wide value during target
/// discovery. [`TargetSubDAG::new`] defaults it to `1` for callers invoking a
/// strategy against one node in isolation. A strategy that only cares about
/// `root`'s shape (for example, [`ASAPStrategies`]) can ignore the
/// count; [`SharedSubDAGStrategy`] consults it directly.
///
/// `strictest_sibling_accuracy` is the strictest accuracy among workload
/// siblings that read the same summary input as `root`, when stricter than
/// `root`'s own. [`search_workload_with`] sets it; [`ASAPStrategies`]
/// also sizes a candidate to it.
#[derive(Debug, Clone, Copy)]
pub struct TargetSubDAG<'a> {
    pub root: &'a Rc<OperatorNode>,
    pub consumer_count: usize,
    pub strictest_sibling_accuracy: Option<&'a AccuracyTarget>,
}

impl<'a> TargetSubDAG<'a> {
    /// A target assumed to have exactly one consumer — the common case for a
    /// caller that isn't already tracking cross-workload sharing.
    pub fn new(root: &'a Rc<OperatorNode>) -> Self {
        Self {
            root,
            consumer_count: 1,
            strictest_sibling_accuracy: None,
        }
    }

    /// A target with an explicit `consumer_count`, used by workload discovery
    /// and by callers that already know how many locations reference `root`.
    pub fn with_consumer_count(root: &'a Rc<OperatorNode>, consumer_count: usize) -> Self {
        Self {
            root,
            consumer_count,
            strictest_sibling_accuracy: None,
        }
    }
}

/// What a [`ReplacementSubDAG`] actually substitutes a [`TargetSubDAG`] with.
///
/// Generalizes [`realizations_for_intent`]'s two possible *kinds* of answer
/// — a post-ASAP binding decision, or a still-pre-ASAP structural alternative
/// — into "one candidate among several", each with its own
/// [`ReplacementSubDAG`].
#[derive(Debug, Clone)]
#[allow(clippy::large_enum_variant)] // Keep the public strategy API value-based.
pub enum Replacement {
    /// A sub-DAG that replaces the target: either a bound summary decision
    /// (a DAG containing ASAP operators, for one particular candidate
    /// realization of the target) or a pre-ASAP rewrite (a logical sub-DAG
    /// with no ASAP operator, structurally different from the target's own
    /// `root` — e.g. sharing vs. not sharing a sub-DAG — but semantically
    /// equivalent to it). [`is_logical_rewrite`] tells the two apart.
    SubDAG(Rc<OperatorNode>),
    /// An exact operator composed over another target's *own* selected
    /// decision across an explicit update/evaluation boundary (issue #171):
    /// `ValueOperationAtQueryTime` over a child's summary evaluation, or
    /// `ValueOperationAtIngestionTime` feeding a maintained summary above. Carries only a
    /// reference to the child target — [`CandidateLogicalASAPDAGs::global_selection`]
    /// commits the compatible parent/child pair and
    /// [`GlobalSelection::assemble_selected_dag`] links it into one validated
    /// `OperatorNode` DAG. See [`crate::exact_composition`].
    ExactComposition(ExactComposition),
}

/// Whether a [`Replacement::SubDAG`] is a pure logical rewrite: a sub-DAG
/// with no ASAP operator and no guarantee established yet (the shape every
/// front end emits and every rewrite strategy builds). A bound summary
/// decision contains an ASAP operator, or is a kept pre-ASAP sub-DAG that
/// already carries its exact guarantee.
pub fn is_logical_rewrite(node: &OperatorNode) -> bool {
    node.guarantee.is_none() && !node.contains_asap()
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

impl ReplacementSubDAG {
    /// Whether this summary still needs accuracy/domain evidence before it can
    /// be treated as certified. A missing guarantee on any summary candidate
    /// (a sub-DAG whose root is an ASAP operator) is unknown; a kept
    /// pre-ASAP sub-DAG carries an explicit exact guarantee.
    pub fn has_missing_accuracy_evidence(&self) -> bool {
        matches!(
            &self.replacement,
            Replacement::SubDAG(node) if !is_logical_rewrite(node) && has_missing_accuracy_evidence(node)
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReplacementProvenance {
    SummaryRealization,
    CseShare,
    CseRecompute,
    LogicalRewrite,
    /// [`crate::accuracy::reconciliation::AccuracyReconciliationStrategy`]'s
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
    /// [`Replacement::ExactComposition`] with
    /// [`OperationPlacement::Read`] (issue #171).
    ValueOperationAtQueryTime,
    /// [`Replacement::ExactComposition`] with
    /// [`OperationPlacement::Maintenance`] (issue #171).
    ValueOperationAtIngestionTime,
    /// A finalized whole-query result over rows carrying the PromQL series
    /// identity, which the logical root does not expose (see
    /// [`ReplacementStrategy::propose_for_root`]). Default selection never
    /// commits it, because its evaluation must be validated and priced by
    /// deployment; otherwise it would silently replace the logical plan.
    RootPhysicalRealization,
}

/// A candidate a strategy considered for a target but refused to propose on
/// accuracy-legality grounds (issue #172) — kept alongside the group's
/// legal candidates in [`TargetSubDAGCandidates::rejected`] so a rejection is as
/// inspectable (and exportable) as a selection. Never ranked: a
/// [`CostModel`] only ever sees [`TargetSubDAGCandidates::candidates`].
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
    domain_error: Option<ExecutionDataStateError>,
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
/// the same target; both [`ASAPStrategies`] and [`SharedSubDAGStrategy`]
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
    /// rejections land in [`TargetSubDAGCandidates::rejected`].
    fn propose(&self, target: &TargetSubDAG<'_>) -> Proposals {
        Proposals {
            candidates: self.replacements(target),
            rejected: Vec::new(),
            domain_error: None,
        }
    }

    /// Whole-query logical alternatives for a workload root under its
    /// end-to-end `target`. These may need input rows the root does not expose
    /// (for example, the PromQL series identity), so
    /// [`search_workload_with_targets`] asks only workload roots, once each.
    /// They decide what to compute, never placement. Default: none.
    fn propose_for_root(&self, _root: &Rc<OperatorNode>, _target: &AccuracyTarget) -> Proposals {
        Proposals::default()
    }
}

// ── Realization: how one AggIntent may be realised ───────────────────────

/// How an [`AggIntent`] may be realised at post-ASAP binding time (issue
/// #98): by an approximate summary (sketch, sample, wavelet, statistical
/// model, …), by an exact mergeable accumulator, or by an ordinary exact
/// operator (pass-through). This is a post-ASAP concern — the pre-ASAP IR
/// carries only the intent + accuracy target, never the realization — and
/// it's a per-node decision, made once per `AggIntent`, not a plan-wide one.
///
/// [`realizations_for_intent`] is where every valid realization gets
/// enumerated, exhaustive and ranked (most-preferred first) — this crate has
/// no separate function that computes just "the one" `Realization`
/// independently of that list. [`ASAPStrategies`] is the sole
/// consumer: it wraps every entry of this list into its own bound
/// [`OperatorNode`] and returns all of them, ranked — a caller wanting a
/// single answer keeps the first one itself (see the module docs above).
#[derive(Debug, Clone, PartialEq)]
pub enum Realization {
    /// An exact **mergeable** accumulator (partial state ≡ the value
    /// itself: `Sum` / `Count` / `Min` / `Max` / `Rate` / `Increase`). The
    /// built state *is* the answer already — no `SummaryEstimate` evaluation
    /// step.
    ExactAggregate {
        kind: ExactKind,
        params: ExactParams,
    },
    /// An approximate sketch sized to the intent's [`AccuracyTarget`].
    /// Needs a `SummaryEstimate` evaluation to recover a value. Already
    /// classified into its [`SketchKind`] category (`SketchKind::new`
    /// having been called) — construction always goes through that
    /// classifier, never this variant directly.
    Sketch(SketchKind),
    /// A sampling-based summary (a retained row subset). Needs a
    /// `SummaryEstimate` evaluation. Not chosen by any core `AggIntent`
    /// dispatch today — see the module docs.
    Sample {
        kind: SamplingKind,
        params: SamplingParams,
    },
    /// A wavelet-transform summary. Needs a `SummaryEstimate` evaluation. Not
    /// chosen by any core `AggIntent` dispatch today — see the module docs.
    Wavelet {
        kind: WaveletKind,
        params: WaveletParams,
    },
    /// A fitted statistical/parametric-model summary. Needs a
    /// `SummaryEstimate` evaluation. Not chosen by any core `AggIntent`
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

/// Does an already-**available** [`Realization`] — e.g. a summary
/// instance a downstream deployment already materialized somewhere, found
/// via whatever inventory/index that deployment keeps — satisfy a
/// **required** [`Realization`] (one of the candidates
/// [`realizations_for_intent`] produced for some [`AggIntent`])?
///
/// This is the query-optimization-literature "materialized view matching"
/// / "answering queries using views" question, narrowed to this crate's
/// summary vocabulary: not "can I build this from scratch" (that's what
/// [`realizations_for_intent`] answers) but "does something that already
/// exists answer this".
///
/// `asap-plan` deliberately ships no implementation of this trait and no
/// default method body — unlike [`realizations_for_intent`], which decision
/// an available `Realization` satisfies a required one is not a fact this
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
/// alongside `Realization`, which this trait's signature doesn't carry
/// because this crate has no inventory concept to carry it in).
pub trait Matcher {
    fn is_satisfied_by(&self, required: &Realization, available: &Realization) -> bool;
}

/// Confidence δ assumed when the target carries only an ε
/// (`AccuracyTarget::Epsilon`): the (ε, δ)-parameterised sketches (CMS) need
/// one. `ln(1/0.01) → depth 5`, matching the conventional CMS sizing.
pub const DEFAULT_DELTA: f64 = 0.01;

/// The sketch kinds that can serve an intent, most-preferred first.
/// This is the `AggIntent → SketchAlgorithm` map of issue #98;
/// [`realizations_for_intent`] sizes and ranks every entry via `cost_model`.
/// Listed here so the candidate set has one home.
pub fn summary_candidates(intent: &AggIntent) -> &'static [SketchAlgorithm] {
    match intent {
        AggIntent::Quantile { .. } => &[SketchAlgorithm::Kll, SketchAlgorithm::DDSketch],
        // A distinct-tuple count hashes the whole tuple as one item
        // (`SummaryInputExpr::Tuple`), which the distinct-count sketches take
        // unchanged. UnivMon is dropped there: it estimates frequency moments
        // over a single value stream, and `realize_value_frequency_summary_input`
        // would feed it one column of the tuple.
        AggIntent::Cardinality { cols, .. } if cols.len() > 1 => &[
            SketchAlgorithm::Hll,
            SketchAlgorithm::Theta,
            SketchAlgorithm::Kmv,
        ],
        AggIntent::Cardinality { .. } => &[
            SketchAlgorithm::Hll,
            SketchAlgorithm::Theta,
            SketchAlgorithm::Kmv,
            SketchAlgorithm::UnivMon,
        ],
        AggIntent::FrequencyL2 { .. } | AggIntent::FrequencyEntropy { .. } => {
            &[SketchAlgorithm::UnivMon]
        }
        // Count-Sketch-with-heap is CMS-with-heap's balanced/zero-mean-error
        // alternative for the same heavy-hitter shape.
        AggIntent::TopK { .. } => &[
            SketchAlgorithm::CmsWithHeap,
            SketchAlgorithm::CountSketchWithHeap,
        ],
        AggIntent::Count { .. } => &[
            SketchAlgorithm::Cms,
            SketchAlgorithm::CountSketch,
            SketchAlgorithm::UnivMon,
        ],
        _ => &[],
    }
}

/// The [`AccuracyTarget`] threaded onto an approximate-capable intent
/// (`Quantile`/`Cardinality`/`Count`/`TopK`), or `None` for every other
/// intent (no sketch candidate applies — [`realizations_for_intent`]'s own
/// match routes those elsewhere). Exposed so callers resolve the exact same
/// accuracy target [`realizations_for_intent`] does, without re-deriving it
/// from scratch.
pub fn accuracy_target(intent: &AggIntent) -> Option<&AccuracyTarget> {
    match intent {
        AggIntent::Quantile { accuracy, .. }
        | AggIntent::Cardinality { accuracy, .. }
        | AggIntent::FrequencyL2 { accuracy, .. }
        | AggIntent::FrequencyEntropy { accuracy, .. }
        | AggIntent::Count { accuracy }
        | AggIntent::TopK { accuracy, .. } => Some(accuracy),
        _ => None,
    }
}

/// Every valid [`Realization`] for `intent`, exhaustive and ranked
/// (most-preferred first via `cost_model`) — the *only* place this crate
/// decides what an `AggIntent` may become. Nothing in this crate computes
/// "the one" `Realization` independently of this list:
/// [`ASAPStrategies`] keeps every entry as a candidate, and a caller
/// that wants a single executable answer takes the head of *that* strategy's
/// output itself.
///
/// Exhaustive over the [`AggIntent`] vocabulary — adding a variant without an
/// explicit realization is a compile error, and the coverage-matrix test pins
/// each variant's category.
///
/// `pub(crate)`: [`ASAPStrategies::replacements`] is this module's
/// own caller; `grouping::HydraGroupingStrategy` (issue #256) is the one
/// caller outside it, needing the exact same already-ranked candidate list
/// to find the `Realization::Sketch` matching the Hydra-eligible kind it
/// is building a candidate for.
pub(crate) fn realizations_for_intent(
    intent: &AggIntent,
    cost_model: &dyn CostModel,
) -> Vec<Realization> {
    match intent {
        // ── Approximate-capable intents — the AccuracyTarget decides ────────
        AggIntent::Quantile { accuracy, .. }
        | AggIntent::Cardinality { accuracy, .. }
        | AggIntent::FrequencyL2 { accuracy, .. }
        | AggIntent::FrequencyEntropy { accuracy, .. }
        | AggIntent::Count { accuracy }
        | AggIntent::TopK { accuracy, .. } => match accuracy {
            AccuracyTarget::Exact if matches!(intent, AggIntent::Count { .. }) => vec![
                exact_realization(intent),
                Realization::Sketch(SketchKind::new(
                    SketchAlgorithm::UnivMon,
                    default_size_params(SketchAlgorithm::UnivMon, intent, 0.0, DEFAULT_DELTA),
                )),
            ],
            AccuracyTarget::Exact => vec![exact_realization(intent)],
            _ if matches!(intent, AggIntent::Count { .. }) => {
                let mut candidates = sketch_realizations(intent, accuracy, cost_model);
                candidates.push(exact_realization(intent));
                candidates
            }
            _ => sketch_realizations(intent, accuracy, cost_model),
        },

        // ── Exact mergeable accumulators ─────────────────────────────────────
        AggIntent::Sum { .. }
        | AggIntent::Min { .. }
        | AggIntent::Max { .. }
        | AggIntent::Rate
        | AggIntent::IRate
        | AggIntent::Increase => {
            let (kind, params) = crate::function_rules::function_rules(intent)
                .and_then(|rules| rules.accumulator)
                .expect("exact accumulator intents have registered realizations");
            vec![exact_accumulator(intent, kind, params)]
        }

        // ── Exact, non-mergeable reducers — richer partial state than a
        //    single value (see `agg_is_mergeable`), so no accumulator form.
        AggIntent::Avg { .. }
        | AggIntent::StdDev { .. }
        | AggIntent::Variance { .. }
        | AggIntent::PearsonCorr { .. } => {
            vec![Realization::PassThrough]
        }

        // ── Classic-bucket histogram_quantile (#79): exact `le`-bucket
        //    interpolation over pre-aggregated counts — NOT re-sketchable.
        //    (The native/raw form lowers to the generic `Quantile` above.)
        AggIntent::HistogramQuantile { .. } => vec![Realization::PassThrough],

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
        | AggIntent::TsOfLastOverTime => vec![Realization::PassThrough],

        // ── Group / count_values (#49): exact per `agg_is_exact`, but their
        //    output is structural (constant-1 / a synthesized label column),
        //    not a value a summary accumulator carries.
        AggIntent::Group | AggIntent::CountValues { .. } => vec![Realization::PassThrough],

        // ── Extension (deployment-model-specific, issue #131) — core has no
        //    realization opinion for a shape it doesn't know, so it defers
        //    entirely to the `CostModel` (issue #150): `realize_extension`
        //    defaults to `PassThrough`, preserving today's behavior for
        //    every deployment that doesn't override it. Core has no way to
        //    enumerate alternatives for an opaque deployment-defined shape,
        //    so this is always exactly one candidate. This is also the only
        //    path that can currently produce `Realization::Sample`/
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
fn exact_realization(intent: &AggIntent) -> Realization {
    match intent {
        AggIntent::Count { .. } => exact_accumulator(intent, ExactKind::Count, ExactParams::Count),
        _ => Realization::PassThrough,
    }
}

fn exact_accumulator(intent: &AggIntent, kind: ExactKind, params: ExactParams) -> Realization {
    // An exact accumulator is only sound when partial states merge
    // (`agg(A ∪ B) = combine(agg(A), agg(B))`).
    debug_assert!(
        agg_is_mergeable(intent),
        "accumulator for non-mergeable {intent:?}"
    );
    Realization::ExactAggregate { kind, params }
}

/// Resolve an [`AccuracyTarget`] into the `(eps, delta)` budget
/// [`CostModel::size_params`] needs. Shared by [`sketch_realizations`] and
/// this crate's own sizing — one place this resolution happens, so nothing
/// can drift apart on it.
///
/// `Exact` is unreachable via [`realizations_for_intent`] (which routes
/// `Exact` to [`exact_realization`] instead); degrades to the tightest
/// parameters for a caller that resolves it directly anyway.
pub fn accuracy_budget(accuracy: &AccuracyTarget) -> (f64, f64) {
    match accuracy {
        AccuracyTarget::Exact => (f64::MIN_POSITIVE, DEFAULT_DELTA),
        AccuracyTarget::Epsilon(e) => (*e, DEFAULT_DELTA),
        AccuracyTarget::EpsilonDelta { epsilon, delta } => (*epsilon, *delta),
    }
}

/// Every candidate sketch [`Realization`] for an approximate-capable
/// intent, sized to `accuracy` and ranked via `cost_model.rank_candidates`
/// (most-preferred first) — [`realizations_for_intent`]'s Sketch branch.
fn sketch_realizations(
    intent: &AggIntent,
    accuracy: &AccuracyTarget,
    cost_model: &dyn CostModel,
) -> Vec<Realization> {
    let (eps, delta) = accuracy_budget(accuracy);
    let ranked = crate::cost_model::validated_candidate_ranking(
        cost_model,
        intent,
        summary_candidates(intent),
    );
    ranked
        .into_iter()
        .filter_map(|algorithm| {
            let params = cost_model.size_params(algorithm.clone(), intent, eps, delta);
            sketch_state_bytes(&params)
                .is_none_or(|bytes| bytes <= DEFAULT_MAX_SKETCH_STATE_BYTES)
                .then(|| Realization::Sketch(SketchKind::new(algorithm, params)))
        })
        .collect()
}

/// Fail-safe ceiling used when a deployment has not supplied a tighter
/// resource model. It applies to one physical keyed state; grouped instance
/// multiplicity must be charged separately by deployment-aware costing.
pub const DEFAULT_MAX_SKETCH_STATE_BYTES: u64 = 512 * 1024 * 1024;

/// Conservative dense-counter allocation for CMS-family states. Returning
/// `None` leaves non-CMS families to their family-specific resource models.
pub fn sketch_state_bytes(params: &SketchParams) -> Option<u64> {
    if let SketchParams::UnivMon {
        heap_size,
        sketch_rows,
        sketch_cols,
        layers,
    } = params
    {
        return u64::from(*sketch_rows)
            .checked_mul(u64::from(*sketch_cols))?
            .checked_mul(8)?
            .checked_add(u64::from(*heap_size).checked_mul(64)?)?
            .checked_mul(u64::from(*layers));
    }
    let (width, depth, heap_size) = match params {
        SketchParams::Cms { width, depth } | SketchParams::CountSketch { width, depth } => {
            (*width, *depth, 0)
        }
        SketchParams::CmsWithHeap {
            width,
            depth,
            heap_size,
        }
        | SketchParams::CountSketchWithHeap {
            width,
            depth,
            heap_size,
        } => (*width, *depth, *heap_size),
        _ => return None,
    };
    // Eight-byte counters plus a conservative 64 bytes for each heap entry.
    u64::from(width)
        .checked_mul(u64::from(depth))?
        .checked_mul(8)?
        .checked_add(u64::from(heap_size).checked_mul(64)?)
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
    crate::accuracy::estimators::size_params(kind, intent, eps, delta)
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
/// called from [`default_size_params`] or [`realizations_for_intent`].
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
                heap_size: crate::accuracy::topk_capacity(k, eps),
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
        SketchAlgorithm::Kmv | SketchAlgorithm::UnivMon => {
            default_size_params(kind, intent, eps, delta)
        }
    }
}

// ── ASAPStrategies ─────────────────────────────────────────────────

/// A single static instance so [`ASAPStrategies::default_cost_model`]
/// can hand out a `&'static dyn CostModel` without heap-allocating one —
/// `DefaultCostModel` is a unit struct with no state, so one instance serves
/// every caller.
static DEFAULT_COST_MODEL: DefaultCostModel = DefaultCostModel;
static DEFAULT_ACCURACY_MODEL: DefaultAccuracyModel = DefaultAccuracyModel;
static DEFAULT_ALLOCATOR: EqualSplitAllocator = EqualSplitAllocator;
static NO_ACCURACY_EVIDENCE: NoAccuracyEvidence = NoAccuracyEvidence;

/// The cost, accuracy, allocation, and evidence inputs consulted during
/// candidate construction, bundled so the construction path threads one argument. `cost` ranks and sizes; `accuracy` and `allocator` decide
/// legality (issue #172) — see [`crate::accuracy`]'s module docs for why
/// those are separate from `cost` and run before it.
#[derive(Clone, Copy)]
pub(crate) struct CandidatePlanningInputs<'a> {
    pub cost: &'a dyn CostModel,
    pub accuracy: &'a dyn AccuracyModel,
    pub allocator: &'a dyn AccuracyBudgetAllocator,
    pub evidence: &'a dyn AccuracyEvidenceProvider,
}

impl<'a> CandidatePlanningInputs<'a> {
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

/// Proposes the supported ASAP realizations for a bindable aggregate, including
/// exact accumulators, approximate sketches, and supported maintained populations.
/// Each valid realization becomes its own [`ReplacementSubDAG`].
///
/// [`realizations_for_intent`] enumerates summary families; extension hooks can
/// supply additional supported families. This is not limited to sketch algorithms.
///
/// Ranked (only to *order the enumeration*, never to drop a candidate) via a
/// [`CostModel`] — [`DefaultCostModel`] unless constructed with
/// [`ASAPStrategies::new`] — so a deployment-specific cost model's
/// other hooks (`size_params`, `realize_extension`, `evaluation_extension`) are
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
pub struct ASAPStrategies<'a> {
    planning_inputs: CandidatePlanningInputs<'a>,
}

impl ASAPStrategies<'static> {
    /// A strategy that ranks/binds via the built-in [`DefaultCostModel`] —
    /// what a deployment gets with no custom cost model plugged in.
    pub fn default_cost_model() -> Self {
        Self {
            planning_inputs: CandidatePlanningInputs::with_default_accuracy(&DEFAULT_COST_MODEL),
        }
    }
}

impl<'a> ASAPStrategies<'a> {
    /// A strategy that ranks/binds via `cost_model` instead of the built-in
    /// static preference order — the same customization point
    /// [`realizations_for_intent`] already offers. Accuracy legality stays
    /// with the built-in [`DefaultAccuracyModel`]/[`EqualSplitAllocator`].
    pub fn new(cost_model: &'a dyn CostModel) -> Self {
        Self {
            planning_inputs: CandidatePlanningInputs::with_default_accuracy(cost_model),
        }
    }

    /// A strategy with every model plugged in explicitly: `cost_model` for
    /// ranking/sizing, `accuracy_model` for guarantee derivation/propagation/
    /// satisfaction, `allocator` for end-to-end budget splits. One model
    /// never overrides another: legality is settled by `accuracy_model`
    /// before `cost_model` ranks what is left.
    pub fn new_with_planning_inputs(
        cost_model: &'a dyn CostModel,
        accuracy_model: &'a dyn AccuracyModel,
        allocator: &'a dyn AccuracyBudgetAllocator,
    ) -> Self {
        Self {
            planning_inputs: CandidatePlanningInputs {
                cost: cost_model,
                accuracy: accuracy_model,
                allocator,
                evidence: &NO_ACCURACY_EVIDENCE,
            },
        }
    }

    /// Like [`Self::new_with_planning_inputs`], with typed planning-time evidence for
    /// rules such as TopK membership and Hydra shared-grid composition.
    pub fn new_with_planning_inputs_and_evidence(
        cost_model: &'a dyn CostModel,
        accuracy_model: &'a dyn AccuracyModel,
        allocator: &'a dyn AccuracyBudgetAllocator,
        evidence: &'a dyn AccuracyEvidenceProvider,
    ) -> Self {
        Self {
            planning_inputs: CandidatePlanningInputs {
                cost: cost_model,
                accuracy: accuracy_model,
                allocator,
                evidence,
            },
        }
    }

    /// Preserve the canonical Sort/Limit representation while exploring heap
    /// realizations of an instant-vector ranking under the caller's target.
    /// The input must carry the complete dynamic series identity. This never
    /// treats a range of historical samples as the instant vector.
    pub fn current_series_topk_candidates(
        &self,
        root: &Rc<OperatorNode>,
        accuracy: &AccuracyTarget,
    ) -> Proposals {
        let Some(NonASAPOp::Limit {
            n: Some(n),
            offset: 0,
            child,
            ..
        }) = root.non_asap()
        else {
            return Proposals::default();
        };
        let Some(NonASAPOp::Sort {
            keys,
            partition_by,
            child,
        }) = child.non_asap()
        else {
            return Proposals::default();
        };
        let [key] = keys.as_slice() else {
            return Proposals::default();
        };
        let ScalarExpr::Column(value) = key.expr else {
            return Proposals::default();
        };
        let schema = &child.schema;
        if key.ascending
            || key.nulls_first
            || partition_by.is_without()
            || !schema.has_promql_series_identity()
            || !schema
                .fields
                .get(value)
                .is_some_and(|column| column.name == "value")
            || !is_current_series_source(child)
        {
            return Proposals::default();
        }
        let Ok(ranked) =
            OperatorNode::new_shared(asap_types::ir::Operator::NonASAP(NonASAPOp::Aggregate {
                reduction: Reduction::Reduce(partition_by.clone()),
                measures: vec![AggIntent::TopK {
                    k: *n,
                    accuracy: accuracy.clone(),
                }],
                output_names: vec![],
                filters: vec![],
                having: None,
                child: Rc::clone(child),
            }))
        else {
            return Proposals::default();
        };
        self.propose_with(&ranked, None, None)
    }

    /// Fixed-window maintenance can finalize each series' counter state and
    /// build a fresh heap or grouped Sum for that evaluation window. Deployment must provide
    /// a complete, synchronized population and bind the matching window; this
    /// candidate never incrementally adds one window's rates to another.
    ///
    pub fn fixed_window_rate_candidates(&self, root: &Rc<OperatorNode>) -> Proposals {
        fn place(node: &Rc<OperatorNode>) -> Option<Rc<OperatorNode>> {
            retime_rate_finalize(node, ExecutionTiming::IngestionTime, true)
        }
        // Legal only if the candidate stays executable with its states maintained.
        let timed = |node: &Rc<OperatorNode>| {
            asap_types::ir::apply_materialization_timings(
                node,
                &asap_types::ir::MaterializationAssignment::all_ingestion_time(),
                &mut asap_types::ir::TimingMemo::new(),
            )
            .ok()
            .and_then(|timed| {
                asap_types::ir::physical_export::compile_physical_asap_dag(&timed).ok()
            })
        };
        let mut proposals = self.propose_with(root, None, None);
        proposals.candidates.retain_mut(|candidate| {
            let Replacement::SubDAG(node) = &candidate.replacement else {
                return false;
            };
            let Some(dag) = timed(node) else {
                return false;
            };
            if !dag.nodes.iter().any(|node| match &node.payload {
                asap_types::ir::physical_export::PhysicalASAPOperatorPayload::ASAP(
                    asap_types::ir::ASAPOp::SummaryAgg {
                        family: FieldDataType::Sketch(kind, _),
                        ..
                    },
                ) => matches!(
                    kind.algorithm(),
                    SketchAlgorithm::CmsWithHeap | SketchAlgorithm::CountSketchWithHeap
                ),
                asap_types::ir::physical_export::PhysicalASAPOperatorPayload::ASAP(
                    asap_types::ir::ASAPOp::SummaryAgg {
                        family: FieldDataType::ExactAggregate(ExactKind::Sum, _),
                        ..
                    },
                ) => true,
                _ => false,
            }) {
                return false;
            }
            let Some(placed) = place(node) else {
                return false;
            };
            if timed(&placed).is_none() {
                return false;
            }
            let Ok(placed) = finalize_query_candidate(placed, root) else {
                return false;
            };
            candidate.replacement = Replacement::SubDAG(placed);
            candidate
                .rationale
                .push_str("; fixed-window precompute over complete per-series counter states");
            true
        });
        proposals
    }

    /// Retain grouped Sum after a per-series Rate evaluation as a query-time
    /// candidate alongside its complete-window maintenance placement.
    ///
    pub fn query_time_rate_aggregation_candidates(&self, root: &Rc<OperatorNode>) -> Proposals {
        let mut proposals = self.fixed_window_rate_candidates(root);
        proposals.candidates.retain_mut(|candidate| {
            let Replacement::SubDAG(node) = &candidate.replacement else { return false };
            if !matches!(&node.operator, Operator::ASAP(ASAPOp::FinalizeExactAccumulator { child })
                if matches!(&child.operator, Operator::ASAP(ASAPOp::SummaryAgg { family: FieldDataType::ExactAggregate(ExactKind::Sum, _), .. }))) { return false; }
            let Some(query_time) = retime_rate_finalize(node, ExecutionTiming::QueryTime, false) else { return false };
            candidate.replacement = Replacement::SubDAG(query_time);
            candidate.rationale = "query-time grouped Sum over complete per-series Rate evaluations".into();
            true
        });
        proposals
    }

    pub(crate) fn from_planning_inputs(planning_inputs: CandidatePlanningInputs<'a>) -> Self {
        Self { planning_inputs }
    }

    /// The whole enumeration for one target, with `intent_override`
    /// substituting the target's own intent (only ever its `AccuracyTarget`
    /// differs — see [`realize_child_with`]). `strictest_sibling` adds each
    /// sketch resized to that stricter sibling accuracy (#509 summary
    /// capability): alone it only costs more, but post-ASAP CSE shares it
    /// with the sibling that needs it.
    fn propose_with(
        &self,
        root: &Rc<OperatorNode>,
        intent_override: Option<&AggIntent>,
        strictest_sibling: Option<&AccuracyTarget>,
    ) -> Proposals {
        let mut proposals = Proposals::default();
        // A selected logical rewrite otherwise stays a kept pre-ASAP sub-DAG
        // during DAG assembly. Also expose its concrete summary realization
        // for selection.
        if intent_override.is_none() {
            if let Some(rewritten) = crate::rewrite::composed_aggregate_rewrite(root) {
                if let Ok(node) = realize_child_with(&rewritten, self.planning_inputs, None) {
                    if node.contains_asap() {
                        proposals.candidates.push(ReplacementSubDAG {
                            replacement: Replacement::SubDAG(node),
                            strategy: "ASAPStrategies",
                            provenance: ReplacementProvenance::SummaryRealization,
                            rationale: "realize a schema-preserving composition of temporal and grouped accumulators".into(),
                        });
                    }
                }
            }
        }
        if let Ok(Some(node)) = exact_topk_over_temporal_values(root, self.planning_inputs) {
            proposals.candidates.push(ReplacementSubDAG {
                replacement: Replacement::SubDAG(node),
                strategy: "ASAPStrategies",
                provenance: ReplacementProvenance::SummaryRealization,
                rationale: "select exact Top-K from independently maintained temporal values"
                    .into(),
            });
        }
        if intent_override.is_none() {
            if let Ok(Some(node)) = realize_temporal_average(root, self.planning_inputs, None) {
                proposals.candidates.push(ReplacementSubDAG {
                    replacement: Replacement::SubDAG(node),
                    strategy: "ASAPStrategies",
                    provenance: ReplacementProvenance::SummaryRealization,
                    rationale: "read temporal average from sum/count only within the finite arithmetic domain; otherwise execute the original average".into(),
                });
            }
        }
        if intent_override.is_none() && is_supported_exact_binary(root) {
            if let Ok(Some(node)) = realize_binary(root, self.planning_inputs, None) {
                let rationale = if node.guarantee.is_none() {
                    "DDSketch quantile ratio without a certified end-to-end accuracy guarantee"
                } else {
                    "preserve exact PromQL arithmetic over independently realized summary operands"
                };
                proposals.candidates.push(ReplacementSubDAG {
                    replacement: Replacement::SubDAG(node),
                    strategy: "ASAPStrategies",
                    provenance: ReplacementProvenance::SummaryRealization,
                    rationale: rationale.into(),
                });
            }
            return proposals;
        }
        let Some(declared) = bindable_intent(root) else {
            return proposals;
        };
        let intent = intent_override.unwrap_or(declared);
        let planning_inputs = self.planning_inputs;

        // Is the child approximate? Probed once, up front: a candidate over
        // an approximate child needs the end-to-end budget split across both
        // layers, which changes which candidates exist at all.
        let child_layers = aggregate_child(root)
            .and_then(|child| realize_child_with(child, planning_inputs, None).ok())
            .and_then(|child| {
                child
                    .guarantee
                    .as_ref()
                    .filter(|g| !g.is_exact())
                    .map(ResultGuarantee::approximate_layer_count)
            });

        // `realizations_for_intent` is already exhaustive and ranked — no
        // separate dispatch needed here. Only `Sketch` has more than one
        // candidate in practice (every other variant's own dispatch produces
        // exactly one `Realization`), but this loop doesn't need to know
        // that; it just constructs whatever the list contains.
        for realization in realizations_for_intent(intent, planning_inputs.cost) {
            let rationale = describe_realization(intent, &realization);
            // The as-declared composition: every layer sized to its own
            // declared `AccuracyTarget`. Legal iff the composed guarantee
            // satisfies this node's target — a front end copying one target
            // onto every node does not make that so.
            proposals.record(
                rationale.clone(),
                construct_summary_with(
                    root,
                    intent,
                    realization.clone(),
                    planning_inputs,
                    None,
                    None,
                ),
            );

            // Sized for the strictest sibling reading the same summary input,
            // when that changes the parameters.
            if let (Some(stricter), Realization::Sketch(kind)) = (strictest_sibling, &realization) {
                let (eps, delta) = accuracy_budget(stricter);
                let algorithm = kind.algorithm().clone();
                let params =
                    planning_inputs
                        .cost
                        .size_params(algorithm.clone(), intent, eps, delta);
                if params != *kind.params() {
                    proposals.record(
                        format!(
                            "{rationale}; sized for the strictest sibling consumer {stricter:?}"
                        ),
                        construct_summary_with(
                            root,
                            &override_accuracy(intent, stricter),
                            Realization::Sketch(SketchKind::new(algorithm, params)),
                            planning_inputs,
                            None,
                            None,
                        ),
                    );
                }
            }

            // Budget-split alternatives (issue #172, PR 2): re-size this
            // layer and the approximate child under each allocation of this
            // node's target across every approximate layer.
            let (Some(child_layers), Realization::Sketch(kind), Some(target)) =
                (child_layers, &realization, accuracy_target(intent))
            else {
                continue;
            };
            let family = FieldDataType::Sketch(kind.clone(), GroupingStrategy::default());
            let Some(child) = aggregate_child(root) else {
                continue;
            };
            let Some(NonASAPOp::Aggregate { reduction, .. }) = root.non_asap() else {
                continue;
            };
            let Ok(input) = realize_physical_summary_input(intent, &family, reduction, child)
            else {
                continue;
            };
            let evaluation_query = evaluation(intent, &input.input, planning_inputs.cost);
            let Some(local) = planning_inputs
                .accuracy
                .local_guarantee(&family, &evaluation_query)
            else {
                continue;
            };
            let shape = CompositionShape {
                metric: local.metric,
                approximate_layer_count: 1 + child_layers,
            };
            let allocations = planning_inputs.allocator.allocations(target, &shape);
            if allocations.is_empty() {
                proposals.rejected.push(RejectedCandidate {
                    strategy: "ASAPStrategies",
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
                let resized = Realization::Sketch(SketchKind::new(
                    kind.algorithm().clone(),
                    planning_inputs
                        .cost
                        .size_params(kind.algorithm().clone(), intent, eps, delta),
                ));
                // Identical to the as-declared composition already recorded
                // above — nothing new to propose.
                if resized == realization && inner_target.as_ref() == declared_child_target {
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
                         {} approximate layers (this layer {outer_target:?}, child sub-DAG \
                         {inner_target:?})",
                        allocation.allocator, shape.approximate_layer_count
                    ),
                    construct_summary_with(
                        root,
                        intent,
                        resized,
                        planning_inputs,
                        inner_target.as_ref(),
                        Some(note),
                    ),
                );
            }
        }
        if proposals.candidates.is_empty() {
            if let Some(error) = &proposals.domain_error {
                if let Ok(node) = retain_exact(root) {
                    proposals.candidates.push(ReplacementSubDAG {
                        strategy: "ASAPStrategies",
                        replacement: Replacement::SubDAG(node),
                        provenance: ReplacementProvenance::SummaryRealization,
                        rationale: format!(
                            "{} stays pre-ASAP because summary construction crosses an illegal \
                             execution-data_state boundary ({error})",
                            describe_intent(intent)
                        ),
                    });
                }
            }
        }
        proposals
    }
}

impl Proposals {
    /// File one construction attempt: a legal node becomes a candidate, an
    /// [`RealizationError::Accuracy`] becomes a [`RejectedCandidate`], and a
    /// schema-derivation failure is skipped exactly as it always was.
    fn record(&mut self, rationale: String, built: Result<Rc<OperatorNode>, RealizationError>) {
        match built {
            Ok(node) => self.candidates.push(ReplacementSubDAG {
                strategy: "ASAPStrategies",
                replacement: Replacement::SubDAG(node),
                provenance: ReplacementProvenance::SummaryRealization,
                rationale,
            }),
            Err(RealizationError::Accuracy(error)) => self.rejected.push(RejectedCandidate {
                strategy: "ASAPStrategies",
                description: rationale,
                error,
            }),
            Err(RealizationError::ExecutionDataState(error)) => {
                self.domain_error.get_or_insert(error);
            }
            Err(
                RealizationError::Schema(_)
                | RealizationError::ExactOperationSchema(_)
                | RealizationError::PhysicalRealization(_),
            ) => {}
        }
    }
}

/// The `child` of a [`bindable_intent`]-shaped `Aggregate`.
fn aggregate_child(node: &OperatorNode) -> Option<&Rc<OperatorNode>> {
    match node.non_asap() {
        Some(NonASAPOp::Aggregate { child, .. }) => Some(child),
        _ => None,
    }
}

impl ReplacementStrategy for ASAPStrategies<'_> {
    fn matches(&self, target: &TargetSubDAG<'_>) -> bool {
        bindable_intent(target.root).is_some() || is_supported_exact_binary(target.root)
    }

    fn replacements(&self, target: &TargetSubDAG<'_>) -> Vec<ReplacementSubDAG> {
        self.propose(target).candidates
    }

    fn propose(&self, target: &TargetSubDAG<'_>) -> Proposals {
        self.propose_with(target.root, None, target.strictest_sibling_accuracy)
    }

    /// Heap realizations of an instant-vector ranking (current-series TopK).
    /// They rank rows that carry the complete PromQL series identity, which
    /// the logical root does not expose, so each is a finalized query result
    /// for the identity-carrying root. Placement variants (for example,
    /// fixed-window or query-time Rate aggregation) are not listed here: the
    /// materialization assigns timing and the physical compiler reads it.
    fn propose_for_root(&self, root: &Rc<OperatorNode>, target: &AccuracyTarget) -> Proposals {
        let Ok(typed) = asap_types::ir::schema_support::with_promql_series_identity(root) else {
            return Proposals::default();
        };

        let mut proposals = self.current_series_topk_candidates(&typed, target);
        for mut candidate in std::mem::take(&mut proposals.candidates) {
            let Replacement::SubDAG(node) = candidate.replacement else {
                continue;
            };
            let Ok(node) = finalize_query_candidate(node, &typed) else {
                continue;
            };
            let duplicate = proposals.candidates.iter().any(|existing| {
                matches!(&existing.replacement, Replacement::SubDAG(other) if *other == node)
            });
            if !duplicate {
                candidate.replacement = Replacement::SubDAG(node);
                candidate.provenance = ReplacementProvenance::RootPhysicalRealization;
                proposals.candidates.push(candidate);
            }
        }
        proposals
    }
}

/// A human-readable rationale for one candidate `Realization`, for
/// [`ReplacementSubDAG::rationale`] text.
fn describe_realization(intent: &AggIntent, realization: &Realization) -> String {
    match realization {
        Realization::Sketch(kind) => format!(
            "{} realizes as a {:?} sketch — one of summary_candidates' \
             candidates for this intent (asap_aware_mapping::replacement::realizations_for_intent)",
            describe_intent(intent),
            kind.algorithm()
        ),
        Realization::ExactAggregate { kind, .. } => format!(
            "{} realizes as an exact {kind:?} accumulator — the only realization \
             realizations_for_intent produces for this intent (no approximate \
             candidate applies)",
            describe_intent(intent)
        ),
        Realization::PassThrough => format!(
            "{} has no summary realization and stays a logical pass-through — the \
             only realization realizations_for_intent produces for this intent",
            describe_intent(intent)
        ),
        Realization::Sample { kind, .. } => format!(
            "{} realizes as a {kind:?} sample — the only realization the plugged-in \
             CostModel produced for this intent",
            describe_intent(intent)
        ),
        Realization::Wavelet { kind, .. } => format!(
            "{} realizes as a {kind:?} wavelet transform — the only realization the \
             plugged-in CostModel produced for this intent",
            describe_intent(intent)
        ),
        Realization::StatModel { kind, .. } => format!(
            "{} realizes as a {kind:?} statistical model — the only realization the \
             plugged-in CostModel produced for this intent",
            describe_intent(intent)
        ),
    }
}

/// A short human-readable label for an `AggIntent`, for
/// [`ReplacementSubDAG::rationale`] text. Not exhaustive by design (unlike
/// this crate's other `AggIntent` matches, e.g. [`realizations_for_intent`]'s)
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
        AggIntent::Cardinality { cols, .. } if cols.len() > 1 => {
            format!("cardinality (distinct count over {} columns)", cols.len())
        }
        AggIntent::Cardinality { .. } => "cardinality (distinct count)".to_string(),
        AggIntent::TopK { k, .. } => format!("top-{k} heavy-hitters"),
        AggIntent::Count { .. } => "count".to_string(),
        other => format!("{other:?}"),
    }
}

// ── realize_child / retain_exact: rank-and-take-first, and its fallback ──

/// Rank-and-take-first selector for a single [`OperatorNode`]: enumerate
/// every candidate via [`ASAPStrategies::replacements`], keep the
/// `cost_model`-preferred (first) one, and fall back to [`retain_exact`]
/// when there's no candidate at all — **not** a general single-answer API
/// for a whole workload. Use [`CandidateLogicalASAPDAGs::global_selection`] and DAG assembly
/// for coordinated logical selection; physical deployment remains downstream.
/// `root` must already be the caller's own
/// `Rc`, never fabricated per call, so this never allocates beyond what the
/// caller already held.
///
/// `pub(crate)`: reachable from this module's own construction helper
/// ([`construct_summary_agg`], so a nested aggregate gets its own
/// independent enumeration instead of inheriting the parent's forced
/// candidate), from this module's own [`realize_one`] (the representative
/// bound `OperatorNode` [`cse_preference`] needs for a
/// [`CostModel::cse_share_decision`] comparison), and from
/// [`crate::cost_model::DefaultCostModel::estimate_cost`] (the same
/// representative-node need, for a [`Replacement::Rewrite`] candidate's own
/// cost estimate). Every other caller goes through
/// [`ASAPStrategies::replacements`] directly and decides for itself.
pub(crate) fn realize_child(
    root: &Rc<OperatorNode>,
    cost_model: &dyn CostModel,
) -> Result<Rc<OperatorNode>, RealizationError> {
    realize_child_with(
        root,
        CandidatePlanningInputs::with_default_accuracy(cost_model),
        None,
    )
}

/// [`realize_child`] with every model explicit, plus an optional
/// `end_to_end_target` for `root`'s own value (issue #172): when an
/// [`AccuracyBudgetAllocator`] hands an approximate child a share of its
/// parent's budget, the child is re-enumerated with that share substituted
/// for its declared `AccuracyTarget` — sizing its sketch (and, recursively,
/// re-splitting for its own approximate children) under the allocated
/// budget. A child whose declared target is `Exact` keeps it: an allocation
/// never approximates something the caller declared exact.
fn exact_topk_over_temporal_values(
    root: &Rc<OperatorNode>,
    planning_inputs: CandidatePlanningInputs<'_>,
) -> Result<Option<Rc<OperatorNode>>, RealizationError> {
    let Some(NonASAPOp::Aggregate {
        reduction,
        measures,
        output_names: _,
        filters,
        having: None,
        child,
    }) = root.non_asap()
    else {
        return Ok(None);
    };
    if any_measure_filtered(filters) {
        return Ok(None);
    }
    let [AggIntent::TopK { k, .. }] = measures.as_slice() else {
        return Ok(None);
    };
    let Some(NonASAPOp::Aggregate {
        reduction: Reduction::PerEntity,
        child: input,
        ..
    }) = child.non_asap()
    else {
        return Ok(None);
    };
    if !matches!(input.non_asap(), Some(NonASAPOp::TimeRange { .. })) {
        return Ok(None);
    }
    let values = realize_child_with(child, planning_inputs, Some(&AccuracyTarget::Exact))?;
    if !values.contains_asap()
        || !values
            .guarantee
            .as_ref()
            .is_some_and(ResultGuarantee::is_exact)
    {
        return Ok(None);
    }
    let values = finalize_query_candidate(values, child)?;
    let partition_by = reduction
        .group_keys()
        .ok_or(RealizationError::PhysicalRealization(
            "temporal ranking requires explicit grouping",
        ))?
        .clone();
    let score = ranking_score_index(child, &values.schema)?;
    let guarantee = values.guarantee.clone();
    let schema = values.schema.clone();
    let sorted = Rc::new(
        OperatorNode::with_schema(
            Operator::NonASAP(NonASAPOp::Sort {
                keys: vec![SortKey {
                    expr: ScalarExpr::Column(score),
                    ascending: false,
                    nulls_first: false,
                }],
                partition_by: partition_by.clone(),
                child: values,
            }),
            schema.clone(),
        )
        .with_guarantee(guarantee.clone()),
    );
    let node = Rc::new(
        OperatorNode::with_schema(
            Operator::NonASAP(NonASAPOp::Limit {
                n: Some(*k),
                offset: 0,
                partition_by,
                child: sorted,
            }),
            schema,
        )
        .with_guarantee(guarantee),
    );
    validate_maintained(&node, ExecutionTiming::QueryTime)?;
    Ok(Some(node))
}

fn realize_temporal_average(
    root: &Rc<OperatorNode>,
    planning_inputs: CandidatePlanningInputs<'_>,
    target: Option<&AccuracyTarget>,
) -> Result<Option<Rc<OperatorNode>>, RealizationError> {
    let Some(components) = crate::rewrite::temporal_average_components(root) else {
        return Ok(None);
    };
    let mut node = realize_child_with(&components, planning_inputs, target)?;
    let Operator::NonASAP(NonASAPOp::BinaryOp { operator, .. }) =
        &mut Rc::make_mut(&mut node).operator
    else {
        return Ok(None);
    };
    operator.checked_finite_division = true;
    validate_maintained(&node, ExecutionTiming::QueryTime)?;
    Ok(Some(node))
}

pub(crate) fn realize_child_with(
    root: &Rc<OperatorNode>,
    planning_inputs: CandidatePlanningInputs<'_>,
    end_to_end_target: Option<&AccuracyTarget>,
) -> Result<Rc<OperatorNode>, RealizationError> {
    if let Some(node) = realize_temporal_average(root, planning_inputs, end_to_end_target)? {
        return Ok(node);
    }
    if let Some(composed) = realize_binary(root, planning_inputs, end_to_end_target)? {
        return Ok(composed);
    }
    let overridden = end_to_end_target.and_then(|target| {
        let declared = bindable_intent(root)?;
        match accuracy_target(declared) {
            Some(AccuracyTarget::Exact) | None => None,
            Some(_) => Some(override_accuracy(declared, target)),
        }
    });
    match ASAPStrategies::from_planning_inputs(planning_inputs)
        .propose_with(root, overridden.as_ref(), None)
        .candidates
        .into_iter()
        .next()
    {
        Some(ReplacementSubDAG {
            replacement: Replacement::SubDAG(node),
            ..
        }) => Ok(node),
        Some(ReplacementSubDAG {
            replacement: Replacement::ExactComposition(_),
            ..
        }) => {
            unreachable!("ASAPStrategies never returns a composition candidate")
        }
        // No candidate at all: `root` isn't `bindable_intent` shape (or its
        // intent has no realization `realizations_for_intent` can't
        // produce — never happens, that match is exhaustive), or every
        // candidate was accuracy-illegal — either way the same conservative
        // fallback `ASAPStrategies::matches` uses: keep the
        // pre-ASAP sub-DAG, executed exactly.
        None => retain_exact(root),
    }
}

/// Preserve an exact arithmetic root while allowing each vector operand to
/// select its own summary realization. If either vector arm cannot be
/// accelerated, return `None` so the caller keeps the whole query exact;
/// mixed raw/summary snapshots are never constructed.
fn realize_binary(
    root: &Rc<OperatorNode>,
    planning_inputs: CandidatePlanningInputs<'_>,
    end_to_end_target: Option<&AccuracyTarget>,
) -> Result<Option<Rc<OperatorNode>>, RealizationError> {
    let Some(NonASAPOp::BinaryOp {
        operator,
        return_bool,
        lhs,
        rhs,
    }) = root.non_asap()
    else {
        return Ok(None);
    };
    let (op, vector_match) = (&operator.kind, &operator.vector_match);
    if !matches!(op, BinaryOpKind::Arithmetic(_)) || vector_match.is_some() {
        return Ok(None);
    }

    let mut lhs_node = realize_binary_operand(lhs, planning_inputs, None)?;
    let mut rhs_node = realize_binary_operand(rhs, planning_inputs, None)?;

    let direct_ddsketch_ratio = matches!(op, BinaryOpKind::Arithmetic(ArithmeticOpKind::Div))
        && shared_quantile_target(lhs, rhs).is_some();
    let ratio_target = end_to_end_target
        .cloned()
        .or_else(|| shared_quantile_target(lhs, rhs));

    let mut ratio_domains = None;
    if direct_ddsketch_ratio {
        if let Some(target) = ratio_target
            .as_ref()
            .and_then(ddsketch_ratio_operand_target)
        {
            let (alpha, _) = accuracy_budget(&target);
            let lhs_domain = planning_inputs.evidence.quantile_input_domain(lhs);
            let rhs_domain = planning_inputs.evidence.quantile_input_domain(rhs);
            if [&lhs_domain, &rhs_domain]
                .into_iter()
                .flatten()
                .any(|domain| !domain.supports_ddsketch(alpha))
            {
                return Ok(None);
            }
            let domains = lhs_domain.zip(rhs_domain).map(|(lhs, rhs)| [lhs, rhs]);
            let has_mean = [lhs, rhs]
                .iter()
                .any(|expr| matches!(bindable_intent(expr), Some(AggIntent::Avg { .. })));
            if has_mean
                && domains.as_ref().is_none_or(|domains| {
                    domains.iter().any(|domain| {
                        !(domain.lower.abs().max(domain.upper.abs()) * domain.max_samples as f64)
                            .is_finite()
                    })
                })
            {
                return Ok(None);
            }
            lhs_node = realize_ddsketch_quantile_operand(lhs, planning_inputs, &target)?;
            rhs_node = realize_ddsketch_quantile_operand(rhs, planning_inputs, &target)?;
            if let Some(domains) = domains.as_ref() {
                for (domain, node) in domains.iter().zip([&lhs_node, &rhs_node]) {
                    if !ddsketch_quantile_alpha(node)
                        .or_else(|| {
                            node.guarantee
                                .as_ref()
                                .is_some_and(ResultGuarantee::is_exact)
                                .then_some(alpha)
                        })
                        .is_some_and(|alpha| domain.supports_ddsketch(alpha))
                    {
                        return Ok(None);
                    }
                }
            }
            ratio_domains = domains;
        }
    } else if let Some(target) = end_to_end_target {
        let operand_guarantees = [lhs_node.guarantee.as_ref(), rhs_node.guarantee.as_ref()];
        if operand_guarantees.iter().any(Option::is_none) {
            return Ok(None);
        }
        let approximate = operand_guarantees
            .into_iter()
            .enumerate()
            .filter_map(|(index, guarantee)| {
                guarantee
                    .filter(|guarantee| !guarantee.is_exact())
                    .map(|guarantee| (index, guarantee.metric))
            })
            .collect::<Vec<_>>();
        if !approximate.is_empty() {
            let metric = approximate[0].1;
            if approximate
                .iter()
                .any(|(_, candidate)| *candidate != metric)
            {
                return Ok(None);
            }
            let shape = CompositionShape {
                metric,
                approximate_layer_count: approximate.len(),
            };
            let Some(allocation) = planning_inputs
                .allocator
                .allocations(target, &shape)
                .into_iter()
                .next()
            else {
                return Ok(None);
            };
            for ((operand_index, _), local_target) in
                approximate.into_iter().zip(allocation.layers.iter())
            {
                if operand_index == 0 {
                    lhs_node = realize_binary_operand(lhs, planning_inputs, Some(local_target))?;
                } else {
                    rhs_node = realize_binary_operand(rhs, planning_inputs, Some(local_target))?;
                }
            }
        }
    }
    // Only direct quantile ratios may consume approximate division operands
    // without domain proof; their root guarantee remains unknown.
    if ratio_domains.is_none()
        && !direct_ddsketch_ratio
        && matches!(op, BinaryOpKind::Arithmetic(ArithmeticOpKind::Div))
        && [&lhs_node, &rhs_node].iter().any(|node| {
            !node
                .guarantee
                .as_ref()
                .is_some_and(ResultGuarantee::is_exact)
        })
    {
        return Ok(None);
    }

    let lhs_accelerated = lhs_node.contains_asap();
    let rhs_accelerated = rhs_node.contains_asap();
    if !lhs_accelerated || !rhs_accelerated {
        return Ok(None);
    }

    lhs_node = finalize_query_candidate(lhs_node, lhs)?;
    rhs_node = finalize_query_candidate(rhs_node, rhs)?;

    let has_ratio_domains = ratio_domains.is_some();
    let guarantee = if matches!(op, BinaryOpKind::Arithmetic(ArithmeticOpKind::Div))
        && direct_ddsketch_ratio
        && has_ratio_domains
        && [&lhs_node, &rhs_node].iter().all(|node| {
            ddsketch_quantile_alpha(node).is_some()
                || node
                    .guarantee
                    .as_ref()
                    .is_some_and(ResultGuarantee::is_exact)
        }) {
        [lhs_node.guarantee.clone(), rhs_node.guarantee.clone()]
            .into_iter()
            .collect::<Option<Vec<_>>>()
            .and_then(|inputs| {
                planning_inputs
                    .accuracy
                    .propagate(
                        &CompositionOperator::ExactDivision,
                        &inputs,
                        None,
                        &crate::accuracy::PropagationStats {
                            division_operand_domains: ratio_domains,
                            ..Default::default()
                        },
                    )
                    .ok()
            })
    } else {
        [lhs_node.guarantee.as_ref(), rhs_node.guarantee.as_ref()]
            .into_iter()
            .all(|guarantee| guarantee.is_some_and(ResultGuarantee::is_exact))
            .then(|| ResultGuarantee::exact("BinaryOp over exact operands"))
    };

    if direct_ddsketch_ratio && has_ratio_domains && guarantee.is_none() {
        return Ok(None);
    }

    Ok(Some(Rc::new(
        OperatorNode::with_schema(
            Operator::NonASAP(NonASAPOp::BinaryOp {
                operator: BinaryOperator {
                    checked_relative_division: false,
                    checked_finite_division: false,
                    kind: op.clone(),
                    vector_match: vector_match.clone(),
                },
                return_bool: *return_bool,
                lhs: lhs_node,
                rhs: rhs_node,
            }),
            root.schema.clone(),
        )
        // Exact arithmetic does not erase approximation error. Until the
        // accuracy algebra has an operator-specific rule (and any value-range
        // evidence needed by multiplication/division), unknown stays unknown.
        .with_guarantee(guarantee),
    )))
}

/// Rebuild the summary chain above a per-series `Rate` accumulator with its
/// `FinalizeExactAccumulator` placed at `timing`. `strict` additionally
/// requires the fixed-window shape (a `PerEntity` Rate over a `TimeRange`);
/// `None` when no such boundary exists (strict only).
fn retime_rate_finalize(
    node: &Rc<OperatorNode>,
    timing: ExecutionTiming,
    strict: bool,
) -> Option<Rc<OperatorNode>> {
    let is_rate_boundary = |child: &OperatorNode| match &child.operator {
        Operator::ASAP(ASAPOp::SummaryAgg {
            family: FieldDataType::ExactAggregate(ExactKind::Rate, _),
            reduction,
            child: source,
            ..
        }) => {
            !strict
                || (matches!(reduction, Reduction::PerEntity)
                    && matches!(source.non_asap(), Some(NonASAPOp::TimeRange { .. })))
        }
        _ => false,
    };
    let rebuilt = |operator: Operator, timing: Option<ExecutionTiming>| {
        let mut copy = node.as_ref().clone();
        copy.operator = operator;
        copy.timing = timing;
        Rc::new(copy)
    };
    match &node.operator {
        Operator::ASAP(ASAPOp::FinalizeExactAccumulator { child }) if is_rate_boundary(child) => {
            Some(rebuilt(node.operator.clone(), Some(timing)))
        }
        Operator::ASAP(
            ASAPOp::FinalizeExactAccumulator { child }
            | ASAPOp::SummaryAgg { child, .. }
            | ASAPOp::SummaryEstimate {
                summary_input: child,
                ..
            },
        ) => {
            let placed = match retime_rate_finalize(child, timing, strict) {
                Some(placed) => placed,
                None if strict => return None,
                None => return Some(Rc::clone(node)),
            };
            let operator = node.operator.with_new_children(|_| Rc::clone(&placed));
            Some(rebuilt(operator, node.timing))
        }
        _ if strict => None,
        _ => Some(Rc::clone(node)),
    }
}

/// Put an explicit read boundary between maintained exact state and a
/// query-time value consumer. Approximate summaries must already carry a
/// `SummaryEstimate`, so they deliberately do not pass this predicate.
pub fn finalize_query_candidate(
    node: Rc<OperatorNode>,
    logical_output: &OperatorNode,
) -> Result<Rc<OperatorNode>, RealizationError> {
    finalize_exact_accumulator(node, logical_output, ExecutionTiming::QueryTime)
}

/// The read boundary's placement is fixed here, where the candidate's
/// semantics decide it (a fresh query-time summary over this evaluation's
/// finalized values vs. finalized values feeding maintenance); the materialization
/// timing pass honors it.
fn finalize_exact_accumulator(
    node: Rc<OperatorNode>,
    logical_output: &OperatorNode,
    placement: ExecutionTiming,
) -> Result<Rc<OperatorNode>, RealizationError> {
    let is_exact_state = matches!(
        node.operator,
        Operator::ASAP(ASAPOp::SummaryAgg {
            family: FieldDataType::ExactAggregate(..),
            ..
        })
    );
    if !is_exact_state {
        return Ok(node);
    }
    // The child edge carries accumulator state, while this explicit read
    // boundary produces the logical operator's ordinary values. Preserve the
    // canonical pre-ASAP output types instead of leaking ExactAggregate into
    // query-time operators that follow this node.
    let schema = logical_output.schema.clone();
    let guarantee = node.guarantee.clone();
    Ok(Rc::new(
        OperatorNode::with_schema(
            Operator::ASAP(ASAPOp::FinalizeExactAccumulator { child: node }),
            schema,
        )
        .with_guarantee(guarantee)
        .with_timing(Some(placement)),
    ))
}

fn is_supported_exact_binary(root: &OperatorNode) -> bool {
    matches!(
        root.non_asap(),
        Some(NonASAPOp::BinaryOp {
            operator: BinaryOperator {
                kind: BinaryOpKind::Arithmetic(_),
                vector_match: None,
                ..
            },
            ..
        })
    )
}

/// Quantile operands inherit one workload target. A temporal mean is exact
/// on its checked finite domain and needs no approximation budget.
fn shared_quantile_target(lhs: &OperatorNode, rhs: &OperatorNode) -> Option<AccuracyTarget> {
    let quantile_target = |expr: &OperatorNode| match bindable_intent(expr) {
        Some(AggIntent::Quantile { accuracy, q, .. })
            if q.is_finite() && (0.0..=1.0).contains(q) =>
        {
            Some(accuracy.clone())
        }
        _ => None,
    };
    match (quantile_target(lhs), quantile_target(rhs)) {
        (Some(lhs), Some(rhs)) => (lhs == rhs).then_some(lhs),
        (Some(target), None) if matches!(bindable_intent(rhs), Some(AggIntent::Avg { .. })) => {
            Some(target)
        }
        (None, Some(target)) if matches!(bindable_intent(lhs), Some(AggIntent::Avg { .. })) => {
            Some(target)
        }
        _ => None,
    }
}

/// For `a / b`, two DDSketches with the same relative bound `alpha` produce
/// at most `2 * alpha / (1 - alpha)` relative error. Inverting that bound
/// gives `alpha = epsilon / (2 + epsilon)`.
fn ddsketch_ratio_operand_target(target: &AccuracyTarget) -> Option<AccuracyTarget> {
    let tighten =
        |epsilon: f64| (epsilon.is_finite() && epsilon > 0.0).then_some(epsilon / (2.0 + epsilon));
    match target {
        AccuracyTarget::Exact => None,
        AccuracyTarget::Epsilon(epsilon) => tighten(*epsilon).map(AccuracyTarget::Epsilon),
        AccuracyTarget::EpsilonDelta { epsilon, delta } => {
            tighten(*epsilon).map(|epsilon| AccuracyTarget::EpsilonDelta {
                epsilon,
                delta: *delta,
            })
        }
    }
}

fn ddsketch_quantile_alpha(node: &OperatorNode) -> Option<f64> {
    let Operator::ASAP(ASAPOp::SummaryEstimate {
        summary_input,
        query: PostAsapSketchStatistic::Quantile { .. },
    }) = &node.operator
    else {
        return None;
    };
    let Operator::ASAP(ASAPOp::SummaryAgg {
        family: FieldDataType::Sketch(kind, _),
        ..
    }) = &summary_input.operator
    else {
        return None;
    };
    match (kind.algorithm(), kind.params()) {
        (SketchAlgorithm::DDSketch, SketchParams::DDSketch { alpha }) => Some(*alpha),
        _ => None,
    }
}

fn has_missing_accuracy_evidence(node: &OperatorNode) -> bool {
    node.guarantee
        .as_ref()
        .is_none_or(ResultGuarantee::has_unknown)
}

/// A direct ratio has an operator-specific DDSketch proof, so it must select
/// DDSketch rather than the cost model's generally preferred KLL candidate.
fn realize_ddsketch_quantile_operand(
    operand: &Rc<OperatorNode>,
    planning_inputs: CandidatePlanningInputs<'_>,
    target: &AccuracyTarget,
) -> Result<Rc<OperatorNode>, RealizationError> {
    let intent = bindable_intent(operand).and_then(|intent| match intent {
        AggIntent::Quantile { .. } => Some(override_accuracy(intent, target)),
        _ => None,
    });
    let Some(intent) = intent else {
        return realize_binary_operand(operand, planning_inputs, Some(target));
    };
    let (epsilon, delta) = accuracy_budget(target);
    let realization = Realization::Sketch(SketchKind::new(
        SketchAlgorithm::DDSketch,
        planning_inputs
            .cost
            .size_params(SketchAlgorithm::DDSketch, &intent, epsilon, delta),
    ));
    construct_summary_with(operand, &intent, realization, planning_inputs, None, None)
}

fn realize_binary_operand(
    operand: &Rc<OperatorNode>,
    planning_inputs: CandidatePlanningInputs<'_>,
    end_to_end_target: Option<&AccuracyTarget>,
) -> Result<Rc<OperatorNode>, RealizationError> {
    realize_child_with(operand, planning_inputs, end_to_end_target)
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

/// Keep an unrewritten pre-ASAP sub-DAG as it is. There is no wrapper node:
/// the sub-DAG itself is the plan, carrying an exact guarantee. The same
/// `Rc` is returned when the node already has a guarantee; otherwise a copy
/// with `guarantee = exact("RetainedExact")` — only for a sub-DAG with no
/// ASAP operator (a sub-DAG containing one keeps whatever its construction
/// established). `pub` so a caller can fall back to this explicitly — e.g.
/// when `ASAPStrategies::replacements()` returns no candidate for a
/// target, or a deployment wants to force a node its own runtime can't
/// actually implement — through the same fallback this crate's own dispatch
/// uses.
pub fn retain_exact(expr: &Rc<OperatorNode>) -> Result<Rc<OperatorNode>, RealizationError> {
    retain_exact_rc(Rc::clone(expr))
}

fn retain_exact_rc(expr: Rc<OperatorNode>) -> Result<Rc<OperatorNode>, RealizationError> {
    if expr.guarantee.is_some() || expr.contains_asap() {
        return Ok(expr);
    }
    // Keeping the same sub-DAG twice (e.g. one `Scan` read by an exact
    // aggregate and by a sketch, or by two candidates) must yield one node:
    // sharing is pointer identity. Memoize the kept copy per input node while
    // both are alive; weak references keep the memo from extending lifetimes
    // or matching a reused address.
    type KeptMemo = HashMap<*const OperatorNode, (Weak<OperatorNode>, Weak<OperatorNode>)>;
    thread_local! {
        static KEPT: RefCell<KeptMemo> = RefCell::new(HashMap::new());
    }
    let key = Rc::as_ptr(&expr);
    if let Some(kept) = KEPT.with(|memo| {
        memo.borrow().get(&key).and_then(|(input, kept)| {
            input
                .upgrade()
                .filter(|input| Rc::ptr_eq(input, &expr))
                .and_then(|_| kept.upgrade())
        })
    }) {
        return Ok(kept);
    }
    let kept = Rc::new(
        expr.as_ref()
            .clone()
            // A kept pre-ASAP sub-DAG is executed exactly by the runtime
            // (`Realization::PassThrough`'s contract) — zero error.
            .with_guarantee(Some(ResultGuarantee::exact("RetainedExact"))),
    );
    KEPT.with(|memo| {
        let mut memo = memo.borrow_mut();
        if memo.len() > 4096 {
            memo.retain(|_, (input, kept)| input.strong_count() > 0 && kept.strong_count() > 0);
        }
        memo.insert(key, (Rc::downgrade(&expr), Rc::downgrade(&kept)));
    });
    Ok(kept)
}

// ── Construction: turn one already-decided Realization into an OperatorNode ─

/// The bindable shape [`ASAPStrategies`] targets: a single intent, no
/// `HAVING`. A multi-intent node (SQL `SELECT SUM(a), AVG(b)`), or one with a
/// `HAVING` predicate (the filter would need the estimate first), stays
/// logical. Unsupported logical parents are conservatively kept as pre-ASAP
/// sub-DAGs ([`retain_exact`]). Relational operators are retained during
/// final DAG assembly so their independently planned children remain
/// visible.
pub fn bindable_intent(node: &OperatorNode) -> Option<&AggIntent> {
    if let Some(NonASAPOp::Aggregate {
        measures,
        filters,
        having,
        ..
    }) = node.non_asap()
    {
        if let ([intent], None) = (measures.as_slice(), having) {
            if !any_measure_filtered(filters) {
                return Some(intent);
            }
        }
    }
    None
}

/// `expr` must still be the [`bindable_intent`] shape for `realization` to
/// have any effect; anything else falls back to [`retain_exact`].
/// Only `expr`'s own top-level decision is forced — recursion into `expr`'s
/// child goes back through [`realize_child`] (fresh candidate
/// enumeration, not a forced pick), so choosing one candidate for a target
/// never leaks into that target's own nested aggregates.
///
/// `pub(crate)`: `grouping::HydraGroupingStrategy` (issue #256) is the one
/// caller outside this module — the same first-class,
/// one-candidate-at-a-time primitive [`ASAPStrategies`] itself
/// calls once per candidate, reused rather than duplicated so a Hydra
/// candidate gets exactly the same schema derivation/column
/// resolution/evaluation construction as every other candidate, patching only
/// the `grouping` field this axis owns.
/// Construct a summary with every model explicit (issue #172). `intent`
/// is `expr`'s own [`bindable_intent`], or a copy of it with an allocated
/// `AccuracyTarget` substituted (see [`realize_child_with`]).
/// `child_target`, when set, is the end-to-end budget the child sub-DAG is
/// re-enumerated under; `allocation` is the provenance note recording the
/// split that produced both. `Err(RealizationError::Accuracy)` is the
/// fail-closed answer for a composition with no sound rule or one that
/// misses `intent`'s target.
pub(crate) fn construct_summary_with(
    expr: &OperatorNode,
    intent: &AggIntent,
    realization: Realization,
    planning_inputs: CandidatePlanningInputs<'_>,
    child_target: Option<&AccuracyTarget>,
    allocation: Option<GuaranteeSource>,
) -> Result<Rc<OperatorNode>, RealizationError> {
    let local_target = match allocation.as_ref() {
        Some(GuaranteeSource::BudgetAllocation { local_target, .. }) => Some(local_target),
        _ => accuracy_target(intent),
    };
    let estimator = crate::accuracy::EstimatorAccuracy::new(
        planning_inputs.accuracy,
        planning_inputs.evidence.estimator_contract(expr),
        local_target,
    );
    let realization = match realization {
        Realization::Sketch(kind) => match estimator.size_params(kind.algorithm()) {
            Some(params) => Realization::Sketch(SketchKind::new(kind.algorithm().clone(), params)),
            None => Realization::Sketch(kind),
        },
        other => other,
    };
    if let Some(NonASAPOp::Aggregate {
        reduction, child, ..
    }) = expr.non_asap()
    {
        // `bindable_intent` already established the shape: exactly one
        // intent, no HAVING. (Multi-intent nodes and HAVING stay logical.)
        if bindable_intent(expr).is_some() {
            if let Some((family, estimate)) = summary_family(realization) {
                let input = realize_physical_summary_input(intent, &family, reduction, child)?;
                let candidate = construct_summary_agg(
                    expr,
                    reduction,
                    intent,
                    input,
                    family,
                    estimate,
                    planning_inputs,
                    child_target,
                    allocation,
                )?;
                if is_snapshot_weighted_topk(intent, child) {
                    return finish_weighted_topk(candidate, expr, intent);
                }
                return Ok(candidate);
            }
        }
    }
    retain_exact_rc(Rc::new(expr.clone()))
}

fn finish_weighted_topk(
    candidate: Rc<OperatorNode>,
    logical: &OperatorNode,
    intent: &AggIntent,
) -> Result<Rc<OperatorNode>, RealizationError> {
    let AggIntent::TopK { k, .. } = intent else {
        unreachable!()
    };
    let Some(NonASAPOp::Aggregate {
        reduction: Reduction::Reduce(groups),
        child,
        ..
    }) = logical.non_asap()
    else {
        return Err(RealizationError::PhysicalRealization(
            "TopK requires explicit grouping",
        ));
    };
    let schema = child.schema.clone();
    let score = ranking_score_index(child, &schema)?;
    let cols = schema
        .fields
        .iter()
        .enumerate()
        .map(|(i, field)| {
            let source = if i == score {
                candidate.schema.fields.len() - 1
            } else {
                let matches = candidate
                    .schema
                    .fields
                    .iter()
                    .enumerate()
                    .filter(|(_, f)| f.name == field.name && f.dtype == field.dtype)
                    .map(|(i, _)| i)
                    .collect::<Vec<_>>();
                match matches.as_slice() {
                    [i] => *i,
                    _ => {
                        return Err(RealizationError::PhysicalRealization(
                            "ambiguous TopK output identity",
                        ))
                    }
                }
            };
            Ok(ProjectItem {
                alias: Some(field.name.clone()),
                expr: ScalarExpr::Column(source),
            })
        })
        .collect::<Result<Vec<_>, _>>()?;
    let guarantee = candidate.guarantee.clone();
    let projected = Rc::new(
        OperatorNode::with_schema(
            Operator::NonASAP(NonASAPOp::Project {
                cols,
                qualifier: None,
                child: candidate,
            }),
            schema.clone(),
        )
        .with_guarantee(guarantee.clone()),
    );
    let sorted = Rc::new(
        OperatorNode::with_schema(
            Operator::NonASAP(NonASAPOp::Sort {
                keys: vec![SortKey {
                    expr: ScalarExpr::Column(score),
                    ascending: false,
                    nulls_first: false,
                }],
                partition_by: groups.clone(),
                child: projected,
            }),
            schema.clone(),
        )
        .with_guarantee(guarantee.clone()),
    );
    let result = Rc::new(
        OperatorNode::with_schema(
            Operator::NonASAP(NonASAPOp::Limit {
                n: Some(*k),
                offset: 0,
                partition_by: groups.clone(),
                child: sorted,
            }),
            schema,
        )
        .with_guarantee(guarantee),
    );
    validate_maintained(&result, ExecutionTiming::QueryTime)?;
    Ok(result)
}

fn is_current_series_source(child: &OperatorNode) -> bool {
    let source = match child.non_asap() {
        Some(NonASAPOp::TimeRange { child, .. }) => child.as_ref(),
        _ => child,
    };
    matches!(source.non_asap(), Some(NonASAPOp::Scan {
        source: asap_types::ir::operator::Source::TimeSeries { .. }, schema, ..
    }) if schema.has_promql_series_identity())
}

fn is_snapshot_weighted_topk(intent: &AggIntent, child: &OperatorNode) -> bool {
    matches!(intent, AggIntent::TopK { .. })
        && (is_current_series_source(child)
            || matches!(child.non_asap(),
            Some(NonASAPOp::Aggregate { measures, child, .. })
                if matches!(measures.as_slice(), [AggIntent::Rate | AggIntent::Increase])
                    || (matches!(measures.as_slice(), [AggIntent::Sum { .. }])
                        && matches!(child.non_asap(), Some(NonASAPOp::Aggregate { measures, .. })
                            if matches!(measures.as_slice(), [AggIntent::Rate | AggIntent::Increase])))))
}

/// Translate an [`Realization`] into the `(family, needs a
/// SummaryEstimate evaluation)` pair [`construct_summary_agg`] needs, or `None`
/// for `PassThrough` (the caller falls back to [`retain_exact`]).
///
/// Every family's partial state needs a evaluation to recover a value, except
/// `ExactAggregate` — its partial state *is* the value already, so no
/// estimate step follows it.
fn summary_family(realization: Realization) -> Option<(FieldDataType, bool)> {
    Some(match realization {
        Realization::ExactAggregate { kind, params } => {
            (FieldDataType::ExactAggregate(kind, params), false)
        }
        Realization::Sketch(kind) => (
            FieldDataType::Sketch(kind, GroupingStrategy::default()),
            true,
        ),
        Realization::Sample { kind, params } => (FieldDataType::Sample(kind, params), true),
        Realization::Wavelet { kind, params } => (FieldDataType::Wavelet(kind, params), true),
        Realization::StatModel { kind, params } => (FieldDataType::StatModel(kind, params), true),
        Realization::PassThrough => return None,
    })
}

/// The physical input consumed by one summary realization. Most summaries
/// consume the logical aggregate's immediate child and summarize its declared
/// input value. Composite realizations can instead consume a larger
/// logical sub-DAG and bind a different key or value.
struct PhysicalSummaryInput {
    child: Rc<OperatorNode>,
    input: SummaryUpdate,
}

enum PhysicalSummaryInputRuleResult {
    NotApplicable,
    Realized(PhysicalSummaryInput),
    Unsupported(&'static str),
}

type PhysicalSummaryInputRule =
    fn(&AggIntent, &FieldDataType, &Reduction, &Rc<OperatorNode>) -> PhysicalSummaryInputRuleResult;

/// Ordered physical-realization rules for realizations that consume more
/// than the immediate logical input. New composite primitives add a rule here
/// instead of adding query- or algorithm-specific branches to
/// `construct_summary_agg`.
const PHYSICAL_SUMMARY_INPUT_RULES: &[PhysicalSummaryInputRule] = &[
    realize_value_frequency_summary_input,
    realize_counter_value_summary_input,
    realize_current_series_summary_input,
    realize_keyed_additive_summary_input,
];

fn realize_value_frequency_summary_input(
    intent: &AggIntent,
    family: &FieldDataType,
    _reduction: &Reduction,
    child: &Rc<OperatorNode>,
) -> PhysicalSummaryInputRuleResult {
    // Frequency counts hash sample values as items but add one per observation.
    // Using the sample as a weight would turn counts into sums and admit signed CMS updates.
    if !matches!(family, FieldDataType::Sketch(kind, _)
        if kind.algorithm() == &SketchAlgorithm::UnivMon
            || (matches!(intent, AggIntent::Count { .. })
                && matches!(kind.algorithm(), SketchAlgorithm::Cms | SketchAlgorithm::CountSketch)))
    {
        return PhysicalSummaryInputRuleResult::NotApplicable;
    }
    let schema = &child.schema;
    // One item per observation is a single value stream. `summary_candidates`
    // already withholds UnivMon from a distinct-tuple count; refused here too
    // so the invariant does not rest on that table alone.
    if intent.input_cols().len() > 1 {
        return PhysicalSummaryInputRuleResult::Unsupported(
            "a value-frequency summary reads a single column",
        );
    }
    PhysicalSummaryInputRuleResult::Realized(PhysicalSummaryInput {
        child: Rc::clone(child),
        input: SummaryUpdate {
            item: Some(SummaryInputExpr::Column(summarised_column(intent, schema))),
            weight: SummaryInputExpr::Constant(1.0),
            weight_domain: WeightDomain::NonNegative {
                proof: NonNegativeWeightProof::UnitCount,
            },
        },
    })
}

fn realize_physical_summary_input(
    intent: &AggIntent,
    family: &FieldDataType,
    reduction: &Reduction,
    child: &Rc<OperatorNode>,
) -> Result<PhysicalSummaryInput, RealizationError> {
    for rule in PHYSICAL_SUMMARY_INPUT_RULES {
        match rule(intent, family, reduction, child) {
            PhysicalSummaryInputRuleResult::NotApplicable => {}
            PhysicalSummaryInputRuleResult::Realized(input) => return Ok(input),
            PhysicalSummaryInputRuleResult::Unsupported(reason) => {
                return Err(RealizationError::PhysicalRealization(reason));
            }
        }
    }

    let child_schema = &child.schema;
    if matches!(intent, AggIntent::TopK { .. }) {
        return Err(RealizationError::PhysicalRealization(
            "Top-K needs an explicit item identity and additive update input",
        ));
    }
    Ok(PhysicalSummaryInput {
        child: Rc::clone(child),
        input: SummaryUpdate {
            item: None,
            weight: summarised_input(intent, child_schema)?,
            weight_domain: WeightDomain::UnknownOrSigned,
        },
    })
}

/// Emit `SummaryAgg` (recursively binding the child), plus the
/// `SummaryEstimate` evaluation when `estimate` is set.
// Retain the exact expression and schema while placing its value production
// on the update path (a node runs when its consumer runs, so beneath a
// maintained summary this value production is ingestion-time work).
// Read-time consumers keep their original shared nodes.
fn maintenance_exact_values(node: Rc<OperatorNode>) -> Option<Rc<OperatorNode>> {
    let operator = match &node.operator {
        // These guards can fall back at read time, but cannot recover a parent
        // sketch after an invalid value has entered its maintained state.
        Operator::NonASAP(NonASAPOp::BinaryOp { operator, .. })
            if operator.checked_finite_division || operator.checked_relative_division =>
        {
            return None;
        }
        Operator::NonASAP(NonASAPOp::BinaryOp {
            lhs,
            rhs,
            operator,
            return_bool,
        }) if operator.vector_match.is_none()
            && matches!(operator.kind, BinaryOpKind::Arithmetic(_))
            && node
                .guarantee
                .as_ref()
                .is_some_and(ResultGuarantee::is_exact) =>
        {
            Operator::NonASAP(NonASAPOp::BinaryOp {
                lhs: maintenance_exact_values(lhs.clone())?,
                rhs: maintenance_exact_values(rhs.clone())?,
                operator: operator.clone(),
                return_bool: *return_bool,
            })
        }
        Operator::ASAP(ASAPOp::FinalizeExactAccumulator { child })
            if matches!(
                child.operator,
                Operator::ASAP(ASAPOp::SummaryAgg {
                    family: FieldDataType::ExactAggregate(..),
                    ..
                })
            ) =>
        {
            Operator::ASAP(ASAPOp::FinalizeExactAccumulator {
                child: child.clone(),
            })
        }
        _ => return Some(node),
    };
    Some(Rc::new(
        OperatorNode::with_schema(operator, node.schema.clone())
            .with_guarantee(node.guarantee.clone()),
    ))
}

#[allow(clippy::too_many_arguments)]
fn construct_summary_agg(
    node: &OperatorNode,
    reduction: &Reduction,
    intent: &AggIntent,
    input: PhysicalSummaryInput,
    family: FieldDataType,
    estimate: bool,
    planning_inputs: CandidatePlanningInputs<'_>,
    child_target: Option<&AccuracyTarget>,
    allocation: Option<GuaranteeSource>,
) -> Result<Rc<OperatorNode>, RealizationError> {
    // The single canonical pre-ASAP derivation (per-series vs cross-series,
    // name overrides) already computes the row shape; binding only retypes
    // the summary state column.
    let keyed_heap = input.input.item.is_some()
        && matches!(
            &family,
            FieldDataType::Sketch(kind, _)
                if matches!(kind.algorithm(), SketchAlgorithm::CmsWithHeap | SketchAlgorithm::CountSketchWithHeap)
        );
    let snapshot_weighted = matches!(node.non_asap(), Some(NonASAPOp::Aggregate { child, .. })
        if is_snapshot_weighted_topk(intent, child));
    let mut family = family;
    let score_population = if snapshot_weighted {
        let bound = planning_inputs.evidence.topk_max_distinct_items(node);
        if bound.is_some_and(|n| n == 0 || n > (1u64 << 53)) {
            return Err(RealizationError::PhysicalRealization(
                "invalid weighted TopK distinct-item bound",
            ));
        }
        if let (Some(n), FieldDataType::Sketch(kind, grouping)) = (bound, &family) {
            let (eps, delta) = accuracy_budget(accuracy_target(intent).expect("TopK target"));
            let params = default_size_params(
                kind.algorithm().clone(),
                intent,
                eps,
                delta / (2.0 * n as f64),
            );
            family = FieldDataType::Sketch(
                SketchKind::new(kind.algorithm().clone(), params),
                grouping.clone(),
            );
        }
        bound
    } else {
        None
    };
    let physical_reduction = if snapshot_weighted {
        let Some(NonASAPOp::Aggregate { child, .. }) = node.non_asap() else {
            unreachable!()
        };
        let source = &input.child.schema;
        let Reduction::Reduce(keys) = reduction else {
            return Err(RealizationError::PhysicalRealization(
                "TopK requires explicit partitions",
            ));
        };
        if keys.is_without() {
            return Err(RealizationError::PhysicalRealization(
                "TopK requires explicit partitions",
            ));
        }
        let mapped = keys
            .iter()
            .map(|index| {
                let reference = schema_column_ref(child, *index).ok_or(
                    RealizationError::PhysicalRealization("invalid TopK partition key"),
                )?;
                let matches = source
                    .fields
                    .iter()
                    .enumerate()
                    .filter(|(_, column)| column_ref(column) == reference)
                    .map(|(index, _)| index)
                    .collect::<Vec<_>>();
                match matches.as_slice() {
                    [index] => Ok(*index),
                    _ => Err(RealizationError::PhysicalRealization(
                        "ambiguous TopK partition key",
                    )),
                }
            })
            .collect::<Result<Vec<_>, _>>()?;
        Reduction::by(mapped)
    } else if keyed_heap && matches!(reduction, Reduction::PerEntity) {
        Reduction::by(vec![])
    } else {
        reduction.clone()
    };
    let out_schema = &node.schema;
    let measures = match node.non_asap() {
        Some(NonASAPOp::Aggregate { measures, .. }) => measures.len(),
        _ => 1,
    };
    let state_idx = summary_col_index(out_schema, reduction, measures);

    let evaluation_schema = if keyed_heap
        && matches!(node.non_asap(), Some(NonASAPOp::Aggregate { child, .. }) if is_snapshot_weighted_topk(intent, child))
    {
        keyed_heap_evaluation_schema(&input, node)?
    } else {
        out_schema.clone()
    };

    let summary_input = input.input;
    let query = estimate.then(|| {
        if snapshot_weighted {
            if let FieldDataType::Sketch(kind, _) = &family {
                let capacity = match kind.params() {
                    SketchParams::CmsWithHeap { heap_size, .. }
                    | SketchParams::CountSketchWithHeap { heap_size, .. } => *heap_size,
                    _ => unreachable!(),
                };
                return PostAsapSketchStatistic::TopK {
                    k: capacity as usize,
                };
            }
        }
        evaluation(intent, &summary_input, planning_inputs.cost)
    });

    let mut state_schema = out_schema.clone();
    if keyed_heap {
        let mut state = state_schema.fields[state_idx].clone();
        state.dtype = family.clone();
        // A top-k's output row holds the ranked item at `state_idx`; the
        // state column is the heap itself.
        if let AggIntent::TopK { k, .. } = intent {
            state.name = format!("topk_{k}");
            state.nullable = false;
        }
        let mut fields = if snapshot_weighted {
            evaluation_schema.fields[..reduction.group_keys().map_or(0, |keys| keys.len())].to_vec()
        } else {
            Vec::new()
        };
        fields.push(state);
        state_schema = Schema::lifted(fields, None);
    } else if let Some(field) = state_schema.fields.get_mut(state_idx) {
        field.dtype = family.clone();
        field.nullable = false;
        if matches!(&family, FieldDataType::Sketch(kind, _) if kind.algorithm() == &SketchAlgorithm::UnivMon)
        {
            // State identity is independent of which statistic reads it.
            field.name = "univmon".into();
        } else if let (AggIntent::Quantile { .. }, SummaryInputExpr::Column(col)) =
            (intent, &summary_input.weight)
        {
            // The quantile is a evaluation parameter: name the state after the
            // column it summarizes, not after the query's output column.
            let child_schema = input.child.schema.clone();
            if let Ok(i) = resolve_column_ref(col, &child_schema) {
                field.name = child_schema.fields[i].name.clone();
            }
        }
    }

    let bound_child = realize_child_with(
        &input.child,
        planning_inputs,
        if snapshot_weighted {
            Some(&AccuracyTarget::Exact)
        } else {
            child_target
        },
    )?;
    let bound_child = if snapshot_weighted && is_current_series_source(&input.child) {
        // Explicit snapshot selection prevents historical observations from
        // becoming repeated weights in an instant-vector heap.
        let root = Rc::new(node.clone());
        let population = crate::maintained_population::MaintainedPopulationStrategy::new(
            std::slice::from_ref(&root),
        )
        .candidate(&root)
        .ok_or(RealizationError::PhysicalRealization(
            "snapshot ranking requires a supported current-series population",
        ))?;
        let Operator::ASAP(ASAPOp::EvaluatePopulation { child, .. }) = &population.operator else {
            return Err(RealizationError::PhysicalRealization(
                "missing population evaluation",
            ));
        };
        Rc::clone(child)
    } else if snapshot_weighted {
        // Each evaluation's finalized rates feed a fresh summary; rate snapshots
        // must never accumulate across evaluations. Query time is only the
        // initial layout; a maintained summary's materialization moves it to ingestion.
        finalize_query_candidate(bound_child, &input.child)?
    } else {
        let child =
            finalize_exact_accumulator(bound_child, &input.child, ExecutionTiming::IngestionTime)?;
        maintenance_exact_values(child).unwrap_or(retain_exact(&input.child)?)
    };

    // ── Guarantee (issue #172) ──────────────────────────────────────────
    // Derived *before* the node exists, so an illegal composition is never
    // materialized: the local guarantee of this family's evaluation (or exact
    // accumulator) composed over the child's, under the operator this
    // family applies to the child's values.
    let local_target = match allocation.as_ref() {
        Some(GuaranteeSource::BudgetAllocation { local_target, .. }) => Some(local_target),
        _ => accuracy_target(intent),
    };
    let estimator = crate::accuracy::EstimatorAccuracy::new(
        planning_inputs.accuracy,
        planning_inputs.evidence.estimator_contract(node),
        local_target,
    );
    let membership_query = if snapshot_weighted {
        Some(evaluation(intent, &summary_input, planning_inputs.cost))
    } else {
        query.clone()
    };
    let mut guarantee = compose_guarantee(
        &family,
        membership_query.as_ref(),
        &bound_child,
        intent,
        &estimator,
        planning_inputs.evidence,
        allocation,
    )?;

    if snapshot_weighted {
        use asap_types::ir::properties::{BoundExpr, ProbabilityExpr};
        let target = accuracy_target(intent).expect("TopK target");
        guarantee = if let Some(mut score) =
            estimator.local_guarantee(&family, query.as_ref().unwrap())
        {
            let count = match score_population {
                Some(n) => BoundExpr::Constant { value: n as f64 },
                None => {
                    score
                        .provenance
                        .push(GuaranteeSource::UnavailableStatistic {
                            statistic: "topk_max_distinct_items".into(),
                        });
                    BoundExpr::Unknown {
                        statistic: "topk_max_distinct_items".into(),
                    }
                }
            };
            score.provenance.push(GuaranteeSource::CompositionStep {
                operator: CompositionOperator::ApproximateAggregate,
                rule: "simultaneous_score_bounds_over_distinct_partition_item_identities".into(),
            });
            score.failure_probability = ProbabilityExpr::Scaled {
                count,
                inner: Box::new(score.failure_probability),
            };
            // #455: missing evidence preserves a logical candidate. Only known
            // contributions that already violate the target reject it here.
            if !estimator.satisfies(&score.optimistic_floor(), target) {
                return Err(RealizationError::PhysicalRealization(
                    "weighted TopK scores miss accuracy target",
                ));
            }
            let stats = planning_inputs.evidence.propagation_stats(
                &CompositionOperator::TopKSelection,
                &family,
                membership_query.as_ref(),
            );
            let mut joint = estimator.propagate(
                &CompositionOperator::TopKSelection,
                std::slice::from_ref(&score),
                None,
                &stats,
            )?;
            joint.failure_probability = ProbabilityExpr::UnionBound {
                terms: vec![joint.failure_probability, score.failure_probability],
            };
            if !estimator.satisfies(&joint.optimistic_floor(), target) {
                return Err(RealizationError::PhysicalRealization(
                    "weighted TopK joint guarantee misses target",
                ));
            }
            Some(joint)
        } else {
            None
        };
    }

    // `reduction` is carried onto `SummaryAgg` verbatim — not flattened to a
    // bare `Vec<ColumnId>` — so `SummaryExecutor::find_candidates` can tell
    // a genuine empty-`by` reduction apart from a per-entity shape with no
    // grouping concept at all (issue #163). `construct_summary_agg` is the
    // single place that decides this; nothing downstream re-derives it.
    let agg = OperatorNode::with_schema(
        asap_types::ir::Operator::ASAP(ASAPOp::SummaryAgg {
            child: bound_child,
            family,
            input: summary_input,
            reduction: physical_reduction,
            grouping: GroupingStrategy::default(),
            filter: None,
        }),
        state_schema,
    )
    .with_guarantee(
        // Summary *state* carries no caller-visible guarantee; only a
        // finalized value does. An exact accumulator's state is its value.
        if estimate { None } else { guarantee.clone() },
    );
    let agg = std::rc::Rc::new(agg);
    match query {
        // The evaluation: downstream of the estimate the schema is the plain
        // pre-ASAP row shape again (the summary-state type does not
        // propagate).
        Some(query) => Ok(std::rc::Rc::new(
            OperatorNode::with_schema(
                asap_types::ir::Operator::ASAP(ASAPOp::SummaryEstimate {
                    summary_input: agg,
                    query,
                }),
                evaluation_schema,
            )
            .with_guarantee(guarantee),
        )),
        None => Ok(agg),
    }
}

// Heap evaluation rows contain the encoded item identity, subpopulation keys,
// and an estimated score. They never inherit the exact-value producer's schema.
fn keyed_heap_evaluation_schema(
    input: &PhysicalSummaryInput,
    node: &OperatorNode,
) -> Result<Schema, RealizationError> {
    let source = &input.child.schema;
    let mut refs = Vec::new();
    let Some(NonASAPOp::Aggregate {
        reduction, child, ..
    }) = node.non_asap()
    else {
        return Err(RealizationError::PhysicalRealization(
            "heap evaluation requires an aggregate",
        ));
    };
    if let Reduction::Reduce(groups) = reduction {
        if groups.is_without() {
            return Err(RealizationError::PhysicalRealization(
                "heap evaluation requires explicit grouping",
            ));
        }
        for index in groups.iter() {
            refs.push(schema_column_ref(child, *index).ok_or(
                RealizationError::PhysicalRealization("invalid heap partition key"),
            )?);
        }
    }
    fn item_refs(
        item: &SummaryInputExpr,
        schema: &Schema,
        refs: &mut Vec<ColumnRef>,
    ) -> Result<(), RealizationError> {
        match item {
            SummaryInputExpr::Column(column) => refs.push(column.clone()),
            SummaryInputExpr::Tuple(items) => {
                for item in items {
                    item_refs(item, schema, refs)?;
                }
            }
            SummaryInputExpr::EntityIdentity(EntityIdentity::PromqlLabelSet { excluding }) => {
                if !schema.closed {
                    return Err(RealizationError::PhysicalRealization(
                        "dynamic label identity requires an explicit row representation",
                    ));
                }
                for (index, column) in schema.fields.iter().enumerate() {
                    if Some(index) != schema.time_index && column.name != "value" {
                        let reference = match &column.table {
                            Some(table) => ColumnRef::Qualified {
                                table: table.clone(),
                                name: column.name.clone(),
                            },
                            None => ColumnRef::Named(column.name.clone()),
                        };
                        if !excluding.contains(&reference) {
                            refs.push(reference);
                        }
                    }
                }
            }
            _ => {
                return Err(RealizationError::PhysicalRealization(
                    "unsupported heap item identity",
                ))
            }
        }
        Ok(())
    }
    item_refs(
        input
            .input
            .item
            .as_ref()
            .ok_or(RealizationError::PhysicalRealization(
                "heap item identity is missing",
            ))?,
        source,
        &mut refs,
    )?;
    let mut fields = Vec::<asap_types::ir::schema::Field>::new();
    for reference in refs {
        let matches: Vec<_> = source
            .fields
            .iter()
            .filter(|column| match &reference {
                ColumnRef::Named(name) => &column.name == name,
                ColumnRef::Qualified { table, name } => {
                    column.table.as_ref() == Some(table) && &column.name == name
                }
                ColumnRef::SampleValue => column.name == "value",
                ColumnRef::Wildcard => false,
            })
            .collect();
        let [column] = matches.as_slice() else {
            return Err(RealizationError::PhysicalRealization(
                "heap key must resolve to exactly one source column",
            ));
        };
        if fields.iter().any(|field| field.name == column.name) || column.name == "__asap_estimate"
        {
            return Err(RealizationError::PhysicalRealization(
                "heap keys must have distinct output names",
            ));
        }
        fields.push(Field::new(
            column.name.clone(),
            column.dtype.clone(),
            column.nullable,
        ));
    }
    if fields.is_empty() {
        return Err(RealizationError::PhysicalRealization(
            "heap evaluation has no identity columns",
        ));
    }
    fields.push(Field::new(
        "__asap_estimate",
        FieldDataType::Plain(asap_types::ir::schema::DataType::Float64),
        false,
    ));
    Ok(Schema::lifted(fields, None))
}

fn ranking_score_index(logical: &OperatorNode, values: &Schema) -> Result<usize, RealizationError> {
    if is_current_series_source(logical) {
        return values
            .fields
            .iter()
            .position(|field| {
                field.name == "value"
                    && field.dtype
                        == FieldDataType::Plain(asap_types::ir::schema::DataType::Float64)
            })
            .ok_or(RealizationError::PhysicalRealization(
                "snapshot ranking requires the sample value column",
            ));
    }
    let Some(NonASAPOp::Aggregate {
        reduction,
        measures,
        ..
    }) = logical.non_asap()
    else {
        return Err(RealizationError::PhysicalRealization(
            "ranking requires an explicit aggregate score",
        ));
    };
    if measures.len() != 1 {
        return Err(RealizationError::PhysicalRealization(
            "ranking requires exactly one score",
        ));
    }
    let index = match reduction {
        Reduction::Reduce(groups) if !groups.is_without() => groups.len(),
        Reduction::PerEntity => values
            .fields
            .iter()
            .position(|field| field.name == "value")
            .ok_or(RealizationError::PhysicalRealization(
                "ranking requires the sample value column",
            ))?,
        _ => {
            return Err(RealizationError::PhysicalRealization(
                "ranking requires explicit grouping",
            ))
        }
    };
    if Some(index) == values.time_index
        || !values.fields.get(index).is_some_and(|field| {
            matches!(
                field.dtype,
                FieldDataType::Plain(
                    asap_types::ir::schema::DataType::Int64
                        | asap_types::ir::schema::DataType::Float64
                )
            )
        })
    {
        return Err(RealizationError::PhysicalRealization(
            "ranking score must be numeric",
        ));
    }
    Ok(index)
}

/// Rebuild a heap from this evaluation's finalized per-series counter values.
/// The rate window is preserved; raw counter samples never become CMS weights.
fn realize_counter_value_summary_input(
    intent: &AggIntent,
    family: &FieldDataType,
    output_reduction: &Reduction,
    child: &Rc<OperatorNode>,
) -> PhysicalSummaryInputRuleResult {
    if !matches!(intent, AggIntent::TopK { .. })
        || !matches!(family, FieldDataType::Sketch(kind, _) if matches!(kind.algorithm(), SketchAlgorithm::CmsWithHeap | SketchAlgorithm::CountSketchWithHeap))
        || !matches!(child.non_asap(), Some(NonASAPOp::Aggregate { reduction: Reduction::PerEntity, measures, .. }) if matches!(measures.as_slice(), [AggIntent::Rate | AggIntent::Increase]))
    {
        return PhysicalSummaryInputRuleResult::NotApplicable;
    }
    let schema = &child.schema;
    if !schema.closed {
        return PhysicalSummaryInputRuleResult::Unsupported(
            "counter ranking needs the complete resolved series identity",
        );
    }
    let Reduction::Reduce(groups) = output_reduction else {
        return PhysicalSummaryInputRuleResult::Unsupported(
            "counter ranking requires explicit partitions",
        );
    };
    if groups.is_without() {
        return PhysicalSummaryInputRuleResult::Unsupported(
            "counter ranking requires resolved partitions",
        );
    }
    // Retain the evaluation timestamp in each returned row. This sketch is a
    // snapshot, not an additive history of successive rate evaluations.
    let items = schema
        .fields
        .iter()
        .enumerate()
        .filter(|(index, column)| column.name != "value" && !groups.contains(index))
        .map(|(index, _)| schema_column_ref(child, index).map(SummaryInputExpr::Column))
        .collect::<Option<Vec<_>>>();
    let Some(items) = items.filter(|items| !items.is_empty()) else {
        return PhysicalSummaryInputRuleResult::Unsupported("counter ranking has no item columns");
    };
    PhysicalSummaryInputRuleResult::Realized(PhysicalSummaryInput {
        child: Rc::clone(child),
        input: SummaryUpdate {
            item: Some(SummaryInputExpr::Tuple(items)),
            weight: SummaryInputExpr::Column(ColumnRef::SampleValue),
            weight_domain: WeightDomain::NonNegative {
                proof: NonNegativeWeightProof::ResetAwareCounterDerivative,
            },
        },
    })
}

/// An instant-vector source has one current value per full series identity.
/// Rebuild the state for each evaluation; historical samples are not updates.
fn realize_current_series_summary_input(
    intent: &AggIntent,
    family: &FieldDataType,
    output_reduction: &Reduction,
    child: &Rc<OperatorNode>,
) -> PhysicalSummaryInputRuleResult {
    if !matches!(intent, AggIntent::TopK { .. }) || !is_current_series_source(child) {
        return PhysicalSummaryInputRuleResult::NotApplicable;
    }
    let FieldDataType::Sketch(kind, _) = family else {
        return PhysicalSummaryInputRuleResult::NotApplicable;
    };
    match kind.algorithm() {
        SketchAlgorithm::CountSketchWithHeap => {}
        SketchAlgorithm::CmsWithHeap => {
            return PhysicalSummaryInputRuleResult::Unsupported(
                "current sample values do not prove non-negative CMS weights",
            )
        }
        _ => return PhysicalSummaryInputRuleResult::NotApplicable,
    }
    let Reduction::Reduce(groups) = output_reduction else {
        return PhysicalSummaryInputRuleResult::Unsupported(
            "snapshot ranking requires explicit partitions",
        );
    };
    if groups.is_without() {
        return PhysicalSummaryInputRuleResult::Unsupported(
            "snapshot ranking requires resolved partitions",
        );
    }
    let schema = &child.schema;
    let items = schema
        .fields
        .iter()
        .enumerate()
        .filter(|(index, column)| column.name != "value" && !groups.contains(index))
        .map(|(index, _)| schema_column_ref(child, index).map(SummaryInputExpr::Column))
        .collect::<Option<Vec<_>>>();
    let Some(items) = items.filter(|items| !items.is_empty()) else {
        return PhysicalSummaryInputRuleResult::Unsupported(
            "snapshot ranking has no item identity",
        );
    };
    PhysicalSummaryInputRuleResult::Realized(PhysicalSummaryInput {
        child: Rc::clone(child),
        input: SummaryUpdate {
            item: Some(SummaryInputExpr::Tuple(items)),
            weight: SummaryInputExpr::Column(ColumnRef::SampleValue),
            weight_domain: WeightDomain::UnknownOrSigned,
        },
    })
}

/// Realize the composite heavy-hitter realization for
/// `TopK(Count GROUP BY key)`. The heap sketch consumes the raw keyed stream;
/// it does not consume an independently materialized Count result.
fn realize_keyed_additive_summary_input(
    intent: &AggIntent,
    family: &FieldDataType,
    output_reduction: &Reduction,
    child: &Rc<OperatorNode>,
) -> PhysicalSummaryInputRuleResult {
    if !matches!(intent, AggIntent::TopK { .. }) {
        return PhysicalSummaryInputRuleResult::NotApplicable;
    }
    let FieldDataType::Sketch(kind, _) = family else {
        return PhysicalSummaryInputRuleResult::NotApplicable;
    };
    let heap_algorithm = kind.algorithm();
    if !matches!(
        heap_algorithm,
        SketchAlgorithm::CmsWithHeap | SketchAlgorithm::CountSketchWithHeap
    ) {
        return PhysicalSummaryInputRuleResult::NotApplicable;
    }
    let Some(NonASAPOp::Aggregate {
        reduction,
        measures,
        having: None,
        child: raw_child,
        ..
    }) = child.non_asap()
    else {
        return PhysicalSummaryInputRuleResult::NotApplicable;
    };
    let counter_input = matches!(measures.as_slice(), [AggIntent::Sum { .. }])
        && matches!(raw_child.non_asap(), Some(NonASAPOp::Aggregate { measures, .. })
            if matches!(measures.as_slice(), [AggIntent::Rate | AggIntent::Increase]));
    let weight = match measures.as_slice() {
        [AggIntent::Count { .. }] => SummaryInputExpr::Constant(1.0),
        [AggIntent::Sum { .. }] if counter_input => {
            SummaryInputExpr::Column(ColumnRef::SampleValue)
        }
        [AggIntent::Sum { col }] => SummaryInputExpr::Column(match col {
            None => ColumnRef::SampleValue,
            Some(index) => match schema_column_ref(raw_child, *index) {
                Some(column) => column,
                None => {
                    return PhysicalSummaryInputRuleResult::Unsupported(
                        "sum-ranked Top-K value column is outside the raw input schema",
                    )
                }
            },
        }),
        _ => return PhysicalSummaryInputRuleResult::NotApplicable,
    };
    let weight_domain = match measures.as_slice() {
        [AggIntent::Count { .. }] => WeightDomain::NonNegative {
            proof: NonNegativeWeightProof::UnitCount,
        },
        [AggIntent::Sum { .. }] if counter_input => WeightDomain::NonNegative {
            proof: NonNegativeWeightProof::ResetAwareCounterDerivative,
        },
        _ => WeightDomain::UnknownOrSigned,
    };
    if matches!(heap_algorithm, SketchAlgorithm::CmsWithHeap)
        && !matches!(weight_domain, WeightDomain::NonNegative { .. })
    {
        return PhysicalSummaryInputRuleResult::Unsupported(
            "value-weighted CMS requires non-negative update evidence; use CountSketch for arbitrary values",
        );
    }
    let subpopulation_columns = match output_reduction {
        Reduction::PerEntity => vec![],
        Reduction::Reduce(keys) => keys
            .iter()
            .filter_map(|index| schema_column_ref(child, *index))
            .collect(),
    };
    let item = match reduction {
        Reduction::PerEntity => SummaryInputExpr::EntityIdentity(EntityIdentity::PromqlLabelSet {
            excluding: subpopulation_columns,
        }),
        Reduction::Reduce(keys) if !keys.is_without() && !keys.is_empty() => {
            let Some(columns) = keys
                .iter()
                .map(|index| schema_column_ref(raw_child, *index))
                .collect::<Option<Vec<_>>>()
            else {
                return PhysicalSummaryInputRuleResult::Unsupported(
                    "ranked item column is outside the raw input schema",
                );
            };
            let item_columns: Vec<_> = columns
                .into_iter()
                .filter(|column| !subpopulation_columns.contains(column))
                .collect();
            match item_columns.as_slice() {
                [] => {
                    return PhysicalSummaryInputRuleResult::Unsupported(
                        "subpopulation columns consume the complete ranked item identity",
                    )
                }
                [column] => SummaryInputExpr::Column(column.clone()),
                _ => SummaryInputExpr::Tuple(
                    item_columns
                        .into_iter()
                        .map(SummaryInputExpr::Column)
                        .collect(),
                ),
            }
        }
        Reduction::Reduce(_) => {
            return PhysicalSummaryInputRuleResult::Unsupported(
                "an empty or without grouping does not identify ranked items",
            )
        }
    };
    PhysicalSummaryInputRuleResult::Realized(PhysicalSummaryInput {
        child: Rc::clone(raw_child),
        input: SummaryUpdate {
            item: Some(item),
            weight,
            weight_domain,
        },
    })
}

fn schema_column_ref(child: &OperatorNode, index: usize) -> Option<ColumnRef> {
    let column = child.schema.fields.get(index)?;
    Some(match &column.table {
        Some(table) => ColumnRef::Qualified {
            table: table.clone(),
            name: column.name.clone(),
        },
        None => ColumnRef::Named(column.name.clone()),
    })
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
    family: &FieldDataType,
    query: Option<&PostAsapSketchStatistic>,
    child: &OperatorNode,
    intent: &AggIntent,
    accuracy: &dyn AccuracyModel,
    evidence: &dyn AccuracyEvidenceProvider,
    allocation: Option<GuaranteeSource>,
) -> Result<Option<ResultGuarantee>, AccuracyError> {
    let (op, local) = match (family, query) {
        (FieldDataType::ExactAggregate(kind, _), _) => {
            let op = match kind {
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
                _ => {
                    crate::function_rules::function_rules(intent)
                        .expect("exact accumulator intents have registered accuracy rules")
                        .accuracy
                }
            };
            (
                op,
                Some(ResultGuarantee::exact(format!("ExactAggregate({kind:?})"))),
            )
        }
        (_, Some(query)) => (
            if matches!(query, PostAsapSketchStatistic::TopK { .. }) {
                CompositionOperator::TopKSelection
            } else {
                CompositionOperator::ApproximateAggregate
            },
            accuracy.local_guarantee(family, query),
        ),
        (_, None) => (CompositionOperator::ApproximateAggregate, None),
    };
    let Some(input) = child.guarantee.clone() else {
        // Shape is constructible, but the child has no accuracy certificate.
        return Ok(None);
    };
    if local.is_none() {
        // No local error model: retain the candidate with unknown accuracy.
        // Propagating the input alone would falsely certify the summary.
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
        if !accuracy.satisfies(&guarantee.optimistic_floor(), target) {
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
/// `reduction` is the caller's already-read `Reduction` (issue #165).
/// `without` output is `kept labels ++ measures`, and its keys are the
/// *excluded* labels, so the state column follows the kept labels instead.
fn summary_col_index(out_schema: &Schema, reduction: &Reduction, measures: usize) -> usize {
    match reduction {
        Reduction::PerEntity => out_schema
            .column_id("value")
            .or_else(|| (0..out_schema.fields.len()).find(|&i| Some(i) != out_schema.time_index))
            .unwrap_or(0),
        Reduction::Reduce(keys) if keys.is_without() => {
            out_schema.fields.len().saturating_sub(measures)
        }
        Reduction::Reduce(keys) => keys.len(),
    }
}

/// The column fed into a *single-column* summary: the intent's leading
/// positional input resolved to a name against the child schema, or the PromQL
/// sample value when it reads none. Callers are responsible for only reaching
/// here with a one-column intent — [`summarised_input`] is the general form.
fn summarised_column(intent: &AggIntent, child_schema: &Schema) -> ColumnRef {
    match intent
        .input_cols()
        .first()
        .and_then(|id| child_schema.fields.get(*id))
    {
        Some(c) => column_ref(c),
        None => ColumnRef::SampleValue,
    }
}

pub(crate) fn column_ref(column: &Field) -> ColumnRef {
    match &column.table {
        Some(t) => ColumnRef::Qualified {
            table: t.clone(),
            name: column.name.clone(),
        },
        None => ColumnRef::Named(column.name.clone()),
    }
}

/// What the summary consumes per input row. An intent that reads one column (or
/// none) feeds that column; `COUNT(DISTINCT a, b)` feeds the whole tuple as one
/// item, so the distinct-count sketch hashes `(a, b)` rather than `a` — the
/// difference between tuple cardinality and single-column cardinality.
///
/// A tuple leg outside the child schema is an error rather than
/// [`summarised_column`]'s sample-value fallback: a leg has no sample-value
/// reading, and silently dropping one would under-count.
pub(crate) fn summarised_input(
    intent: &AggIntent,
    child_schema: &Schema,
) -> Result<SummaryInputExpr, RealizationError> {
    let cols = intent.input_cols();
    if cols.len() < 2 {
        return Ok(SummaryInputExpr::Column(summarised_column(
            intent,
            child_schema,
        )));
    }
    let legs = cols
        .iter()
        .map(|id| child_schema.fields.get(*id).map(column_ref))
        .collect::<Option<Vec<_>>>()
        .ok_or(RealizationError::PhysicalRealization(
            "a tuple column is outside the input schema",
        ))?;
    Ok(SummaryInputExpr::Tuple(
        legs.into_iter().map(SummaryInputExpr::Column).collect(),
    ))
}

/// The `SummaryEstimate` evaluation for a summary-bound intent.
fn evaluation(
    intent: &AggIntent,
    input: &SummaryUpdate,
    cost_model: &dyn CostModel,
) -> PostAsapSketchStatistic {
    match intent {
        AggIntent::Quantile { q, .. } => PostAsapSketchStatistic::Quantile { q: *q },
        AggIntent::Cardinality { .. } => PostAsapSketchStatistic::Cardinality,
        AggIntent::FrequencyL2 { .. } => PostAsapSketchStatistic::FrequencyL2,
        AggIntent::FrequencyEntropy { .. } => PostAsapSketchStatistic::FrequencyEntropy,
        AggIntent::TopK { k, .. } => PostAsapSketchStatistic::TopK { k: *k },
        AggIntent::Count { .. } => PostAsapSketchStatistic::PointCount {
            key: match &input.weight {
                SummaryInputExpr::Column(col) => col.clone(),
                SummaryInputExpr::Constant(1.0) => ColumnRef::SampleValue,
                _ => unreachable!("point count requires one column"),
            },
            value: None,
        },
        // Core doesn't know the shape of a deployment-specific `Extension`
        // intent, so it can't build its evaluation either — delegate to the
        // same `CostModel` that decided (via `realize_extension`) this
        // intent gets a summary realization at all. See `evaluation_extension`'s
        // doc for the invariant this depends on.
        AggIntent::Extension { ext_kind, payload } => match &input.weight {
            SummaryInputExpr::Column(col) => {
                cost_model.evaluation_extension(ext_kind, payload, col)
            }
            _ => unreachable!("extension evaluation requires one column"),
        },
        other => {
            unreachable!("no summary realization for {other:?} (realizations_for_intent)")
        }
    }
}

// ── SharedSubDAGStrategy ────────────────────────────────────────────────

/// Wraps `asap_types::ir::cse::share_common_sub_dags`'s sharing
/// decision as an explicit candidate pair, wherever a [`TargetSubDAG`]
/// already has two or more consumers.
///
/// This strategy does not decide sharing itself, nor does it discover which
/// nodes are shared — by the time a caller builds a `TargetSubDAG` with
/// `consumer_count >= 2`, `share_common_sub_dags` has already made that
/// (legality-gated, `PartialEq`-checked) call; [`discover_targets`] below
/// discovers real consumer counts across a workload the same way for
/// [`search_workload_with`] (this module's own tests reuse the identical
/// dedup logic to build realistic fixtures — see the module docs'
/// "Non-goals" on why that traversal isn't itself part of this strategy).
/// This strategy only reframes "two or more consumers already share this
/// `Rc`" as the two-way choice a downstream cost model (today,
/// [`CostModel::cse_share_decision`]) picks between: build once and share, or
/// build independently at each consumer.
pub struct SharedSubDAGStrategy;

impl ReplacementStrategy for SharedSubDAGStrategy {
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
                strategy: "SharedSubDAGStrategy",
                // The already-interned `Rc` itself: reusing it verbatim *is*
                // "build once and share" — no new node to construct.
                replacement: Replacement::SubDAG(Rc::clone(target.root)),
                provenance: ReplacementProvenance::CseShare,
                rationale: format!(
                    "build once and share: share_common_sub_dags already interned this \
                     sub_dag once and reused it across {count} consumers — one build can \
                     answer all of them instead of computing it {count} times"
                ),
            },
            ReplacementSubDAG {
                strategy: "SharedSubDAGStrategy",
                // A structurally-identical but freshly-allocated `Rc`: same
                // value (`PartialEq`), deliberately *not* the same pointer,
                // representing "undo the sharing and recompute independently".
                replacement: Replacement::SubDAG(Rc::new((**target.root).clone())),
                provenance: ReplacementProvenance::CseRecompute,
                rationale: format!(
                    "build independently: undo the sharing share_common_sub_dags found and \
                     recompute this sub_dag separately at each of its {count} consumers — \
                     worth it only when independence outweighs the shared-maintenance cost, \
                     a CostModel's call (e.g. CostModel::cse_share_decision) and not this \
                     strategy's"
                ),
            },
        ]
    }
}

// ── Workload-wide search: TargetSubDAGCandidates / CandidateLogicalASAPDAGs / search_workload ──────────
//
// Merged in from the former `search.rs` (issue #252, part of #33) — see this
// file's own top-level "Workload-wide search" doc section for the full
// design rationale.

/// A generous, documented backstop against a hypothetically ill-behaved
/// future [`ReplacementStrategy`] (see the module docs' "Termination"
/// section) — not a bound either shipped strategy could ever approach.
/// [`ASAPStrategies`] and [`SharedSubDAGStrategy`] both converge in
/// exactly 2 passes over a fixed target set, regardless of workload size.
pub const MAX_SEARCH_ITERATIONS: usize = 1_000;

// ── TargetSubDAGCandidates ──────────────────────────────────────────────

/// Candidates for one distinct [`TargetSubDAG`] (its
/// own `target` `Rc<OperatorNode>`, keyed by pointer identity in
/// [`CandidateLogicalASAPDAGs`]'s internal map — never re-derived by value) plus every
/// [`ReplacementSubDAG`] alternative any registered [`ReplacementStrategy`]
/// proposed for it.
///
/// `candidates` is deliberately *not* required to be non-empty — a
/// `TargetSubDAG` no registered strategy has an opinion on still gets a
/// group (with an empty candidate list), so [`CandidateLogicalASAPDAGs`] always has
/// exactly one group per discovered `TargetSubDAG`, not "one group per
/// `TargetSubDAG` something matched".
#[derive(Debug, Clone)]
pub struct TargetSubDAGCandidates {
    /// The target sub-DAG this group is for.
    pub target: Rc<OperatorNode>,
    /// How many operator-child positions across the whole workload
    /// reference this exact `Rc` — see [`discover_targets`].
    pub consumer_count: usize,
    /// Every distinct alternative discovered for `target`, in discovery
    /// order (not ranked — see [`CandidateLogicalASAPDAGs::cost_sorted`] for the ranked
    /// view).
    pub candidates: Vec<ReplacementSubDAG>,
    /// Every candidate a strategy considered for `target` but refused on
    /// accuracy-legality grounds (issue #172), plus any `candidates` entry
    /// the root-target check ([`search_workload_with_targets`]) moved here.
    /// Never ranked — [`CandidateLogicalASAPDAGs::cost_sorted`]/[`CandidateLogicalASAPDAGs::global_selection`]
    /// read only `candidates`, so a [`CostModel`] cannot resurrect one.
    pub rejected: Vec<RejectedCandidate>,
}

impl TargetSubDAGCandidates {
    pub(crate) fn new(target: Rc<OperatorNode>, consumer_count: usize) -> Self {
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
                (Replacement::SubDAG(existing_rc), Replacement::SubDAG(rc))
                    if is_logical_rewrite(existing_rc) && is_logical_rewrite(rc) =>
                {
                    is_duplicate_rewrite(existing_rc, rc, &self.target)
                }
                (Replacement::SubDAG(existing_node), Replacement::SubDAG(node)) => {
                    is_duplicate_summary(existing_node, node)
                }
                (
                    Replacement::ExactComposition(existing),
                    Replacement::ExactComposition(candidate),
                ) => existing.same_as(candidate),
                // Different `Replacement` variants are never the same
                // candidate.
                _ => false,
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

/// Are `existing` and `candidate` the same logical-rewrite
/// [`Replacement::SubDAG`] candidate for a group targeting `target`?
///
/// Structural (`OperatorNode`) value equality alone is *not* enough here:
/// this module's one shipped multi-candidate logical-rewrite source,
/// [`SharedSubDAGStrategy`], deliberately returns **two** candidates that
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
/// [`structural_hash`] as a candidate-narrowing filter, `OperatorNode`'s
/// derived `PartialEq` as the actual decision — protecting against the
/// (currently hypothetical, since neither shipped strategy causes it)
/// case of the exact same alternative being proposed twice. A fresh
/// [`HashCache`] per call: this is a pairwise check between two candidates
/// for one group, not a bottom-up pass over a whole DAG, so there is no
/// wider traversal to amortize the cache across the way `InternTable`'s own
/// use of `structural_hash` does.
fn is_duplicate_rewrite(
    existing: &Rc<OperatorNode>,
    candidate: &Rc<OperatorNode>,
    target: &Rc<OperatorNode>,
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

/// Are `existing` and `candidate` the same bound-summary
/// [`Replacement::SubDAG`] candidate?
///
/// A bound summary embeds `SketchParams`/`f64`-bearing accuracy targets and
/// guarantees, so value equality is not a dedup decision this module is
/// willing to make (see [`structural_hash`]'s own doc on `f64` hashing).
/// Per this module's inherited "hash is a filter, `PartialEq` is the
/// decision, no exceptions" rule, there is no real equality check to back a
/// dedup *decision* here — and skipping the check is the only choice that
/// rule permits: never merging two candidates
/// is harmless (at worst, a redundant entry in a group's candidate list),
/// while comparing by some proxy this module can't actually verify (e.g.
/// `Debug` text, or `ReplacementSubDAG::rationale` — documented elsewhere in
/// this crate as prose for a report, "not machine parsing") risks exactly
/// the false-positive merge the rule exists to prevent. Both strategies
/// shipped today already return a structurally distinct candidate for every
/// entry of one `replacements()` call, so this is future-proofing against a
/// hypothetical repeat call, not a gap either strategy's own tests exercise.
fn is_duplicate_summary(_existing: &Rc<OperatorNode>, _candidate: &Rc<OperatorNode>) -> bool {
    false
}

// ── CandidateLogicalASAPDAGs ────────────────────────────────────────────────────────────

/// The deduped candidate space [`search_workload`]/[`search_workload_with`]
/// discover: one [`TargetSubDAGCandidates`] per distinct `TargetSubDAG` in the
/// (already-CSE'd) workload, plus the workload's own post-CSE roots so a
/// caller can still map a `Root`'s `Id` back to the `Rc<OperatorNode>` whose
/// group holds its alternatives. Memos are keyed by `*const OperatorNode`.
pub struct CandidateLogicalASAPDAGs<Id> {
    /// The workload's roots, after the one `share_common_sub_dags` pass
    /// [`search_workload_with`] runs up front — the same post-CSE roots
    /// every `TargetSubDAG` in `groups` was discovered from.
    pub roots: Vec<(Id, Rc<OperatorNode>)>,
    pub(crate) groups: HashMap<*const OperatorNode, TargetSubDAGCandidates>,
    /// Discovery order — stable iteration for [`CandidateLogicalASAPDAGs::target_subdag_candidates`]/
    /// [`CandidateLogicalASAPDAGs::cost_sorted`], since `HashMap` iteration order isn't.
    pub(crate) order: Vec<*const OperatorNode>,
    /// Composition proofs are computed with the search model, then retained
    /// through costing and DAG assembly so no later default can replace it.
    pub(crate) composition_plans: Vec<PreparedComposition>,
}

pub(crate) struct PreparedComposition {
    pub(crate) target: *const OperatorNode,
    pub(crate) operation: ExactComposition,
    pub(crate) child: Rc<OperatorNode>,
    pub(crate) plan: Rc<OperatorNode>,
}

impl<Id> CandidateLogicalASAPDAGs<Id> {
    fn prepare_compositions(
        &mut self,
        accuracy: &dyn AccuracyModel,
        targets: &HashMap<*const OperatorNode, Vec<AccuracyTarget>>,
    ) {
        self.composition_plans.clear();
        for group in self.groups.values() {
            for candidate in &group.candidates {
                let Replacement::ExactComposition(operation) = &candidate.replacement else {
                    continue;
                };
                let children: Vec<_> = match operation.placement {
                    OperationPlacement::Read => self
                        .groups
                        .get(&Rc::as_ptr(&operation.child_target))
                        .into_iter()
                        .flat_map(|g| &g.candidates)
                        .filter_map(|c| match &c.replacement {
                            Replacement::SubDAG(child)
                                if !is_logical_rewrite(child) && operation.accepts_child(child) =>
                            {
                                Some(Rc::clone(child))
                            }
                            _ => None,
                        })
                        .collect(),
                    OperationPlacement::Maintenance => {
                        retain_exact(&operation.child_target).into_iter().collect()
                    }
                };
                for child in children {
                    let Ok(plan) = operation.compose_with_accuracy(Rc::clone(&child), accuracy)
                    else {
                        continue;
                    };
                    if let Some(requirements) = targets.get(&Rc::as_ptr(&group.target)) {
                        if operation.placement == OperationPlacement::Maintenance
                            || !requirements.iter().all(|target| {
                                plan.guarantee
                                    .as_ref()
                                    .is_some_and(|g| accuracy.satisfies(g, target))
                            })
                        {
                            continue;
                        }
                    }
                    self.composition_plans.push(PreparedComposition {
                        target: Rc::as_ptr(&group.target),
                        operation: operation.clone(),
                        child,
                        plan,
                    });
                }
            }
        }
    }
}

/// DAG candidates assembled from an unpriced search space.
/// This is an internal planning stage: callers must still validate materialization
/// requirements and compile supported physical operators before deployment.
/// The caller supplies a finite expansion budget; exceeding it is an error,
/// never a silently truncated inventory presented as exhaustive.
#[derive(Debug)]
pub struct CandidateDAGInventory<Id> {
    pub candidates: Vec<Vec<(Id, Rc<OperatorNode>)>>,
    pub rejected_assemblies: Vec<String>,
}

type CandidateDAGChoice<'a> = (Option<&'a ReplacementSubDAG>, Option<Rc<OperatorNode>>);

impl<Id: Clone + PartialEq> CandidateLogicalASAPDAGs<Id> {
    pub fn enumerate_candidate_dags(
        &self,
        expansion_limit: usize,
    ) -> Result<CandidateDAGInventory<Id>, RealizationError> {
        self.enumerate_candidate_roots(&self.roots, expansion_limit)
    }

    /// Enumerate one workload root without expanding independent roots' choices.
    /// Discovery and composition proofs still come from the shared workload
    /// space. Deployment may price combinations lazily; this API does not rank
    /// candidates or claim that independently cheapest roots minimize shared cost.
    pub fn enumerate_candidate_dags_for_root(
        &self,
        id: &Id,
        expansion_limit: usize,
    ) -> Result<CandidateDAGInventory<Id>, RealizationError> {
        let roots = self
            .roots
            .iter()
            .filter(|(candidate, _)| candidate == id)
            .cloned()
            .collect::<Vec<_>>();
        if roots.len() != 1 {
            return Err(RealizationError::PhysicalRealization(
                "candidate enumeration requires one uniquely identified workload root",
            ));
        }
        self.enumerate_candidate_roots(&roots, expansion_limit)
    }

    fn enumerate_candidate_roots(
        &self,
        roots: &[(Id, Rc<OperatorNode>)],
        expansion_limit: usize,
    ) -> Result<CandidateDAGInventory<Id>, RealizationError> {
        let mut reachable = Vec::new();
        let mut nodes = HashMap::new();
        let mut counts = HashMap::new();
        for (_, root) in roots {
            walk(root, &mut reachable, &mut nodes, &mut counts);
        }
        // Rewrites may introduce descendants absent from the original root.
        let mut cursor = 0;
        while cursor < reachable.len() {
            let ptr = reachable[cursor];
            cursor += 1;
            if let Some(group) = self.groups.get(&ptr) {
                for candidate in &group.candidates {
                    if let Replacement::SubDAG(rewritten) = &candidate.replacement {
                        if is_logical_rewrite(rewritten) {
                            walk(rewritten, &mut reachable, &mut nodes, &mut counts);
                        }
                    }
                }
            }
        }
        let order = self
            .order
            .iter()
            .copied()
            .filter(|ptr| counts.contains_key(ptr))
            .collect::<Vec<_>>();
        // Composition plans carry the proofs established during discovery.
        // No cost ranking is consulted while expanding these choices.
        let options: Vec<Vec<CandidateDAGChoice<'_>>> = order
            .iter()
            .map(|ptr| {
                let group = &self.groups[ptr];
                let mut choices = vec![(None, None)];
                for candidate in &group.candidates {
                    match &candidate.replacement {
                        Replacement::ExactComposition(operation) => {
                            for prepared in &self.composition_plans {
                                if prepared.target == *ptr
                                    && prepared.operation.placement == operation.placement
                                    && prepared.operation.op == operation.op
                                    && Rc::ptr_eq(
                                        &prepared.operation.child_target,
                                        &operation.child_target,
                                    )
                                {
                                    choices
                                        .push((Some(candidate), Some(Rc::clone(&prepared.plan))));
                                }
                            }
                        }
                        _ => choices.push((Some(candidate), None)),
                    }
                }
                choices
            })
            .collect();
        let combinations = options
            .iter()
            .try_fold(1usize, |n, choices| n.checked_mul(choices.len()))
            .filter(|n| *n <= expansion_limit)
            .ok_or(RealizationError::PhysicalRealization(
                "candidate expansion budget exceeded; no partial inventory returned",
            ))?;
        let mut inventory = CandidateDAGInventory {
            candidates: Vec::new(),
            rejected_assemblies: Vec::new(),
        };
        // Hash buckets avoid quadratic comparisons across a large workload
        // inventory. Equality still decides deduplication, including collisions.
        let mut seen = HashMap::<u64, Vec<usize>>::new();
        for mut ordinal in 0..combinations {
            let mut groups = HashMap::new();
            let mut assembled_nodes = HashMap::new();
            for (ptr, choices) in order.iter().zip(&options) {
                let (chosen, prepared) = &choices[ordinal % choices.len()];
                ordinal /= choices.len();
                let group = &self.groups[ptr];
                if let Some(node) = prepared {
                    assembled_nodes.insert(*ptr, Rc::clone(node));
                }
                groups.insert(
                    *ptr,
                    TargetSubDAGSelection {
                        target: &group.target,
                        consumer_count: group.consumer_count,
                        effective_consumer_count: group.consumer_count,
                        chosen: *chosen,
                        composition: None,
                    },
                );
            }
            let assembly = GlobalSelection {
                order: order.clone(),
                groups,
                assembled_nodes: RefCell::new(assembled_nodes),
            };
            let roots = roots
                .iter()
                .map(|(id, root)| {
                    assembly
                        .assemble_target(root)
                        // Exposed query candidates return values. Internal assembly
                        // still retains accumulator states for sharing and storage.
                        .and_then(|node| finalize_query_candidate(node, root))
                        .map(|node| (id.clone(), node))
                })
                .collect::<Result<Vec<_>, _>>();
            match roots {
                Ok(roots) => {
                    let roots = share_common_sub_dags(roots);
                    use std::hash::{Hash, Hasher};
                    let mut hash = std::collections::hash_map::DefaultHasher::new();
                    let mut cache = HashCache::new();
                    for (_, node) in &roots {
                        structural_hash(node, &mut cache).hash(&mut hash);
                    }
                    let bucket = seen.entry(hash.finish()).or_default();
                    if !bucket
                        .iter()
                        .any(|&index| inventory.candidates[index] == roots)
                    {
                        bucket.push(inventory.candidates.len());
                        inventory.candidates.push(roots);
                    }
                }
                Err(error) => {
                    let reason = error.to_string();
                    if !inventory.rejected_assemblies.contains(&reason) {
                        inventory.rejected_assemblies.push(reason);
                    }
                }
            }
        }
        Ok(inventory)
    }
}

impl<Id> CandidateLogicalASAPDAGs<Id> {
    /// One candidate set per discovered target sub-DAG, in discovery order.
    pub fn target_subdag_candidates(&self) -> impl Iterator<Item = &TargetSubDAGCandidates> {
        self.order.iter().map(move |ptr| &self.groups[ptr])
    }

    /// How many distinct targets were discovered.
    pub fn len(&self) -> usize {
        self.groups.len()
    }

    /// Whether no targets were discovered at all (an empty workload, or one
    /// with no `OperatorNode`s reachable from any root — never true for a
    /// non-empty `roots`, since every root is itself a target).
    pub fn is_empty(&self) -> bool {
        self.groups.is_empty()
    }

    /// The candidate set for `target`, if `target`'s own `Rc` is a discovered
    /// `TargetSubDAG` (i.e. `Rc::ptr_eq` to some node reachable from
    /// `roots`).
    pub fn candidates_for_target(
        &self,
        target: &Rc<OperatorNode>,
    ) -> Option<&TargetSubDAGCandidates> {
        self.groups.get(&Rc::as_ptr(target))
    }
}

/// Find the explicitly-tagged CSE share/recompute pair inside `group`, even
/// when other strategies contributed additional alternatives to the same
/// memo group. Provenance makes these two orthogonal choices identifiable
/// without inferring semantics from pointer or expression shape.
pub(crate) fn cse_candidate_pair(
    group: &TargetSubDAGCandidates,
) -> Option<(&ReplacementSubDAG, &ReplacementSubDAG)> {
    let mut share = None;
    let mut recompute = None;
    for candidate in &group.candidates {
        match candidate.provenance {
            ReplacementProvenance::CseShare => {
                let Replacement::SubDAG(rc) = &candidate.replacement else {
                    return None;
                };
                if !Rc::ptr_eq(rc, &group.target) || share.replace(candidate).is_some() {
                    return None;
                }
            }
            ReplacementProvenance::CseRecompute => {
                let Replacement::SubDAG(rc) = &candidate.replacement else {
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
/// Direct relational-skeleton children and their edge multiplicities.
/// `Concat` is transparent, matching [`walk_children`]'s site scope.
pub(crate) fn direct_child_counts(node: &OperatorNode) -> Vec<(*const OperatorNode, usize)> {
    fn push(children: &mut Vec<(*const OperatorNode, usize)>, child: &Rc<OperatorNode>) {
        let ptr = Rc::as_ptr(child);
        match children.iter_mut().find(|(existing, _)| *existing == ptr) {
            Some((_, count)) => *count += 1,
            None => children.push((ptr, 1)),
        }
    }

    fn collect(node: &OperatorNode, children: &mut Vec<(*const OperatorNode, usize)>) {
        if let Some(NonASAPOp::Concat {
            children: concat_children,
            ..
        }) = node.non_asap()
        {
            for c in concat_children {
                collect(c, children);
            }
            return;
        }
        for child in node.children() {
            push(children, child);
        }
    }

    let mut children = Vec::new();
    collect(node, &mut children);
    children
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
/// the target itself) exactly like [`SharedSubDAGStrategy`], so it belongs
/// in this list rather than being derived per-workload the way
/// [`RollupStrategy`] is. Rewriting `avg` into `sum`/`count` upfront is what
/// lets [`ASAPStrategies`] and [`SharedSubDAGStrategy`] see a
/// mergeable accumulator to sketch or share at all — see that module's own
/// doc comment for why a bare `avg` node otherwise never becomes a
/// [`ReplacementStrategy`] target for anything.
pub fn default_strategies() -> Vec<Box<dyn ReplacementStrategy>> {
    vec![
        Box::new(ASAPStrategies::default_cost_model()),
        Box::new(HydraGroupingStrategy::default_cost_model()),
        Box::new(SharedSubDAGStrategy),
        Box::new(crate::rewrite::AvgToSumOverCountStrategy),
        Box::new(ExactCompositionStrategy::default_cost_model()),
    ]
}

/// Like [`default_strategies`], but [`ASAPStrategies`] ranks/binds via
/// `cost_model` instead of the built-in [`DefaultCostModel`] — the same
/// customization point [`ASAPStrategies::new`] itself offers.
pub fn default_strategies_with<'a>(
    cost_model: &'a dyn CostModel,
) -> Vec<Box<dyn ReplacementStrategy + 'a>> {
    vec![
        Box::new(ASAPStrategies::new(cost_model)),
        Box::new(HydraGroupingStrategy::new(cost_model)),
        Box::new(SharedSubDAGStrategy),
        Box::new(crate::rewrite::SemanticEquivalentRewriteStrategy),
        Box::new(ExactCompositionStrategy::new(cost_model)),
    ]
}

/// Default context-free strategies with both deployment costing and typed
/// planning-time accuracy evidence. This is the production counterpart of
/// constructing [`ASAPStrategies::new_with_planning_inputs_and_evidence`] and
/// [`HydraGroupingStrategy::new_with_planning_inputs_and_evidence`] separately.
pub fn default_strategies_with_evidence<'a>(
    cost_model: &'a dyn CostModel,
    evidence: &'a dyn AccuracyEvidenceProvider,
) -> Vec<Box<dyn ReplacementStrategy + 'a>> {
    vec![
        Box::new(ASAPStrategies::new_with_planning_inputs_and_evidence(
            cost_model,
            &DEFAULT_ACCURACY_MODEL,
            &DEFAULT_ALLOCATOR,
            evidence,
        )),
        Box::new(
            HydraGroupingStrategy::new_with_planning_inputs_and_evidence(
                cost_model,
                &DEFAULT_ACCURACY_MODEL,
                &DEFAULT_ALLOCATOR,
                evidence,
            ),
        ),
        Box::new(SharedSubDAGStrategy),
        Box::new(crate::rewrite::AvgToSumOverCountStrategy),
        Box::new(ExactCompositionStrategy::new(cost_model)),
    ]
}

// ── search_workload ──────────────────────────────────────────────────────

/// Search a whole workload's pre-ASAP roots for every candidate replacement
/// [`default_strategies`] can find, deduped into a [`CandidateLogicalASAPDAGs`]. Candidate
/// *generation* uses the built-in [`DefaultCostModel`] (via
/// [`default_strategies`], the same way [`ASAPStrategies::default_cost_model`]
/// does); call [`CandidateLogicalASAPDAGs::cost_sorted`] on the result for the final
/// `sorted_by(cost_model)` step. Use [`search_workload_with`] to plug in a
/// custom strategy set (e.g. built via [`default_strategies_with`] for a
/// deployment-specific [`CostModel`]).
pub fn search_workload<Id>(roots: Vec<(Id, Rc<OperatorNode>)>) -> CandidateLogicalASAPDAGs<Id> {
    search_workload_with(roots, &default_strategies())
}

/// Like [`search_workload`], but with an explicit set of context-free
/// `strategies` (see [`default_strategies_with`] to plug in a
/// deployment-specific [`CostModel`]). The workload-dependent
/// [`RollupStrategy`] is derived and added automatically after CSE for both
/// entry points, because only this function owns the post-CSE sibling set.
///
/// Runs [`share_common_sub_dags`] once over `roots` first — so every
/// strategy (and, transitively, every
/// [`crate::explanation::ReplacementExplanation`] a caller reads off the
/// result) sees the same already-deduplicated DAG — then discovers every
/// `TargetSubDAG` (see [`discover_targets`]) and runs the
/// fixpoint loop the module docs describe, capped at
/// [`MAX_SEARCH_ITERATIONS`] passes (see the module docs' "Termination"
/// section). Deduping candidate plans this way needs no
/// [`CostModel`] at all — that only enters at two well-defined points: each
/// [`ReplacementStrategy`] in `strategies` may already carry its own (e.g.
/// [`ASAPStrategies::new`]'s), and [`CandidateLogicalASAPDAGs::cost_sorted`]'s final
/// ranking step takes one explicitly.
pub fn search_workload_with<'s, Id>(
    roots: Vec<(Id, Rc<OperatorNode>)>,
    strategies: &[Box<dyn ReplacementStrategy + 's>],
) -> CandidateLogicalASAPDAGs<Id> {
    let mut space = search_cse_workload_with(cse_workload(roots), strategies);
    space.prepare_compositions(&DefaultAccuracyModel, &HashMap::new());
    space
}

/// [`search_workload_with`] plus a per-root end-to-end `AccuracyTarget`
/// (issue #172) — the workload's `QueryRequirements.accuracy`, threaded
/// alongside each root. After the search, every root that carries a target
/// has its group's bound-summary [`Replacement::SubDAG`] candidates checked with
/// `accuracy_model`'s [`AccuracyModel::satisfies`]: a candidate whose
/// guarantee is fully known and misses the target is moved from
/// [`TargetSubDAGCandidates::candidates`] to [`TargetSubDAGCandidates::rejected`] *before*
/// [`CandidateLogicalASAPDAGs::cost_sorted`]/[`CandidateLogicalASAPDAGs::global_selection`] ever rank the
/// group. A constructible candidate with unknown accuracy remains visible for
/// downstream review under an approximate target, but default whole-plan
/// selection does not commit it. An exact target cannot accept an unknown
/// approximate summary. A kept pre-ASAP candidate is
/// exact and always survives — the raw/pre-ASAP alternative is what an
/// unsatisfiable root keeps. Logical-rewrite [`Replacement::SubDAG`]
/// candidates are not bound values and are left alone; the targets *inside*
/// a rewrite are their own groups.
///
/// Precedence against per-node `AggIntent.accuracy` is documented in
/// [`crate::accuracy`]'s module docs.
pub fn search_workload_with_targets<'s, Id>(
    roots: Vec<(Id, Rc<OperatorNode>, Option<AccuracyTarget>)>,
    strategies: &[Box<dyn ReplacementStrategy + 's>],
    accuracy_model: &dyn AccuracyModel,
) -> CandidateLogicalASAPDAGs<Id> {
    let mut targets = Vec::with_capacity(roots.len());
    let roots = roots
        .into_iter()
        .map(|(id, root, target)| {
            targets.push(target);
            (id, root)
        })
        .collect();
    let mut space = search_cse_workload_with(cse_workload(roots), strategies);
    // `cse_workload` preserves root order, so targets zip by position.
    let root_ptrs: Vec<(*const OperatorNode, AccuracyTarget)> = space
        .roots
        .iter()
        .zip(targets)
        .filter_map(|((_, root), target)| target.map(|t| (Rc::as_ptr(root), t)))
        .collect();
    // Whole-root proposals join the root group before its target check.
    for (index, (ptr, target)) in root_ptrs.iter().enumerate() {
        if root_ptrs[..index].contains(&(*ptr, target.clone())) {
            continue;
        }
        let group = space.groups.get_mut(ptr).expect("every root has a group");
        let root = Rc::clone(&group.target);
        for strategy in strategies {
            let name = strategy.name();
            let proposals = strategy.propose_for_root(&root, target);
            for mut candidate in proposals.candidates {
                candidate.strategy = name;
                group.add_candidate(candidate);
            }
            group
                .rejected
                .extend(proposals.rejected.into_iter().map(|mut rejection| {
                    rejection.strategy = name;
                    rejection
                }));
        }
    }
    let mut composition_targets: HashMap<_, Vec<_>> = HashMap::new();
    for (ptr, target) in root_ptrs {
        composition_targets
            .entry(ptr)
            .or_default()
            .push(target.clone());
        let Some(group) = space.groups.get_mut(&ptr) else {
            continue;
        };
        let (legal, illegal): (Vec<_>, Vec<_>) =
            group
                .candidates
                .drain(..)
                .partition(|candidate| match &candidate.replacement {
                    Replacement::SubDAG(node) if is_logical_rewrite(node) => true,
                    Replacement::SubDAG(node) => node.guarantee.as_ref().map_or_else(
                        || !matches!(target, AccuracyTarget::Exact),
                        |g| accuracy_model.satisfies(&g.optimistic_floor(), &target),
                    ),
                    // A composition's guarantee depends on the concrete child;
                    // prepare_compositions checks those pairs after all roots.
                    Replacement::ExactComposition(_) => true,
                });
        group.candidates = legal;
        group.rejected.extend(illegal.into_iter().map(|candidate| {
            let (metric, bound, failure_probability) = match &candidate.replacement {
                Replacement::SubDAG(node) => node
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
                        asap_types::ir::properties::ErrorMetric::AbsoluteValue,
                        None,
                        None,
                    )),
                Replacement::ExactComposition(_) => (
                    asap_types::ir::properties::ErrorMetric::AbsoluteValue,
                    None,
                    None,
                ),
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
    space.prepare_compositions(accuracy_model, &composition_targets);
    space
}

/// The strictest accuracy among `siblings` that read the same summary input
/// as `root` — same child, grouping and filters, and the same intent apart
/// from its accuracy (and a quantile's rank, a evaluation parameter) — when
/// stricter than `root`'s own. One summary sized for the strictest consumer
/// serves every sibling: #509's summary-capability rule.
fn strictest_sibling_accuracy(
    root: &OperatorNode,
    siblings: &[Rc<OperatorNode>],
) -> Option<AccuracyTarget> {
    fn approximate(intent: &AggIntent) -> Option<&AccuracyTarget> {
        accuracy_target(intent).filter(|accuracy| !matches!(accuracy, AccuracyTarget::Exact))
    }
    let Some(NonASAPOp::Aggregate {
        reduction,
        filters,
        child,
        ..
    }) = root.non_asap()
    else {
        return None;
    };
    let intent = bindable_intent(root)?;
    let own = accuracy_budget(approximate(intent)?);
    let (mut eps, mut delta) = own;
    for sibling in siblings {
        let Some(NonASAPOp::Aggregate {
            reduction: sibling_reduction,
            filters: sibling_filters,
            child: sibling_child,
            ..
        }) = sibling.non_asap()
        else {
            continue;
        };
        let Some(other) = bindable_intent(sibling) else {
            continue;
        };
        let Some(accuracy) = approximate(other) else {
            continue;
        };
        let same_intent = match (intent, other) {
            (AggIntent::Quantile { col, .. }, AggIntent::Quantile { col: other_col, .. }) => {
                col == other_col
            }
            _ => override_accuracy(intent, accuracy) == *other,
        };
        if same_intent
            && sibling_reduction == reduction
            && sibling_filters == filters
            && (Rc::ptr_eq(sibling_child, child) || sibling_child == child)
        {
            let (sibling_eps, sibling_delta) = accuracy_budget(accuracy);
            eps = eps.min(sibling_eps);
            delta = delta.min(sibling_delta);
        }
    }
    if (eps, delta) == own {
        None
    } else if delta == DEFAULT_DELTA {
        Some(AccuracyTarget::Epsilon(eps))
    } else {
        Some(AccuracyTarget::EpsilonDelta {
            epsilon: eps,
            delta,
        })
    }
}

fn cse_workload<Id>(roots: Vec<(Id, Rc<OperatorNode>)>) -> Vec<(Id, Rc<OperatorNode>)> {
    share_common_sub_dags(roots)
}

fn search_cse_workload_with<'s, Id>(
    cse_roots: Vec<(Id, Rc<OperatorNode>)>,
    strategies: &[Box<dyn ReplacementStrategy + 's>],
) -> CandidateLogicalASAPDAGs<Id> {
    for (_, root) in &cse_roots {
        assert!(
            !root.contains_asap(),
            "search_workload: a workload root already contains an ASAP operator \
             ({}); replacement search takes the front end's pre-ASAP DAG only",
            root.operator.kind_name()
        );
    }
    let mut order = Vec::new();
    let mut nodes = HashMap::new();
    let mut counts: HashMap<*const OperatorNode, usize> = HashMap::new();
    discover_targets(&cse_roots, &mut order, &mut nodes, &mut counts);
    let siblings: Vec<Rc<OperatorNode>> = order
        .iter()
        .filter_map(|ptr| {
            let node = &nodes[ptr];
            matches!(node.non_asap(), Some(NonASAPOp::Aggregate { .. })).then(|| Rc::clone(node))
        })
        .collect();
    let rollup_strategy = RollupStrategy::new(&siblings);
    let accuracy_reconciliation_strategy = AccuracyReconciliationStrategy::new(&siblings);
    let limits: Vec<Rc<OperatorNode>> = order
        .iter()
        .filter_map(|ptr| {
            let node = &nodes[ptr];
            matches!(node.non_asap(), Some(NonASAPOp::Limit { .. })).then(|| Rc::clone(node))
        })
        .collect();
    let topk_reuse_strategy = TopKLimitReuseStrategy::new(&limits);

    let mut groups: HashMap<*const OperatorNode, TargetSubDAGCandidates> = HashMap::new();
    for ptr in &order {
        groups.insert(
            *ptr,
            TargetSubDAGCandidates::new(Rc::clone(&nodes[ptr]), counts[ptr]),
        );
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
             ASAPStrategies/SharedSubDAGStrategy never do this (see replacement.rs's \
             module docs' \"Termination\" section); check any custom strategies passed to \
             search_workload_with.",
        );

        let targets_before = order.len();
        for ptr in &frontier {
            let (root, consumer_count) = {
                let group = &groups[ptr];
                (Rc::clone(&group.target), group.consumer_count)
            };
            let strictest = strictest_sibling_accuracy(&root, &siblings);
            let mut target = TargetSubDAG::with_consumer_count(&root, consumer_count);
            target.strictest_sibling_accuracy = strictest.as_ref();

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
                if let Replacement::SubDAG(rc) = &candidate.replacement {
                    if is_logical_rewrite(rc) {
                        discover_new_descendant_targets(rc, &mut order, &mut nodes, &mut counts);
                    }
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
            groups.entry(*ptr).or_insert_with(|| {
                TargetSubDAGCandidates::new(Rc::clone(&nodes[ptr]), counts[ptr])
            });
        }
        frontier = new_targets.to_vec();
    }

    add_effective_count_cse_candidates(&order, &mut groups);

    CandidateLogicalASAPDAGs {
        roots: cse_roots,
        groups,
        order,
        composition_plans: Vec::new(),
    }
}

/// Materialize share/recompute alternatives for descendants whose raw edge
/// count is one but whose effective count can exceed one when a repeated
/// ancestor is recomputed. We only do this when an ordinary repeated group
/// proves that `SharedSubDAGStrategy` is part of this search's strategy set.
fn add_effective_count_cse_candidates(
    order: &[*const OperatorNode],
    groups: &mut HashMap<*const OperatorNode, TargetSubDAGCandidates>,
) {
    let mut possible_children: HashMap<*const OperatorNode, Vec<*const OperatorNode>> =
        HashMap::new();
    for ptr in order {
        let group = &groups[ptr];
        let children = possible_children.entry(*ptr).or_default();
        for (child, _) in direct_child_counts(&group.target) {
            if !children.contains(&child) {
                children.push(child);
            }
        }
        for candidate in &group.candidates {
            if let Replacement::SubDAG(rewrite) = &candidate.replacement {
                if !is_logical_rewrite(rewrite) {
                    continue;
                }
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
            for mut candidate in SharedSubDAGStrategy.replacements(&site) {
                candidate.rationale = format!(
                    "{}: this sub-DAG can become repeated when a repeated ancestor is recomputed; \
                     global_selection decides using its effective consumer count",
                    match candidate.provenance {
                        ReplacementProvenance::CseShare => "build once and share",
                        ReplacementProvenance::CseRecompute => "recompute independently",
                        _ => unreachable!("SharedSubDAGStrategy only emits CSE candidates"),
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
pub(crate) fn discover_targets<Id>(
    roots: &[(Id, Rc<OperatorNode>)],
    order: &mut Vec<*const OperatorNode>,
    nodes: &mut HashMap<*const OperatorNode, Rc<OperatorNode>>,
    counts: &mut HashMap<*const OperatorNode, usize>,
) {
    for (_, root) in roots {
        walk(root, order, nodes, counts);
    }
}

/// Scan `candidate`'s **children** (deliberately never `candidate`'s own
/// top-level pointer — see the module docs' "Termination" section: a
/// logical rewrite's value is an alternative *for* the target that
/// proposed it, never a new target of its own) for any `Rc` not already
/// known, appending each to `order`/`nodes`/`counts` so
/// [`search_workload_with`]'s next round processes it. A no-op when every
/// child is already known — the case both shipped strategies always produce
/// (see that section).
fn discover_new_descendant_targets(
    candidate: &Rc<OperatorNode>,
    order: &mut Vec<*const OperatorNode>,
    nodes: &mut HashMap<*const OperatorNode, Rc<OperatorNode>>,
    counts: &mut HashMap<*const OperatorNode, usize>,
) {
    walk_children(candidate, order, nodes, counts);
}

/// Visit `node`: count this occurrence, and — the first time this exact
/// `Rc` is seen — record it as a target and recurse into its children.
fn walk(
    node: &Rc<OperatorNode>,
    order: &mut Vec<*const OperatorNode>,
    nodes: &mut HashMap<*const OperatorNode, Rc<OperatorNode>>,
    counts: &mut HashMap<*const OperatorNode, usize>,
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

/// `node`'s own operator children ([`OperatorNode::children`]: operator
/// inputs plus the operator nodes its scalar expressions read), the same
/// scope `asap_types::ir::cse::share_common_sub_dags` itself uses and
/// `tests::count_consumers` mirrors for its own fixtures. `Concat` is
/// transparent: its branches are walked in place of it.
fn walk_children(
    node: &OperatorNode,
    order: &mut Vec<*const OperatorNode>,
    nodes: &mut HashMap<*const OperatorNode, Rc<OperatorNode>>,
    counts: &mut HashMap<*const OperatorNode, usize>,
) {
    if let Some(NonASAPOp::Concat { children, .. }) = node.non_asap() {
        for c in children {
            walk_children(c, order, nodes, counts);
        }
        return;
    }
    for child in node.children() {
        walk(child, order, nodes, counts);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::accuracy::PropagationStats;
    use crate::plan_selection::candidate_selection::sketch_kind_of;
    use crate::test_support::{agg, agg_per_entity, lower_promql, maintained, metric_scan, timed};
    use asap_types::ir::operator::{
        agg_is_exact, default_cardinality, default_quantile, MathFunc, TimeFunc,
    };
    use asap_types::ir::operator::operator_properties::{Reduction as ReductionTy, Source};
    use asap_types::ir::schema::ColumnId;
    use asap_types::ir::schema::{DataType, Field, Schema as SchemaTy};
    use asap_types::ir::Predicate;
    use asap_types::ir::TimeRangeKind;

    use asap_types::types::AccuracyTarget;
    use std::collections::HashMap;

    // Candidate shape without execution timing: what is computed, not where.
    fn timing_free_shape(node: &Rc<OperatorNode>) -> serde_json::Value {
        fn strip(value: &mut serde_json::Value) {
            match value {
                serde_json::Value::Object(fields) => {
                    fields.remove("timing");
                    fields.values_mut().for_each(strip);
                }
                serde_json::Value::Array(values) => values.iter_mut().for_each(strip),
                _ => {}
            }
        }
        let mut shape = serde_json::to_value(
            asap_types::ir::physical_export::compile_physical_asap_dag(&timed(node)).unwrap(),
        )
        .unwrap();
        strip(&mut shape);
        shape
    }

    // Rate inventories never offer two candidates that differ only in timing.
    #[test]
    fn rate_candidate_inventories_have_no_timing_only_duplicates() {
        for (query, accuracy) in [
            ("sum by(job)(rate(m[1m]))", AccuracyTarget::Exact),
            ("topk by(job)(2, rate(m[1m]))", AccuracyTarget::Epsilon(0.1)),
        ] {
            let root = lower_promql(query, accuracy);
            let inventory = search_workload(vec![(0usize, root)])
                .enumerate_candidate_dags(4096)
                .unwrap();
            let shapes = inventory
                .candidates
                .iter()
                .map(|forest| timing_free_shape(&forest[0].1))
                .collect::<Vec<_>>();
            for (i, shape) in shapes.iter().enumerate() {
                assert!(!shapes[..i].contains(shape), "{query}: duplicate {i}");
            }
        }
    }

    // Grouped Sum over Rate evaluations stays a summary state in the inventory,
    // so materialization assignment can place it in precompute or at query time.
    #[test]
    fn grouped_rate_sum_inventory_keeps_sum_state_for_materialization_placement() {
        let root = lower_promql("sum by(job)(rate(m[1m]))", AccuracyTarget::Exact);
        let inventory = search_workload(vec![(0usize, root)])
            .enumerate_candidate_dags(4096)
            .unwrap();
        let is_exact = |node: &OperatorNode, kind: ExactKind| {
            matches!(&node.operator, Operator::ASAP(ASAPOp::SummaryAgg {
                family: FieldDataType::ExactAggregate(k, _), ..
            }) if *k == kind)
        };
        assert!(inventory.candidates.iter().any(|forest| {
            let Operator::ASAP(ASAPOp::FinalizeExactAccumulator { child: sum }) = &forest[0].1.operator else {
                return false;
            };
            let Operator::ASAP(ASAPOp::SummaryAgg { child: rate, .. }) = &sum.operator else {
                return false;
            };
            is_exact(sum, ExactKind::Sum)
                && matches!(&rate.operator, Operator::ASAP(ASAPOp::FinalizeExactAccumulator { child }) if is_exact(child, ExactKind::Rate))
        }));
    }

    // Every exposed query result has a evaluation; internal accumulator frontiers stay states.
    #[test]
    fn query_candidate_roots_do_not_leak_exact_accumulator_state() {
        for query in [
            "sum by(job)(rate(m[1m]))",
            "sum by(job)(m)",
            "sum_over_time(m[1m])",
        ] {
            let root = lower_promql(query, AccuracyTarget::Exact);
            let space = search_workload(vec![(0usize, root.clone())]);
            let inventory = space.enumerate_candidate_dags(4096).unwrap();
            assert!(!inventory.candidates.is_empty());
            let strategy = ASAPStrategies::new(&DefaultCostModel);
            for candidate in strategy.propose(&TargetSubDAG::new(&root)).candidates {
                if let Replacement::SubDAG(node) = candidate.replacement {
                    let output = finalize_query_candidate(node, &root).unwrap();
                    assert!(
                        output
                            .schema
                            .fields
                            .iter()
                            .all(|field| matches!(field.dtype, FieldDataType::Plain(_))),
                        "direct candidate {query} leaks state"
                    );
                }
            }
            let selected = space
                .global_selection(&DefaultCostModel)
                .assemble_selected_query(&space.roots[0].1)
                .unwrap()
                .unwrap();
            for node in inventory
                .candidates
                .iter()
                .map(|forest| &forest[0].1)
                .chain(std::iter::once(&selected))
            {
                assert!(
                    node.schema
                        .fields
                        .iter()
                        .all(|field| matches!(field.dtype, FieldDataType::Plain(_))),
                    "{query}: query root leaks state: {:?}",
                    node.schema
                );
            }
        }
    }

    #[test]
    fn unpriced_inventory_retains_quantile_families_and_raw_execution() {
        let query = agg(vec![2], default_quantile(0.9), metric_scan(&["job"]));
        let space = search_workload(vec![(0usize, query)]);
        let inventory = space.enumerate_candidate_dags(4096).unwrap();
        let roots = inventory
            .candidates
            .iter()
            .map(|forest| format!("{:?}", forest[0].1))
            .collect::<Vec<_>>();
        assert!(roots.iter().any(|root| root.contains("Kll")));
        assert!(roots.iter().any(|root| root.contains("DDSketch")));
        assert!(inventory
            .candidates
            .iter()
            .any(|forest| !forest[0].1.contains_asap()));
    }

    // Independent roots must not require materializing their Cartesian product.
    #[test]
    fn root_inventory_preserves_choices_without_workload_cartesian_expansion() {
        let roots = (0..24usize)
            .map(|id| {
                (
                    id,
                    agg(
                        vec![2],
                        default_quantile((id + 1) as f64 / 25.0),
                        metric_scan(&["job"]),
                    ),
                )
            })
            .collect();
        let space = search_workload(roots);
        assert!(space.enumerate_candidate_dags(4096).is_err());
        for id in 0..24 {
            let inventory = space.enumerate_candidate_dags_for_root(&id, 4096).unwrap();
            assert!(inventory
                .candidates
                .iter()
                .all(|forest| forest.len() == 1 && forest[0].0 == id));
            let descriptions = inventory
                .candidates
                .iter()
                .map(|forest| format!("{:?}", forest[0].1))
                .collect::<Vec<_>>();
            assert!(descriptions.iter().any(|node| node.contains("Kll")));
            assert!(descriptions.iter().any(|node| node.contains("DDSketch")));
            assert!(inventory
                .candidates
                .iter()
                .any(|forest| !forest[0].1.contains_asap()));
        }
        assert!(space.enumerate_candidate_dags_for_root(&24, 4096).is_err());
        assert!(space.enumerate_candidate_dags_for_root(&0, 0).is_err());
    }

    // Factoring changes enumeration, not the set of root computations.
    #[test]
    fn root_inventory_matches_projection_of_exhaustive_workload_inventory() {
        let roots = (0..2usize)
            .map(|id| {
                (
                    id,
                    agg(
                        vec![2],
                        default_quantile(0.5 + id as f64 * 0.4),
                        metric_scan(&["job"]),
                    ),
                )
            })
            .collect();
        let space = search_workload(roots);
        let full = space.enumerate_candidate_dags(4096).unwrap();
        for id in 0..2 {
            let inventory = space.enumerate_candidate_dags_for_root(&id, 4096).unwrap();
            for forest in &full.candidates {
                let node = &forest.iter().find(|(root, _)| *root == id).unwrap().1;
                assert!(inventory.candidates.iter().any(|one| &one[0].1 == node));
            }
            for one in &inventory.candidates {
                assert!(full.candidates.iter().any(|forest| forest
                    .iter()
                    .any(|(root, node)| *root == id && node == &one[0].1)));
            }
        }
    }

    #[test]
    fn inventory_budget_never_returns_a_silent_partial_search() {
        let query = agg(vec![2], default_quantile(0.9), metric_scan(&["job"]));
        let space = search_workload(vec![(0usize, query)]);
        assert!(space.enumerate_candidate_dags(0).is_err());
    }

    fn equi_pred(left: ColumnId, right: ColumnId) -> Predicate {
        Predicate(ScalarExpr::Compare {
            left: Box::new(ScalarExpr::Column(left)),
            op: asap_types::ir::scalar::CompareOpKind::Eq,
            right: Box::new(ScalarExpr::Column(right)),
            semantics: asap_types::ir::ExprSemantics::Sql,
        })
    }

    // Finite samples can overflow a sum although their native average is finite.
    #[test]
    fn temporal_average_requires_finite_division_guard() {
        let root = lower_promql("avg_over_time(a[5m])", AccuracyTarget::Exact);
        let candidates =
            ASAPStrategies::default_cost_model().replacements(&TargetSubDAG::new(&root));
        let operator = candidates
            .iter()
            .find_map(|c| match &c.replacement {
                Replacement::SubDAG(node) => match &node.operator {
                    Operator::NonASAP(NonASAPOp::BinaryOp { operator, .. }) => Some(operator),
                    _ => None,
                },
                _ => None,
            })
            .expect("maintained average candidate");
        assert!(operator.checked_finite_division);
        assert!(
            crate::rewrite::SemanticEquivalentRewriteStrategy
                .replacements(&TargetSubDAG::new(&root))
                .is_empty(),
            "an unconditional pre-ASAP rewrite would bypass the runtime guard"
        );
    }

    // Approximate requests also admit exact temporal ranking candidates.
    #[test]
    fn approximate_temporal_topk_admits_exact_maintained_values() {
        let root = lower_promql(
            "topk by(job)(1,count_over_time(a[5m]))",
            AccuracyTarget::EpsilonDelta {
                epsilon: 0.01,
                delta: 0.01,
            },
        );
        let planning_inputs =
            CandidatePlanningInputs::with_default_accuracy(&crate::cost_model::DefaultCostModel);
        let node = exact_topk_over_temporal_values(&root, planning_inputs)
            .unwrap()
            .expect("exact ranking is legal for an approximate request");
        assert!(node.guarantee.as_ref().unwrap().is_exact());
        crate::test_support::time_and_export(&node).unwrap();
    }

    // Exact Top-K consumes the Planner's maintained temporal values.
    #[test]
    fn exact_temporal_topk_has_a_maintained_value_candidate() {
        for query in [
            "topk(5, sum_over_time(a[5m]))",
            "topk by(job)(5, count_over_time(a[5m]))",
        ] {
            let root = lower_promql(query, AccuracyTarget::Exact);
            let planning_inputs = CandidatePlanningInputs::with_default_accuracy(
                &crate::cost_model::DefaultCostModel,
            );
            let node = exact_topk_over_temporal_values(&root, planning_inputs)
                .unwrap()
                .expect("exact Top-K candidate");
            assert!(node.guarantee.as_ref().unwrap().is_exact());
            let Operator::NonASAP(NonASAPOp::Limit {
                child: sorted,
                n,
                offset,
                partition_by,
            }) = &node.operator
            else {
                panic!("temporal TopK must compose Sort and Limit");
            };
            assert_eq!((*n, *offset), (Some(5), 0));
            let Operator::NonASAP(NonASAPOp::Sort {
                keys,
                partition_by: sort_groups,
                child: values,
            }) = &sorted.operator
            else {
                panic!("Limit must consume sorted temporal values");
            };
            assert_eq!(sort_groups, partition_by);
            assert_eq!(
                partition_by.keys().len(),
                usize::from(query.contains("by(job)"))
            );
            assert_eq!(keys.len(), 1);
            assert!(!keys[0].ascending);
            assert_eq!(node.schema, values.schema);
            crate::test_support::time_and_export(&node).unwrap();
        }
    }

    // A bounded exact mean can share the relative division proof with a quantile.
    #[test]
    fn bounded_mean_quantile_ratio_is_certified() {
        struct Domain;
        impl AccuracyEvidenceProvider for Domain {
            fn quantile_input_domain(
                &self,
                _: &OperatorNode,
            ) -> Option<crate::accuracy::QuantileInputDomain> {
                Some(crate::accuracy::QuantileInputDomain {
                    lower: 1.0,
                    upper: 1000.0,
                    max_samples: 10000,
                    contract: "finite test population".into(),
                })
            }
        }
        let target = AccuracyTarget::EpsilonDelta {
            epsilon: 0.01,
            delta: 0.01,
        };
        let inputs = CandidatePlanningInputs {
            evidence: &Domain,
            ..CandidatePlanningInputs::with_default_accuracy(&DefaultCostModel)
        };
        for query in [
            "avg_over_time(a[5m]) / quantile_over_time(0.5,a[5m])",
            "quantile_over_time(0.5,a[5m]) / avg_over_time(a[5m])",
        ] {
            let root = lower_promql(query, target.clone());
            let node = realize_binary(&root, inputs, Some(&target))
                .unwrap()
                .expect("bounded ratio candidate");
            assert!(DefaultAccuracyModel.satisfies(node.guarantee.as_ref().unwrap(), &target));
        }
    }

    // Missing domain proof permits an uncertified direct quantile ratio only.
    #[test]
    fn quantile_ratio_without_input_proof_has_no_root_guarantee() {
        let target = AccuracyTarget::EpsilonDelta {
            epsilon: 0.01,
            delta: 0.01,
        };
        let root = lower_promql(
            "quantile_over_time(0.5,a[5m]) / quantile_over_time(0.9,a[5m])",
            target.clone(),
        );
        let planning_inputs =
            CandidatePlanningInputs::with_default_accuracy(&crate::cost_model::DefaultCostModel);
        let candidate = realize_binary(&root, planning_inputs, Some(&target))
            .unwrap()
            .expect("direct quantile ratio candidate");
        assert!(candidate.guarantee.is_none());

        let other = lower_promql(
            "avg_over_time(a[5m]) / quantile_over_time(0.5,a[5m])",
            target.clone(),
        );
        assert!(realize_binary(&other, planning_inputs, Some(&target))
            .unwrap()
            .is_none());
    }

    fn eps(e: f64) -> AccuracyTarget {
        AccuracyTarget::Epsilon(e)
    }

    // ── realizations_for_intent / sizing ───────────────────────────────

    /// The most-preferred `Realization` — `realizations_for_intent(intent,
    /// &DefaultCostModel)`'s head — for tests that only care about the
    /// default pick, not the full candidate list.
    fn preferred(intent: &AggIntent) -> Realization {
        realizations_for_intent(intent, &DefaultCostModel)
            .into_iter()
            .next()
            .expect("every intent has at least one Realization")
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
            Realization::ExactAggregate { kind, .. } => Cat::Acc(kind),
            Realization::Sketch(kind) => Cat::Sketch(kind.algorithm().clone()),
            Realization::PassThrough => Cat::Pass,
            other => {
                panic!("this coverage matrix expects only Exact/Sketch/PassThrough, got {other:?}")
            }
        }
    }

    /// The `AggIntent → SummaryKind` coverage matrix (issue #98): every intent
    /// variant maps to a sketch, an exact accumulator, or an explicit
    /// pass-through. `realizations_for_intent`'s match is exhaustive, so a
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
                A::Cardinality {
                    cols: vec![0, 1],
                    accuracy: eps(0.01),
                },
                Sketch(K::Hll),
            ),
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
                    cols: vec![],
                    accuracy: AccuracyTarget::Exact,
                },
                Pass,
            ),
            (
                A::Cardinality {
                    cols: vec![0, 1],
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
            (A::Min { col: None }, Acc(E::Min)),
            (A::Max { col: None }, Acc(E::Max)),
            (A::Rate, Acc(E::Rate)),
            (A::IRate, Acc(E::IRate)),
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
            (A::HistogramQuantile { q: 0.99, le: 0 }, Pass),
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

    // Correlation must never acquire a single-input sketch or scalar accumulator.
    #[test]
    fn pearson_corr_keeps_exact_paired_input() {
        let intent = AggIntent::PearsonCorr { left: 0, right: 1 };
        assert!(matches!(
            realizations_for_intent(&intent, &crate::cost_model::DefaultCostModel).as_slice(),
            [Realization::PassThrough]
        ));
        assert!(summary_candidates(&intent).is_empty());
    }

    #[test]
    fn accuracy_target_drives_the_boundary() {
        // Same intent, three targets → three different decisions.
        let exact = AggIntent::Quantile {
            col: None,
            q: 0.99,
            accuracy: AccuracyTarget::Exact,
        };
        assert_eq!(preferred(&exact), Realization::PassThrough);

        let approx = default_quantile(0.99); // ε = 0.01
        assert_eq!(
            preferred(&approx),
            Realization::Sketch(SketchKind::new(
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
            Realization::Sketch(SketchKind::new(
                SketchAlgorithm::Kll,
                SketchParams::Kll { k: 52 },
            ))
        );
    }

    #[test]
    fn default_cardinality_sizes_hll_to_its_rse_magnitude() {
        assert_eq!(
            preferred(&default_cardinality()),
            Realization::Sketch(SketchKind::new(
                SketchAlgorithm::Hll,
                SketchParams::Hll { precision: 14 },
            ))
        );
    }

    // Exact counting remains a legal candidate under an approximate target.
    #[test]
    fn approximate_count_includes_exact_accumulator_candidate() {
        let intent = AggIntent::Count {
            accuracy: eps(0.01),
        };
        assert!(realizations_for_intent(&intent, &DefaultCostModel)
            .iter()
            .any(|candidate| matches!(
                candidate,
                Realization::ExactAggregate {
                    kind: ExactKind::Count,
                    ..
                }
            )));
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
            Realization::Sketch(SketchKind::new(
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
            Realization::Sketch(SketchKind::new(
                SketchAlgorithm::Cms,
                SketchParams::Cms {
                    width: 2719,
                    depth: 5
                },
            ))
        );
    }

    #[test]
    fn topk_heap_capacity_respects_accuracy_and_output_count() {
        let intent = AggIntent::TopK {
            k: 25,
            accuracy: eps(0.01),
        };
        match preferred(&intent) {
            Realization::Sketch(kind) if kind.algorithm() == &SketchAlgorithm::CmsWithHeap => {
                let SketchParams::CmsWithHeap {
                    width,
                    depth,
                    heap_size,
                } = kind.params()
                else {
                    unreachable!("SketchKind validates CmsWithHeap params")
                };
                assert_eq!(*heap_size, 100);
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
                SketchAlgorithm::Kmv,
                SketchAlgorithm::UnivMon
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
            &[
                SketchAlgorithm::Cms,
                SketchAlgorithm::CountSketch,
                SketchAlgorithm::UnivMon
            ]
        );
        assert!(summary_candidates(&AggIntent::Rate).is_empty());
    }

    #[test]
    fn realizations_for_intent_enumerates_every_candidate_ranked() {
        // Quantile's candidate list is [Kll, DDSketch] — realizations_for_intent
        // must return both, ranked with the DefaultCostModel's preferred
        // (Kll) first.
        let kinds: Vec<SketchAlgorithm> =
            realizations_for_intent(&default_quantile(0.99), &DefaultCostModel)
                .into_iter()
                .map(|realization| match realization {
                    Realization::Sketch(kind) => kind.algorithm().clone(),
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
            Realization::Sketch(SketchKind::new(
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
                assert_eq!(heap_size, 100);
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

    // ── ASAPStrategies / SharedSubDAGStrategy fixtures ───────────

    // ── ASAPStrategies ─────────────────────────────────────────────

    #[test]
    fn matches_a_bindable_aggregate() {
        let q = agg(vec![2], default_quantile(0.99), metric_scan(&["job"]));
        let target = TargetSubDAG::new(&q);
        assert!(ASAPStrategies::default_cost_model().matches(&target));
    }

    #[test]
    fn does_not_match_a_multi_intent_or_having_aggregate() {
        let strategy = ASAPStrategies::default_cost_model();

        let multi =
            OperatorNode::new_shared(asap_types::ir::Operator::NonASAP(NonASAPOp::Aggregate {
                reduction: ReductionTy::by(vec![2]),
                measures: vec![AggIntent::Sum { col: None }, AggIntent::Avg { col: None }],
                output_names: vec![],
                filters: vec![],
                having: None,
                child: metric_scan(&["job"]),
            }))
            .unwrap();
        let target = TargetSubDAG::new(&multi);
        assert!(!strategy.matches(&target));
        assert!(strategy.replacements(&target).is_empty());

        let having_q = crate::test_support::aggregate(
            ReductionTy::by(vec![2]),
            vec![default_quantile(0.99)],
            vec![],
            Some(asap_types::ir::Predicate(ScalarExpr::Literal(
                asap_types::ir::scalar::ScalarValue::Boolean(true),
            ))),
            metric_scan(&["job"]),
        );
        let target = TargetSubDAG::new(&having_q);
        assert!(!strategy.matches(&target));
        assert!(strategy.replacements(&target).is_empty());
    }

    #[test]
    fn does_not_match_a_non_aggregate_node() {
        let scan = metric_scan(&["job"]);
        let target = TargetSubDAG::new(&scan);
        assert!(!ASAPStrategies::default_cost_model().matches(&target));
        assert!(ASAPStrategies::default_cost_model()
            .replacements(&target)
            .is_empty());
    }

    #[test]
    fn approximate_quantile_enumerates_every_summary_candidate() {
        // Quantile's candidate list is [Kll, DDSketch] (summary_candidates) —
        // every entry must come back as its own bound summary candidate,
        // not just Kll (the CostModel-ranked head realizations_for_intent commits to).
        let q = agg(vec![2], default_quantile(0.99), metric_scan(&["job"]));
        let target = TargetSubDAG::new(&q);
        let replacements = ASAPStrategies::default_cost_model().replacements(&target);
        assert_eq!(
            replacements.len(),
            2,
            "expected 2 candidates, got {replacements:?}"
        );

        let kinds: Vec<SketchAlgorithm> = replacements
            .iter()
            .map(|r| match &r.replacement {
                Replacement::SubDAG(node) => summary_family_algorithm(node),
                Replacement::ExactComposition(_) => {
                    panic!("expected a Summary replacement")
                }
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
    fn cardinality_epsilon_delta_keeps_unknown_accuracy_candidates() {
        let q = agg(vec![2], default_cardinality(), metric_scan(&["job"]));
        let target = TargetSubDAG::new(&q);
        let replacements = ASAPStrategies::default_cost_model().replacements(&target);
        let kinds: Vec<SketchAlgorithm> = replacements
            .iter()
            .map(|r| match &r.replacement {
                Replacement::SubDAG(node) => summary_family_algorithm(node),
                Replacement::ExactComposition(_) => {
                    panic!("expected a Summary replacement")
                }
            })
            .collect();
        assert_eq!(
            kinds,
            vec![
                SketchAlgorithm::Hll,
                SketchAlgorithm::Theta,
                SketchAlgorithm::Kmv,
                SketchAlgorithm::UnivMon,
            ]
        );

        let q = agg(
            vec![2],
            AggIntent::Cardinality {
                cols: vec![],
                accuracy: AccuracyTarget::EpsilonDelta {
                    epsilon: 0.01,
                    delta: 0.01,
                },
            },
            metric_scan(&["job"]),
        );
        let kinds: Vec<_> = ASAPStrategies::default_cost_model()
            .replacements(&TargetSubDAG::new(&q))
            .iter()
            .map(|r| match &r.replacement {
                Replacement::SubDAG(node) => summary_family_algorithm(node),
                Replacement::ExactComposition(_) => {
                    panic!("expected a Summary replacement")
                }
            })
            .collect();
        assert_eq!(
            kinds,
            vec![
                SketchAlgorithm::Hll,
                SketchAlgorithm::Theta,
                SketchAlgorithm::Kmv,
                SketchAlgorithm::UnivMon,
            ]
        );
    }

    #[test]
    fn exact_accuracy_target_yields_exactly_one_pass_through_candidate() {
        // Exact quantile has no sketch candidate at all — realizations_for_intent
        // produces PassThrough, the only option, so exactly one candidate.
        let intent = AggIntent::Quantile {
            col: None,
            q: 0.99,
            accuracy: AccuracyTarget::Exact,
        };
        let q = agg(vec![2], intent, metric_scan(&["job"]));
        let target = TargetSubDAG::new(&q);
        let replacements = ASAPStrategies::default_cost_model().replacements(&target);
        assert_eq!(replacements.len(), 1, "{replacements:?}");
        assert!(matches!(
            &replacements[0].replacement,
            Replacement::SubDAG(node) if !node.contains_asap()
        ));
        assert!(replacements[0].rationale.contains("only realization"));
    }

    #[test]
    fn exact_mergeable_intent_yields_exactly_one_accumulator_candidate() {
        let q = agg(vec![2], AggIntent::Sum { col: None }, metric_scan(&["job"]));
        let target = TargetSubDAG::new(&q);
        let replacements = ASAPStrategies::default_cost_model().replacements(&target);
        assert_eq!(replacements.len(), 1, "{replacements:?}");
        assert!(matches!(
            &replacements[0].replacement,
            Replacement::SubDAG(node) if matches!(
                node.operator,
                Operator::ASAP(ASAPOp::SummaryAgg { .. })
            )
        ));
    }

    /// A custom `CostModel` doesn't change *which* candidates are enumerated
    /// (still every `summary_candidates` entry) — only which one
    /// `realizations_for_intent` itself would prefer first, and how each
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
        let q = agg(vec![2], default_quantile(0.99), metric_scan(&["job"]));
        let target = TargetSubDAG::new(&q);
        let custom = PreferDDSketch;
        let replacements = ASAPStrategies::new(&custom).replacements(&target);
        let kinds: Vec<SketchAlgorithm> = replacements
            .iter()
            .map(|r| match &r.replacement {
                Replacement::SubDAG(node) => summary_family_algorithm(node),
                Replacement::ExactComposition(_) => {
                    panic!("expected a Summary replacement")
                }
            })
            .collect();
        assert!(kinds.contains(&SketchAlgorithm::Kll));
        assert!(kinds.contains(&SketchAlgorithm::DDSketch));
        assert_eq!(kinds.len(), 2);
    }

    /// Constructing the outer target's candidates never leaks its algorithm
    /// choice into the nested aggregate. Existing approximate composition
    /// remains governed by the accuracy model, independently of #171's exact
    /// value-operation candidates.
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
        let outer = agg(vec![], default_quantile(0.99), inner);
        let target = TargetSubDAG::new(&outer);
        let replacements = ASAPStrategies::new_with_planning_inputs(
            &DefaultCostModel,
            &RankAdditiveModel,
            &EqualSplitAllocator,
        )
        .replacements(&target);

        assert_eq!(replacements.len(), 2, "{replacements:?}");
        assert!(replacements.iter().all(|candidate| {
            matches!(&candidate.replacement, Replacement::SubDAG(n) if n.contains_asap())
        }));
        // The inner target is still independently enumerated and ranked —
        // a custom cost model that prefers DDSketch for it is honored, and
        // nothing about the outer target's choice reaches it.
        let space = search_workload_with(
            vec![("q", Rc::clone(&outer))],
            &default_strategies_with(&PreferDDSketchViaCostModel),
        );
        let Some(NonASAPOp::Aggregate { child, .. }) = space.roots[0].1.non_asap() else {
            unreachable!()
        };
        let inner_group = space
            .candidates_for_target(child)
            .expect("inner quantile is a target");
        let inner_kinds: Vec<SketchAlgorithm> = inner_group
            .candidates
            .iter()
            .filter_map(|c| match &c.replacement {
                Replacement::SubDAG(node) => sketch_kind_of(node),
                _ => None,
            })
            .collect();
        assert_eq!(
            inner_kinds,
            vec![SketchAlgorithm::DDSketch, SketchAlgorithm::Kll],
            "the nested inner aggregate keeps its own cost-model-ranked candidates"
        );
    }

    /// The `FieldDataType`'s committed `SketchAlgorithm`, from the top
    /// `SummaryAgg` reachable under a (possibly `SummaryEstimate`-wrapped)
    /// bound root.
    fn summary_family_algorithm(node: &OperatorNode) -> SketchAlgorithm {
        match &node.operator {
            Operator::ASAP(ASAPOp::SummaryEstimate { summary_input, .. }) => {
                summary_family_algorithm(summary_input)
            }
            Operator::ASAP(ASAPOp::SummaryAgg { family, .. }) => match family {
                asap_types::ir::schema::FieldDataType::Sketch(kind, _) => kind.algorithm().clone(),
                other => panic!("expected a Sketch family, got {other:?}"),
            },
            other => panic!("expected SummaryAgg/SummaryEstimate, got {other:?}"),
        }
    }

    // ── SharedSubDAGStrategy ────────────────────────────────────────────

    #[test]
    fn does_not_match_a_single_consumer_target() {
        let q = agg(vec![2], AggIntent::Sum { col: None }, metric_scan(&["job"]));
        let target = TargetSubDAG::new(&q);
        assert_eq!(target.consumer_count, 1);
        assert!(!SharedSubDAGStrategy.matches(&target));
        assert!(SharedSubDAGStrategy.replacements(&target).is_empty());
    }

    #[test]
    fn two_or_more_consumers_yields_the_share_vs_independent_pair() {
        let q = agg(vec![2], AggIntent::Sum { col: None }, metric_scan(&["job"]));
        let target = TargetSubDAG::with_consumer_count(&q, 2);
        assert!(SharedSubDAGStrategy.matches(&target));

        let replacements = SharedSubDAGStrategy.replacements(&target);
        assert_eq!(replacements.len(), 2, "{replacements:?}");

        let shared = match &replacements[0].replacement {
            Replacement::SubDAG(rc) => rc,
            other => panic!("expected a Rewrite replacement, got {other:?}"),
        };
        assert!(
            Rc::ptr_eq(shared, &q),
            "the 'build once and share' candidate must be the same Rc as the target"
        );
        assert!(replacements[0].rationale.contains("build once and share"));

        let independent = match &replacements[1].replacement {
            Replacement::SubDAG(rc) => rc,
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
        let q = agg(vec![2], AggIntent::Sum { col: None }, metric_scan(&["job"]));
        let target = TargetSubDAG::with_consumer_count(&q, 3);
        let replacements = SharedSubDAGStrategy.replacements(&target);
        assert!(replacements[0].rationale.contains('3'));
        assert!(replacements[1].rationale.contains('3'));
    }

    /// Builds realistic multi-consumer `TargetSubDAG`s the same way this
    /// module's own [`discover_targets`]/`walk` does: dedup by `Rc::as_ptr`,
    /// walking only the relational-skeleton operator children
    /// `asap_types::ir::cse::share_common_sub_dags` itself scopes to,
    /// so a shared node nested below another shared node is only ever
    /// counted at the highest (maximal) point sharing starts. Test-only:
    /// this module deliberately does not ship a workload-wide discovery
    /// pass of its own (see the module docs' "Non-goals").
    fn count_consumers(roots: &[Rc<OperatorNode>]) -> HashMap<*const OperatorNode, usize> {
        fn walk(node: &Rc<OperatorNode>, counts: &mut HashMap<*const OperatorNode, usize>) {
            let ptr = Rc::as_ptr(node);
            let already_visited = counts.contains_key(&ptr);
            *counts.entry(ptr).or_insert(0) += 1;
            if !already_visited {
                walk_children(node, counts);
            }
        }
        fn walk_children(node: &OperatorNode, counts: &mut HashMap<*const OperatorNode, usize>) {
            if let Some(NonASAPOp::Concat { children, .. }) = node.non_asap() {
                for c in children {
                    walk_children(c, counts);
                }
                return;
            }
            for child in node.children() {
                walk(child, counts);
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
        // Two workload roots that `share_common_sub_dags` collapses onto one
        // Rc (mirrors `explanation`'s and `cse`'s own fixtures): a grouped
        // Sum aggregate over the same scan, built independently at each root.
        let a = agg(vec![2], AggIntent::Sum { col: None }, metric_scan(&["job"]));
        let b = agg(vec![2], AggIntent::Sum { col: None }, metric_scan(&["job"]));
        let shared = asap_types::ir::cse::share_common_sub_dags(vec![("a", a), ("b", b)]);
        let [(_, ra), (_, rb)] = shared.as_slice() else {
            panic!("expected 2 roots");
        };
        assert!(Rc::ptr_eq(ra, rb), "fixture sanity: the two roots merged");

        let roots: Vec<Rc<OperatorNode>> = shared.into_iter().map(|(_, rc)| rc).collect();
        let counts = count_consumers(&roots);
        let count = counts[&Rc::as_ptr(&roots[0])];
        assert_eq!(count, 2);

        let target = TargetSubDAG::with_consumer_count(&roots[0], count);
        assert!(SharedSubDAGStrategy.matches(&target));
        assert_eq!(SharedSubDAGStrategy.replacements(&target).len(), 2);
    }

    // ── search_workload / CandidateLogicalASAPDAGs / TargetSubDAGCandidates (merged from search.rs) ──
    //
    // Reuses this test module's own `metric_scan`/`agg` fixture helpers
    // above (identical to `search.rs`'s own copies, which are dropped here
    // to avoid a duplicate-definition collision now that both test modules
    // share one file) and `count_consumers` above (which mirrors
    // `discover_targets`' own real, non-test traversal for these fixtures).

    // ── discovery + MEMO shape ───────────────────────────────────────────

    #[test]
    fn single_bindable_aggregate_keeps_unprovable_hydra_candidates() {
        let intent = AggIntent::Count {
            accuracy: AccuracyTarget::EpsilonDelta {
                epsilon: 0.01,
                delta: 0.01,
            },
        };
        let root = agg(vec![2], intent, metric_scan(&["job"]));
        let space = search_workload(vec![("q", root)]);

        // One group for the Aggregate, one for its Scan child.
        assert_eq!(space.len(), 2);

        let agg_group = space
            .target_subdag_candidates()
            .find(|g| matches!(g.target.non_asap(), Some(NonASAPOp::Aggregate { .. })))
            .expect("an Aggregate group must be discovered");
        assert_eq!(agg_group.consumer_count, 1);
        assert_eq!(
            agg_group.candidates.len(),
            6,
            "Hydra candidates with unknown evidence remain available: {:?}",
            agg_group.candidates
        );
        assert!(agg_group
            .candidates
            .iter()
            .all(|c| matches!(&c.replacement, Replacement::SubDAG(n) if n.contains_asap())));
        assert_eq!(
            agg_group
                .candidates
                .iter()
                .filter(|candidate| {
                    let Replacement::SubDAG(node) = &candidate.replacement else {
                        return false;
                    };
                    let Operator::ASAP(ASAPOp::SummaryEstimate { summary_input, .. }) =
                        &node.operator
                    else {
                        return false;
                    };
                    matches!(
                        &summary_input.operator,
                        Operator::ASAP(ASAPOp::SummaryAgg {
                            grouping: GroupingStrategy::SharedMultiSubpopulation { .. },
                            ..
                        })
                    )
                })
                .count(),
            2,
            "Hydra candidates remain visible with symbolic shared-grid error"
        );
        assert_eq!(
            agg_group
                .candidates
                .iter()
                .filter(|candidate| candidate.has_missing_accuracy_evidence())
                .count(),
            2
        );
        let selected = space.global_selection(&DefaultCostModel);
        assert!(!selected
            .for_target(&space.roots[0].1)
            .unwrap()
            .chosen
            .is_some_and(ReplacementSubDAG::has_missing_accuracy_evidence));

        let scan_group = space
            .target_subdag_candidates()
            .find(|g| matches!(g.target.non_asap(), Some(NonASAPOp::Scan { .. })))
            .expect("a Scan group must be discovered");
        assert_eq!(scan_group.consumer_count, 1);
        assert!(
            scan_group.candidates.is_empty(),
            "no strategy matches a bare Scan"
        );
    }

    #[test]
    fn cardinality_group_keeps_all_four_candidates() {
        let root = agg(vec![2], default_cardinality(), metric_scan(&["job"]));
        let space = search_workload(vec![("q", root)]);
        let agg_group = space
            .target_subdag_candidates()
            .find(|g| matches!(g.target.non_asap(), Some(NonASAPOp::Aggregate { .. })))
            .unwrap();
        assert_eq!(agg_group.candidates.len(), 4);
        assert!(agg_group.candidates.iter().any(|candidate| matches!(
            &candidate.replacement,
            Replacement::SubDAG(node) if node.guarantee.is_none()
                && candidate.has_missing_accuracy_evidence()
        )));

        let root = agg(vec![2], default_cardinality(), metric_scan(&["job"]));
        let targeted = search_workload_with_targets(
            vec![(
                "q",
                root,
                Some(AccuracyTarget::EpsilonDelta {
                    epsilon: 0.01,
                    delta: 0.01,
                }),
            )],
            &default_strategies(),
            &DefaultAccuracyModel,
        );
        let target = &targeted.roots[0].1;
        assert!(targeted
            .candidates_for_target(target)
            .unwrap()
            .candidates
            .iter()
            .any(|candidate| matches!(
                &candidate.replacement,
                Replacement::SubDAG(node) if node.guarantee.is_none()
                    && candidate.has_missing_accuracy_evidence()
            )));
        assert!(!targeted
            .global_selection(&DefaultCostModel)
            .for_target(target)
            .unwrap()
            .chosen
            .is_some_and(ReplacementSubDAG::has_missing_accuracy_evidence));

        let exact_target = search_workload_with_targets(
            vec![(
                "q",
                agg(vec![2], default_cardinality(), metric_scan(&["job"])),
                Some(AccuracyTarget::Exact),
            )],
            &default_strategies(),
            &DefaultAccuracyModel,
        );
        assert!(exact_target
            .candidates_for_target(&exact_target.roots[0].1)
            .unwrap()
            .candidates
            .iter()
            .all(|candidate| !candidate.has_missing_accuracy_evidence()));
    }

    #[test]
    fn shared_aggregate_across_two_roots_gets_both_strategies_candidates() {
        // Two independently-built, structurally identical Sum aggregates:
        // share_common_sub_dags (run inside search_workload) collapses them
        // onto one Rc with consumer_count 2, so this single group should
        // carry ASAPStrategies's one ExactAggregate candidate *and*
        // SharedSubDAGStrategy's share-vs-recompute pair.
        let a = agg(vec![2], AggIntent::Sum { col: None }, metric_scan(&["job"]));
        let b = agg(vec![2], AggIntent::Sum { col: None }, metric_scan(&["job"]));
        let space = search_workload(vec![("a", a), ("b", b)]);

        // roots[0] and roots[1] must have merged onto the same Rc.
        assert!(Rc::ptr_eq(&space.roots[0].1, &space.roots[1].1));

        let group = space.candidates_for_target(&space.roots[0].1).unwrap();
        assert_eq!(group.consumer_count, 2);
        assert_eq!(
            group.candidates.len(),
            3,
            "1 ExactAggregate Summary + 2 Rewrite (share/recompute): {:?}",
            group.candidates
        );

        // Old `Replacement::Summary` ↔ a `Subtree` containing an ASAP node;
        // old `Replacement::Rewrite` ↔ a pure pre-ASAP `Subtree`.
        let summary_count = group
            .candidates
            .iter()
            .filter(|c| matches!(&c.replacement, Replacement::SubDAG(n) if n.contains_asap()))
            .count();
        let rewrite_count = group
            .candidates
            .iter()
            .filter(|c| matches!(&c.replacement, Replacement::SubDAG(n) if !n.contains_asap()))
            .count();
        assert_eq!(summary_count, 1);
        assert_eq!(rewrite_count, 2);

        // The two Rewrite candidates must NOT have collapsed into one
        // (the "false-positive dedup" failure mode `is_duplicate_rewrite`
        // exists to prevent).
        let one_is_the_target = group.candidates.iter().any(
            |c| matches!(&c.replacement, Replacement::SubDAG(rc) if Rc::ptr_eq(rc, &group.target)),
        );
        let one_is_not = group.candidates.iter().any(
            |c| matches!(&c.replacement, Replacement::SubDAG(rc) if !Rc::ptr_eq(rc, &group.target)),
        );
        assert!(one_is_the_target && one_is_not);
    }

    #[test]
    fn nested_shared_sub_dag_below_an_unshared_parent_is_still_discovered() {
        // A shared grouped Aggregate nested under two *different*,
        // unshared Filter parents — real consumer_count must come from
        // walking the whole DAG, not just root-level pointer identity
        // (a naive whole-root-only consumer-count pass would miss this;
        // this module's discover_targets must not).
        use asap_types::ir::scalar::ScalarValue;
        use asap_types::ir::Predicate;

        let shared = agg(vec![2], AggIntent::Sum { col: None }, metric_scan(&["job"]));
        // Different predicates so the two Filter *parents* stay distinct
        // (don't themselves merge under CSE) — only their shared `child`
        // should collapse onto one `Rc`.
        let root_a =
            OperatorNode::new_shared(asap_types::ir::Operator::NonASAP(NonASAPOp::Filter {
                pred: Predicate(ScalarExpr::Literal(ScalarValue::Int64(1))),
                child: Rc::clone(&shared),
            }))
            .unwrap();
        let root_b =
            OperatorNode::new_shared(asap_types::ir::Operator::NonASAP(NonASAPOp::Filter {
                pred: Predicate(ScalarExpr::Literal(ScalarValue::Int64(2))),
                child: Rc::clone(&shared),
            }))
            .unwrap();

        let space = search_workload(vec![("a", root_a), ("b", root_b)]);
        assert_eq!(
            space.len(),
            4,
            "2 distinct Filters + 1 shared Aggregate + 1 shared Scan"
        );

        // `share_common_sub_dags` re-clones+re-interns anything that already
        // had more than one owner going in (see `cse.rs`'s own doc on
        // `intern_child`'s clone-fallback path) — so the post-CSE shared
        // node is a *fresh* Rc, structurally equal to (but not the same
        // pointer as) the pre-search `shared` variable. Recover it from the
        // post-CSE root's own `child` field instead of the stale `shared`
        // handle.
        let Some(NonASAPOp::Filter {
            child: post_cse_shared_a,
            ..
        }) = space.roots[0].1.non_asap()
        else {
            panic!("expected a Filter root");
        };
        let Some(NonASAPOp::Filter {
            child: post_cse_shared_b,
            ..
        }) = space.roots[1].1.non_asap()
        else {
            panic!("expected a Filter root");
        };
        assert!(
            Rc::ptr_eq(post_cse_shared_a, post_cse_shared_b),
            "fixture sanity: the two Filters' children must still merge"
        );
        let post_cse_shared = post_cse_shared_a;
        let group = space
            .candidates_for_target(post_cse_shared)
            .expect("shared node must be a discovered target");
        assert_eq!(group.consumer_count, 2);
        assert!(
            SharedSubDAGStrategy.matches(&TargetSubDAG::with_consumer_count(
                post_cse_shared,
                group.consumer_count
            ))
        );
    }

    // ── dedup ────────────────────────────────────────────────────────────

    #[test]
    fn add_candidate_rejects_a_true_rewrite_duplicate() {
        // SharedSubDAGStrategy's `Replacement::Rewrite` candidates are
        // real `OperatorNode` values with `PartialEq`, so `add_candidate` can
        // (and must) actually reject a genuine repeat — unlike the
        // `Replacement::Summary` case (see the test below).
        let root = agg(vec![2], AggIntent::Sum { col: None }, metric_scan(&["job"]));
        let mut group = TargetSubDAGCandidates::new(Rc::clone(&root), 2);
        let target = TargetSubDAG::with_consumer_count(&root, 2);
        let mut inserted = 0;
        for candidate in SharedSubDAGStrategy.replacements(&target) {
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
        for candidate in SharedSubDAGStrategy.replacements(&target) {
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
        // Documented, deliberate consequence of `is_duplicate_summary`
        // refusing value equality on `f64`-bearing summaries: re-proposing
        // the same `Replacement::Summary` candidates DOES grow the group —
        // this module refuses to guess at an equality check it can't back
        // with a real `PartialEq`. `search_workload_with` never actually
        // does this in practice (every target is asked exactly once — see the
        // module docs' "Termination" section), so this test exists to pin
        // the documented behavior, not to endorse calling `replacements`
        // twice for the same target.
        let root = agg(vec![2], default_quantile(0.99), metric_scan(&["job"]));
        let mut group = TargetSubDAGCandidates::new(Rc::clone(&root), 1);
        let strategy = ASAPStrategies::default_cost_model();
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
        let target = agg(vec![2], AggIntent::Sum { col: None }, metric_scan(&["job"]));
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
        let target = agg(vec![2], AggIntent::Sum { col: None }, metric_scan(&["job"]));
        let first_recompute = Rc::new((*target).clone());
        let second_recompute = Rc::new((*target).clone());
        assert!(!Rc::ptr_eq(&first_recompute, &second_recompute));
        assert!(is_duplicate_rewrite(
            &first_recompute,
            &second_recompute,
            &target
        ));
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
        let space = search_workload(vec![("a", a), ("b", b)]);
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
            use asap_types::ir::scalar::ScalarValue;
            use asap_types::ir::Predicate;
            let fresh_inner_layer =
                OperatorNode::new_shared(asap_types::ir::Operator::NonASAP(NonASAPOp::Filter {
                    pred: Predicate(ScalarExpr::Literal(ScalarValue::Int64(n))),
                    child: Rc::clone(target.root),
                }))
                .unwrap();
            let outer_wrapper =
                OperatorNode::new_shared(asap_types::ir::Operator::NonASAP(NonASAPOp::Filter {
                    pred: Predicate(ScalarExpr::Literal(ScalarValue::Boolean(true))),
                    child: fresh_inner_layer,
                }))
                .unwrap();
            vec![ReplacementSubDAG {
                strategy: "AlwaysGrowingStrategy",
                replacement: Replacement::SubDAG(outer_wrapper),
                provenance: ReplacementProvenance::LogicalRewrite,
                rationale: format!("pathological candidate #{n}"),
            }]
        }
    }

    #[test]
    #[should_panic(expected = "did not converge")]
    fn a_pathologically_growing_strategy_trips_the_iteration_cap() {
        let root = metric_scan(&["job"]);
        let strategies: Vec<Box<dyn ReplacementStrategy>> = vec![Box::new(AlwaysGrowingStrategy {
            next: std::cell::Cell::new(0),
        })];
        let _ = search_workload_with(vec![("q", root)], &strategies);
    }
    // ── realize_child / retain_exact: end-to-end single-target realization ──
    //
    // Moved from the former `bind.rs` (issue #251): `bind.rs`'s own
    // workload-wide orchestration (`implement_workload`/
    // `implement_workload_with`) was deleted. Current whole-workload logical
    // selection uses `CandidateLogicalASAPDAGs::global_selection`; these tests exercise
    // `construct_summary_agg`'s schema derivation end to end through
    // `realize_child` — production logic that still lives in this module —
    // so they move here rather than disappear. Unlike `bind.rs` (an
    // external caller that had to reconstruct the rank-and-take-first
    // pattern by hand since `realize_child` is `pub(crate)`), these tests
    // call `realize_child` directly.

    fn field<'a>(schema: &'a Schema, name: &str) -> &'a Field {
        schema
            .fields
            .iter()
            .find(|f| f.name == name)
            .unwrap_or_else(|| panic!("no field {name:?} in {schema:?}"))
    }

    fn realize_first(
        expr: &OperatorNode,
        cost_model: &dyn CostModel,
    ) -> Result<Rc<OperatorNode>, RealizationError> {
        realize_child(&Rc::new(expr.clone()), cost_model)
    }

    fn realize(expr: &OperatorNode) -> Result<Rc<OperatorNode>, RealizationError> {
        realize_first(expr, &DefaultCostModel)
    }

    #[test]
    fn quantile_realizes_kll_wrapped_in_estimate() {
        // quantile by (job) (m) at ε=0.01 → Estimate(Quantile) over
        // SummaryAgg(Kll{k:269}) over the kept Scan. job = col 2.
        let q = agg(vec![2], default_quantile(0.99), metric_scan(&["job"]));
        let root = realize(&q).unwrap();

        let Operator::ASAP(ASAPOp::SummaryEstimate {
            summary_input,
            query,
        }) = &root.operator
        else {
            panic!("expected SummaryEstimate root, got {:?}", root.operator);
        };
        assert!(matches!(query, PostAsapSketchStatistic::Quantile { q } if *q == 0.99));
        // Estimate edge: plain row shape — group key + Float64 answer.
        assert_eq!(
            field(&root.schema, "quantile_0_99").dtype,
            FieldDataType::Plain(DataType::Float64)
        );
        assert_eq!(
            field(&root.schema, "job").dtype,
            FieldDataType::Plain(DataType::Utf8)
        );

        let Operator::ASAP(ASAPOp::SummaryAgg {
            child,
            family,
            input,
            reduction,
            ..
        }) = &summary_input.operator
        else {
            panic!("expected SummaryAgg, got {:?}", summary_input.operator);
        };
        assert_eq!(
            family,
            &FieldDataType::Sketch(
                SketchKind::new(SketchAlgorithm::Kll, SketchParams::Kll { k: 269 }),
                GroupingStrategy::default()
            )
        );
        assert_eq!(input, &SummaryUpdate::column(ColumnRef::SampleValue));
        assert_eq!(reduction, &ReductionTy::by(vec![2]));
        // SummaryAgg edge: the state column, named after its input, carries
        // the committed family.
        assert_eq!(
            field(&summary_input.schema, "value").dtype,
            FieldDataType::Sketch(
                SketchKind::new(SketchAlgorithm::Kll, SketchParams::Kll { k: 269 }),
                GroupingStrategy::default()
            )
        );
        // The kept pre-ASAP leaf is the Scan node itself (no wrapper).
        assert!(matches!(child.non_asap(), Some(NonASAPOp::Scan { .. })));
        assert!(!child.contains_asap());
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
        let Operator::ASAP(ASAPOp::SummaryEstimate { summary_input, .. }) = &default_root.operator
        else {
            panic!(
                "expected SummaryEstimate root, got {:?}",
                default_root.operator
            );
        };
        let Operator::ASAP(ASAPOp::SummaryAgg { family, .. }) = &summary_input.operator else {
            panic!("expected SummaryAgg, got {:?}", summary_input.operator);
        };
        assert!(matches!(
            family,
            FieldDataType::Sketch(kind, _) if kind.algorithm() == &SketchAlgorithm::Kll
        ));

        // With `PreferDDSketchViaCostModel`: DDSketch instead, same query.
        let custom_root = realize_first(&q, &PreferDDSketchViaCostModel).unwrap();
        let Operator::ASAP(ASAPOp::SummaryEstimate { summary_input, .. }) = &custom_root.operator
        else {
            panic!(
                "expected SummaryEstimate root, got {:?}",
                custom_root.operator
            );
        };
        let Operator::ASAP(ASAPOp::SummaryAgg { family, .. }) = &summary_input.operator else {
            panic!("expected SummaryAgg, got {:?}", summary_input.operator);
        };
        assert_eq!(
            family,
            &FieldDataType::Sketch(
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
    /// #150) — `realizations_for_intent` must consult `realize_extension`
    /// for the `Extension` arm, and `evaluation` must consult
    /// `evaluation_extension` to build its `SketchStatistic` without panicking.
    struct FrequencyCostModel;

    impl CostModel for FrequencyCostModel {
        fn rank_candidates(
            &self,
            _intent: &AggIntent,
            candidates: &[SketchAlgorithm],
        ) -> Vec<SketchAlgorithm> {
            candidates.to_vec()
        }

        fn realize_extension(&self, ext_kind: &str, _payload: &serde_json::Value) -> Realization {
            if ext_kind == "frequency" {
                Realization::Sketch(SketchKind::new(
                    SketchAlgorithm::CountSketch,
                    SketchParams::CountSketch {
                        width: 256,
                        depth: 4,
                    },
                ))
            } else {
                Realization::PassThrough
            }
        }

        fn evaluation_extension(
            &self,
            ext_kind: &str,
            payload: &serde_json::Value,
            _col: &ColumnRef,
        ) -> PostAsapSketchStatistic {
            assert_eq!(ext_kind, "frequency");
            let value = payload["item"].as_str().map(str::to_string);
            PostAsapSketchStatistic::PointCount {
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
        assert!(!root.contains_asap());
    }

    #[test]
    fn extension_intent_realizes_via_custom_cost_model() {
        let intent = AggIntent::Extension {
            ext_kind: "frequency".to_string(),
            payload: serde_json::json!({ "item": "checkout" }),
        };
        let q = agg(vec![], intent, metric_scan(&[]));
        let root = realize_first(&q, &FrequencyCostModel).unwrap();

        let Operator::ASAP(ASAPOp::SummaryEstimate {
            summary_input,
            query,
        }) = &root.operator
        else {
            panic!("expected SummaryEstimate root, got {:?}", root.operator);
        };
        assert!(matches!(
            query,
            PostAsapSketchStatistic::PointCount { key: ColumnRef::Named(k), value: Some(v) }
                if k == "item" && v == "checkout"
        ));

        let Operator::ASAP(ASAPOp::SummaryAgg { family, .. }) = &summary_input.operator else {
            panic!("expected SummaryAgg, got {:?}", summary_input.operator);
        };
        assert_eq!(
            family,
            &FieldDataType::Sketch(
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
        let Operator::ASAP(ASAPOp::SummaryAgg { family, .. }) = &root.operator else {
            panic!(
                "expected bare SummaryAgg (no estimate), got {:?}",
                root.operator
            );
        };
        assert_eq!(
            family,
            &FieldDataType::ExactAggregate(ExactKind::Sum, ExactParams::Sum)
        );
        assert_eq!(
            field(&root.schema, "sum").dtype,
            FieldDataType::ExactAggregate(ExactKind::Sum, ExactParams::Sum)
        );
    }

    #[test]
    fn per_series_rate_keeps_labels_and_retypes_value() {
        // rate(m[5m]) — per-series: every label survives; the sample value
        // column becomes the Rate accumulator state.
        use std::time::Duration;
        let q = agg_per_entity(
            AggIntent::Rate,
            OperatorNode::new_shared(asap_types::ir::Operator::NonASAP(NonASAPOp::TimeRange {
                kind: TimeRangeKind::Range,
                range: Duration::from_secs(300),
                child: metric_scan(&["job"]),
            }))
            .unwrap(),
        );
        let root = realize(&q).unwrap();
        let Operator::ASAP(ASAPOp::SummaryAgg { family, .. }) = &root.operator else {
            panic!("expected SummaryAgg, got {:?}", root.operator);
        };
        assert_eq!(
            family,
            &FieldDataType::ExactAggregate(ExactKind::Rate, ExactParams::Rate)
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
            FieldDataType::ExactAggregate(ExactKind::Rate, ExactParams::Rate)
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
            OperatorNode::new_shared(asap_types::ir::Operator::NonASAP(NonASAPOp::TimeRange {
                kind: TimeRangeKind::Range,
                range: Duration::from_secs(10),
                child: metric_scan(&["job"]),
            }))
            .unwrap(),
        );
        let root = realize(&q).unwrap();
        let Operator::ASAP(ASAPOp::SummaryEstimate { summary_input, .. }) = &root.operator else {
            panic!("expected estimate root, got {:?}", root.operator);
        };
        let Operator::ASAP(ASAPOp::SummaryAgg { reduction, .. }) = &summary_input.operator else {
            panic!("expected SummaryAgg, got {:?}", summary_input.operator);
        };
        assert_eq!(reduction, &ReductionTy::PerEntity);
    }

    /// Issue #163, case 2: an aggregation operator explicitly invoked with
    /// no grouping keys realizes to `SummaryAgg {
    /// reduction: Reduce(vec![]), .. }` — byte-identical `by: []` to the
    /// previous test at the old `Vec<ColumnId>` shape; `reduction` is what
    /// tells them apart now.
    #[test]
    fn explicit_empty_by_aggregate_realizes_summary_agg_with_reduce_reduction() {
        let intent = AggIntent::Cardinality {
            cols: vec![],
            accuracy: AccuracyTarget::Epsilon(0.01),
        };
        let q = agg(vec![], intent, metric_scan(&["job"]));
        let root = realize(&q).unwrap();
        let Operator::ASAP(ASAPOp::SummaryEstimate { summary_input, .. }) = &root.operator else {
            panic!("expected estimate root, got {:?}", root.operator);
        };
        let Operator::ASAP(ASAPOp::SummaryAgg { reduction, .. }) = &summary_input.operator else {
            panic!("expected SummaryAgg, got {:?}", summary_input.operator);
        };
        assert_eq!(reduction, &ReductionTy::by(vec![]));
    }

    #[test]
    fn nested_aggregates_realize_per_node() {
        // quantile(0.9, sum by (job) (m)) — the realization decision
        // fires per node over the nested DAG: KLL over an exact Sum
        // accumulator.
        let inner = agg(vec![2], AggIntent::Sum { col: None }, metric_scan(&["job"]));
        let outer = agg(vec![], default_quantile(0.9), inner);
        // Timing is not stored during realization: time the candidate under
        // a maintained materialization assignment to read the maintenance boundary.
        let root = maintained(&realize(&outer).unwrap());

        let Operator::ASAP(ASAPOp::SummaryEstimate { summary_input, .. }) = &root.operator else {
            panic!("expected estimate root, got {:?}", root.operator);
        };
        let Operator::ASAP(ASAPOp::SummaryAgg { child, family, .. }) = &summary_input.operator
        else {
            panic!(
                "expected outer SummaryAgg, got {:?}",
                summary_input.operator
            );
        };
        assert!(matches!(
            family,
            FieldDataType::Sketch(kind, _) if kind.algorithm() == &SketchAlgorithm::Kll
        ));
        assert_eq!(child.timing, Some(ExecutionTiming::IngestionTime));
        let Operator::ASAP(ASAPOp::FinalizeExactAccumulator { child }) = &child.operator else {
            panic!("expected explicit maintenance evaluation");
        };
        let Operator::ASAP(ASAPOp::SummaryAgg {
            family: inner_family,
            child: leaf,
            ..
        }) = &child.operator
        else {
            panic!("expected inner SummaryAgg, got {:?}", child.operator);
        };
        assert_eq!(
            inner_family,
            &FieldDataType::ExactAggregate(ExactKind::Sum, ExactParams::Sum)
        );
        assert!(!leaf.contains_asap());
    }

    /// Issue #115: the summary is built over the intent's own input columns.
    /// Before `Cardinality`/`Quantile` carried them, `summarised_input` always
    /// fell through to `ColumnRef::SampleValue`, so an HLL was built over the
    /// wrong column for every SQL `COUNT(DISTINCT c)`. A distinct-tuple count
    /// hashes the whole tuple as one item — feeding the sketch a single leg
    /// would report single-column cardinality instead.
    #[test]
    fn sketch_realizes_over_the_intents_input_columns() {
        // `metric_scan(&["job"])` → columns [ts=0, value=1, job=2].
        let column = |name: &str| SummaryInputExpr::Column(ColumnRef::Named(name.into()));
        let cases = [
            (vec![2], column("job")),
            (vec![1], column("value")),
            // PromQL convention: no column ⇒ the synthetic sample value.
            (vec![], SummaryInputExpr::Column(ColumnRef::SampleValue)),
            (
                vec![1, 2],
                SummaryInputExpr::Tuple(vec![column("value"), column("job")]),
            ),
        ];
        for (cols, want) in cases {
            let intent = AggIntent::Cardinality {
                cols: cols.clone(),
                accuracy: AccuracyTarget::Epsilon(0.01),
            };
            let root = realize(&agg(vec![0], intent, metric_scan(&["job"]))).unwrap();
            let bound = find_summary_input(&root)
                .unwrap_or_else(|| panic!("expected a SummaryAgg for cols={cols:?}"));
            assert_eq!(bound, want, "wrong summarised input for cols={cols:?}");
        }
    }

    /// The update expression of the first `SummaryAgg` in the tree.
    fn find_summary_input(node: &OperatorNode) -> Option<SummaryInputExpr> {
        match &node.operator {
            Operator::ASAP(ASAPOp::SummaryAgg { input, .. }) if input.item.is_none() => {
                Some(input.weight.clone())
            }
            Operator::ASAP(ASAPOp::SummaryEstimate { summary_input, .. }) => {
                find_summary_input(summary_input)
            }
            _ => None,
        }
    }

    #[test]
    fn pass_through_intents_stay_logical() {
        // avg is exact but non-mergeable; histogram_quantile (classic
        // buckets, #79) is never sketchable; exact quantile is exact by
        // decree. All three stay whole logical sub-DAGs.
        for intent in [
            AggIntent::Avg { col: None },
            AggIntent::HistogramQuantile { q: 0.99, le: 0 },
            AggIntent::Quantile {
                col: None,
                q: 0.99,
                accuracy: AccuracyTarget::Exact,
            },
        ] {
            let q = agg(vec![2], intent.clone(), metric_scan(&["job"]));
            let root = realize(&q).unwrap();
            // Kept pass-through: the pre-ASAP node itself, not a wrapper.
            assert!(
                !root.contains_asap() && root.operator == q.operator && root.schema == q.schema,
                "expected kept pre-ASAP passthrough for {intent:?}"
            );
        }
    }

    #[test]
    fn logical_parent_subsumes_bindable_child() {
        // Filter over a bindable quantile: a kept non-ASAP sub-DAG has no
        // summary children, so the conservative fallback keeps the whole sub-DAG
        // logical.
        use asap_types::ir::scalar::{CompareOpKind, ScalarValue};
        use asap_types::ir::Predicate;
        let q = OperatorNode::new_shared(asap_types::ir::Operator::NonASAP(NonASAPOp::Filter {
            pred: Predicate(ScalarExpr::Compare {
                left: Box::new(ScalarExpr::Column(0)),
                op: CompareOpKind::Gt,
                right: Box::new(ScalarExpr::Literal(ScalarValue::Float64(0.5))),
                semantics: asap_types::ir::ExprSemantics::Promql,
            }),
            child: agg(vec![], default_quantile(0.99), metric_scan(&[])),
        }))
        .unwrap();
        let root = realize(&q).unwrap();
        assert!(
            !root.contains_asap() && root.operator == q.operator && root.schema == q.schema,
            "expected the whole Filter sub_dag kept pre-ASAP"
        );
    }

    #[test]
    fn having_and_multi_intent_stay_logical() {
        use asap_types::ir::scalar::ScalarValue;
        use asap_types::ir::Predicate;
        let q = crate::test_support::aggregate(
            ReductionTy::by(vec![2]),
            vec![default_quantile(0.99)],
            vec![],
            Some(Predicate(ScalarExpr::Literal(ScalarValue::Boolean(true)))),
            metric_scan(&["job"]),
        );
        assert!(!realize(&q).unwrap().contains_asap());

        let multi =
            OperatorNode::new_shared(asap_types::ir::Operator::NonASAP(NonASAPOp::Aggregate {
                reduction: ReductionTy::by(vec![2]),
                measures: vec![AggIntent::Sum { col: None }, AggIntent::Avg { col: None }],
                output_names: vec![],
                filters: vec![],
                having: None,
                child: metric_scan(&["job"]),
            }))
            .unwrap();
        assert!(!realize(&multi).unwrap().contains_asap());
    }

    // No binding rule applies a per-measure `FILTER` (#466), so the
    // aggregate is retained exactly rather than bound to a summary.
    #[test]
    fn filtered_measure_stays_logical() {
        use asap_types::ir::scalar::ScalarValue;
        let mut q = agg(vec![2], default_quantile(0.99), metric_scan(&["job"]));
        if let Operator::NonASAP(NonASAPOp::Aggregate { filters, .. }) =
            &mut Rc::make_mut(&mut q).operator
        {
            *filters = vec![Some(Predicate(ScalarExpr::Literal(ScalarValue::Boolean(
                true,
            ))))];
        }
        assert!(bindable_intent(&q).is_none());
        assert!(!realize(&q).unwrap().contains_asap());
    }

    #[test]
    fn unsupported_direct_topk_without_margin_evidence_falls_back_to_pre_asap() {
        let q = agg(
            vec![2],
            AggIntent::TopK {
                k: 5,
                accuracy: AccuracyTarget::Epsilon(0.01),
            },
            metric_scan(&["job"]),
        );
        let root = realize(&q).unwrap();
        assert!(!root.contains_asap());
    }

    #[test]
    fn count_ranked_topk_without_margin_evidence_keeps_an_uncertified_candidate() {
        let inner = agg(
            vec![2],
            AggIntent::Count {
                accuracy: AccuracyTarget::Epsilon(0.01),
            },
            metric_scan(&["job"]),
        );
        let root = agg(
            vec![],
            AggIntent::TopK {
                k: 5,
                accuracy: AccuracyTarget::Epsilon(0.01),
            },
            inner,
        );
        let proposals =
            ASAPStrategies::default_cost_model().replacements(&TargetSubDAG::new(&root));
        assert!(!proposals.is_empty());
        assert!(proposals.iter().any(|candidate| matches!(
            &candidate.replacement,
            Replacement::SubDAG(node) if node.guarantee.as_ref().is_some_and(|g|
                g.bound.evaluate().is_none()
                    && g.failure_probability.evaluate().is_none())
        )));
    }

    struct SeparatedTopKEvidence;

    impl AccuracyEvidenceProvider for SeparatedTopKEvidence {
        fn propagation_stats(
            &self,
            op: &CompositionOperator,
            _family: &FieldDataType,
            _query: Option<&PostAsapSketchStatistic>,
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
        let inner = agg(
            vec![2],
            AggIntent::Count {
                accuracy: AccuracyTarget::EpsilonDelta {
                    epsilon: 0.01,
                    delta: 0.01,
                },
            },
            metric_scan(&["job"]),
        );
        let q = agg(
            vec![],
            AggIntent::TopK {
                k: 5,
                accuracy: AccuracyTarget::EpsilonDelta {
                    epsilon: 0.01,
                    delta: 0.01,
                },
            },
            inner,
        );
        let strategy = ASAPStrategies::new_with_planning_inputs_and_evidence(
            &DefaultCostModel,
            &DefaultAccuracyModel,
            &EqualSplitAllocator,
            &SeparatedTopKEvidence,
        );
        let replacements = strategy.replacements(&TargetSubDAG::new(&q));
        assert!(!replacements.is_empty());
        assert!(replacements.iter().all(|candidate| matches!(
            &candidate.replacement,
            Replacement::SubDAG(node)
                if node.guarantee.as_ref().is_some_and(|g|
                    g.metric == ErrorMetric::TopKMembership
                        && g.failure_probability.evaluate() == Some(0.005))
        )));
    }

    #[test]
    fn count_ranked_topk_fuses_to_one_global_heap_sketch() {
        let inner = agg(
            vec![2],
            AggIntent::Count {
                accuracy: AccuracyTarget::Epsilon(0.01),
            },
            metric_scan(&["service"]),
        );
        let outer = agg(
            vec![],
            AggIntent::TopK {
                k: 10,
                accuracy: AccuracyTarget::Epsilon(0.01),
            },
            inner,
        );
        let strategy = ASAPStrategies::new_with_planning_inputs_and_evidence(
            &DefaultCostModel,
            &DefaultAccuracyModel,
            &EqualSplitAllocator,
            &SeparatedTopKEvidence,
        );
        let candidates = strategy.replacements(&TargetSubDAG::new(&outer));
        let node = candidates
            .iter()
            .find_map(|candidate| match &candidate.replacement {
                Replacement::SubDAG(node) if candidate.rationale.contains("CmsWithHeap") => {
                    Some(node)
                }
                _ => None,
            })
            .expect("CmsWithHeap candidate");
        let Operator::ASAP(ASAPOp::SummaryEstimate {
            summary_input,
            query,
        }) = &node.operator
        else {
            panic!("expected Top-K evaluation")
        };
        assert!(matches!(query, PostAsapSketchStatistic::TopK { k: 10 }));
        let Operator::ASAP(ASAPOp::SummaryAgg {
            child,
            family,
            input,
            ..
        }) = &summary_input.operator
        else {
            panic!("expected fused summary aggregation")
        };
        assert!(matches!(
            input.item.as_ref(),
            Some(SummaryInputExpr::Column(ColumnRef::Named(name))) if name == "service"
        ));
        assert_eq!(input.weight, SummaryInputExpr::Constant(1.0));
        assert_eq!(
            input.weight_domain,
            WeightDomain::NonNegative {
                proof: NonNegativeWeightProof::UnitCount,
            }
        );
        assert!(matches!(
            family,
            FieldDataType::Sketch(kind, _)
                if kind.algorithm() == &SketchAlgorithm::CmsWithHeap
        ));
        assert!(!child.contains_asap());
    }

    #[test]
    fn sum_ranked_topk_fuses_to_one_value_weighted_heap_sketch() {
        let inner = agg(
            vec![2],
            AggIntent::Sum { col: None },
            metric_scan(&["service"]),
        );
        let outer = agg(
            vec![],
            AggIntent::TopK {
                k: 5,
                accuracy: AccuracyTarget::Epsilon(0.01),
            },
            inner,
        );
        let strategy = ASAPStrategies::new_with_planning_inputs_and_evidence(
            &DefaultCostModel,
            &DefaultAccuracyModel,
            &EqualSplitAllocator,
            &SeparatedTopKEvidence,
        );
        let candidates = strategy.replacements(&TargetSubDAG::new(&outer));
        assert!(
            candidates.iter().all(|candidate| {
                !candidate.rationale.contains("CmsWithHeap")
                    || candidate.rationale.contains("CountSketchWithHeap")
            }),
            "generic weighted Sum has no non-negative proof and must reject CMS"
        );
        let node = candidates
            .iter()
            .find_map(|candidate| match &candidate.replacement {
                Replacement::SubDAG(node)
                    if candidate.rationale.contains("CountSketchWithHeap") =>
                {
                    Some(node)
                }
                _ => None,
            })
            .expect("CountSketchWithHeap candidate");
        let Operator::ASAP(ASAPOp::SummaryEstimate {
            summary_input,
            query,
        }) = &node.operator
        else {
            panic!("expected Top-K evaluation")
        };
        assert!(matches!(query, PostAsapSketchStatistic::TopK { k: 5 }));
        let Operator::ASAP(ASAPOp::SummaryAgg { child, input, .. }) = &summary_input.operator
        else {
            panic!("expected fused summary aggregation")
        };
        assert!(matches!(
            input.item.as_ref(),
            Some(SummaryInputExpr::Column(ColumnRef::Named(name))) if name == "service"
        ));
        assert_eq!(
            input.weight,
            SummaryInputExpr::Column(ColumnRef::SampleValue)
        );
        assert!(!child.contains_asap());
    }

    #[test]
    fn temporal_per_entity_topk_uses_series_identity_and_sample_value() {
        let inner =
            OperatorNode::new_shared(asap_types::ir::Operator::NonASAP(NonASAPOp::Aggregate {
                reduction: ReductionTy::PerEntity,
                measures: vec![AggIntent::Sum { col: None }],
                output_names: vec![],
                filters: vec![],
                having: None,
                child: OperatorNode::new_shared(asap_types::ir::Operator::NonASAP(
                    NonASAPOp::TimeRange {
                        kind: TimeRangeKind::Range,
                        range: std::time::Duration::from_secs(60),
                        child: metric_scan(&["service"]),
                    },
                ))
                .unwrap(),
            }))
            .unwrap();
        let outer = agg(
            vec![2],
            AggIntent::TopK {
                k: 5,
                accuracy: AccuracyTarget::Epsilon(0.01),
            },
            inner,
        );
        let strategy = ASAPStrategies::new_with_planning_inputs_and_evidence(
            &DefaultCostModel,
            &DefaultAccuracyModel,
            &EqualSplitAllocator,
            &SeparatedTopKEvidence,
        );
        let candidates = strategy.replacements(&TargetSubDAG::new(&outer));
        let input = candidates.iter().find_map(|candidate| {
            let Replacement::SubDAG(node) = &candidate.replacement else {
                return None;
            };
            let Operator::ASAP(ASAPOp::SummaryEstimate { summary_input, .. }) = &node.operator
            else {
                return None;
            };
            let Operator::ASAP(ASAPOp::SummaryAgg { input, .. }) = &summary_input.operator else {
                return None;
            };
            input.item.is_some().then_some(input)
        });
        let input = input.expect("keyed summary input");
        assert_eq!(
            input.item.as_ref(),
            Some(&SummaryInputExpr::EntityIdentity(
                EntityIdentity::PromqlLabelSet {
                    excluding: vec![ColumnRef::Named("service".into())]
                }
            ))
        );
        assert_eq!(
            input.weight,
            SummaryInputExpr::Column(ColumnRef::SampleValue)
        );
    }

    #[test]
    fn multidimensional_ranked_item_is_a_structured_tuple() {
        let inner = agg(
            vec![2, 3],
            AggIntent::Count {
                accuracy: AccuracyTarget::Epsilon(0.01),
            },
            metric_scan(&["service", "region"]),
        );
        let outer = agg(
            vec![],
            AggIntent::TopK {
                k: 10,
                accuracy: AccuracyTarget::Epsilon(0.01),
            },
            inner,
        );
        let strategy = ASAPStrategies::new_with_planning_inputs_and_evidence(
            &DefaultCostModel,
            &DefaultAccuracyModel,
            &EqualSplitAllocator,
            &SeparatedTopKEvidence,
        );

        let update = strategy
            .replacements(&TargetSubDAG::new(&outer))
            .into_iter()
            .find_map(|candidate| match candidate.replacement {
                Replacement::SubDAG(node) => match &node.operator {
                    Operator::ASAP(ASAPOp::SummaryEstimate { summary_input, .. }) => {
                        match &summary_input.operator {
                            Operator::ASAP(ASAPOp::SummaryAgg { input, .. }) => Some(input.clone()),
                            _ => None,
                        }
                    }
                    _ => None,
                },
                _ => None,
            })
            .expect("tuple-keyed summary candidate");
        assert!(matches!(
            update.item,
            Some(SummaryInputExpr::Tuple(ref items))
                if items == &[
                    SummaryInputExpr::Column(ColumnRef::Named("service".into())),
                    SummaryInputExpr::Column(ColumnRef::Named("region".into())),
                ]
        ));
        assert_eq!(update.weight, SummaryInputExpr::Constant(1.0));
    }

    #[test]
    fn subpopulation_columns_are_not_part_of_the_ranked_item_tuple() {
        let inner = agg(
            vec![2, 3, 4],
            AggIntent::Count {
                accuracy: AccuracyTarget::Epsilon(0.01),
            },
            metric_scan(&["service", "method", "region"]),
        );
        // The inner aggregate outputs its grouping keys first, so column 2 is
        // `region`. Each region is a separate Top-K subpopulation.
        let outer = agg(
            vec![2],
            AggIntent::TopK {
                k: 10,
                accuracy: AccuracyTarget::Epsilon(0.01),
            },
            inner,
        );
        let strategy = ASAPStrategies::new_with_planning_inputs_and_evidence(
            &DefaultCostModel,
            &DefaultAccuracyModel,
            &EqualSplitAllocator,
            &SeparatedTopKEvidence,
        );
        let (update, reduction) = strategy
            .replacements(&TargetSubDAG::new(&outer))
            .into_iter()
            .find_map(|candidate| match candidate.replacement {
                Replacement::SubDAG(node) => match &node.operator {
                    Operator::ASAP(ASAPOp::SummaryEstimate { summary_input, .. }) => {
                        match &summary_input.operator {
                            Operator::ASAP(ASAPOp::SummaryAgg {
                                input, reduction, ..
                            }) => Some((input.clone(), reduction.clone())),
                            _ => None,
                        }
                    }
                    _ => None,
                },
                _ => None,
            })
            .expect("subpopulation-aware summary candidate");
        assert_eq!(reduction, ReductionTy::by(vec![2]));
        assert!(matches!(
            update.item,
            Some(SummaryInputExpr::Tuple(ref items))
                if items == &[
                    SummaryInputExpr::Column(ColumnRef::Named("service".into())),
                    SummaryInputExpr::Column(ColumnRef::Named("method".into())),
                ]
        ));
        assert_eq!(update.weight, SummaryInputExpr::Constant(1.0));
    }

    #[test]
    fn sql_reducer_resolves_named_input_column() {
        // SUM(bytes) over a tabular scan: `col` resolves positionally to the
        // named column, not the PromQL sample value.
        let scan = OperatorNode::new_shared(asap_types::ir::Operator::NonASAP(NonASAPOp::Scan {
            source: Source::Table {
                table_ref: "t".into(),
            },
            predicates: vec![],
            schema: SchemaTy {
                fields: vec![
                    Field::plain("host", DataType::Utf8, false),
                    Field::plain("bytes", DataType::Int64, false),
                ],
                time_index: None,
                unique_keys: vec![],
                closed: true,
            },
        }))
        .unwrap();
        let q = agg(vec![0], AggIntent::Sum { col: Some(1) }, scan);
        let root = realize(&q).unwrap();
        let Operator::ASAP(ASAPOp::SummaryAgg { input, .. }) = &root.operator else {
            panic!("expected SummaryAgg, got {:?}", root.operator);
        };
        let SummaryInputExpr::Column(col) = &input.weight else {
            panic!("expected observation column")
        };
        assert_eq!(col, &ColumnRef::Named("bytes".into()));
    }

    // ── Accuracy guarantees and fail-closed composition (issue #172) ─────

    use asap_types::ir::properties::ErrorMetric;

    /// A test-only `AccuracyModel` that *registers* a rule the default
    /// deliberately lacks — a sketch over rank-bounded inputs composes
    /// additively, keeping the outer sketch's own metric — so the
    /// composition/allocation machinery can be exercised end to end.
    /// Everything else delegates to `DefaultAccuracyModel`.
    struct RankAdditiveModel;

    impl AccuracyModel for RankAdditiveModel {
        fn local_guarantee(
            &self,
            family: &FieldDataType,
            query: &PostAsapSketchStatistic,
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

    fn summary_child(node: &OperatorNode) -> &Rc<OperatorNode> {
        match &node.operator {
            Operator::ASAP(ASAPOp::SummaryEstimate { summary_input, .. }) => {
                summary_child(summary_input)
            }
            Operator::ASAP(ASAPOp::SummaryAgg { child, .. }) => child,
            other => panic!("expected a SummaryAgg, got {other:?}"),
        }
    }

    #[test]
    fn approximate_over_approximate_is_rejected_by_default_not_treated_as_exact() {
        // quantile(0.99, quantile by (job) (0.5, m)): rank over rank — no
        // registered rule, so every outer sketch candidate is refused with a
        // typed reason and the raw/pre-ASAP alternative is what remains.
        let inner = agg(vec![2], default_quantile(0.5), metric_scan(&["job"]));
        let outer = agg(vec![], default_quantile(0.99), inner);
        let proposals = ASAPStrategies::default_cost_model().propose(&TargetSubDAG::new(&outer));
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
        // Fallback keeps the whole sub-DAG pre-ASAP — executed exactly.
        let realized = realize_child(&outer, &DefaultCostModel).unwrap();
        assert!(!realized.contains_asap());
        assert!(realized
            .guarantee
            .as_ref()
            .is_some_and(ResultGuarantee::is_exact));

        // Cross-metric: a quantile over a cardinality estimate.
        let inner = agg(vec![2], default_cardinality(), metric_scan(&["job"]));
        let outer = agg(vec![], default_quantile(0.99), inner);
        let proposals = ASAPStrategies::default_cost_model().propose(&TargetSubDAG::new(&outer));
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
        // — the evaluation's guarantee is exactly KLL's own local guarantee.
        let inner = agg(vec![2], AggIntent::Sum { col: None }, metric_scan(&["job"]));
        let outer = agg(vec![], default_quantile(0.9), inner);
        let root = realize(&outer).unwrap();
        let guarantee = root
            .guarantee
            .as_ref()
            .expect("a evaluation carries a guarantee");
        assert_eq!(guarantee.metric, ErrorMetric::Rank);
        assert_eq!(
            guarantee.bound.evaluate(),
            Some(crate::accuracy::estimators::kll::kll_rank_error_99(269))
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
        let Operator::ASAP(ASAPOp::SummaryEstimate { summary_input, .. }) = &root.operator else {
            panic!()
        };
        assert!(summary_input.guarantee.is_none());
        assert!(summary_child(&root)
            .guarantee
            .as_ref()
            .is_some_and(ResultGuarantee::is_exact));
    }

    #[test]
    fn exact_sum_can_consume_an_approximate_evaluation() {
        // sum(count_distinct by (job) (m)) is an outer exact summary over
        // the inner HLL evaluation. Both summary levels remain explicit.
        let inner = agg(vec![2], default_cardinality(), metric_scan(&["job"]));
        let outer = agg(vec![], AggIntent::Sum { col: None }, inner);
        let root = realize(&outer).unwrap();
        let Operator::ASAP(ASAPOp::SummaryAgg { child, .. }) = &root.operator else {
            panic!("outer exact sum should remain a SummaryAgg")
        };
        assert!(matches!(
            child.operator,
            Operator::ASAP(ASAPOp::SummaryEstimate { .. })
        ));
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
    fn equal_split_allocation_supports_nested_summary_evaluations() {
        // A registered rank-additive rule and valid budget split make both
        // summary levels explicit while preserving the composed guarantee.
        let inner = agg(vec![2], quantile_eps(0.5, 0.1), metric_scan(&["job"]));
        let outer = agg(vec![], quantile_eps(0.99, 0.1), inner);
        let strategy = ASAPStrategies::new_with_planning_inputs(
            &DefaultCostModel,
            &RankAdditiveModel,
            &EqualSplitAllocator,
        );
        let proposals = strategy.propose(&TargetSubDAG::new(&outer));

        assert!(!proposals.candidates.is_empty());
        assert!(proposals.candidates.iter().all(|candidate| {
            let Replacement::SubDAG(node) = &candidate.replacement else {
                return false;
            };
            matches!(
                node.operator,
                Operator::ASAP(ASAPOp::SummaryEstimate { .. })
            ) && node.guarantee.as_ref().is_some_and(|guarantee| {
                DefaultAccuracyModel.satisfies(guarantee, &AccuracyTarget::Epsilon(0.1))
            })
        }));
    }

    #[test]
    fn global_selection_can_choose_nested_summaries() {
        // The same nested summary remains available through workload search
        // and global cost ranking.
        let inner = agg(vec![2], quantile_eps(0.5, 0.1), metric_scan(&["job"]));
        let outer = agg(vec![], quantile_eps(0.99, 0.1), inner);
        let strategies: Vec<Box<dyn ReplacementStrategy>> =
            vec![Box::new(ASAPStrategies::new_with_planning_inputs(
                &DefaultCostModel,
                &RankAdditiveModel,
                &EqualSplitAllocator,
            ))];
        let space = search_workload_with(vec![("q", Rc::clone(&outer))], &strategies);
        let root = &space.roots[0].1;
        let group = space.candidates_for_target(root).unwrap();
        assert!(!group.rejected.is_empty());
        assert!(group.candidates.iter().all(|c| match &c.replacement {
            // A summary candidate (old `Replacement::Summary`) contains an
            // ASAP node; a logical rewrite (old `Replacement::Rewrite`) does not.
            Replacement::SubDAG(node) if node.contains_asap() => {
                node.guarantee.as_ref().is_some_and(|g| {
                    DefaultAccuracyModel.satisfies(g, &AccuracyTarget::Epsilon(0.1))
                })
            }
            Replacement::SubDAG(_) => false,
            Replacement::ExactComposition(_) => false,
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
        let Replacement::SubDAG(node) = &chosen.replacement else {
            panic!()
        };
        assert!(matches!(
            node.operator,
            Operator::ASAP(ASAPOp::SummaryEstimate { .. })
        ));
    }

    #[test]
    fn root_target_check_removes_candidates_before_cost_ranking() {
        let q = agg(vec![2], default_quantile(0.99), metric_scan(&["job"]));
        // A root target tighter than the node's own ε=0.01: every sketch
        // candidate misses it and is moved to `rejected`; nothing is left
        // for the cost model to rank.
        let space = search_workload_with_targets(
            vec![("q", Rc::clone(&q), Some(AccuracyTarget::Epsilon(0.001)))],
            &default_strategies(),
            &DefaultAccuracyModel,
        );
        let root = &space.roots[0].1;
        let group = space.candidates_for_target(root).unwrap();
        assert!(group
            .candidates
            .iter()
            .all(|c| matches!(&c.replacement, Replacement::SubDAG(n) if !n.contains_asap())));
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
        let group = space.candidates_for_target(&space.roots[0].1).unwrap();
        assert!(group
            .candidates
            .iter()
            .any(|c| matches!(&c.replacement, Replacement::SubDAG(n) if n.contains_asap())));

        // An `Exact` root target admits only exact candidates.
        let space = search_workload_with_targets(
            vec![("q", Rc::clone(&q), Some(AccuracyTarget::Exact))],
            &default_strategies(),
            &DefaultAccuracyModel,
        );
        let group = space.candidates_for_target(&space.roots[0].1).unwrap();
        assert!(group.candidates.iter().all(|c| match &c.replacement {
            Replacement::SubDAG(node) if node.contains_asap() => node
                .guarantee
                .as_ref()
                .is_some_and(ResultGuarantee::is_exact),
            Replacement::SubDAG(_) => true,
            Replacement::ExactComposition(_) => false,
        }));
    }

    #[test]
    fn topk_accuracy_target_keeps_uncertified_membership() {
        let inner = agg(
            vec![2],
            AggIntent::Count {
                accuracy: AccuracyTarget::Epsilon(0.01),
            },
            metric_scan(&["job"]),
        );
        let q = agg(
            vec![],
            AggIntent::TopK {
                k: 10,
                accuracy: AccuracyTarget::Epsilon(0.01),
            },
            inner,
        );
        let space = search_workload_with_targets(
            vec![("q", Rc::clone(&q), Some(AccuracyTarget::Epsilon(0.01)))],
            &default_strategies(),
            &DefaultAccuracyModel,
        );
        let group = space.candidates_for_target(&space.roots[0].1).unwrap();

        assert!(group.candidates.iter().any(|candidate| matches!(
            &candidate.replacement,
            Replacement::SubDAG(node) if node.guarantee.as_ref().is_some_and(ResultGuarantee::has_unknown)
        )));
        let candidate = group
            .candidates
            .iter()
            .find(|candidate| candidate.has_missing_accuracy_evidence())
            .unwrap();
        let Replacement::SubDAG(node) = &candidate.replacement else {
            unreachable!()
        };
        let exported = asap_types::dag_export::export_summary(node);
        assert!(exported.nodes[exported.root as usize]
            .guarantee
            .as_ref()
            .is_some_and(ResultGuarantee::has_unknown));
        let selected = space.global_selection(&DefaultCostModel);
        assert!(!selected
            .for_target(&space.roots[0].1)
            .unwrap()
            .chosen
            .is_some_and(ReplacementSubDAG::has_missing_accuracy_evidence));
        assert!(selected
            .assemble_selected_dag(&space.roots[0].1)
            .unwrap()
            .is_some());
    }
    // Source evidence alone must enable Planner-owned sizing and certification.
    #[test]
    fn scoped_hll_evidence_sizes_and_certifies_without_a_deployment_model() {
        use crate::accuracy::EstimatorContract;
        struct SourceEvidence {
            expression: OperatorNode,
            max_distinct: u32,
        }
        impl AccuracyEvidenceProvider for SourceEvidence {
            fn estimator_contract(&self, expression: &OperatorNode) -> Option<EstimatorContract> {
                (expression == &self.expression).then_some(EstimatorContract::ClassicHll {
                    max_distinct_per_evaluation: self.max_distinct,
                })
            }
        }
        let target = AccuracyTarget::EpsilonDelta {
            epsilon: 0.05,
            delta: 0.01,
        };
        let root = agg(
            vec![],
            AggIntent::Cardinality {
                cols: vec![],
                accuracy: target.clone(),
            },
            metric_scan(&[]),
        );
        let evidence = SourceEvidence {
            expression: (*root).clone(),
            max_distinct: 128,
        };
        let strategy = ASAPStrategies::new_with_planning_inputs_and_evidence(
            &DefaultCostModel,
            &DefaultAccuracyModel,
            &EqualSplitAllocator,
            &evidence,
        );
        let candidates = strategy.replacements(&TargetSubDAG::new(&root));
        let hll = candidates
            .iter()
            .find_map(|candidate| match &candidate.replacement {
                Replacement::SubDAG(node)
                    if summary_family_algorithm(node) == SketchAlgorithm::Hll =>
                {
                    Some(node)
                }
                _ => None,
            })
            .expect("HLL candidate");
        assert!(DefaultAccuracyModel
            .satisfies(hll.guarantee.as_ref().expect("HLL confidence"), &target));
        let Operator::ASAP(ASAPOp::SummaryEstimate { summary_input, .. }) = &hll.operator else {
            panic!("evaluation")
        };
        let Operator::ASAP(ASAPOp::SummaryAgg {
            family: FieldDataType::Sketch(kind, _),
            ..
        }) = &summary_input.operator
        else {
            panic!("HLL state")
        };
        let expected = crate::accuracy::estimators::hll::ClassicHllConfidence::new(128, 0.05)
            .unwrap()
            .precision(0.01)
            .unwrap();
        assert_eq!(
            kind.params(),
            &SketchParams::Hll {
                precision: expected
            }
        );
        let absent = ASAPStrategies::default_cost_model().replacements(&TargetSubDAG::new(&root));
        assert!(!absent.iter().any(|candidate| matches!(&candidate.replacement, Replacement::SubDAG(node)
            if summary_family_algorithm(node) == SketchAlgorithm::Hll && node.guarantee.as_ref().is_some_and(|g| DefaultAccuracyModel.satisfies(g, &target)))));
        // Invalid contracts, infeasible targets and evidence for another source
        // must never authorize a confidence-bearing HLL candidate.
        for (max_distinct, delta, wrong_scope) in [
            (0, 0.01, false),
            (4097, 0.01, false),
            (128, 1e-12, false),
            (128, 0.01, true),
        ] {
            let target = AccuracyTarget::EpsilonDelta {
                epsilon: 0.05,
                delta,
            };
            let query = agg(
                vec![],
                AggIntent::Cardinality {
                    cols: vec![],
                    accuracy: target.clone(),
                },
                metric_scan(&[]),
            );
            let evidence = SourceEvidence {
                expression: if wrong_scope {
                    (*metric_scan(&["other"])).clone()
                } else {
                    (*query).clone()
                },
                max_distinct,
            };
            let strategy = ASAPStrategies::new_with_planning_inputs_and_evidence(
                &DefaultCostModel,
                &DefaultAccuracyModel,
                &EqualSplitAllocator,
                &evidence,
            );
            assert!(!strategy.replacements(&TargetSubDAG::new(&query)).iter().any(|candidate|
                matches!(&candidate.replacement, Replacement::SubDAG(node)
                if summary_family_algorithm(node) == SketchAlgorithm::Hll && node.guarantee.as_ref().is_some_and(|g| DefaultAccuracyModel.satisfies(g, &target)))));
        }
    }

    // A numeric group key must not be mistaken for the ranked aggregate score.
    #[test]
    fn ranking_uses_aggregate_output_position_not_first_numeric_column() {
        let logical = agg(vec![2], AggIntent::Sum { col: None }, metric_scan(&["id"]));
        let mut values = logical.schema.clone();
        values.fields[0].dtype = FieldDataType::Plain(DataType::Int64);
        assert_eq!(ranking_score_index(&logical, &values).unwrap(), 1);
    }
    // A heap's key schema is derived from its encoded item, not all label columns.
    #[test]
    fn heap_evaluation_preserves_numeric_item_identity() {
        let mut schema = metric_scan(&["id", "description"]).schema.clone();
        schema.fields[2].dtype = FieldDataType::Plain(DataType::Int64);
        let raw = crate::test_support::scan("m", schema);
        let node = agg(
            vec![],
            AggIntent::TopK {
                k: 2,
                accuracy: AccuracyTarget::Epsilon(0.01),
            },
            agg(vec![2], AggIntent::Sum { col: None }, raw.clone()),
        );
        let input = PhysicalSummaryInput {
            child: raw,
            input: SummaryUpdate {
                item: Some(SummaryInputExpr::Column(ColumnRef::Named("id".into()))),
                weight: SummaryInputExpr::Constant(1.0),
                weight_domain: WeightDomain::NonNegative {
                    proof: NonNegativeWeightProof::UnitCount,
                },
            },
        };
        let schema = keyed_heap_evaluation_schema(&input, &node).unwrap();
        assert_eq!(
            schema
                .fields
                .iter()
                .map(|f| f.name.as_str())
                .collect::<Vec<_>>(),
            vec!["id", "__asap_estimate"]
        );
        assert_eq!(
            schema.fields[0].dtype,
            FieldDataType::Plain(DataType::Int64)
        );
    }

    // Every SummaryAgg a strategy proposes derives coverage of its whole
    // source: an unrestricted selection over a definition reading that source.
    #[test]
    fn proposed_summary_states_cover_their_whole_source() {
        let root = agg(
            vec![],
            AggIntent::Quantile {
                q: 0.9,
                col: None,
                accuracy: AccuracyTarget::Epsilon(0.01),
            },
            metric_scan(&["job"]),
        );
        let source = Source::TimeSeries { metric: "m".into() };
        let proposals = ASAPStrategies::default_cost_model().propose(&TargetSubDAG::new(&root));
        let states: Vec<_> = proposals
            .candidates
            .iter()
            .filter_map(|candidate| match &candidate.replacement {
                Replacement::SubDAG(node) => Some(node),
                _ => None,
            })
            .flat_map(OperatorNode::reachable)
            .filter(|node| matches!(node.asap(), Some(ASAPOp::SummaryAgg { .. })))
            .collect();
        assert!(!states.is_empty());
        for state in states {
            let coverage = state.coverage().expect("summary state has coverage");
            assert_eq!(coverage.selection, [Default::default()]);
            let reads = OperatorNode::reachable(&coverage.definition)
                .into_iter()
                .filter_map(|node| match node.non_asap() {
                    Some(NonASAPOp::Scan { source, .. }) => Some(source.clone()),
                    _ => None,
                })
                .collect::<Vec<_>>();
            assert_eq!(reads, std::slice::from_ref(&source));
        }
    }
}

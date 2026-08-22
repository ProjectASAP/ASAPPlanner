//! Cost model interface (issues #6, #33).
//!
//! `asap-plan` deliberately has no cost model *implementation* of its own —
//! ranking candidate summaries by real cost (bandwidth budget, memory
//! footprint, site count, observed drift, workload-level CSE credit, …)
//! needs knowledge this crate doesn't have and shouldn't acquire: the crate
//! doc's layering invariant is that `asap-plan` depends only on [`asap_ir`],
//! never on a runtime or a deployment model. What it *can* own is the
//! interface every deployment's cost model plugs into, so [`boundary`]'s
//! summary selection has exactly one extension point instead of forcing
//! each downstream (ASAPCollector + ASAPQuery-backend, ASAPFusion, …) to
//! fork [`boundary::implementation_for`].
//!
//! This trait is scoped to the approximate-**sketch** family specifically
//! ([`CostModel::rank_candidates`]/[`size_params`](CostModel::size_params)
//! take/return [`SketchKind`]/[`SketchParams`]) — `asap_sketch` also has
//! sibling families for sampling-based, wavelet-transform, and fitted
//! statistical-model summaries
//! ([`asap_types::post_asap::SamplingKind`]/…/[`asap_types::post_asap::StatModelKind`]),
//! each with its own `(Kind, Params)` pair, deliberately *not* folded into
//! this trait: no core `AggIntent` picks one of those families today (only
//! [`CostModel::realize_extension`] can, for a deployment-specific
//! `AggIntent::Extension`), so there is no ranking/sizing decision for this
//! trait to own yet. Should a family other than `Sketch` ever need its own
//! `rank_candidates`/`size_params`, it gets its own trait methods rather
//! than overloading these ones across incompatible `Kind`/`Params` types.
//!
//! Every entry point that doesn't take an explicit `&dyn CostModel`
//! ([`implementation_for`](crate::boundary::implementation_for),
//! [`implement_tree`](crate::bind::implement_tree)) runs against
//! [`DefaultCostModel`], so a deployment that never plugs in its own cost
//! model keeps today's static-preference-order behavior exactly, byte for
//! byte.
//!
//! ## Decision (issue #237): rule-based vs. cost-based CSE sharing
//!
//! [`asap_types::pre_asap::cse::share_common_subtrees`] (issue #223 stages
//! 1–2, landed in PR #235) already *detects* every structurally-identical,
//! legally-shareable (`Schema::unique_keys`-gated) subtree and shares it
//! **unconditionally** — there is no cost gate on top of legality yet. This
//! section decides the framework for stage 4, "wire workload-level CSE
//! credit into `CostModel`" (named as planned above, and in this crate's own
//! module doc), the still-deferred step that turns "these two subtrees are
//! the same computation" into "and it's actually worth maintaining one
//! shared summary for them." It is a decision only — no code in this file
//! changes as a result; a follow-up PR implements it.
//!
//! **The two textbook framings** (as posed in #237):
//!
//! | Framework | Mechanism | CSE policy |
//! |---|---|---|
//! | Volcano/Cascades (SQL Server, Snowflake, Calcite) | cost-based, explores a plan space via DP + memo | share iff a real cost comparison (materialize/maintain vs. recompute-per-site) favors it |
//! | System R (classic) | heuristic, fixed rules over basic statistics | share whenever a fixed rule says to (e.g. "referenced more than once"), no per-case comparison |
//!
//! **Decision: a hybrid, matching #237's own suggestion — unconditional
//! sharing below a cheap-recompute threshold, a real cost comparison above
//! it.** Neither pure model fits this codebase on its own:
//!
//! - **Pure Volcano/Cascades is disproportionate.** This repo has no plan
//!   enumeration or DP memo search anywhere — `CostModel` is deliberately a
//!   narrow, single-shot ranking/sizing interface
//!   ([`rank_candidates`](CostModel::rank_candidates)/[`size_params`](CostModel::size_params)),
//!   not a cost-driven search engine, and building one solely to arbitrate a
//!   binary "share or don't" per CSE candidate would be new infrastructure
//!   out of proportion to the decision it answers.
//! - **Pure System R (today's stage 1/2 behavior: always share when legal)
//!   ignores a real, repo-specific asymmetry.** A shared summary here is not
//!   a free win the way sharing a relational scan is in a textbook OLTP
//!   optimizer — it is a sketch/accumulator that (per this crate's own
//!   stated purpose: *workload*-level planning, not single-query) is
//!   typically kept **continuously updated** as new data arrives, for as
//!   long as the workload runs, regardless of how often it's actually read.
//!   A structurally-shareable subtree that is cheap to recompute on demand,
//!   or rarely queried, can cost more to keep alive as a standing shared
//!   summary than to just recompute independently at each of its (few, or
//!   cheap) use sites — exactly the case #237 calls out.
//! - **The hybrid is what this crate already does one layer over**, for the
//!   structurally analogous sketch-vs-exact question: [`boundary`](crate::boundary)/[`bind`](crate::bind)
//!   don't run a full cost search either — they pick a cheap built-in
//!   default and let a deployment's `CostModel` override specific decisions
//!   ([`rank_candidates`](CostModel::rank_candidates)/[`size_params`](CostModel::size_params))
//!   with real cost knowledge this
//!   crate doesn't have. CSE-sharing is the same shape of question —
//!   "realize this once, shared, or recompute it" is the same family of
//!   decision as "realize this as a sketch, or exactly" — so it should be
//!   answered the same way: a cheap default (share; the *legality* gate
//!   already did the hard safety work) that a deployment overrides for the
//!   candidates expensive enough for the override to matter.
//!
//! **Why the hybrid is also the layering-forced answer, not just the
//! performance-preferred one.** `share_common_subtrees` lives in
//! `asap-types::pre_asap` — a lower layer that this crate depends on, never
//! the reverse (see this crate's own "arrows point up" layering invariant).
//! It therefore *cannot* consult a `CostModel` (defined here, in
//! `asap-aware-mapping`) even if it wanted to — detection is necessarily
//! cost-agnostic. That forces stage 1/2's default to be System R-style
//! ("share whenever legal," which is what it does today, correctly, as a
//! stage-1/2 default) and forces the cost-aware override to live downstream,
//! in this crate, applied *after* detection rather than fused into it. The
//! hybrid isn't a compromise chosen for its own sake — it's what the
//! existing crate boundary already requires; #237 just makes explicit that
//! the downstream override should itself be threshold-gated rather than a
//! blanket cost comparison on every candidate.
//!
//! ## Shape for stage 4 (not implemented here — for a follow-up PR)
//!
//! **Where it hooks in.** [`bind::implement_workload_with`](crate::bind::implement_workload_with)
//! is where sharing currently becomes concrete: it walks a workload's
//! already-CSE'd roots and, on a memo hit (`Rc::as_ptr` match — a root that
//! `share_common_subtrees` already pointed at a subtree some earlier root
//! also uses), unconditionally clones the cached `SummaryNode` instead of
//! rebinding. That memo-hit branch is the natural call site for the stage-4
//! decision: instead of an unconditional `Ok(Rc::clone(cached))`, consult
//! `CostModel` and either reuse the cached summary or bind this occurrence
//! independently via the ordinary `implement_tree_with` path (as if this
//! occurrence hadn't been detected as shared at all). `implement_workload_with`'s
//! own doc already flags that today's memoization is whole-root only — a
//! subtree shared below two roots' top level isn't memoized yet
//! ("widening this to sub-root memoization is future work"); stage 4's gate
//! should apply at whichever memo-hit points exist at the time it lands,
//! root-level today, any future sub-root memoization too.
//!
//! **New trait surface**, added the same way [`realize_extension`](CostModel::realize_extension)
//! was (issue #150) — a new method with a default that preserves current
//! behavior exactly, so `DefaultCostModel` and every deployment that doesn't
//! override it keeps today's unconditional-share semantics byte for byte:
//!
//! ```text
//! /// A detected, legality-gated CSE candidate — a subtree
//! /// `share_common_subtrees` already collapsed onto one `Rc`, at the point
//! /// a second (or later) consumer is about to reuse it.
//! pub struct CseCandidate<'a> {
//!     /// The shared pre-ASAP subtree itself.
//!     pub subtree: &'a QueryExpr,
//!     /// The `SummaryNode` this subtree already bound to on its first
//!     /// occurrence — gives the cost model the concrete
//!     /// `SummaryFamilyType`/`(kind, params)` actually at stake, not just
//!     /// the pre-ASAP shape.
//!     pub bound_summary: &'a SummaryNode,
//!     /// How many use sites reference this subtree so far (always >= 2 —
//!     /// only constructed on a memo hit; the first occurrence always
//!     /// binds independently, there being nothing yet to compare against).
//!     pub consumer_count: usize,
//! }
//!
//! pub enum ShareDecision {
//!     /// Reuse the cached `SummaryNode` (today's only behavior).
//!     Share,
//!     /// Bind this occurrence independently — the shared-maintenance cost
//!     /// isn't worth it for this candidate.
//!     RecomputeIndependently,
//! }
//!
//! trait CostModel {
//!     // ...existing methods...
//!
//!     /// Default: `Share`, unconditionally — preserves today's behavior.
//!     /// A deployment with real cost knowledge overrides this with the
//!     /// #237 hybrid rule: `Share` when an estimated recompute cost for
//!     /// `candidate` is below a cheap threshold (no comparison needed —
//!     /// System R-style); above the threshold, compare
//!     /// `estimated_recompute_cost * consumer_count` against an estimated
//!     /// shared-maintenance cost and pick whichever is cheaper
//!     /// (Volcano/Cascades-style). The exact cost formulas are
//!     /// deployment-specific, same as `size_params` today — this trait
//!     /// commits to the two-tier *shape* of the decision, not fixed
//!     /// numbers.
//!     fn cse_share_decision(&self, candidate: &CseCandidate) -> ShareDecision {
//!         ShareDecision::Share
//!     }
//! }
//! ```
//!
//! This keeps the extension-point pattern this file already uses throughout
//! (`rank_candidates`, `size_params`, `realize_extension`): core ships a
//! cheap, safe default; a deployment with actual cost data opts into
//! smarter behavior one method at a time, with zero forced changes anywhere
//! else that constructs a `CostModel`.
//!
//! **Scope note.** This section is the decision for issue #237 only. It
//! feeds into #223's stage 4 and does not implement `CseCandidate`,
//! `ShareDecision`, `cse_share_decision`, or any change to
//! `implement_workload_with` — those land in a follow-up PR, from this
//! decision, not in this commit.

use asap_types::post_asap::{SketchKind, SketchParams, SketchQuery};
use asap_types::pre_asap::agg_intent::AggIntent;
use asap_types::pre_asap::expr_ir::ColumnRef;

use crate::boundary::Implementation;

/// Ranks the candidate summary families for one [`AggIntent`], best choice
/// first.
///
/// [`boundary::summary_candidates`] returns every family that *can* answer an
/// intent, in an arbitrary static preference order (issue #98's "one home"
/// for the candidate set). A `CostModel` re-orders that list under real,
/// deployment-specific cost knowledge this crate has no way to know about —
/// [`boundary::implementation_for_with`] implements whichever candidate ends
/// up first after ranking.
pub trait CostModel {
    /// Rank `candidates` (as returned by
    /// [`summary_candidates`](crate::boundary::summary_candidates)) for
    /// `intent`, best choice first.
    ///
    /// Implementations MAY reorder freely and MAY drop entries that aren't
    /// available in their deployment, but MUST NOT invent a candidate that
    /// wasn't in the input — an unknown [`SketchKind`] has no
    /// [`SketchParams`](asap_types::post_asap::SketchParams) sizing logic in
    /// [`boundary::implementation_for_with`] and binding it will panic.
    /// Returning an empty `Vec` means "no candidate is acceptable";
    /// `implementation_for_with` treats that the same as `candidates`
    /// having been empty to begin with.
    fn rank_candidates(&self, intent: &AggIntent, candidates: &[SketchKind]) -> Vec<SketchKind>;

    /// Size [`SketchParams`] for `kind` (one of the candidates
    /// [`rank_candidates`](Self::rank_candidates) put first) under the
    /// resolved `(eps, delta)` accuracy budget.
    ///
    /// Splitting sizing out from candidate selection lets a deployment own
    /// its own parameter-sizing math (e.g. an empirically-tuned table, or
    /// discrete rungs required by a downstream catalog) without forking
    /// [`boundary::implementation_for_with`] — the same "one extension
    /// point" rationale as `rank_candidates`, one level deeper. Default:
    /// [`boundary::default_size_params`], `asap-plan`'s built-in formulas
    /// (unchanged) — a deployment that only needs to reorder candidates,
    /// not resize them, can leave this method unimplemented.
    fn size_params(
        &self,
        kind: SketchKind,
        intent: &AggIntent,
        eps: f64,
        delta: f64,
    ) -> SketchParams {
        crate::boundary::default_size_params(kind, intent, eps, delta)
    }

    /// Realize an `AggIntent::Extension { ext_kind, payload }` — a
    /// deployment-specific intent shape core has no realization opinion
    /// for (issue #131). `boundary::implementation_for_with` consults this
    /// for every `Extension` node instead of hardcoding `PassThrough`
    /// (issue #150). Default: `PassThrough` — preserves today's behavior
    /// for every deployment that doesn't override this, exactly like
    /// `size_params`'s default-delegates pattern above.
    fn realize_extension(&self, _ext_kind: &str, _payload: &serde_json::Value) -> Implementation {
        Implementation::PassThrough
    }

    /// Build the `SummaryEstimate` readout for an `Extension` intent this
    /// same `CostModel` realized as `Implementation::Sketch` via
    /// [`realize_extension`](Self::realize_extension). Only ever called
    /// when `realize_extension` returned `Sketch` for the same
    /// `(ext_kind, payload)` — `bind::readout` has no other way to build a
    /// `SketchQuery` for a shape core doesn't know. A deployment that
    /// overrides `realize_extension` to return `Sketch` for some
    /// `ext_kind` MUST also override this for that same `ext_kind`, or
    /// this default panics loudly (rather than silently misinterpreting
    /// `payload`) the first time that intent is actually read out.
    fn readout_extension(
        &self,
        ext_kind: &str,
        _payload: &serde_json::Value,
        _col: &ColumnRef,
    ) -> SketchQuery {
        unimplemented!(
            "CostModel::realize_extension returned Sketch for ext_kind={ext_kind:?} but \
             readout_extension wasn't overridden to match"
        )
    }
}

/// The default cost model: preserves [`summary_candidates`]'s built-in static
/// order and [`boundary::default_size_params`]'s built-in sizing unchanged.
///
/// [`summary_candidates`]: crate::boundary::summary_candidates
pub struct DefaultCostModel;

impl CostModel for DefaultCostModel {
    fn rank_candidates(&self, _intent: &AggIntent, candidates: &[SketchKind]) -> Vec<SketchKind> {
        candidates.to_vec()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::boundary::summary_candidates;
    use asap_types::pre_asap::agg_intent::default_cardinality;

    #[test]
    fn default_cost_model_preserves_static_order() {
        let intent = default_cardinality();
        let candidates = summary_candidates(&intent);
        assert_eq!(
            DefaultCostModel.rank_candidates(&intent, candidates),
            candidates.to_vec()
        );
    }

    struct AlwaysPreferLast;

    impl CostModel for AlwaysPreferLast {
        fn rank_candidates(
            &self,
            _intent: &AggIntent,
            candidates: &[SketchKind],
        ) -> Vec<SketchKind> {
            let mut v = candidates.to_vec();
            v.reverse();
            v
        }
    }

    #[test]
    fn custom_cost_model_can_reorder_candidates() {
        let intent = default_cardinality();
        let candidates = summary_candidates(&intent);
        let ranked = AlwaysPreferLast.rank_candidates(&intent, candidates);
        assert_eq!(ranked.first(), candidates.last());
    }

    /// A deployment that only overrides `rank_candidates` keeps
    /// `asap-plan`'s built-in sizing via the trait's default `size_params`
    /// body — the split is opt-in per method, not all-or-nothing.
    #[test]
    fn size_params_default_body_matches_default_size_params() {
        let intent = default_cardinality();
        assert_eq!(
            AlwaysPreferLast.size_params(SketchKind::Hll, &intent, 0.01, 0.01),
            crate::boundary::default_size_params(SketchKind::Hll, &intent, 0.01, 0.01),
        );
    }

    /// A deployment CAN override `size_params` independently of
    /// `rank_candidates` — e.g. to size against a catalog-constrained set
    /// of discrete parameter rungs instead of `asap-plan`'s continuous
    /// formulas.
    struct DiscreteKllRungs;

    impl CostModel for DiscreteKllRungs {
        fn rank_candidates(
            &self,
            _intent: &AggIntent,
            candidates: &[SketchKind],
        ) -> Vec<SketchKind> {
            candidates.to_vec()
        }

        fn size_params(
            &self,
            kind: SketchKind,
            intent: &AggIntent,
            eps: f64,
            delta: f64,
        ) -> SketchParams {
            match kind {
                SketchKind::Kll => {
                    let k = if eps >= 0.01 { 200 } else { 2048 };
                    SketchParams::Kll { k }
                }
                other => crate::boundary::default_size_params(other, intent, eps, delta),
            }
        }
    }

    #[test]
    fn custom_cost_model_can_override_sizing_independently_of_ranking() {
        use asap_types::pre_asap::agg_intent::default_quantile;

        let intent = default_quantile(0.99);
        assert_eq!(
            DiscreteKllRungs.size_params(SketchKind::Kll, &intent, 0.001, 0.01),
            SketchParams::Kll { k: 2048 },
        );
        // Untouched kinds still fall through to the default formula.
        assert_eq!(
            DiscreteKllRungs.size_params(SketchKind::Hll, &intent, 0.01, 0.01),
            crate::boundary::default_size_params(SketchKind::Hll, &intent, 0.01, 0.01),
        );
    }
}

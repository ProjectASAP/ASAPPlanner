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

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
//! "Summary" here is deliberately broader than the classic streaming
//! sketches [`SummaryKind`] enumerates today (Kll/DDSketch/Hll/Cms/…):
//! [`CostModel::rank_candidates`] ranks whatever [`SummaryKind`] variants
//! [`boundary::summary_candidates`] offers for an intent, so it already
//! extends to non-sketch realizations — sampling-based summaries,
//! wavelet-transform summaries, OMP/compressive-sensing summaries, … —
//! the day a variant for one lands in [`asap_sketch`]; nothing in this
//! trait or [`boundary`]'s dispatch is sketch-specific.
//!
//! Every entry point that doesn't take an explicit `&dyn CostModel`
//! ([`implementation_for`](crate::boundary::implementation_for),
//! [`implement_tree`](crate::bind::implement_tree),
//! [`implement_tree_in`](crate::bind::implement_tree_in)) runs against
//! [`DefaultCostModel`], so a deployment that never plugs in its own cost
//! model keeps today's static-preference-order behavior exactly, byte for
//! byte.

use asap_ir::intent_algebra::agg_intent::AggIntent;
use asap_sketch::{SummaryKind, SummaryParams};

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
    /// wasn't in the input — an unknown [`SummaryKind`] has no
    /// [`SummaryParams`](asap_sketch::SummaryParams) sizing logic in
    /// [`boundary::implementation_for_with`] and binding it will panic.
    /// Returning an empty `Vec` means "no candidate is acceptable";
    /// `implementation_for_with` treats that the same as `candidates`
    /// having been empty to begin with.
    fn rank_candidates(&self, intent: &AggIntent, candidates: &[SummaryKind]) -> Vec<SummaryKind>;

    /// Size [`SummaryParams`] for `kind` (one of the candidates
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
        kind: SummaryKind,
        intent: &AggIntent,
        eps: f64,
        delta: f64,
    ) -> SummaryParams {
        crate::boundary::default_size_params(kind, intent, eps, delta)
    }
}

/// The default cost model: preserves [`summary_candidates`]'s built-in static
/// order and [`boundary::default_size_params`]'s built-in sizing unchanged.
///
/// [`summary_candidates`]: crate::boundary::summary_candidates
pub struct DefaultCostModel;

impl CostModel for DefaultCostModel {
    fn rank_candidates(&self, _intent: &AggIntent, candidates: &[SummaryKind]) -> Vec<SummaryKind> {
        candidates.to_vec()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::boundary::summary_candidates;
    use asap_ir::intent_algebra::agg_intent::default_cardinality;

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
            candidates: &[SummaryKind],
        ) -> Vec<SummaryKind> {
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
            AlwaysPreferLast.size_params(SummaryKind::Hll, &intent, 0.01, 0.01),
            crate::boundary::default_size_params(SummaryKind::Hll, &intent, 0.01, 0.01),
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
            candidates: &[SummaryKind],
        ) -> Vec<SummaryKind> {
            candidates.to_vec()
        }

        fn size_params(
            &self,
            kind: SummaryKind,
            intent: &AggIntent,
            eps: f64,
            delta: f64,
        ) -> SummaryParams {
            match kind {
                SummaryKind::Kll => {
                    let k = if eps >= 0.01 { 200 } else { 2048 };
                    SummaryParams::Kll { k }
                }
                other => crate::boundary::default_size_params(other, intent, eps, delta),
            }
        }
    }

    #[test]
    fn custom_cost_model_can_override_sizing_independently_of_ranking() {
        use asap_ir::intent_algebra::agg_intent::default_quantile;

        let intent = default_quantile(0.99);
        assert_eq!(
            DiscreteKllRungs.size_params(SummaryKind::Kll, &intent, 0.001, 0.01),
            SummaryParams::Kll { k: 2048 },
        );
        // Untouched kinds still fall through to the default formula.
        assert_eq!(
            DiscreteKllRungs.size_params(SummaryKind::Hll, &intent, 0.01, 0.01),
            crate::boundary::default_size_params(SummaryKind::Hll, &intent, 0.01, 0.01),
        );
    }
}

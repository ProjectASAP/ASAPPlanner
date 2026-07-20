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
use asap_sketch::SummaryKind;

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
}

/// The default cost model: preserves [`summary_candidates`]'s built-in static
/// order unchanged.
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
}

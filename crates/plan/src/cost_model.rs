//! Cost model interface (issues #6, #33).
//!
//! `asap-plan` deliberately has no cost model *implementation* of its own —
//! ranking candidate sketches by real cost (bandwidth budget, memory
//! footprint, site count, observed drift, workload-level CSE credit, …)
//! needs knowledge this crate doesn't have and shouldn't acquire: the crate
//! doc's layering invariant is that `asap-plan` depends only on [`asap_ir`],
//! never on a runtime or a deployment model. What it *can* own is the
//! interface every deployment's cost model plugs into, so [`boundary`]'s
//! sketch selection has exactly one extension point instead of forcing each
//! downstream (ASAPCollector + ASAPQuery-backend, ASAPFusion, …) to fork
//! [`boundary::bind_sketch`].
//!
//! Every entry point that doesn't take an explicit `&dyn CostModel`
//! ([`realize`](crate::boundary::realize), [`bind`](crate::bind::bind),
//! [`bind_in`](crate::bind::bind_in)) runs against [`DefaultCostModel`], so
//! a deployment that never plugs in its own cost model keeps today's
//! static-preference-order behavior exactly, byte for byte.

use asap_ir::intent_algebra::agg_intent::AggIntent;
use asap_sketch::SummaryKind;

/// Ranks the candidate sketch families for one [`AggIntent`], best choice
/// first.
///
/// [`boundary::sketch_candidates`] returns every family that *can* answer an
/// intent, in an arbitrary static preference order (issue #98's "one home"
/// for the candidate set). A `CostModel` re-orders that list under real,
/// deployment-specific cost knowledge this crate has no way to know about —
/// [`boundary::bind_sketch`] binds whichever candidate ends up first after
/// ranking.
pub trait CostModel {
    /// Rank `candidates` (as returned by
    /// [`sketch_candidates`](crate::boundary::sketch_candidates)) for
    /// `intent`, best choice first.
    ///
    /// Implementations MAY reorder freely and MAY drop entries that aren't
    /// available in their deployment, but MUST NOT invent a candidate that
    /// wasn't in the input — an unknown [`SummaryKind`] has no
    /// [`SummaryParams`](asap_sketch::SummaryParams) sizing logic in
    /// [`boundary::bind_sketch`] and binding it will panic. Returning an
    /// empty `Vec` means "no candidate is acceptable"; `bind_sketch` treats
    /// that the same as `candidates` having been empty to begin with.
    fn rank_candidates(&self, intent: &AggIntent, candidates: &[SummaryKind]) -> Vec<SummaryKind>;
}

/// The default cost model: preserves [`sketch_candidates`]'s built-in static
/// order unchanged.
///
/// [`sketch_candidates`]: crate::boundary::sketch_candidates
pub struct DefaultCostModel;

impl CostModel for DefaultCostModel {
    fn rank_candidates(&self, _intent: &AggIntent, candidates: &[SummaryKind]) -> Vec<SummaryKind> {
        candidates.to_vec()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::boundary::sketch_candidates;
    use asap_ir::intent_algebra::agg_intent::default_cardinality;

    #[test]
    fn default_cost_model_preserves_static_order() {
        let intent = default_cardinality();
        let candidates = sketch_candidates(&intent);
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
        let candidates = sketch_candidates(&intent);
        let ranked = AlwaysPreferLast.rank_candidates(&intent, candidates);
        assert_eq!(ranked.first(), candidates.last());
    }
}

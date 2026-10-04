//! UnivMon certifies only its unit-update total, which the kernel reads
//! exactly (`calc_l1` returns the update count).
//!
//! Distinct count, L2 norm and entropy are deliberately left uncertified,
//! so Stage 3 rejects them unless a deployment supplies accuracy evidence:
//!
//! - Liu et al., "One Sketch to Rule Them All" (SIGCOMM 2016), rely on
//!   Braverman and Ostrovsky's recursive sketch. With O(log n) layers, each of
//!   which returns a (g, ε)-cover of its sampled substream (every g-heavy item
//!   with a (1 ± ε) frequency), the recursive G-sum is a (1 ± ε)
//!   approximation with probability 1 − δ. The layer sketch's size is only
//!   stated asymptotically (O(ε⁻² log(1/δ)) per CountSketch, times
//!   polylogarithmic factors). No constants are given that could be inverted
//!   into a concrete `(heap_size, sketch_rows, sketch_cols, layers)`.
//! - The kernel (`asap_sketchlib::UnivMon::calc_g_sum_heuristic`, behind
//!   `asap-executor`'s `UnivMonAccumulator`) keeps a fixed top-`heap_size`
//!   heap per layer and estimates its items with the layer's CountSketch.
//!   For distinct counts it also drops items below `L2 / sqrt(heap_size)`.
//!   Nothing bounds the probability that a layer's heap is such a cover, so
//!   the theorem's premise does not hold for these readouts. Even when every
//!   heap holds every item, a CountSketch estimate may be at most zero for a
//!   present item, so the distinct count is not certified in that case
//!   either.
//! - A per-layer CountSketch does give a sound F₂ (AMS) bound, but `calc_l2`
//!   reads the recursive sum, not that estimate.
//!
//! A sound guarantee needs either a readout with a proven bound (for
//! example, L2 from layer 0's CountSketch) or a per-layer cover guarantee for
//! the heap. Both are kernel changes.
use super::*;

pub(super) fn guarantee(query: &SketchStatistic) -> Option<ResultGuarantee> {
    matches!(query, SketchStatistic::PointCount { value: None, .. })
        .then(|| ResultGuarantee::exact("univmon_unit_update_total"))
}

/// One shape for every readout and requirement: no bound is inverted (see
/// the module docs), so every consumer of a key reads identical states.
pub(super) fn size_params() -> SketchParams {
    SketchParams::UnivMon {
        heap_size: 256,
        sketch_rows: 5,
        sketch_cols: 1024,
        layers: 16,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use asap_types::ir::scalar::ColumnRef;
    use asap_types::ir::schema::{GroupingStrategy, SketchKind};

    fn family() -> FieldDataType {
        FieldDataType::Sketch(
            SketchKind::new(SketchAlgorithm::UnivMon, size_params()),
            GroupingStrategy::default(),
        )
    }

    /// The exact total is certified; distinct count, L2 and entropy have no
    /// sound bound for the shipped kernel and stay uncertified.
    #[test]
    fn only_the_total_is_certified() {
        let total = SketchStatistic::PointCount {
            key: ColumnRef::SampleValue,
            value: None,
        };
        assert!(DefaultAccuracyModel
            .local_guarantee(&family(), &total)
            .is_some_and(|g| g.is_exact()));
        for query in [
            SketchStatistic::Cardinality,
            SketchStatistic::FrequencyL2,
            SketchStatistic::FrequencyEntropy,
        ] {
            assert!(
                DefaultAccuracyModel
                    .local_guarantee(&family(), &query)
                    .is_none(),
                "{query:?}"
            );
        }
    }
}

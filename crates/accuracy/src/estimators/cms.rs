//! Count-Min Sketch L1 frequency error and parameter sizing.
use super::*;

pub(super) fn guarantee(
    algorithm: &SketchAlgorithm,
    params: &SketchParams,
    query: &SketchStatistic,
) -> Option<AccuracyGuarantee> {
    let (SketchParams::Cms { width, depth } | SketchParams::CmsWithHeap { width, depth, .. }) =
        params
    else {
        return None;
    };
    Some(super::bounded_guarantee(
        algorithm,
        params,
        query,
        AccuracyMetric::ABSOLUTE_STREAM_L1,
        std::f64::consts::E / f64::from(*width),
        ProbabilityExpr::Constant {
            value: (-f64::from(*depth)).exp(),
        },
        "count_min_l1_markov_v1",
    ))
}

/// CMS: over-count ≤ ε·N with width `w = ⌈e/ε⌉` columns.
pub fn cms_width(eps: f64) -> u32 {
    saturating_ceil(std::f64::consts::E / eps, 2, 1 << 26)
}

/// CMS: failure probability ≤ δ with depth `d = ⌈ln(1/δ)⌉` rows.
/// δ = 0.01 → depth 5.
pub fn cms_depth(delta: f64) -> u32 {
    saturating_ceil((1.0 / delta).ln(), 1, 32)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_guarantee_inverts_frequency_sizing() {
        use crate::estimators::size_params;
        use asap_types::ir::schema::{GroupingStrategy, SketchKind};
        let c = asap_types::ir::operator::agg_intent::default_cardinality();
        let params = size_params(SketchAlgorithm::Cms, &c, 0.01, 0.001);
        let g = local_guarantee(
            &FieldDataType::Sketch(
                SketchKind::new(SketchAlgorithm::Cms, params),
                GroupingStrategy::default(),
            ),
            &SketchStatistic::PointCount {
                key: asap_types::ir::scalar::expr_ir::ColumnRef::SampleValue,
                value: None,
            },
        )
        .unwrap();
        assert_eq!(g.spec.accuracy_metric, AccuracyMetric::ABSOLUTE_STREAM_L1);
        assert!(satisfies(
            &g,
            &AccuracyTarget::max_error(0.01)
                .failure_probability(0.001)
                .accuracy_metric(AccuracyMetric::ABSOLUTE_STREAM_L1)
                .accuracy_scope(asap_types::ir::properties::AccuracyScope::PerValue)
        ));
    }

    #[test]
    fn heap_evaluation_retains_frequency_metric() {
        use asap_types::ir::schema::{GroupingStrategy, SketchKind};
        let cms_heap = SketchParams::CmsWithHeap {
            width: 272,
            depth: 5,
            heap_size: 10,
        };
        let topk_frequency = local_guarantee(
            &FieldDataType::Sketch(
                SketchKind::new(SketchAlgorithm::CmsWithHeap, cms_heap),
                GroupingStrategy::default(),
            ),
            &SketchStatistic::TopK { k: 10 },
        )
        .expect("heap sketch still provides per-key frequency intervals");
        assert_eq!(
            topk_frequency.spec.accuracy_metric,
            AccuracyMetric::ABSOLUTE_STREAM_L1
        );
    }

    /// A top-k heap holds `max(k, ⌈1/ε⌉)` entries over a CMS sized as for counts.
    #[test]
    fn topk_heap_capacity_respects_accuracy_and_output_count() {
        use crate::estimators::size_params;
        let intent = AggIntent::TopK {
            k: 25,
            accuracy: AccuracyTarget::max_error(0.01),
        };
        assert_eq!(
            size_params(SketchAlgorithm::CmsWithHeap, &intent, 0.01, 0.01),
            SketchParams::CmsWithHeap {
                width: 272, // ⌈e/0.01⌉
                depth: 5,
                heap_size: 100,
            }
        );
    }
}

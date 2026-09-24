//! Count-Min Sketch L1 frequency error and parameter sizing.
use super::*;

pub(super) fn guarantee(
    algorithm: &SketchAlgorithm,
    params: &SketchParams,
    query: &SketchQuery,
) -> Option<ResultGuarantee> {
    let (SketchParams::Cms { width, depth } | SketchParams::CmsWithHeap { width, depth, .. }) =
        params
    else {
        return None;
    };
    Some(super::bounded_guarantee(
        algorithm,
        params,
        query,
        ErrorMetric::Frequency,
        std::f64::consts::E / f64::from(*width),
        ProbabilityExpr::Constant {
            value: (-f64::from(*depth)).exp(),
        },
        "count_min_l1_markov_v1",
    ))
}

/// CMS: over-count ≤ ε·N with width `w = ⌈e/ε⌉` columns.
pub(crate) fn cms_width(eps: f64) -> u32 {
    saturating_ceil(std::f64::consts::E / eps, 2, 1 << 26)
}

/// CMS: failure probability ≤ δ with depth `d = ⌈ln(1/δ)⌉` rows.
/// δ = 0.01 → depth 5.
pub(crate) fn cms_depth(delta: f64) -> u32 {
    saturating_ceil((1.0 / delta).ln(), 1, 32)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_guarantee_inverts_frequency_sizing() {
        use crate::replacement::default_size_params;
        use asap_types::post_asap::{GroupingStrategy, SketchKind};
        let c = asap_types::pre_asap::agg_intent::default_cardinality();
        let params = default_size_params(SketchAlgorithm::Cms, &c, 0.01, 0.001);
        let g = DefaultAccuracyModel
            .local_guarantee(
                &SummaryFamilyType::Sketch(
                    SketchKind::new(SketchAlgorithm::Cms, params),
                    GroupingStrategy::default(),
                ),
                &SketchQuery::Cardinality,
            )
            .unwrap();
        assert_eq!(g.metric, ErrorMetric::Frequency);
        assert!(DefaultAccuracyModel.satisfies(
            &g,
            &AccuracyTarget::EpsilonDelta {
                epsilon: 0.01,
                delta: 0.001
            }
        ));
    }

    #[test]
    fn heap_readout_retains_frequency_metric() {
        use asap_types::post_asap::{GroupingStrategy, SketchKind};
        let cms_heap = SketchParams::CmsWithHeap {
            width: 272,
            depth: 5,
            heap_size: 10,
        };
        let topk_frequency = DefaultAccuracyModel
            .local_guarantee(
                &SummaryFamilyType::Sketch(
                    SketchKind::new(SketchAlgorithm::CmsWithHeap, cms_heap),
                    GroupingStrategy::default(),
                ),
                &SketchQuery::TopK { k: 10 },
            )
            .expect("heap sketch still provides per-key frequency intervals");
        assert_eq!(topk_frequency.metric, ErrorMetric::Frequency);
    }
}

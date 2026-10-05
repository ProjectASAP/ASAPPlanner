//! Count-Min Sketch L1 frequency error and parameter sizing.
use super::*;

pub(super) fn guarantee(
    algorithm: &SketchAlgorithm,
    params: &SketchParams,
    query: &SketchStatistic,
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
        use crate::pass1::realization::default_size_params;
        use asap_types::ir::schema::{GroupingStrategy, SketchKind};
        let c = asap_types::ir::operator::agg_intent::default_cardinality();
        let params = default_size_params(SketchAlgorithm::Cms, &c, 0.01, 0.001);
        let g = DefaultAccuracyModel
            .local_guarantee(
                &FieldDataType::Sketch(
                    SketchKind::new(SketchAlgorithm::Cms, params),
                    GroupingStrategy::default(),
                ),
                &SketchStatistic::Cardinality,
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

    /// A shared HydraCms grid adds its collision term to the inner sketch's
    /// error: sized like one per-group sketch for ε it misses ε, and sized
    /// for ε/2 and δ/2 (Pass 1's split) it meets it.
    #[test]
    fn hydra_guarantee_adds_the_shared_grid_term() {
        use crate::pass1::realization::default_size_params;
        use asap_types::ir::schema::{
            default_hydra_params, GroupingStrategy, HydraKind, SketchKind,
        };
        let count = AggIntent::Count {
            accuracy: AccuracyTarget::EpsilonDelta {
                epsilon: 0.01,
                delta: 0.01,
            },
        };
        let target = AccuracyTarget::EpsilonDelta {
            epsilon: 0.01,
            delta: 0.01,
        };
        let group_count = SketchStatistic::PointCount {
            key: asap_types::ir::scalar::ColumnRef::SampleValue,
            value: None,
        };
        let guarantee = |epsilon: f64, delta: f64, hydra: bool| {
            let params = default_size_params(SketchAlgorithm::Cms, &count, epsilon, delta);
            let grouping = match hydra {
                true => GroupingStrategy::SharedMultiSubpopulation {
                    kind: HydraKind::HydraCms,
                    params: default_hydra_params(HydraKind::HydraCms, &params).unwrap(),
                },
                false => GroupingStrategy::default(),
            };
            DefaultAccuracyModel
                .local_guarantee(
                    &FieldDataType::Sketch(SketchKind::new(SketchAlgorithm::Cms, params), grouping),
                    &group_count,
                )
                .unwrap()
        };
        assert!(DefaultAccuracyModel.satisfies(&guarantee(0.01, 0.01, false), &target));
        assert!(!DefaultAccuracyModel.satisfies(&guarantee(0.01, 0.01, true), &target));
        assert!(DefaultAccuracyModel.satisfies(&guarantee(0.005, 0.005, true), &target));
    }

    #[test]
    fn heap_evaluation_retains_frequency_metric() {
        use asap_types::ir::schema::{GroupingStrategy, SketchKind};
        let cms_heap = SketchParams::CmsWithHeap {
            width: 272,
            depth: 5,
            heap_size: 10,
        };
        let topk_frequency = DefaultAccuracyModel
            .local_guarantee(
                &FieldDataType::Sketch(
                    SketchKind::new(SketchAlgorithm::CmsWithHeap, cms_heap),
                    GroupingStrategy::default(),
                ),
                &SketchStatistic::TopK { k: 10 },
            )
            .expect("heap sketch still provides per-key frequency intervals");
        assert_eq!(topk_frequency.metric, ErrorMetric::Frequency);
    }
}

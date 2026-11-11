//! Count-Min Sketch L1 frequency error.
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_guarantee_inverts_frequency_sizing() {
        use crate::guarantee::sizing_oracle::size_params;
        let c = asap_types::ir::operator::agg_intent::default_cardinality();
        let params = size_params(SketchAlgorithm::Cms, &c, 0.01, 0.001);
        let g = sketch_guarantee(
            &SketchAlgorithm::Cms,
            &params,
            &SketchStatistic::PointCount {
                key: asap_types::ir::scalar::expr_ir::ColumnRef::SampleValue,
                value: None,
            },
        )
        .unwrap();
        assert_eq!(g.spec.accuracy_metric, AccuracyMetric::ABSOLUTE_STREAM_L1);
        assert!(meets(&g, 0.01, Some(0.001)));
    }

    #[test]
    fn heap_evaluation_retains_frequency_metric() {
        let cms_heap = SketchParams::CmsWithHeap {
            width: 272,
            depth: 5,
            heap_size: 10,
        };
        let topk_frequency = sketch_guarantee(
            &SketchAlgorithm::CmsWithHeap,
            &cms_heap,
            &SketchStatistic::TopK { k: 10 },
        )
        .expect("heap sketch still provides per-key frequency intervals");
        assert_eq!(
            topk_frequency.spec.accuracy_metric,
            AccuracyMetric::ABSOLUTE_STREAM_L1
        );
    }
}

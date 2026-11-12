//! Count-Min Sketch L1 frequency error.
use super::*;

pub(super) fn guarantee(params: &SketchParams) -> Option<AccuracyGuarantee> {
    let (SketchParams::Cms { width, depth } | SketchParams::CmsWithHeap { width, depth, .. }) =
        params
    else {
        return None;
    };
    Some(super::bounded_guarantee(
        AccuracyMetric::ABSOLUTE_STREAM_L1,
        std::f64::consts::E / f64::from(*width),
        ProbabilityExpr::Constant {
            value: (-f64::from(*depth)).exp(),
        },
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn heap_evaluation_retains_frequency_metric() {
        let cms_heap = SketchParams::CmsWithHeap {
            width: 272,
            depth: 5,
            heap_size: 10,
        };
        let topk_frequency =
            guarantee_of_parameters(&cms_heap, &SketchStatistic::TopK { k: 10 }, 1.0)
                .expect("heap sketch still provides per-key frequency intervals");
        assert_eq!(
            topk_frequency.spec.accuracy_metric,
            AccuracyMetric::ABSOLUTE_STREAM_L1
        );
    }
}

//! CountSketch L2 frequency error and odd-depth median concentration.
use super::*;

pub(super) fn guarantee(
    algorithm: &SketchAlgorithm,
    params: &SketchParams,
    query: &SketchQuery,
) -> Option<ResultGuarantee> {
    let (SketchParams::CountSketch { width, depth }
    | SketchParams::CountSketchWithHeap { width, depth, .. }) = params
    else {
        return None;
    };
    Some(super::bounded_guarantee(
        algorithm,
        params,
        query,
        ErrorMetric::L2Frequency,
        (3.0 / f64::from(*width)).sqrt(),
        ProbabilityExpr::Constant {
            value: count_sketch_failure_probability(*depth)?,
        },
        "count_sketch_l2_median_hoeffding_v1",
    ))
}
fn count_sketch_failure_probability(depth: u32) -> Option<f64> {
    if depth == 0 || depth.is_multiple_of(2) {
        return None;
    }
    Some((-f64::from(depth) / 18.0).exp())
}

/// CountSketch `L2` point-query width: ε = sqrt(3/w).
pub(crate) fn count_sketch_width(eps: f64) -> u32 {
    saturating_ceil(3.0 / (eps * eps), 2, 1 << 26)
}

/// Positive odd depth satisfying Hoeffding's median failure bound
/// `exp(-depth/18) <= delta` for per-row failure at most 1/3.
pub(crate) fn count_sketch_depth(delta: f64) -> u32 {
    if !(delta.is_finite() && delta > 0.0 && delta < 1.0) {
        return 255;
    }
    let depth = saturating_ceil(18.0 * (1.0 / delta).ln(), 1, 255);
    if depth.is_multiple_of(2) {
        (depth + 1).min(255)
    } else {
        depth
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn count_sketch_uses_an_l2_guarantee() {
        use crate::replacement::default_size_params;
        use asap_types::post_asap::{GroupingStrategy, SketchKind};
        use asap_types::pre_asap::agg_intent::default_cardinality;
        let intent = default_cardinality();
        let count_sketch = default_size_params(SketchAlgorithm::CountSketch, &intent, 0.01, 0.01);
        let guarantee = DefaultAccuracyModel
            .local_guarantee(
                &SummaryFamilyType::Sketch(
                    SketchKind::new(SketchAlgorithm::CountSketch, count_sketch),
                    GroupingStrategy::default(),
                ),
                &SketchQuery::PointCount {
                    key: asap_types::pre_asap::expr_ir::ColumnRef::SampleValue,
                    value: None,
                },
            )
            .expect("CountSketch has a parameter-derived L2 guarantee");
        assert_eq!(guarantee.metric, ErrorMetric::L2Frequency);
        assert!(DefaultAccuracyModel.satisfies(
            &guarantee,
            &AccuracyTarget::EpsilonDelta {
                epsilon: 0.01,
                delta: 0.01,
            }
        ));
    }
}

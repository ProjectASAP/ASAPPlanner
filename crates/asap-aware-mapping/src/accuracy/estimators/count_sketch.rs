//! CountSketch L2 frequency error and odd-depth median concentration: the
//! median fails only when a majority of rows fail, an exact binomial tail.
use super::*;

pub(super) fn guarantee(
    algorithm: &SketchAlgorithm,
    params: &SketchParams,
    query: &SketchStatistic,
) -> Option<AccuracyGuarantee> {
    let (SketchParams::CountSketch { width, depth }
    | SketchParams::CountSketchWithHeap { width, depth, .. }) = params
    else {
        return None;
    };
    Some(super::bounded_guarantee(
        algorithm,
        params,
        query,
        ErrorMetric::ABSOLUTE_STREAM_L2,
        (3.0 / f64::from(*width)).sqrt(),
        ProbabilityExpr::Constant {
            value: count_sketch_failure_probability(*width, *depth)?,
        },
        "count_sketch_l2_median_binomial_v1",
    ))
}
/// One row misses a point query by more than `ε ‖f‖₂ = √(3/w) ‖f‖₂` with
/// probability at most `q/ε² = q w/3` (Chebyshev, `Var ≤ q F2`), with `q`
/// the column collision probability ([`super::column_collision`]): `1/3`
/// for a power-of-two width and `3/8` otherwise.
fn row_failure_probability(width: u32) -> f64 {
    super::column_collision(width) * f64::from(width.max(1)) / 3.0
}

/// The median of an odd `depth` of independent rows misses only when at
/// least `(depth + 1)/2` rows miss: `P(Binomial(depth, p) ≥ (depth+1)/2)`.
fn count_sketch_failure_probability(width: u32, depth: u32) -> Option<f64> {
    if depth == 0 || depth.is_multiple_of(2) {
        return None;
    }
    Some(super::binomial_upper_tail(
        depth,
        row_failure_probability(width),
        depth.div_ceil(2),
    ))
}

/// CountSketch `L2` point-query width: ε = sqrt(3/w).
pub(crate) fn count_sketch_width(eps: f64) -> u32 {
    saturating_ceil(3.0 / (eps * eps), 2, 1 << 26)
}

/// The smallest positive odd depth whose binomial median failure
/// probability at `width` is at most `delta`, capped at 255.
pub(crate) fn count_sketch_depth(width: u32, delta: f64) -> u32 {
    if !(delta.is_finite() && delta > 0.0 && delta < 1.0) {
        return 255;
    }
    (1..=255)
        .step_by(2)
        .find(|&depth| {
            count_sketch_failure_probability(width, depth).is_some_and(|tail| tail <= delta)
        })
        .unwrap_or(255)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn count_sketch_uses_an_l2_guarantee() {
        use crate::realization::default_size_params;
        use asap_types::post_asap::{GroupingStrategy, SketchKind};
        use asap_types::pre_asap::agg_intent::default_cardinality;
        let intent = default_cardinality();
        let count_sketch = default_size_params(SketchAlgorithm::CountSketch, &intent, 0.01, 0.01);
        let guarantee = DefaultAccuracyModel
            .local_guarantee(
                &FieldDataType::Sketch(
                    SketchKind::new(SketchAlgorithm::CountSketch, count_sketch),
                    GroupingStrategy::default(),
                ),
                &SketchStatistic::PointCount {
                    key: asap_types::pre_asap::expr_ir::ColumnRef::SampleValue,
                    value: None,
                },
            )
            .expect("CountSketch has a parameter-derived L2 guarantee");
        assert_eq!(guarantee.spec.metric, ErrorMetric::ABSOLUTE_STREAM_L2);
        assert!(DefaultAccuracyModel.satisfies(
            &guarantee,
            &AccuracyTarget::epsilon_delta(0.01, 0.01)
                .with_metric(ErrorMetric::ABSOLUTE_STREAM_L2)
                .with_scope(asap_types::types::AccuracyScope::PerValue)
        ));
    }

    /// The depth is the smallest odd one whose binomial tail meets δ. Each
    /// is at most Hoeffding's `ln(1/δ) / (2 (1/2 − p)²)` at the row's own
    /// failure probability: `18 ln(1/δ)` for a power-of-two width (`p = 1/3`)
    /// and `32 ln(1/δ)` otherwise (`p = 3/8`).
    #[test]
    fn count_sketch_depth_is_the_smallest_meeting_the_binomial_tail() {
        for width in [1024, 3000, 30_000] {
            let hoeffding = if width == 1024 { 18.0 } else { 32.0 };
            for delta in [0.1, 0.01, 0.001] {
                let depth = count_sketch_depth(width, delta);
                assert!(depth % 2 == 1);
                assert!(count_sketch_failure_probability(width, depth).unwrap() <= delta);
                if depth > 1 {
                    assert!(count_sketch_failure_probability(width, depth - 2).unwrap() > delta);
                }
                assert!(
                    f64::from(depth) <= hoeffding * (1.0 / delta).ln(),
                    "{width} {delta}"
                );
            }
        }
        // A power-of-two width fails a row with probability 1/3, any other
        // with 3/8: P(Bin(3, 1/3) ≥ 2) = 7/27.
        assert!((count_sketch_failure_probability(1024, 3).unwrap() - 7.0 / 27.0).abs() < 1e-9);
        let general = 3.0 * (3.0f64 / 8.0).powi(2) * (5.0 / 8.0) + (3.0f64 / 8.0).powi(3);
        assert!((count_sketch_failure_probability(1000, 3).unwrap() - general).abs() < 1e-9);
        assert_eq!(count_sketch_failure_probability(1000, 4), None);
    }
}

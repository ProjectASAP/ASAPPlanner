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
        AccuracyMetric::ABSOLUTE_STREAM_L2,
        (3.0 / f64::from(*width)).sqrt(),
        ProbabilityExpr::Constant {
            value: count_sketch_failure_probability(*width, *depth)?,
        },
        "count_sketch_l2_median_binomial_v1",
    ))
}
/// One row misses a point query by more than `ε ‖f‖₂ = √(3/w) ‖f‖₂` with
/// probability at most `q/ε² = q w/3` (Chebyshev, `Var ≤ q F2`), with `q`
/// the column collision probability ([`column_collision`]): `1/3` to
/// within `w/(3 · 2^32)`.
fn row_failure_probability(width: u32) -> f64 {
    column_collision(width) * f64::from(width.max(1)) / 3.0
}

/// `P(c_i = c_j)` for two keys in a row of `cols` columns, at most
/// `1/cols + 2^-32`. The executor builds `CountSketch<_, RegularPath>` and
/// `CountSketchWithHeap<_, RegularPath>`, whose rows take a key's column as
/// `(hash & (2^32 − 1)) % cols`: each column holds `⌊2^32/cols⌋` or
/// `⌈2^32/cols⌉` of the `2^32` hash values, so a key lands in any one with
/// probability at most `1/cols + 2^-32`, and two keys collide no more
/// often than that.
fn column_collision(cols: u32) -> f64 {
    1.0 / f64::from(cols.max(1)) + 2f64.powi(-32)
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

    /// The executor's `CountSketch<_, RegularPath>` takes a column as the
    /// low 32 hash bits modulo the width, so a row fails with probability
    /// `1/3` (to within `2^-32`) at any width, not `3/8` off powers of two:
    /// ε = 0.05 (width 1200) needs depth 47 at δ = 0.01, 81 at 1e-3 and
    /// 117 at 1e-4, not 85, 149 and 215.
    #[test]
    fn depth_follows_the_libraries_modulo_column_hash() {
        let width = count_sketch_width(0.05);
        assert_eq!(width, 1200);
        assert!((row_failure_probability(width) - 1.0 / 3.0).abs() < 1e-6);
        assert_eq!(
            [0.01, 1e-3, 1e-4].map(|delta| count_sketch_depth(width, delta)),
            [47, 81, 117]
        );
    }

    /// Measured on the library's own `CountSketch<_, RegularPath>`: two
    /// keys share a column of a 1536-wide row about `1/1536` of the time.
    /// A `⌈log2 w⌉`-bit hash slice modulo `w` would give `1.125/1536`, 6
    /// standard deviations above; the bound used here is `1/w + 2^-32`.
    #[test]
    fn library_rows_collide_at_one_over_the_width() {
        use asap_sketchlib::{CountSketch, DataInput, RegularPath, Vector2D};
        let width = 1536;
        let mut row = CountSketch::<Vector2D<f64>, RegularPath>::with_dimensions(1, width);
        row.update(&DataInput::U64(u64::MAX));
        let probes = 4_000_000u64;
        let collisions = (0..probes)
            .filter(|&key| row.estimate(&DataInput::U64(key)) != 0.0)
            .count() as f64;
        let expected = probes as f64 * column_collision(width as u32);
        let sigma = expected.sqrt();
        assert!(
            (collisions - expected).abs() < 4.0 * sigma,
            "{collisions} vs {expected}"
        );
    }

    /// Seeded end to end: the sized sketch (ε = 0.05, δ = 0.01, width 1200,
    /// depth 47) answers each key of a skewed stream within `ε ‖f‖₂`, at
    /// most a δ share of keys missing.
    #[test]
    fn sized_sketch_meets_its_guarantee_on_a_seeded_stream() {
        use asap_sketchlib::{CountSketch, DataInput, RegularPath, Vector2D};
        let (eps, delta) = (0.05, 0.01);
        let width = count_sketch_width(eps);
        let depth = count_sketch_depth(width, delta);
        let mut sketch = CountSketch::<Vector2D<f64>, RegularPath>::with_dimensions(
            depth as usize,
            width as usize,
        );
        // 20 000 keys, key i weighted 1e6 / (i + 1) (Zipf, s = 1), in a
        // seeded order.
        let keys = 20_000u64;
        let weight = |key: u64| (1e6 / (key + 1) as f64).round();
        let mut state = 0x9E37_79B9_7F4A_7C15u64;
        let mut order: Vec<u64> = (0..keys).collect();
        for i in (1..order.len()).rev() {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            order.swap(i, (state % (i as u64 + 1)) as usize);
        }
        for &key in &order {
            sketch.update_weighted(&DataInput::U64(key), weight(key));
        }
        let l2 = (0..keys).map(|k| weight(k).powi(2)).sum::<f64>().sqrt();
        let bound = (3.0 / f64::from(width)).sqrt() * l2;
        let misses = (0..keys)
            .filter(|&k| (sketch.estimate(&DataInput::U64(k)) - weight(k)).abs() > bound)
            .count();
        assert!(misses as f64 <= delta * keys as f64, "{misses} misses");
    }

    #[test]
    fn count_sketch_uses_an_l2_guarantee() {
        use crate::pass1::realization::default_size_params;
        use asap_types::ir::operator::agg_intent::default_cardinality;
        use asap_types::ir::schema::{GroupingStrategy, SketchKind};
        let intent = default_cardinality();
        let count_sketch = default_size_params(SketchAlgorithm::CountSketch, &intent, 0.01, 0.01);
        let guarantee = local_guarantee(
            &FieldDataType::Sketch(
                SketchKind::new(SketchAlgorithm::CountSketch, count_sketch),
                GroupingStrategy::default(),
            ),
            &SketchStatistic::PointCount {
                key: asap_types::ir::scalar::expr_ir::ColumnRef::SampleValue,
                value: Some("1".into()),
            },
        )
        .expect("CountSketch has a parameter-derived L2 guarantee");
        assert_eq!(
            guarantee.spec.accuracy_metric,
            AccuracyMetric::ABSOLUTE_STREAM_L2
        );
        assert!(satisfies(
            &guarantee,
            &AccuracyTarget::max_error(0.01)
                .failure_probability(0.01)
                .accuracy_metric(AccuracyMetric::ABSOLUTE_STREAM_L2)
                .accuracy_scope(asap_types::ir::properties::AccuracyScope::PerValue)
        ));
    }

    /// The depth is the smallest odd one whose binomial tail meets δ. Each
    /// is at most Hoeffding's `ln(1/δ) / (2 (1/2 − p)²)` at the row's
    /// failure probability `p ≈ 1/3`: `18 ln(1/δ)` at any width.
    #[test]
    fn count_sketch_depth_is_the_smallest_meeting_the_binomial_tail() {
        for width in [1024, 3000, 30_000] {
            let hoeffding = 18.0 * (1.0 + 1e-4);
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
        // Any width fails a row with probability 1/3 (to within 2^-32 w/3):
        // P(Bin(3, 1/3) ≥ 2) = 7/27.
        for width in [1024, 1000] {
            let tail = count_sketch_failure_probability(width, 3).unwrap();
            assert!((tail - 7.0 / 27.0).abs() < 1e-6, "{width}");
        }
        assert_eq!(count_sketch_failure_probability(1000, 4), None);
    }
}

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
pub(super) fn row_failure_probability(width: u32) -> f64 {
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
pub(super) fn count_sketch_failure_probability(width: u32, depth: u32) -> Option<f64> {
    if depth == 0 || depth.is_multiple_of(2) {
        return None;
    }
    Some(super::binomial_upper_tail(
        depth,
        row_failure_probability(width),
        depth.div_ceil(2),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

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
        use crate::guarantee::sizing_oracle::{count_sketch_depth, count_sketch_width};
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
        use crate::guarantee::sizing_oracle::size_params;
        use asap_types::ir::operator::agg_intent::default_cardinality;
        let intent = default_cardinality();
        let count_sketch = size_params(SketchAlgorithm::CountSketch, &intent, 0.01, 0.01);
        let guarantee = sketch_guarantee(
            &SketchAlgorithm::CountSketch,
            &count_sketch,
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
        assert!(meets(&guarantee, 0.01, Some(0.01)));
    }
}

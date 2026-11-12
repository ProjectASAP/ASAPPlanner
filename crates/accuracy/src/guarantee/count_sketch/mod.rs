//! CountSketch L2 frequency error and odd-depth median concentration: the
//! median fails only when a majority of rows fail, an exact binomial tail.
use super::*;

pub(super) fn guarantee(params: &SketchParams) -> Option<AccuracyGuarantee> {
    let (SketchParams::CountSketch { width, depth }
    | SketchParams::CountSketchWithHeap { width, depth, .. }) = params
    else {
        return None;
    };
    Some(super::bounded_guarantee(
        AccuracyMetric::ABSOLUTE_STREAM_L2,
        (3.0 / f64::from(*width)).sqrt(),
        ProbabilityExpr::Constant {
            value: count_sketch_failure_probability(*width, *depth)?,
        },
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

    #[test]
    fn count_sketch_uses_an_l2_guarantee() {
        // ε = √(3/30 000) = 0.01; 47 rows fail together with probability ≤ 0.01.
        let count_sketch = SketchParams::CountSketch {
            width: 30_000,
            depth: 47,
        };
        let guarantee = guarantee_of_parameters(
            &count_sketch,
            &SketchStatistic::PointCount {
                key: asap_types::ir::scalar::expr_ir::ColumnRef::SampleValue,
                value: Some("1".into()),
            },
            1.0,
        )
        .expect("CountSketch has a parameter-derived L2 guarantee");
        assert_eq!(
            guarantee.spec.accuracy_metric,
            AccuracyMetric::ABSOLUTE_STREAM_L2
        );
        assert!(guarantee
            .spec
            .error
            .evaluate()
            .is_some_and(|error| within_limit(error, 0.01)));
        assert!(guarantee
            .spec
            .failure_probability
            .evaluate()
            .is_some_and(|failure_probability| within_limit(failure_probability, 0.01)));
    }
}

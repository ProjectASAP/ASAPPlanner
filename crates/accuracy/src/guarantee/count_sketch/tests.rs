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

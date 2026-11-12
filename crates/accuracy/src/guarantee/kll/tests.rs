use super::*;

/// A hand-built `k` above the library's cap gives no guarantee.
#[test]
fn a_k_above_the_library_cap_gives_no_guarantee() {
    let g = |k| guarantee(&SketchParams::Kll { k }, KLL_CALIBRATED_FAILURE_PROBABILITY);
    assert!(g(KLL_MAX_K).is_some());
    assert!(g(KLL_MAX_K + 1).is_none());
}

/// Below δ = 0.01 the error scales by the Gaussian quantile ratio
/// `z(1 − δ/2) / z(1 − 0.01/2)`.
#[test]
fn delta_scale_is_the_gaussian_quantile_ratio() {
    assert_eq!(delta_scale(0.01), Some(1.0));
    assert_eq!(delta_scale(0.5), Some(1.0));
    let ratio = delta_scale(0.001).unwrap();
    assert!((ratio - 1.277_463_039_557_138_7).abs() < 1e-8, "{ratio}");
    assert!((normal_quantile(0.975) - 1.959_963_984_540_054).abs() < 1e-8);
    // Far in the tail, `1 − δ/2` rounds to 1 (or near it) in f64, so the
    // scale must be computed from the lower tail, `−z(δ/2)`.
    for (delta, expected) in [
        (1e-16, 3.224_121_029_197_093_4),
        (1e-15, 3.116_223_140_825_122_4),
    ] {
        let scale = delta_scale(delta).unwrap();
        assert!((scale - expected).abs() < 1e-8, "{delta}: {scale}");
    }
}

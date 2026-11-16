use super::*;

/// The failure probability at a relative error, over a bound, at a
/// precision; 1 outside the contract.
fn classic_failure_probability(precision: u8, relative_error: f64, max_distinct: u64) -> f64 {
    u32::try_from(max_distinct)
        .ok()
        .and_then(|n| ClassicHllConfidence::new(n, relative_error))
        .and_then(|model| model.failure_probability(precision))
        .unwrap_or(1.0)
}

/// The largest distinct bound certified at δ = 0.01 for a target ε, by
/// precision; a bound outside the contract certifies nothing.
#[test]
fn largest_certified_bounds_by_precision() {
    for (eps, precision, largest) in [
        (0.05, 15, 1176),
        (0.05, 18, 9636),
        (0.02, 17, 1842),
        (0.02, 18, 3791),
        (0.01, 18, 72),
    ] {
        assert!(
            classic_failure_probability(precision, eps, largest) <= 0.01,
            "eps={eps} p={precision} n={largest}"
        );
        assert!(
            classic_failure_probability(precision, eps, largest + 1) > 0.01,
            "eps={eps} p={precision} n={}",
            largest + 1
        );
    }
    for too_large in [u64::from(MAX_CONTRACT_DISTINCT) + 1, u64::MAX] {
        assert_eq!(classic_failure_probability(18, 0.05, too_large), 1.0);
    }
}

/// Exact occupancy probabilities independently check both tails for every
/// population up to the bound.
#[test]
fn probability_bound_dominates_exact_occupancy_distribution() {
    for precision in 4..=10 {
        let m = 1usize << precision;
        let max_n = 64.min(m / 2);
        for eps in [0.05, 0.2, 0.6] {
            let model = ClassicHllConfidence::new(max_n as u32, eps).unwrap();
            let Some(bound) = model.failure_probability(precision) else {
                continue;
            };
            let mut occupancy = vec![0.0; max_n + 1];
            occupancy[0] = 1.0;
            for n in 1..=max_n {
                let mut next = vec![0.0; max_n + 1];
                for k in 0..n {
                    next[k] += occupancy[k] * k as f64 / m as f64;
                    next[k + 1] += occupancy[k] * (m - k) as f64 / m as f64;
                }
                occupancy = next;
                let actual: f64 = occupancy
                    .iter()
                    .enumerate()
                    .filter_map(|(k, &prob)| {
                        let estimate = -(m as f64) * (-(k as f64) / (m as f64)).ln_1p();
                        ((estimate - n as f64).abs() > eps * n as f64).then_some(prob)
                    })
                    .sum();
                assert!(
                    actual <= bound + 1e-12,
                    "p={precision} n={n} eps={eps}: {actual}>{bound}"
                );
            }
        }
    }
}

/// `COUNT(DISTINCT user_id)` within relative 0.05 at δ = 0.01 from classic
/// HLL at precision 18 is certified over a source of at most 9,636
/// distinct users per evaluation; over 1,000,000 (outside the contract) and
/// without a bound only the relative standard error is stated, whose
/// failure probability is unknown.
#[test]
fn a_distinct_count_is_certified_over_the_source_s_distinct_bound() {
    let precision_18 = SketchParams::Hll { precision: 18 };
    let at = |bound| guarantee(&precision_18, 0.01, bound).unwrap();
    let certified = at(Some(9_636));
    assert_eq!(certified.spec.accuracy_metric, AccuracyMetric::Relative);
    let error = certified.spec.error.evaluate().unwrap();
    assert!(error <= 0.05 && error > 0.04, "{error}");
    assert!(certified.spec.failure_probability.evaluate().unwrap() <= 0.01);
    for uncertified in [Some(1_000_000), None] {
        let guarantee = at(uncertified);
        assert_eq!(guarantee.spec.accuracy_metric, AccuracyMetric::Relative);
        assert_eq!(guarantee.spec.failure_probability.evaluate(), None);
    }
}

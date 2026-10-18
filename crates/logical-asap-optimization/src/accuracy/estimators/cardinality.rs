//! KMV's provable distinct-count bound and Theta's Chebyshev calibration.
//!
//! # KMV (#694)
//!
//! `asap_sketchlib`'s KMV keeps the `k` smallest distinct 64-bit hashes of
//! its items and estimates (`src/sketches/kmv.rs`):
//! - the number of hashes it keeps, exactly, while it keeps fewer than `k`
//!   (fewer than `k` distinct items);
//! - `(k-1)/U`, where `U = (h+1)/2^64` is the `k`-th smallest hash `h`
//!   mapped into (0, 1], once it keeps `k`.
//!
//! A merge keeps the `k` smallest distinct hashes of both states, so a
//! merged state is the one a single build over the union keeps.
//!
//! Model: the hashes of the `N` distinct items are independent, uniform on
//! (0, 1] and distinct (the 64-bit range's `2^-64` granularity and hash
//! collisions are not modelled). For `N < k` the estimate is `N`. For
//! `N ≥ k`, let `X(u) ~ Binomial(N, u)` count the hashes below `u`:
//! - overestimate: `(k-1)/U > (1+ε)N` iff `U < u = (k-1)/((1+ε)N)`, iff
//!   `X(u) ≥ k`. With `μ = E[X(u)] = (k-1)/(1+ε)`, `k > (1+ε)μ`, so by
//!   Chernoff `P ≤ exp(-ε²μ/(2+ε)) = exp(-ε²(k-1)/((1+ε)(2+ε)))`;
//! - underestimate: `(k-1)/U < (1-ε)N` iff `U > u' = (k-1)/((1-ε)N)`, iff
//!   `X(u') ≤ k-1 = (1-ε)μ'` with `μ' = (k-1)/(1-ε)`, so
//!   `P ≤ exp(-ε²μ'/2) = exp(-ε²(k-1)/(2(1-ε)))` (and `0` for `ε ≥ 1`).
//!
//! Neither bound depends on `N`, so no distinct bound is needed: the
//! failure probability ([`kmv_failure_probability`]) is their sum, per
//! output row (each group's estimate). Sizing ([`kmv_k`]) takes the
//! smallest `k` with that sum at most `δ`, about
//! `1 + (1+ε)(2+ε)·ln(2/δ)/ε²` (below `3·ln(2/δ)/ε² + 1` for `ε ≤ 1`).
use super::*;

/// The smallest `k` `asap_sketchlib`'s KMV estimate `(k-1)/U` is defined for.
const KMV_MIN_K: u32 = 2;
/// The largest `k` sizing returns.
const KMV_MAX_K: u32 = 1 << 26;

pub(super) fn guarantee(
    algorithm: &SketchAlgorithm,
    params: &SketchParams,
    query: &SketchStatistic,
) -> Option<AccuracyGuarantee> {
    match params {
        SketchParams::Kmv { k, relative_error } => {
            // Not sized for a target: its relative standard error.
            let eps = relative_error
                .unwrap_or_else(|| 1.0 / f64::from(k.saturating_sub(2).max(1)).sqrt());
            Some(super::bounded_guarantee(
                algorithm,
                params,
                query,
                AccuracyMetric::Relative,
                eps,
                ProbabilityExpr::Constant {
                    value: kmv_failure_probability(*k, eps),
                },
                "kmv_exact_below_k_chernoff_v1",
            ))
        }
        SketchParams::Theta { k } => Some(super::bounded_guarantee(
            algorithm,
            params,
            query,
            AccuracyMetric::Relative,
            10.0 / f64::from(k.saturating_sub(2).max(1)).sqrt(),
            ProbabilityExpr::Constant { value: 0.01 },
            "theta_variance_chebyshev_99_v1",
        )),
        _ => None,
    }
}

/// The probability that `asap_sketchlib`'s KMV with `k` misses relative
/// error `eps` on any number of distinct items (the module's bound): `1`
/// when it certifies nothing.
pub(crate) fn kmv_failure_probability(k: u32, eps: f64) -> f64 {
    if k < KMV_MIN_K || !(eps.is_finite() && eps > 0.0) {
        return 1.0;
    }
    let ranks = f64::from(k - 1);
    let over = (-eps * eps * ranks / ((1.0 + eps) * (2.0 + eps))).exp();
    let under = if eps < 1.0 {
        (-eps * eps * ranks / (2.0 * (1.0 - eps))).exp()
    } else {
        0.0
    };
    // Rounded up, so rounding never certifies a smaller probability.
    ((over + under) * (1.0 + 1e-12)).min(1.0)
}

/// The smallest `k` whose KMV failure probability at relative error `eps`
/// is at most `delta` ([`kmv_failure_probability`]), up to `2^26`; a
/// degenerate `eps` or `delta` saturates to `2^26`.
pub(crate) fn kmv_k(eps: f64, delta: f64) -> u32 {
    if !(eps.is_finite() && eps > 0.0 && delta.is_finite() && delta > 0.0) {
        return KMV_MAX_K;
    }
    // The overestimate term alone at most `δ` bounds `k` below; both terms
    // at most `δ/2` bound it above.
    let k_at = |ln_inverse: f64| {
        saturating_ceil(
            1.0 + (1.0 + eps) * (2.0 + eps) * ln_inverse / (eps * eps),
            KMV_MIN_K,
            KMV_MAX_K,
        )
    };
    let (mut lo, mut hi) = (k_at((1.0 / delta).ln()), k_at((2.0 / delta).ln()));
    if kmv_failure_probability(hi, eps) > delta {
        return KMV_MAX_K;
    }
    while lo < hi {
        let mid = lo + (hi - lo) / 2;
        if kmv_failure_probability(mid, eps) <= delta {
            hi = mid;
        } else {
            lo = mid + 1;
        }
    }
    hi
}

/// 99%-confidence Theta relative bound via Chebyshev, using
/// `RSE <= 1/sqrt(k-2)` and a ten-standard-deviation interval.
pub(crate) fn theta_k_99(eps: f64) -> u32 {
    saturating_ceil(100.0 / (eps * eps) + 2.0, 16, 1 << 26)
}

#[cfg(test)]
mod tests {
    use super::*;
    use asap_types::ir::operator::agg_intent::default_cardinality;

    fn error_and_delta(params: &SketchParams) -> (f64, f64) {
        let g = guarantee(&SketchAlgorithm::Kmv, params, &SketchStatistic::Cardinality).unwrap();
        (
            g.spec.error.evaluate().unwrap(),
            g.spec.failure_probability.evaluate().unwrap(),
        )
    }

    /// KMV sized for (ε, δ) certifies both (#700): its failure probability
    /// is the requested δ's, not a fixed 0.01.
    #[test]
    fn kmv_sized_for_a_failure_probability_meets_it() {
        for (eps, delta) in [(0.01, 0.001), (0.05, 0.01), (0.02, 1e-6)] {
            let params =
                super::super::size_params(SketchAlgorithm::Kmv, &default_cardinality(), eps, delta);
            let (error, failure_probability) = error_and_delta(&params);
            assert!(error <= eps, "{params:?}: {error}");
            assert!(
                failure_probability <= delta,
                "{params:?}: {failure_probability}"
            );
        }
    }

    /// KMV's sizing grows `k` for a smaller δ at the same ε, near the
    /// module's closed form, and a KMV sized for one δ is enlarged for a
    /// smaller one at the same relative error.
    #[test]
    fn kmv_k_follows_the_failure_probability() {
        // Ten times smaller than the former Chebyshev calibration's 40,002.
        assert_eq!(kmv_k(0.05, 0.01), 4323);
        assert_eq!(kmv_k(0.02, 0.01), 26656);
        assert!(kmv_k(0.05, 1e-6) > kmv_k(0.05, 0.01));
        assert!(kmv_failure_probability(kmv_k(0.05, 0.01) - 1, 0.05) > 0.01);
        assert_eq!(kmv_k(0.0, 0.01), KMV_MAX_K);
        let sized =
            super::super::size_params(SketchAlgorithm::Kmv, &default_cardinality(), 0.05, 0.01);
        let enlarged = super::super::with_failure_probability(&sized, 1e-4);
        let (error, delta) = error_and_delta(&enlarged);
        assert_eq!(error, 0.05);
        assert!(delta <= 1e-4);
        assert_ne!(enlarged, sized);
        // Not sized for a relative error: nothing to enlarge for.
        let unsized_kmv = SketchParams::Kmv {
            k: 1024,
            relative_error: None,
        };
        assert_eq!(
            super::super::with_failure_probability(&unsized_kmv, 1e-4),
            unsized_kmv
        );
        assert_eq!(error_and_delta(&unsized_kmv).1, 1.0);
    }

    /// The bound dominates the exact failure probability of the order
    /// statistic for every number of distinct items checked: exact below
    /// `k`, and both binomial tails from `k` on.
    #[test]
    fn kmv_bound_dominates_the_exact_order_statistic_tails() {
        use super::super::binomial_upper_tail;
        for k in [16u32, 64, 256] {
            for eps in [0.1, 0.3, 0.6] {
                let bound = kmv_failure_probability(k, eps);
                for n in (k..8 * k).step_by(7).chain([k, 20 * k]) {
                    let nf = f64::from(n);
                    let ranks = f64::from(k - 1);
                    let over = binomial_upper_tail(n, ranks / ((1.0 + eps) * nf), k);
                    let under_p = ranks / ((1.0 - eps) * nf);
                    let under = if under_p >= 1.0 {
                        0.0
                    } else {
                        1.0 - binomial_upper_tail(n, under_p, k)
                    };
                    assert!(
                        over + under <= bound + 1e-9,
                        "k={k} eps={eps} n={n}: {}>{bound}",
                        over + under
                    );
                }
            }
        }
    }
}

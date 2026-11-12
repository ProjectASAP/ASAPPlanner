//! KMV's provable distinct-count bound.
//!
//! # KMV
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
//! output row (each group's estimate). The guarantee at a failure
//! probability δ ([`guarantee`]) is the smallest `ε` with that sum at
//! most δ.
use super::*;

/// The smallest `k` `asap_sketchlib`'s KMV estimate `(k-1)/U` is defined for.
const KMV_MIN_K: u32 = 2;

/// The smallest relative error `k` misses with probability at most
/// `failure_probability` ([`kmv_failure_probability`], by bisection);
/// `None` when no error up to 1 does.
pub(super) fn guarantee(
    params: &SketchParams,
    failure_probability: f64,
) -> Option<AccuracyGuarantee> {
    let SketchParams::Kmv { k } = params else {
        return None;
    };
    if kmv_failure_probability(*k, 1.0) > failure_probability {
        return None;
    }
    let (mut met, mut missed) = (1.0, 0.0);
    for _ in 0..60 {
        let eps = (met + missed) / 2.0;
        if kmv_failure_probability(*k, eps) <= failure_probability {
            met = eps;
        } else {
            missed = eps;
        }
    }
    Some(super::bounded_guarantee(
        AccuracyMetric::Relative,
        met,
        ProbabilityExpr::Constant {
            value: failure_probability,
        },
    ))
}

/// The probability that `asap_sketchlib`'s KMV with `k` misses relative
/// error `eps` on any number of distinct items (the module's bound): `1`
/// when it certifies nothing.
fn kmv_failure_probability(k: u32, eps: f64) -> f64 {
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

#[cfg(test)]
mod tests;

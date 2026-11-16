//! HLL's distinct count: its relative standard error, without a failure
//! probability, and classic HLL's certified guarantee over a source of at
//! most a declared number of distinct items per evaluation
//! ([`classic_guarantee_at`]), from its linear-counting branch.
//!
//! The certificate assumes independent uniform bucket hashes and an upper
//! bound on the distinct items in the complete evaluation population
//! (including all merged tumbling windows), which the data workload declares
//! (`SourceFacts::max_distinct_per_evaluation`); observations alone do not
//! establish it. It is not an RSE-to-normal conversion or a fit.

use super::*;

/// HLL's guarantee at `params` for its distinct count: classic HLL's
/// certified one at `failure_probability` when the source declares at most
/// `max_distinct_per_evaluation` distinct items and the contract covers that
/// bound ([`classic_guarantee_at`]); else the relative standard error, whose
/// failure probability is unknown.
pub(super) fn guarantee(
    params: &SketchParams,
    failure_probability: f64,
    max_distinct_per_evaluation: Option<u64>,
) -> Option<AccuracyGuarantee> {
    let SketchParams::Hll { precision, .. } = params else {
        return None;
    };
    max_distinct_per_evaluation
        .and_then(|bound| classic_guarantee_at(*precision, failure_probability, bound))
        .or_else(|| {
            Some(super::bounded_guarantee(
                AccuracyMetric::Relative,
                relative_standard_error(*precision),
                ProbabilityExpr::Unknown {
                    statistic: "hll_estimator_failure_probability".into(),
                },
            ))
        })
}

/// HLL's relative standard error, `1.04 / √m` for `m = 2^precision` buckets.
fn relative_standard_error(precision: u8) -> f64 {
    1.04 / 2f64.powi(i32::from(precision)).sqrt()
}

/// Classic HLL's guarantee at `precision` over a source of at most
/// `max_distinct` distinct items per evaluation: the least relative error
/// whose failure probability ([`ClassicHllConfidence`]) is at most
/// `failure_probability`, with that failure probability. `None` when no
/// relative error below 1 is certified (the bound is outside the contract
/// for this precision).
///
/// Example: at precision 18 and δ = 0.01, a bound of 9,636 certifies a
/// relative error just under 0.05; a bound of 1,000,000 certifies none.
fn classic_guarantee_at(
    precision: u8,
    failure_probability: f64,
    max_distinct: u64,
) -> Option<AccuracyGuarantee> {
    let max_distinct = u32::try_from(max_distinct).ok()?;
    let certified = |relative_error: f64| {
        ClassicHllConfidence::new(max_distinct, relative_error)
            .and_then(|model| model.failure_probability(precision))
            .is_some_and(|delta| delta <= failure_probability)
    };
    // The failure probability falls as the relative error grows: bisect
    // for the least certified one, keeping the certified end.
    let (mut uncertified, mut certified_error) = (1e-6, 1.0 - 1e-6);
    if !certified(certified_error) {
        return None;
    }
    for _ in 0..60 {
        let middle = (uncertified + certified_error) / 2.0;
        match certified(middle) {
            true => certified_error = middle,
            false => uncertified = middle,
        }
    }
    let delta =
        ClassicHllConfidence::new(max_distinct, certified_error)?.failure_probability(precision)?;
    Some(super::bounded_guarantee(
        AccuracyMetric::Relative,
        certified_error,
        ProbabilityExpr::Constant { value: delta },
    ))
}

/// The largest precision classic HLL's contract covers.
const MAX_PRECISION: u8 = 18;

/// The largest distinct bound the contract supports at any precision:
/// `m/2` at precision 18. Evaluating a bound is O(bound).
const MAX_CONTRACT_DISTINCT: u32 = 1 << 17;

/// A finite-population contract for `m * ln(m / zero_registers)` with the
/// classic HLL small-range switch, over at most `max_distinct` distinct
/// items and at relative error `relative_error`. Hashing is assumed
/// independent and uniform.
#[derive(Debug, Clone, Copy)]
struct ClassicHllConfidence {
    max_distinct: u32,
    relative_error: f64,
}

impl ClassicHllConfidence {
    /// `None` for a bound of 0 or above [`MAX_CONTRACT_DISTINCT`], or a
    /// relative error outside [1e-6, 1).
    fn new(max_distinct: u32, relative_error: f64) -> Option<Self> {
        (max_distinct > 0
            && max_distinct <= MAX_CONTRACT_DISTINCT
            && relative_error.is_finite()
            && (1e-6..1.0).contains(&relative_error))
        .then_some(Self {
            max_distinct,
            relative_error,
        })
    }

    /// The probability that the estimate at `precision` is off by more
    /// than the relative error, for any population up to the bound; `None`
    /// outside the contract (a precision outside 4..=18, a bound above
    /// `m/2`, or a relative error the deterministic overestimate exceeds).
    ///
    /// A finite bound, not an asymptotic RSE fit. With N distinct hashes
    /// and K occupied buckets, C = N − K collision arrivals satisfy
    /// P(C ≥ t) ≤ λ^t / t!, λ = N(N−1)/(2m): each arrival's conditional
    /// collision probability is at most (i−1)/m, and a union bound over t
    /// arrivals is bounded by the t-th power of their sum divided by t!.
    ///
    /// N ≤ m/2 makes the classic raw estimate ≤ 2 α_m m < 2.5 m, so the
    /// small-range switch always uses L = −m ln(1 − K/m). Then
    /// K ≤ L ≤ N + N²/(2(m − N)). The latter bounds overestimation
    /// deterministically; underestimation implies C > ε N. The collision
    /// bound is maximized over every integer N in the contract, not just
    /// its upper end (small populations' tails matter).
    fn failure_probability(&self, precision: u8) -> Option<f64> {
        if !(4..=MAX_PRECISION).contains(&precision) {
            return None;
        }
        let m = f64::from(1u32 << precision);
        let max_n = f64::from(self.max_distinct);
        // Reserve numerical slack; do not certify sub-floating-point error.
        let eps = self.relative_error - 1e-8;
        if max_n > m / 2.0 || max_n / (2.0 * (m - max_n)) > eps {
            return None;
        }
        let mut log_factorial = vec![0.0; self.max_distinct as usize + 1];
        for i in 1..log_factorial.len() {
            log_factorial[i] = log_factorial[i - 1] + (i as f64).ln();
        }
        let mut worst = 0.0_f64;
        for n in 2..=self.max_distinct {
            let nf = f64::from(n);
            // Including a boundary collision event is conservative.
            let t = ((eps * nf).floor() as usize + 1).min(n as usize);
            let lambda = nf * (nf - 1.0) / (2.0 * m);
            let log_tail = (t as f64) * lambda.ln() - log_factorial[t];
            worst = worst.max(log_tail.min(0.0).exp());
        }
        // Never return a spurious zero from underflow or cancellation.
        Some((worst * (1.0 + 1e-10) + 1e-12).min(1.0))
    }
}

#[cfg(test)]
mod tests;

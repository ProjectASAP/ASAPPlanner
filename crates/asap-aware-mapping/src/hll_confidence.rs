//! Estimator-specific confidence for classic HLL's linear-counting branch.
//!
//! This is conditional on independent uniform bucket hashes and an enforced
//! upper bound on distinct items in the complete readout population (including
//! all merged panes). It is not an RSE-to-normal conversion or an ERP fit.

use asap_types::post_asap::{
    BoundExpr, ErrorMetric, GuaranteeSource, ProbabilityExpr, ResultGuarantee,
};

/// A finite-population contract for `m * ln(m / zero_registers)` with the
/// classic HLL small-range switch. Hashing is assumed independent and uniform.
/// The deployment must establish the population bound; observations alone do
/// not establish it. Unsupported precisions/populations return no certificate.
#[derive(Debug, Clone, Copy)]
pub struct ClassicHllConfidence {
    max_distinct: u32,
    relative_error: f64,
}

impl ClassicHllConfidence {
    pub fn new(max_distinct: u32, relative_error: f64) -> Option<Self> {
        (max_distinct > 0
            && max_distinct <= 4096
            && relative_error.is_finite()
            && (1e-6..1.0).contains(&relative_error))
        .then_some(Self {
            max_distinct,
            relative_error,
        })
    }

    pub fn guarantee(&self, precision: u8) -> Option<ResultGuarantee> {
        let delta = self.failure_probability(precision)?;
        Some(ResultGuarantee {
            metric: ErrorMetric::Cardinality,
            bound: BoundExpr::Constant {
                value: self.relative_error,
            },
            failure_probability: ProbabilityExpr::Constant { value: delta },
            provenance: vec![GuaranteeSource::SketchReadout {
                algorithm: "Hll".into(),
                contract: "classic_hll_linear_counting_collision_bound_v1".into(),
                params: serde_json::json!({"precision": precision,
                    "max_distinct": self.max_distinct, "relative_error": self.relative_error,
                    "hash_assumption": "independent_uniform_buckets",
                    "population_scope": "complete_readout_including_merged_panes"}),
                query: "Cardinality".into(),
            }],
        })
    }

    pub fn precision(&self, delta: f64) -> Option<u8> {
        if !delta.is_finite() || !(0.0..1.0).contains(&delta) || delta == 0.0 {
            return None;
        }
        (4..=18).find(|&p| self.failure_probability(p).is_some_and(|d| d <= delta))
    }

    /// Finite bound, not an asymptotic RSE fit. With N distinct hashes and K
    /// occupied buckets, C=N-K collision arrivals satisfy
    /// P(C>=t) <= lambda^t/t!, lambda=N(N-1)/(2m): each arrival's conditional
    /// collision probability is at most (i-1)/m, and a union bound over t
    /// arrivals is bounded by the t-th power of their sum divided by t!.
    ///
    /// N<=m/2 makes the classic raw estimate <=2*alpha_m*m<2.5m,
    /// so the small-range switch always uses L=-m*ln(1-K/m). Then
    /// K<=L<=N + N^2/(2(m-N)). The latter bounds overestimation
    /// deterministically; underestimation implies C>epsilon*N.
    /// We maximize the collision bound over EVERY integer N in the contract,
    /// not just its upper endpoint (small-cardinality tails matter).
    fn failure_probability(&self, precision: u8) -> Option<f64> {
        if !(4..=18).contains(&precision) {
            return None;
        }
        let m = f64::from(1u32 << precision);
        let max_n = f64::from(self.max_distinct);
        // Reserve numerical slack; do not certify sub-floating-point error.
        let eps = self.relative_error * (1.0 - 1e-8);
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
        // Never return a spurious zero from underflow or numeric cancellation.
        Some((worst * (1.0 + 1e-10) + 1e-12).min(1.0))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A supported estimator contract supplies a probability, unlike generic HLL RSE.
    #[test]
    fn bounded_classic_hll_has_a_feasible_confidence_target() {
        let model = ClassicHllConfidence::new(128, 0.05).unwrap();
        let precision = model.precision(0.01).expect("finite confidence-sized HLL");
        let guarantee = model.guarantee(precision).unwrap();
        assert!(!guarantee.has_unknown());
        assert!(guarantee.failure_probability.evaluate().unwrap() <= 0.01);
        assert_eq!(guarantee.bound.evaluate(), Some(0.05));
    }
    /// Tighter confidence must increase precision or explicitly become unavailable.
    #[test]
    fn sizing_and_domain_limits_are_consistent() {
        let model = ClassicHllConfidence::new(128, 0.05).unwrap();
        assert!(model.precision(0.001).unwrap() > model.precision(0.01).unwrap());
        assert!(model.precision(1e-12).is_none());
        assert!(model.precision(0.0).is_none());
        assert!(model.precision(f64::NAN).is_none());
        assert!(model.guarantee(3).is_none());
        assert!(model.guarantee(19).is_none());
        assert!(model.guarantee(7).is_none());
        for (n, e) in [(0, 0.05), (4097, 0.05), (128, 0.0), (128, f64::NAN)] {
            assert!(ClassicHllConfidence::new(n, e).is_none());
        }
    }

    /// Exact occupancy probabilities independently check both tails for every N.
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
    /// The model's readout formula matches the actual classic estimator after merge.
    #[test]
    fn native_classic_estimator_and_merged_registers_use_the_same_contract() {
        use asap_sketchlib::sketches::hll::{Classic, HyperLogLogP16};
        let model = ClassicHllConfidence::new(128, 0.05).unwrap();
        assert!(
            model
                .guarantee(16)
                .unwrap()
                .failure_probability
                .evaluate()
                .unwrap()
                < 0.01
        );
        let mut single = HyperLogLogP16::<Classic>::new();
        let mut left = HyperLogLogP16::<Classic>::new();
        let mut right = HyperLogLogP16::<Classic>::new();
        for n in 0..128u64 {
            // SplitMix64 supplies deterministic test hashes, not a proof of randomness.
            let mut h = n.wrapping_add(0x9e3779b97f4a7c15);
            h = (h ^ (h >> 30)).wrapping_mul(0xbf58476d1ce4e5b9);
            h = (h ^ (h >> 27)).wrapping_mul(0x94d049bb133111eb);
            h ^= h >> 31;
            single.insert_with_hash(h);
            if n % 2 == 0 {
                left.insert_with_hash(h);
            } else {
                right.insert_with_hash(h);
            }
        }
        left.merge(&right);
        assert_eq!(single.registers_as_slice(), left.registers_as_slice());
        let zeroes = left
            .registers_as_slice()
            .iter()
            .filter(|&&r| r == 0)
            .count();
        let expected = (65536.0 * (65536.0 / zeroes as f64).ln()) as usize;
        assert_eq!(left.estimate(), expected);
        assert!((expected as f64 - 128.0).abs() / 128.0 <= 0.05);
    }
}

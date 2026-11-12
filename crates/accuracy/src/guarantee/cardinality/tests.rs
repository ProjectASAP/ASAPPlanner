use super::*;

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

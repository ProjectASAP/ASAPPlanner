//! Split an end-to-end accuracy target equally among the approximate inputs
//! of one composition (#669, part b).
//!
//! When `n` approximate values compose into one output, each is sized for an
//! equal share of the output's target, so the composed bound meets the
//! target: `ε_i = ε / n` for an additively composed metric (no slack),
//! `ε_i = 1 − (1 + ε)^{−1/n}` for a relative one, and `δ_i = δ / n` (the
//! union bound). Relative errors compose through products and quotients:
//! each input contributes a factor of at most `1 + ε_i` as a factor and
//! `1/(1 − ε_i)` as a divisor, so the true error of any mix stays within
//! `(1 − ε_i)^{−n} − 1 = ε`. The composed bound Stage 3 checks meets it for
//! one product or quotient of two inputs (a ratio's `(a + b)/(1 − b)`), not
//! for a nested quotient such as `x / (y / z)`, whose bound composes each
//! step's slack. A lone approximate denominator over an exact numerator is
//! sized for the whole target, so its quotient's bound `ε/(1 − ε)` misses
//! it. Only equal splits are made.
use super::*;

/// The target of one of `approximate_inputs` approximate inputs that
/// compose into a value with `target`, split equally in `metric`'s
/// composition (see the module doc). `None` for an exact target or an ε
/// that is not positive and finite: no approximate input meets it. One
/// input (or none) gets `target` itself.
pub fn per_input_target(
    target: &AccuracyTarget,
    metric: AccuracyMetric,
    approximate_inputs: usize,
) -> Option<AccuracyTarget> {
    if target.is_exact() || !(target.max_error.is_finite() && target.max_error > 0.0) {
        return None;
    }
    if approximate_inputs <= 1 {
        return Some(target.clone());
    }
    let fraction = 1.0 / approximate_inputs.max(1) as f64;
    let max_error = match metric {
        AccuracyMetric::Relative => 1.0 - (1.0 + target.max_error).powf(-fraction),
        _ => target.max_error * fraction,
    };
    Some(AccuracyTarget {
        max_error,
        failure_probability: target.failure_probability.map(|delta| delta * fraction),
        ..target.clone()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Additive metrics split ε and δ by `n`; the `n` shares add up to the
    /// target.
    #[test]
    fn an_additive_target_splits_epsilon_and_delta_equally() {
        let target = AccuracyTarget::max_error(0.1).failure_probability(0.02);
        let share = per_input_target(&target, AccuracyMetric::ABSOLUTE, 2).unwrap();
        assert_eq!(
            share,
            AccuracyTarget::max_error(0.05).failure_probability(0.01)
        );
        assert_eq!(
            per_input_target(&target, AccuracyMetric::Rank, 1),
            Some(target.clone())
        );
        assert_eq!(
            per_input_target(&AccuracyTarget::max_error(0.1), AccuracyMetric::Rank, 2)
                .unwrap()
                .failure_probability,
            None
        );
    }

    /// A relative target splits so that a quotient meets it too:
    /// `(1 − ε_i)^{−n} = 1 + ε`. For ε = 0.05 over two inputs each gets
    /// 0.0241, and both a ratio `(a + b)/(1 − b)` and a product
    /// `(1 + a)(1 + b) − 1` of them stay within 0.05; the product-only split
    /// `(1 + ε)^{1/2} − 1 = 0.0247` would give the ratio 0.0507.
    #[test]
    fn a_relative_target_splits_so_a_quotient_meets_it() {
        let target = AccuracyTarget::max_error(0.05);
        let share = per_input_target(&target, AccuracyMetric::Relative, 2)
            .unwrap()
            .max_error;
        assert!((share - (1.0 - 1.05f64.powf(-0.5))).abs() < 1e-12);
        let ratio = (share + share) / (1.0 - share);
        let product = (1.0 + share) * (1.0 + share) - 1.0;
        assert!(ratio <= 0.05 && product <= 0.05, "{ratio} {product}");
        assert_eq!(
            per_input_target(&target, AccuracyMetric::Relative, 1),
            Some(target)
        );
    }

    /// No approximate input meets an exact target.
    #[test]
    fn an_exact_target_has_no_share() {
        assert_eq!(
            per_input_target(&AccuracyTarget::exact(), AccuracyMetric::ABSOLUTE, 2),
            None
        );
        assert_eq!(
            per_input_target(&AccuracyTarget::max_error(0.0), AccuracyMetric::ABSOLUTE, 2),
            None
        );
    }
}

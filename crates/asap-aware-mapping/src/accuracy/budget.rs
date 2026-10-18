//! Split an end-to-end accuracy target equally among the approximate inputs
//! of one composition (#669, part b).
//!
//! When `n` approximate values compose into one output, each is sized for an
//! equal share of the output's target, so the composed bound meets the
//! target with no slack: `ε_i = ε / n` for an additively composed metric,
//! `ε_i = (1 + ε)^{1/n} − 1` for a relative one (relative errors compose as
//! `Π(1 + ε_i) − 1`), and `δ_i = δ / n` (the union bound). Only equal
//! splits are made.
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
    let fraction = 1.0 / approximate_inputs.max(1) as f64;
    let max_error = match metric {
        AccuracyMetric::Relative => (1.0 + target.max_error).powf(fraction) - 1.0,
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

    /// A relative target splits multiplicatively: `(1 + ε_i)^n = 1 + ε`,
    /// not `n ε_i = ε`.
    #[test]
    fn a_relative_target_splits_multiplicatively() {
        let share = per_input_target(
            &AccuracyTarget::max_error(0.21),
            AccuracyMetric::Relative,
            2,
        )
        .unwrap();
        assert!((share.max_error - 0.1).abs() < 1e-12);
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

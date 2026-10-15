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

/// `shares` of `of` equal shares of `target`, in `metric`'s composition:
/// the target of one approximate input (`shares = 1`), or of a nested
/// sub-DAG holding `shares` of the `of` inputs. `None` for an exact target
/// or an ε that is not positive and finite: no approximate input meets it.
/// `of = 1` (or 0) is `target` itself.
pub fn equal_shares(
    target: &AccuracyTarget,
    metric: AccuracyMetric,
    of: usize,
    shares: usize,
) -> Option<AccuracyTarget> {
    if target.is_exact() || !(target.error.is_finite() && target.error > 0.0) {
        return None;
    }
    let fraction = shares.min(of.max(1)) as f64 / of.max(1) as f64;
    let error = match metric {
        AccuracyMetric::Relative => (1.0 + target.error).powf(fraction) - 1.0,
        _ => target.error * fraction,
    };
    Some(AccuracyTarget {
        error,
        failure_probability: target.failure_probability.map(|delta| delta * fraction),
        ..target.clone()
    })
}

/// One of `approximate_inputs` equal shares of `target` ([`equal_shares`]).
pub fn equal_share(
    target: &AccuracyTarget,
    metric: AccuracyMetric,
    approximate_inputs: usize,
) -> Option<AccuracyTarget> {
    equal_shares(target, metric, approximate_inputs, 1)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Additive metrics split ε and δ by `n`; the `n` shares add up to the
    /// target.
    #[test]
    fn an_additive_target_splits_epsilon_and_delta_equally() {
        let target = AccuracyTarget::max_error(0.1).failure_probability(0.02);
        let share = equal_share(&target, AccuracyMetric::ABSOLUTE, 2).unwrap();
        assert_eq!(
            share,
            AccuracyTarget::max_error(0.05).failure_probability(0.01)
        );
        let rest = equal_shares(&target, AccuracyMetric::ABSOLUTE, 3, 2).unwrap();
        assert!((rest.error - 0.1 * 2.0 / 3.0).abs() < 1e-12);
        assert!((rest.failure_probability.unwrap() - 0.02 * 2.0 / 3.0).abs() < 1e-12);
        assert_eq!(
            equal_share(&target, AccuracyMetric::Rank, 1),
            Some(target.clone())
        );
        assert_eq!(
            equal_share(&AccuracyTarget::max_error(0.1), AccuracyMetric::Rank, 2)
                .unwrap()
                .failure_probability,
            None
        );
    }

    /// A relative target splits multiplicatively: `(1 + ε_i)^n = 1 + ε`,
    /// not `n ε_i = ε`.
    #[test]
    fn a_relative_target_splits_multiplicatively() {
        let share = equal_share(
            &AccuracyTarget::max_error(0.21),
            AccuracyMetric::Relative,
            2,
        )
        .unwrap();
        assert!((share.error - 0.1).abs() < 1e-12);
    }

    /// No approximate input meets an exact target.
    #[test]
    fn an_exact_target_has_no_share() {
        assert_eq!(
            equal_share(&AccuracyTarget::exact(), AccuracyMetric::ABSOLUTE, 2),
            None
        );
        assert_eq!(
            equal_share(&AccuracyTarget::max_error(0.0), AccuracyMetric::ABSOLUTE, 2),
            None
        );
    }
}

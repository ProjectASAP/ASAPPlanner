//! The planner's built-in analytical accuracy rules: estimators, composition, budget split and target check.
//!
//! Estimator models derive local guarantees, composition propagates them,
//! the budget split sizes each approximate input of a composition for an
//! equal share of its target, and `evidence` holds the scoped contracts and
//! statistics the rules consult. The deployment's evidence provider and
//! accuracy model live in plan selection (`asap_plan_selection::accuracy`).
//! See `docs/design_docs/concepts/accuracy-models.md` for the design.

pub mod budget;
pub mod composition;
pub mod estimators;
pub mod evidence;

pub use budget::per_input_target;
pub use evidence::{OperandDomain, PropagationStats};

use asap_types::ir::properties::{
    AccuracyError, AccuracyGuarantee, AccuracyMetric, BoundExpr, CompositionOperator,
    GuaranteeSource, ProbabilityExpr,
};
use asap_types::ir::properties::{AccuracyScope, AccuracyTarget};
use asap_types::ir::schema::{FieldDataType, SketchAlgorithm, SketchParams, SketchStatistic};

/// Small relative tolerance for comparing an evaluated bound against a
/// target, so a parameter sized by `⌈·⌉` to *exactly* meet ε is not rejected
/// by floating-point noise.
const SATISFACTION_TOLERANCE: f64 = 1e-9;

/// The built-in conservative check of whether `guarantee` meets `target`:
/// a bound within its ε, and within its δ when it states one, over the
/// values the target's scope covers (a per-value δ does not meet a
/// whole-result target). When the target sets a metric, the bound must be
/// in that metric (or one with a sound direct comparison): a bound in
/// another metric does not satisfy, however small. When it sets none, a
/// bound in any metric does, each compared in its own metric. Unknown
/// required dimensions fail. Plan selection's default accuracy model
/// (`asap_plan_selection::BuiltinAccuracyModel`) delegates to it.
pub fn satisfies(guarantee: &AccuracyGuarantee, target: &AccuracyTarget) -> bool {
    let within = |value: Option<f64>, limit: f64| {
        value.is_some_and(|v| v <= limit * (1.0 + SATISFACTION_TOLERANCE) + f64::EPSILON)
    };
    if target.is_exact() || guarantee.is_exact() {
        return guarantee.is_exact();
    }
    target
        .accuracy_metric
        .is_none_or(|metric| metric_meets(guarantee.spec.accuracy_metric, metric))
        && scope_meets(guarantee, target)
        && within(guarantee.spec.error.evaluate(), target.max_error)
        && target
            .failure_probability
            .is_none_or(|delta| within(guarantee.spec.failure_probability.evaluate(), delta))
}

/// Whether an error bound stated in `guarantee` bounds the error `target`
/// states with the same ε, so the two numbers compare directly. The same
/// metric does; so does an L2-scaled bound for an L1-scaled target, since
/// both are scaled by the norm of the one stream the root reads and
/// `‖f‖₂ ≤ ‖f‖₁`. Any other pair needs a conversion backed by evidence
/// (Absolute ↔ Relative needs a bound on the true value's magnitude; a rank
/// error has no distribution-free value error), which the default model
/// does not make: it does not satisfy. The scope is compared separately
/// ([`scope_meets`]).
fn metric_meets(guarantee: AccuracyMetric, target: AccuracyMetric) -> bool {
    guarantee == target
        || (guarantee == AccuracyMetric::ABSOLUTE_STREAM_L2
            && target == AccuracyMetric::ABSOLUTE_STREAM_L1)
}

/// Whether `guarantee`'s failure probability covers the values `target`'s
/// does. A whole-result guarantee covers a per-value target (each value is
/// in the result), but a per-value one does not cover a whole-result
/// target: it must first be stated over the result's values
/// ([`AccuracyGuarantee::over_whole_result`]). The scope bounds only the failure
/// probability, so it does not matter for a target without a δ requirement
/// or for a guarantee that never fails.
fn scope_meets(guarantee: &AccuracyGuarantee, target: &AccuracyTarget) -> bool {
    target.accuracy_scope == AccuracyScope::PerValue
        || guarantee.spec.accuracy_scope == AccuracyScope::WholeResult
        || target.failure_probability.is_none()
        || guarantee.spec.failure_probability.is_zero()
}

/// `target` restated in `guarantee`'s metric, per value: the check of one
/// summary against the target it was sized for. A summary is sized in its
/// own native terms (#690 §4) and for each value it answers; an aggregate
/// intent's metric and scope are not checked: only a query root's are, by
/// plan selection.
pub fn native_target(target: &AccuracyTarget, guarantee: &AccuracyGuarantee) -> AccuracyTarget {
    target
        .clone()
        .accuracy_metric(guarantee.spec.accuracy_metric)
        .accuracy_scope(AccuracyScope::PerValue)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn abs(bound: f64, delta: f64) -> AccuracyGuarantee {
        AccuracyGuarantee::new(
            AccuracyMetric::ABSOLUTE,
            BoundExpr::Constant { value: bound },
            ProbabilityExpr::Constant { value: delta },
            vec![],
        )
    }
    #[test]
    fn satisfies_is_fail_closed_on_unknowns_and_exact() {
        let mut unknown = abs(0.0, 0.0);
        unknown.spec.error = BoundExpr::Unknown {
            statistic: "x".into(),
        };
        assert!(!satisfies(&unknown, &AccuracyTarget::max_error(1.0)));
        assert!(!satisfies(&abs(0.0, 0.01), &AccuracyTarget::exact()));
        assert!(satisfies(
            &AccuracyGuarantee::exact("x"),
            &AccuracyTarget::exact()
        ));
    }

    fn guarantee(metric: AccuracyMetric, bound: f64, delta: f64) -> AccuracyGuarantee {
        AccuracyGuarantee::new(
            metric,
            BoundExpr::Constant { value: bound },
            ProbabilityExpr::Constant { value: delta },
            vec![],
        )
    }

    /// A bound in another metric does not satisfy a target, however small
    /// its number: e.g. a Count-Min L1-scaled frequency bound never meets a
    /// distinct count's relative target, and a rank bound never meets a
    /// value target. An L2-scaled bound meets an L1-scaled target of the
    /// same stream (`‖f‖₂ ≤ ‖f‖₁`), not the reverse; and an exact value
    /// meets every metric.
    #[test]
    fn satisfies_requires_the_targets_metric() {
        let target = |metric| {
            AccuracyTarget::max_error(0.5)
                .failure_probability(0.5)
                .accuracy_metric(metric)
                .accuracy_scope(AccuracyScope::PerValue)
        };
        let (l1, l2) = (
            AccuracyMetric::ABSOLUTE_STREAM_L1,
            AccuracyMetric::ABSOLUTE_STREAM_L2,
        );
        for (bound, wanted, meets) in [
            (l1, l1, true),
            (l2, l2, true),
            (l2, l1, true),
            (l1, l2, false),
            (l1, AccuracyMetric::Relative, false),
            (l1, AccuracyMetric::ABSOLUTE, false),
            (AccuracyMetric::ABSOLUTE, l1, false),
            (AccuracyMetric::Relative, AccuracyMetric::ABSOLUTE, false),
            (AccuracyMetric::ABSOLUTE, AccuracyMetric::Relative, false),
            (AccuracyMetric::Rank, AccuracyMetric::Relative, false),
            (AccuracyMetric::Relative, AccuracyMetric::Rank, false),
            (AccuracyMetric::Rank, AccuracyMetric::Rank, true),
        ] {
            assert_eq!(
                satisfies(&guarantee(bound, 0.01, 0.01), &target(wanted)),
                meets,
                "{bound:?} against {wanted:?}"
            );
        }
        assert!(satisfies(
            &AccuracyGuarantee::exact("x"),
            &target(AccuracyMetric::Rank)
        ));
    }

    /// A target without a δ requirement checks only the error, even when
    /// the guarantee's failure probability is unknown.
    #[test]
    fn a_target_without_delta_ignores_the_failure_probability() {
        let mut g = guarantee(AccuracyMetric::ABSOLUTE, 0.01, 0.0);
        g.spec.failure_probability = ProbabilityExpr::Unknown {
            statistic: "x".into(),
        };
        assert!(satisfies(&g, &AccuracyTarget::max_error(0.01)));
        assert!(!satisfies(
            &g,
            &AccuracyTarget::max_error(0.01).failure_probability(0.5)
        ));
    }

    /// A per-value guarantee does not meet a whole-result δ target
    /// directly: over `G` output values it holds for all of them with
    /// δ_whole = G · δ_per ([`AccuracyGuarantee::over_whole_result`]), and an unbound `G`
    /// leaves it unmet. A whole-result guarantee meets a per-value target
    /// directly.
    #[test]
    fn per_value_guarantees_meet_whole_result_targets_over_g_values() {
        let g = guarantee(AccuracyMetric::ABSOLUTE, 0.01, 0.001);
        assert_eq!(g.spec.accuracy_scope, AccuracyScope::PerValue);
        let whole = AccuracyTarget::max_error(0.01).failure_probability(0.01);
        assert_eq!(whole.accuracy_scope, AccuracyScope::WholeResult);
        assert!(!satisfies(&g, &whole));
        let per_value = whole.clone().accuracy_scope(AccuracyScope::PerValue);
        assert!(satisfies(&g, &per_value));
        let over = |values| g.over_whole_result(values);
        assert_eq!(
            over(Some(10)).spec.accuracy_scope,
            AccuracyScope::WholeResult
        );
        assert_eq!(
            over(Some(10)).spec.failure_probability.evaluate(),
            Some(0.01)
        );
        assert!(satisfies(&over(Some(10)), &whole));
        assert!(!satisfies(&over(Some(11)), &whole));
        assert!(!satisfies(&over(None), &whole));
        assert!(satisfies(&over(Some(10)), &per_value));
    }

    /// The scope bounds only the failure probability: a target without a δ
    /// requirement, or a guarantee that never fails, meets a whole-result
    /// target whatever the number of values.
    #[test]
    fn scope_matters_only_with_a_failure_probability() {
        let g = guarantee(AccuracyMetric::ABSOLUTE, 0.01, 0.001);
        assert!(satisfies(&g, &AccuracyTarget::max_error(0.01)));
        let never_fails = AccuracyGuarantee::new(
            AccuracyMetric::Relative,
            BoundExpr::Constant { value: 0.01 },
            ProbabilityExpr::Zero,
            vec![],
        );
        let whole = AccuracyTarget::max_error(0.01)
            .failure_probability(0.01)
            .accuracy_metric(AccuracyMetric::Relative);
        assert!(satisfies(&never_fails, &whole));
        assert!(satisfies(&never_fails.over_whole_result(None), &whole));
    }

    /// A target whose metric the user did not set accepts a guarantee in
    /// any metric whose own ε, δ and scope meet the target's numbers: a
    /// rank, a relative or an absolute bound at any scale. A larger bound
    /// or failure probability still misses it.
    #[test]
    fn a_target_without_a_metric_accepts_any_metric() {
        let target = AccuracyTarget::max_error(0.01)
            .failure_probability(0.01)
            .accuracy_scope(AccuracyScope::PerValue);
        assert_eq!(target.accuracy_metric, None);
        for metric in [
            AccuracyMetric::ABSOLUTE,
            AccuracyMetric::ABSOLUTE_STREAM_L1,
            AccuracyMetric::ABSOLUTE_STREAM_L2,
            AccuracyMetric::Relative,
            AccuracyMetric::Rank,
        ] {
            assert!(
                satisfies(&guarantee(metric, 0.01, 0.01), &target),
                "{metric:?}"
            );
            assert!(
                !satisfies(&guarantee(metric, 0.02, 0.01), &target),
                "{metric:?}"
            );
            assert!(
                !satisfies(&guarantee(metric, 0.01, 0.02), &target),
                "{metric:?}"
            );
        }
    }
}

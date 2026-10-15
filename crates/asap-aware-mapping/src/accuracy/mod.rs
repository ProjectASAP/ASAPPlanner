//! Planner accuracy interfaces and model dispatch.
//!
//! Estimator models derive local guarantees, composition propagates them,
//! allocation proposes local budgets, and evidence supplies scoped contracts.
//! Unknown evidence may retain a candidate but does not authorize selection.
//! See `docs/design_docs/concepts/accuracy-models.md` for the design.

pub mod allocation;
pub mod composition;
pub mod estimators;
pub mod evidence;
pub mod reconciliation;

pub use allocation::{
    AccuracyAllocation, AccuracyBudgetAllocator, CompositionShape, EqualSplitAllocator,
};
pub use evidence::{
    AccuracyEvidenceProvider, NoAccuracyEvidence, OperandDomain, PropagationStats,
    WorkloadAccuracyEvidence,
};

use asap_types::ir::OperatorNode;
use asap_types::post_asap::{
    AccuracyError, AccuracyGuarantee, BoundExpr, CompositionOperator, ErrorMetric, FieldDataType,
    GuaranteeSource, ProbabilityExpr, SketchAlgorithm, SketchParams, SketchStatistic,
};
use asap_types::types::AccuracyTarget;

use crate::exact_composition::ExactOperation;

/// The deployment-extensible accuracy algebra. `asap-aware-mapping` ships
/// [`DefaultAccuracyModel`]; a deployment with a proof for a composition the
/// default rejects (a registered cross-metric conversion, say) implements
/// this trait and passes it to
/// [`crate::replacement::ASAPStrategies::new_with_planning_inputs`].
pub trait AccuracyModel {
    /// The definition-registered rule for applying `operation` to an
    /// approximate input. `None` means the function is exact only over exact
    /// inputs; callers must fail closed for approximate input.
    fn exact_operation_rule(&self, _operation: &ExactOperation) -> Option<CompositionOperator> {
        None
    }

    /// The guarantee of reading `query` out of a summary of family `family`
    /// built over an **exact** input — derived from the family's committed
    /// parameters by inverting the same sizing formulas
    /// [`crate::realization::default_size_params`] uses. `None` when this
    /// model has no error model for the family (the default has none for
    /// `Sample`/`Wavelet`/`StatModel`).
    fn local_guarantee(
        &self,
        family: &FieldDataType,
        query: &SketchStatistic,
    ) -> Option<AccuracyGuarantee>;

    /// Compose `inputs`' guarantees (in the parent's child order) with the
    /// parent's own `local` guarantee under `op`. `Err` is the fail-closed
    /// answer: no registered rule, or a missing input guarantee.
    fn propagate(
        &self,
        op: &CompositionOperator,
        inputs: &[AccuracyGuarantee],
        local: Option<&AccuracyGuarantee>,
        stats: &PropagationStats,
    ) -> Result<AccuracyGuarantee, AccuracyError>;

    /// Whether `guarantee` meets `target`: a bound in the target's metric
    /// (or one with a sound direct comparison) within its ε, and within its
    /// δ when it states one. A bound in another metric does not satisfy,
    /// however small. Unknown required dimensions fail; selection
    /// separately excludes missing accuracy evidence.
    fn satisfies(&self, guarantee: &AccuracyGuarantee, target: &AccuracyTarget) -> bool;
}

/// The built-in estimator and composition models, with conservative target checks.
#[derive(Debug, Default, Clone, Copy)]
pub struct DefaultAccuracyModel;

/// Small relative tolerance for comparing an evaluated bound against a
/// target, so a parameter sized by `⌈·⌉` to *exactly* meet ε is not rejected
/// by floating-point noise.
const SATISFACTION_TOLERANCE: f64 = 1e-9;

impl DefaultAccuracyModel {
    /// Derive the guarantee for the committed estimator parameters and evaluation.
    pub fn sketch_guarantee(
        algorithm: &SketchAlgorithm,
        params: &SketchParams,
        query: &SketchStatistic,
    ) -> Option<AccuracyGuarantee> {
        estimators::sketch_guarantee(algorithm, params, query)
    }
}

impl AccuracyModel for DefaultAccuracyModel {
    fn exact_operation_rule(&self, operation: &ExactOperation) -> Option<CompositionOperator> {
        composition::exact_operation_rule(operation)
    }
    fn local_guarantee(
        &self,
        family: &FieldDataType,
        query: &SketchStatistic,
    ) -> Option<AccuracyGuarantee> {
        estimators::local_guarantee(family, query)
    }
    fn propagate(
        &self,
        op: &CompositionOperator,
        inputs: &[AccuracyGuarantee],
        local: Option<&AccuracyGuarantee>,
        stats: &PropagationStats,
    ) -> Result<AccuracyGuarantee, AccuracyError> {
        composition::propagate(op, inputs, local, stats)
    }
    fn satisfies(&self, guarantee: &AccuracyGuarantee, target: &AccuracyTarget) -> bool {
        let within = |value: Option<f64>, limit: f64| {
            value.is_some_and(|v| v <= limit * (1.0 + SATISFACTION_TOLERANCE) + f64::EPSILON)
        };
        if target.is_exact() || guarantee.is_exact() {
            return guarantee.is_exact();
        }
        metric_meets(guarantee.spec.metric, target.metric)
            && within(guarantee.spec.error.evaluate(), target.error)
            && target
                .failure_probability
                .is_none_or(|delta| within(guarantee.spec.failure_probability.evaluate(), delta))
    }
}

/// Whether an error bound stated in `guarantee` bounds the error `target`
/// states with the same ε, so the two numbers compare directly. The same
/// metric does; so does an L2-scaled bound for an L1-scaled target, since
/// both are scaled by the norm of the one stream the root reads and
/// `‖f‖₂ ≤ ‖f‖₁`. Any other pair needs a conversion backed by evidence
/// (Absolute ↔ Relative needs a bound on the true value's magnitude; a rank
/// error has no distribution-free value error), which the default model
/// does not make: it does not satisfy. The scope is not compared yet: a
/// per-value guarantee is checked against a whole-result target directly
/// until the union bound over the output values (#669b).
fn metric_meets(guarantee: ErrorMetric, target: ErrorMetric) -> bool {
    guarantee == target
        || (guarantee == ErrorMetric::ABSOLUTE_STREAM_L2
            && target == ErrorMetric::ABSOLUTE_STREAM_L1)
}

/// `target` restated in `guarantee`'s metric: the check of one summary
/// against the target it was sized for. A summary is sized in its own
/// native terms (#690 §4) and an aggregate intent's metric is not checked:
/// only a query root's metric is, by plan selection.
pub fn native_target(target: &AccuracyTarget, guarantee: &AccuracyGuarantee) -> AccuracyTarget {
    target.clone().with_metric(guarantee.spec.metric)
}

pub(crate) use estimators::topk_capacity;

#[cfg(test)]
mod tests {
    use super::*;
    use asap_types::types::AccuracyScope;
    fn abs(bound: f64, delta: f64) -> AccuracyGuarantee {
        AccuracyGuarantee::new(
            ErrorMetric::ABSOLUTE,
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
        assert!(!DefaultAccuracyModel.satisfies(&unknown, &AccuracyTarget::epsilon(1.0)));
        assert!(!DefaultAccuracyModel.satisfies(&abs(0.0, 0.01), &AccuracyTarget::exact()));
        assert!(DefaultAccuracyModel
            .satisfies(&AccuracyGuarantee::exact("x"), &AccuracyTarget::exact()));
    }

    fn guarantee(metric: ErrorMetric, bound: f64, delta: f64) -> AccuracyGuarantee {
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
        let model = DefaultAccuracyModel;
        let target = |metric| AccuracyTarget::epsilon_delta(0.5, 0.5).with_metric(metric);
        let (l1, l2) = (
            ErrorMetric::ABSOLUTE_STREAM_L1,
            ErrorMetric::ABSOLUTE_STREAM_L2,
        );
        for (bound, wanted, meets) in [
            (l1, l1, true),
            (l2, l2, true),
            (l2, l1, true),
            (l1, l2, false),
            (l1, ErrorMetric::Relative, false),
            (l1, ErrorMetric::ABSOLUTE, false),
            (ErrorMetric::ABSOLUTE, l1, false),
            (ErrorMetric::Relative, ErrorMetric::ABSOLUTE, false),
            (ErrorMetric::ABSOLUTE, ErrorMetric::Relative, false),
            (ErrorMetric::Rank, ErrorMetric::Relative, false),
            (ErrorMetric::Relative, ErrorMetric::Rank, false),
            (ErrorMetric::Rank, ErrorMetric::Rank, true),
        ] {
            assert_eq!(
                model.satisfies(&guarantee(bound, 0.01, 0.01), &target(wanted)),
                meets,
                "{bound:?} against {wanted:?}"
            );
        }
        assert!(model.satisfies(&AccuracyGuarantee::exact("x"), &target(ErrorMetric::Rank)));
    }

    /// A target without a δ requirement checks only the error, even when
    /// the guarantee's failure probability is unknown.
    #[test]
    fn a_target_without_delta_ignores_the_failure_probability() {
        let mut g = guarantee(ErrorMetric::ABSOLUTE, 0.01, 0.0);
        g.spec.failure_probability = ProbabilityExpr::Unknown {
            statistic: "x".into(),
        };
        assert!(DefaultAccuracyModel.satisfies(&g, &AccuracyTarget::epsilon(0.01)));
        assert!(!DefaultAccuracyModel.satisfies(&g, &AccuracyTarget::epsilon_delta(0.01, 0.5)));
    }

    /// Pins the current scope behavior: a per-value guarantee is compared
    /// with a whole-result target directly. The union bound over the output
    /// values (δ_whole = G · δ_per) arrives with #669b, which changes this
    /// test.
    #[test]
    fn per_value_guarantees_meet_whole_result_targets_directly_until_669b() {
        let g = guarantee(ErrorMetric::ABSOLUTE, 0.01, 0.01);
        assert_eq!(g.spec.scope, AccuracyScope::PerValue);
        let target = AccuracyTarget::epsilon_delta(0.01, 0.01);
        assert_eq!(target.scope, AccuracyScope::WholeResult);
        assert!(DefaultAccuracyModel.satisfies(&g, &target));
    }
}

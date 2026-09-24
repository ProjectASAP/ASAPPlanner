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
pub(crate) use estimators::EstimatorAccuracy;
pub use evidence::{
    AccuracyEvidenceProvider, EstimatorContract, NoAccuracyEvidence, PropagationStats,
    QuantileInputDomain, WorkloadAccuracyEvidence,
};

use asap_types::post_asap::{
    AccuracyError, BoundExpr, CompositionOperator, ErrorMetric, ExactOperation, GuaranteeSource,
    ProbabilityExpr, ResultGuarantee, SketchAlgorithm, SketchParams, SketchQuery,
    SummaryFamilyType,
};
use asap_types::types::AccuracyTarget;

/// The deployment-extensible accuracy algebra. `asap-aware-mapping` ships
/// [`DefaultAccuracyModel`]; a deployment with a proof for a composition the
/// default rejects (a registered cross-metric conversion, say) implements
/// this trait and passes it to
/// [`crate::replacement::SketchAlgorithmStrategy::new_with_planning_inputs`].
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
    /// [`crate::replacement::default_size_params`] uses. `None` when this
    /// model has no error model for the family (the default has none for
    /// `Sample`/`Wavelet`/`StatModel`).
    fn local_guarantee(
        &self,
        family: &SummaryFamilyType,
        query: &SketchQuery,
    ) -> Option<ResultGuarantee>;

    /// Compose `inputs`' guarantees (in the parent's child order) with the
    /// parent's own `local` guarantee under `op`. `Err` is the fail-closed
    /// answer: no registered rule, or a missing input guarantee.
    fn propagate(
        &self,
        op: &CompositionOperator,
        inputs: &[ResultGuarantee],
        local: Option<&ResultGuarantee>,
        stats: &PropagationStats,
    ) -> Result<ResultGuarantee, AccuracyError>;

    /// Compare the dimensions requested by `target`. Unknown required
    /// dimensions fail; selection separately excludes missing accuracy evidence.
    fn satisfies(&self, guarantee: &ResultGuarantee, target: &AccuracyTarget) -> bool;
}

/// The built-in estimator and composition models, with conservative target checks.
#[derive(Debug, Default, Clone, Copy)]
pub struct DefaultAccuracyModel;

/// Small relative tolerance for comparing an evaluated bound against a
/// target, so a parameter sized by `⌈·⌉` to *exactly* meet ε is not rejected
/// by floating-point noise.
const SATISFACTION_TOLERANCE: f64 = 1e-9;

impl DefaultAccuracyModel {
    /// Derive the guarantee for the committed estimator parameters and readout.
    pub fn sketch_guarantee(
        algorithm: &SketchAlgorithm,
        params: &SketchParams,
        query: &SketchQuery,
    ) -> Option<ResultGuarantee> {
        estimators::sketch_guarantee(algorithm, params, query)
    }
}

impl AccuracyModel for DefaultAccuracyModel {
    fn exact_operation_rule(&self, operation: &ExactOperation) -> Option<CompositionOperator> {
        composition::exact_operation_rule(operation)
    }
    fn local_guarantee(
        &self,
        family: &SummaryFamilyType,
        query: &SketchQuery,
    ) -> Option<ResultGuarantee> {
        estimators::local_guarantee(family, query)
    }
    fn propagate(
        &self,
        op: &CompositionOperator,
        inputs: &[ResultGuarantee],
        local: Option<&ResultGuarantee>,
        stats: &PropagationStats,
    ) -> Result<ResultGuarantee, AccuracyError> {
        composition::propagate(op, inputs, local, stats)
    }
    fn satisfies(&self, guarantee: &ResultGuarantee, target: &AccuracyTarget) -> bool {
        let within = |value: Option<f64>, limit: f64| {
            value.is_some_and(|v| v <= limit * (1.0 + SATISFACTION_TOLERANCE) + f64::EPSILON)
        };
        match target {
            AccuracyTarget::Exact => guarantee.is_exact(),
            AccuracyTarget::Epsilon(eps) => within(guarantee.bound.evaluate(), *eps),
            AccuracyTarget::EpsilonDelta { epsilon, delta } => {
                within(guarantee.bound.evaluate(), *epsilon)
                    && within(guarantee.failure_probability.evaluate(), *delta)
            }
        }
    }
}

pub(crate) use estimators::topk_capacity;

#[cfg(test)]
mod tests {
    use super::*;
    fn abs(bound: f64, delta: f64) -> ResultGuarantee {
        ResultGuarantee {
            metric: ErrorMetric::AbsoluteValue,
            bound: BoundExpr::Constant { value: bound },
            failure_probability: ProbabilityExpr::Constant { value: delta },
            provenance: vec![],
        }
    }
    #[test]
    fn satisfies_is_fail_closed_on_unknowns_and_exact() {
        let unknown = ResultGuarantee {
            bound: BoundExpr::Unknown {
                statistic: "x".into(),
            },
            ..abs(0.0, 0.0)
        };
        assert!(!DefaultAccuracyModel.satisfies(&unknown, &AccuracyTarget::Epsilon(1.0)));
        assert!(!DefaultAccuracyModel.satisfies(&abs(0.0, 0.01), &AccuracyTarget::Exact));
        assert!(
            DefaultAccuracyModel.satisfies(&ResultGuarantee::exact("x"), &AccuracyTarget::Exact)
        );
    }
}

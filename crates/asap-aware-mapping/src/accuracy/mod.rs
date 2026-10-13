//! Planner accuracy interfaces and model dispatch.
//!
//! Estimator models derive local guarantees, and evidence supplies the
//! deployment's empirical accuracy for a candidate's output. Unknown evidence
//! may retain a candidate but does not authorize selection. See
//! `docs/design_docs/concepts/accuracy-models.md` for the design.

pub mod estimators;
pub mod evidence;

pub use evidence::{AccuracyEvidenceProvider, NoAccuracyEvidence};

use asap_types::post_asap::{
    BoundExpr, ErrorMetric, FieldDataType, GuaranteeSource, ProbabilityExpr, ResultGuarantee,
    SketchAlgorithm, SketchParams, SketchStatistic,
};
use asap_types::types::AccuracyTarget;

/// The deployment-extensible accuracy model. `asap-aware-mapping` ships
/// [`DefaultAccuracyModel`]; a deployment with its own error model for a
/// family implements this trait and passes it to plan selection through
/// [`crate::PlanningModels`].
pub trait AccuracyModel {
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
    ) -> Option<ResultGuarantee>;

    /// Compare the dimensions requested by `target`. Unknown required
    /// dimensions fail; selection separately excludes missing accuracy evidence.
    fn satisfies(&self, guarantee: &ResultGuarantee, target: &AccuracyTarget) -> bool;
}

/// The built-in estimator models, with conservative target checks.
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
    ) -> Option<ResultGuarantee> {
        estimators::sketch_guarantee(algorithm, params, query)
    }
}

impl AccuracyModel for DefaultAccuracyModel {
    fn local_guarantee(
        &self,
        family: &FieldDataType,
        query: &SketchStatistic,
    ) -> Option<ResultGuarantee> {
        estimators::local_guarantee(family, query)
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

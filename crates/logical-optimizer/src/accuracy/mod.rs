//! Planner accuracy interfaces and model dispatch.
//!
//! Estimator models derive local guarantees, and evidence supplies scoped
//! contracts. Unknown evidence may retain a candidate but does not authorize
//! selection. See `docs/design_docs/concepts/accuracy-models.md` for the
//! design.

pub mod estimators;
pub mod evidence;

pub use evidence::{
    AccuracyEvidenceProvider, EstimatorContract, NoAccuracyEvidence, PropagationStats,
    QuantileInputDomain, WorkloadAccuracyEvidence,
};

use asap_types::ir::properties::{
    BoundExpr, CompositionOperator, ErrorMetric, GuaranteeSource, ProbabilityExpr, ResultGuarantee,
};
use asap_types::ir::schema::{FieldDataType, SketchAlgorithm, SketchParams, SketchStatistic};
use asap_types::ir::OperatorNode;
use asap_types::types::AccuracyTarget;

/// The deployment-extensible accuracy model. `asap-logical-optimizer` ships
/// [`DefaultAccuracyModel`]; a deployment with its own error model for a
/// family implements this trait and passes it to Stage 3.
pub trait AccuracyModel {
    /// The guarantee of reading `query` out of a summary of family `family`
    /// built over an **exact** input — derived from the family's committed
    /// parameters by inverting the same sizing formulas
    /// [`crate::pass1::realization::default_size_params`] uses. `None` when this
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

    /// Whether `guarantee` bounds the error a target on `statistic` is
    /// stated in, so that [`Self::satisfies`] compares like with like. An
    /// exact guarantee answers every statistic; otherwise the metric must be
    /// one the statistic's ε is measured in. A deployment with a registered
    /// cross-metric conversion overrides this.
    fn answers(&self, statistic: &SketchStatistic, guarantee: &ResultGuarantee) -> bool {
        use ErrorMetric::*;
        guarantee.is_exact()
            || match statistic {
                SketchStatistic::Quantile { .. } => {
                    matches!(guarantee.metric, Rank | RelativeValue)
                }
                SketchStatistic::Cardinality => {
                    matches!(guarantee.metric, Cardinality | RelativeValue)
                }
                SketchStatistic::FrequencyL2 | SketchStatistic::FrequencyEntropy => {
                    guarantee.metric == RelativeValue
                }
                SketchStatistic::PointCount { .. } => {
                    matches!(guarantee.metric, Frequency | L2Frequency)
                }
                // A top-k target bounds the item scores, as Pass 1 checks.
                SketchStatistic::TopK { .. } => {
                    matches!(guarantee.metric, Frequency | L2Frequency | TopKMembership)
                }
            }
    }
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

    /// A bound in another statistic's metric does not answer a target:
    /// Count-Min's L1 frequency bound says nothing about a distinct count.
    #[test]
    fn answers_requires_the_statistics_metric() {
        let frequency = ResultGuarantee {
            metric: ErrorMetric::Frequency,
            ..abs(0.01, 0.01)
        };
        let count = SketchStatistic::PointCount {
            key: asap_types::ir::scalar::ColumnRef::SampleValue,
            value: None,
        };
        assert!(DefaultAccuracyModel.answers(&count, &frequency));
        assert!(!DefaultAccuracyModel.answers(&SketchStatistic::Cardinality, &frequency));
        assert!(!DefaultAccuracyModel.answers(&SketchStatistic::Quantile { q: 0.5 }, &frequency));
        assert!(!DefaultAccuracyModel.answers(&SketchStatistic::FrequencyL2, &abs(0.01, 0.01)));
        assert!(DefaultAccuracyModel
            .answers(&SketchStatistic::Cardinality, &ResultGuarantee::exact("x")));
    }
}

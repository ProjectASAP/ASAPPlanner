//! The analytical accuracy of each summary family.
//!
//! Estimator models derive local guarantees ([`local_guarantee`]) and the
//! conservative target check ([`satisfies`]); Stage 3's accuracy model
//! (`asap_plan_selection::DefaultAccuracyModel`) delegates to them. Evidence
//! supplies scoped contracts. Unknown evidence may retain a candidate but does
//! not authorize selection. See `docs/design_docs/concepts/accuracy-models.md`
//! for the design.

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

pub use estimators::{local_guarantee, sketch_guarantee};

/// Whether `guarantee` bounds the error a target on `statistic` is stated
/// in, so that [`satisfies`] compares like with like. An exact guarantee
/// answers every statistic; otherwise the metric must be one the statistic's
/// ε is measured in.
pub fn answers(statistic: &SketchStatistic, guarantee: &ResultGuarantee) -> bool {
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

/// Small relative tolerance for comparing an evaluated bound against a
/// target, so a parameter sized by `⌈·⌉` to *exactly* meet ε is not rejected
/// by floating-point noise.
const SATISFACTION_TOLERANCE: f64 = 1e-9;

/// Whether `guarantee` meets every dimension `target` requests, conservatively:
/// an unknown required dimension fails, and only an exact guarantee meets
/// `Exact`.
pub fn satisfies(guarantee: &ResultGuarantee, target: &AccuracyTarget) -> bool {
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
        assert!(!satisfies(&unknown, &AccuracyTarget::Epsilon(1.0)));
        assert!(!satisfies(&abs(0.0, 0.01), &AccuracyTarget::Exact));
        assert!(satisfies(
            &ResultGuarantee::exact("x"),
            &AccuracyTarget::Exact
        ));
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
        assert!(answers(&count, &frequency));
        assert!(!answers(&SketchStatistic::Cardinality, &frequency));
        assert!(!answers(&SketchStatistic::Quantile { q: 0.5 }, &frequency));
        assert!(!answers(&SketchStatistic::FrequencyL2, &abs(0.01, 0.01)));
        assert!(answers(
            &SketchStatistic::Cardinality,
            &ResultGuarantee::exact("x")
        ));
    }
}

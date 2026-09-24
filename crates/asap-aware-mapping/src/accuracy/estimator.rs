//! Source contracts are evidence; sizing and guarantees remain Planner-owned.
use super::*;
use asap_types::post_asap::GroupingStrategy;

/// A trusted source assertion scoped by `AccuracyEvidenceProvider` to one
/// complete readout. Choosing this variant asserts the estimator and hash
/// assumptions; it must not be inferred from sampled population statistics.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EstimatorContract {
    /// Classic HLL with independent uniform bucket hashing, including merged panes.
    ClassicHll { max_distinct_per_readout: u32 },
}

pub(crate) struct EstimatorAccuracy<'a> {
    base: &'a dyn AccuracyModel,
    contract: Option<EstimatorContract>,
    epsilon: f64,
    delta: f64,
}

impl<'a> EstimatorAccuracy<'a> {
    pub(crate) fn new(
        base: &'a dyn AccuracyModel,
        contract: Option<EstimatorContract>,
        target: Option<&AccuracyTarget>,
    ) -> Self {
        let (epsilon, delta) = target
            .map(crate::replacement::accuracy_budget)
            .unwrap_or((0.0, 0.0));
        Self {
            base,
            contract,
            epsilon,
            delta,
        }
    }

    fn hll(&self) -> Option<hll::ClassicHllConfidence> {
        let EstimatorContract::ClassicHll {
            max_distinct_per_readout,
        } = self.contract?;
        hll::ClassicHllConfidence::new(max_distinct_per_readout, self.epsilon)
    }

    pub(crate) fn size_params(&self, algorithm: &SketchAlgorithm) -> Option<SketchParams> {
        if *algorithm != SketchAlgorithm::Hll || self.contract.is_none() {
            return None;
        }
        // Retain the strongest supported parameter for diagnostics if sizing
        // is infeasible. The normal guarantee check rejects it below.
        Some(SketchParams::Hll {
            precision: self
                .hll()
                .and_then(|model| model.precision(self.delta))
                .unwrap_or(18),
        })
    }
}

impl AccuracyModel for EstimatorAccuracy<'_> {
    fn exact_operation_rule(&self, operation: &ExactOperation) -> Option<CompositionOperator> {
        self.base.exact_operation_rule(operation)
    }
    fn local_guarantee(
        &self,
        family: &SummaryFamilyType,
        query: &SketchQuery,
    ) -> Option<ResultGuarantee> {
        if let (Some(_), SummaryFamilyType::Sketch(kind, grouping), SketchQuery::Cardinality) =
            (self.contract, family, query)
        {
            if let (SketchAlgorithm::Hll, SketchParams::Hll { precision }) =
                (kind.algorithm(), kind.params())
            {
                if *grouping != GroupingStrategy::PerSubpopulationInstance {
                    return None;
                }
                return self.hll()?.guarantee(*precision);
            }
        }
        self.base.local_guarantee(family, query)
    }
    fn propagate(
        &self,
        op: &CompositionOperator,
        inputs: &[ResultGuarantee],
        local: Option<&ResultGuarantee>,
        stats: &PropagationStats,
    ) -> Result<ResultGuarantee, AccuracyError> {
        self.base.propagate(op, inputs, local, stats)
    }
    fn satisfies(&self, guarantee: &ResultGuarantee, target: &AccuracyTarget) -> bool {
        self.base.satisfies(guarantee, target)
    }
}

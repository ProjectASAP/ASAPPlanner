//! Scoped source contracts and evidence required by accuracy rules.
use super::*;

/// A trusted source assertion scoped by `AccuracyEvidenceProvider` to one
/// complete readout. Choosing this variant asserts the estimator and hash
/// assumptions; it must not be inferred from sampled population statistics.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EstimatorContract {
    /// Classic HLL with independent uniform bucket hashing, including merged panes.
    ClassicHll { max_distinct_per_readout: u32 },
}

/// An enforced domain for every sample of a direct quantile operand, in every
/// evaluation window. The provider promises a nonempty population containing
/// only finite values in this interval. Sampled min/max statistics are not a
/// proof: the contract must be enforced by the source or execution layer.
#[derive(Debug, Clone, PartialEq)]
pub struct QuantileInputDomain {
    pub lower: f64,
    pub upper: f64,
    /// Upper bound on samples per evaluation, matching the pinned readout's
    /// exact Float64 rank limit. The population must also be nonempty.
    pub max_samples: u64,
    pub contract: String,
}

impl QuantileInputDomain {
    pub(crate) fn supports_ddsketch(&self, alpha: f64) -> bool {
        if !alpha.is_finite()
            || alpha <= 0.0
            || alpha >= 1.0
            || !self.lower.is_finite()
            || !self.upper.is_finite()
            || self.lower > self.upper
            || self.max_samples == 0
            || self.max_samples > (1u64 << 53)
            || self.contract.trim().is_empty()
        {
            return false;
        }
        let (min, max) = asap_sketchlib::sketches::ddsketch::ddsketch_indexable_bounds(alpha);
        // Same-sign interpolation preserves relative error. Zero alone is
        // exact; an interval touching zero also admits tiny zero-mapped values.
        (self.lower >= min && self.upper <= max)
            || (self.upper <= -min && self.lower >= -max)
            || (self.lower == 0.0 && self.upper == 0.0)
    }
}

/// Statistics a propagation rule may consult. Every field is optional and
/// defaults to "unknown": a rule that needs a missing statistic emits a
/// [`BoundExpr::Unknown`] leaf (or rejects) rather than guessing.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct PropagationStats {
    /// Certified finite ranges for the true numerator and denominator values.
    /// Quantile ratios obtain these from their enforced input domains.
    pub division_operand_domains: Option<[QuantileInputDomain; 2]>,
    /// Provenance for supplied evidence (source, observation identity, etc.).
    pub evidence_provenance: Vec<GuaranteeSource>,
    /// Whether every input value is known to be non-negative — required by
    /// the multiplicative relative-error rule, which is unsound across a
    /// sign change.
    pub values_non_negative: Option<bool>,
    /// Number of input rows an exact aggregation consumes (e.g. the number
    /// of groups a function folds), for exact aggregate union bounds
    /// bound over per-input failures.
    pub input_row_count: Option<u64>,
    /// Fresh key-frequency distribution evidence from the data workload.
    /// Built-in rules preserve it for deployment-specific accuracy models;
    /// they do not assume a favorable distribution when it is absent.
    pub data_distribution: Option<asap_types::workload::DataDistribution>,
    /// Lower confidence bound of the kth selected TopK item, after widening
    /// the interval by the sketch's own estimation error.
    pub topk_selected_lower_bound: Option<f64>,
    /// Greatest upper confidence bound among excluded TopK items, after
    /// widening the interval by the sketch's own estimation error.
    pub topk_excluded_upper_bound: Option<f64>,
    /// Union-bound failure probability of all intervals used by the margin
    /// certificate.
    pub topk_interval_failure_probability: Option<f64>,
    /// Hydra shared-grid collision error in the inner guarantee's metric.
    pub hydra_shared_grid_collision_bound: Option<f64>,
    /// Failure probability assigned to the Hydra shared-grid term.
    pub hydra_shared_grid_failure_probability: Option<f64>,
}

/// Supplies typed planning-time evidence required by propagation rules.
pub trait AccuracyEvidenceProvider {
    /// Trusted estimator contract for this complete aggregate expression,
    /// including source, filters, grouping and all panes in each readout.
    /// An observed cardinality is not an enforced population bound.
    fn estimator_contract(
        &self,
        _expression: &asap_types::pre_asap::QueryExpr,
    ) -> Option<EstimatorContract> {
        None
    }

    /// Proof scoped to this complete quantile expression, including its source,
    /// filters, grouping and window. `None` means unknown, including emptiness.
    fn quantile_input_domain(
        &self,
        _operand: &asap_types::pre_asap::query_expr::QueryExpr,
    ) -> Option<QuantileInputDomain> {
        None
    }

    fn propagation_stats(
        &self,
        _op: &CompositionOperator,
        _family: &SummaryFamilyType,
        _query: Option<&SketchQuery>,
    ) -> PropagationStats {
        PropagationStats::default()
    }
}

#[derive(Debug, Default, Clone, Copy)]
pub struct NoAccuracyEvidence;

impl AccuracyEvidenceProvider for NoAccuracyEvidence {}

/// Accuracy evidence backed by the normalized data workload. Freshness is
/// checked at the planning time before values reach any accuracy rule.
#[derive(Debug, Clone, Copy)]
pub struct WorkloadAccuracyEvidence<'a> {
    pub data: &'a asap_types::workload::DataWorkload,
    pub now_ms: u64,
}

impl AccuracyEvidenceProvider for WorkloadAccuracyEvidence<'_> {
    fn propagation_stats(
        &self,
        _op: &CompositionOperator,
        _family: &SummaryFamilyType,
        _query: Option<&SketchQuery>,
    ) -> PropagationStats {
        PropagationStats {
            input_row_count: self.data.input_cardinality.value_at(self.now_ms).copied(),
            data_distribution: self.data.distribution.value_at(self.now_ms).cloned(),
            ..PropagationStats::default()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use asap_types::workload::{DataDistribution, DataWorkload, Evidence, EvidenceSource};
    #[test]
    fn workload_accuracy_evidence_uses_only_fresh_data_characteristics() {
        let data = DataWorkload {
            input_cardinality: Evidence {
                value: Some(42),
                source: EvidenceSource::Observed,
                observed_at_ms: Some(1_000),
                valid_for_ms: Some(500),
            },
            distribution: Evidence {
                value: Some(DataDistribution::Bursty),
                source: EvidenceSource::Observed,
                observed_at_ms: Some(1_000),
                valid_for_ms: Some(500),
            },
            ..Default::default()
        };
        let provider = WorkloadAccuracyEvidence {
            data: &data,
            now_ms: 1_500,
        };
        let fresh = provider.propagation_stats(
            &CompositionOperator::ExactSum,
            &SummaryFamilyType::ExactAggregate(
                asap_types::post_asap::ExactKind::Sum,
                asap_types::post_asap::ExactParams::Sum,
            ),
            None,
        );
        assert_eq!(fresh.input_row_count, Some(42));
        assert_eq!(fresh.data_distribution, Some(DataDistribution::Bursty));

        let stale = WorkloadAccuracyEvidence {
            data: &data,
            now_ms: 1_501,
        }
        .propagation_stats(
            &CompositionOperator::ExactSum,
            &SummaryFamilyType::ExactAggregate(
                asap_types::post_asap::ExactKind::Sum,
                asap_types::post_asap::ExactParams::Sum,
            ),
            None,
        );
        assert_eq!(stale.input_row_count, None);
        assert_eq!(stale.data_distribution, None);
    }
}

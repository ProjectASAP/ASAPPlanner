//! $\alpha^{\log}$ of a logical edge
//! ([`ErrorMetricsOfMemo::edge_meets_logical_requirements`]), and the
//! comparison of an evaluated bound with a required limit
//! ([`within_limit`]).

use asap_types::accuracy::AccuracyMetric;
use asap_types::ir::{ASAPOp, Operator};
use asap_types::stages::{CandidateForMemoGroupId, Memo, RootPruneReason};
use asap_types::summary_type::{MergedError, SketchParams, SketchStatistic, SummaryTypeCapability};

use crate::guarantee::{guarantee_before_failure_probability_check, holds_at_failure_probability};
use crate::readout::sketch_readout;
use crate::{AccuracyEvidence, ErrorMetricsOfMemo};

/// Small relative tolerance for comparing an evaluated bound against a
/// target, so a parameter sized by `⌈·⌉` to *exactly* meet ε is not rejected
/// by floating-point noise.
const SATISFACTION_TOLERANCE: f64 = 1e-9;

/// Whether an `evaluated` error (or failure probability) meets a required
/// `limit`: at most `limit` widened by [`SATISFACTION_TOLERANCE`], so a
/// bound equal to it up to floating-point noise meets it.
pub fn within_limit(evaluated: f64, limit: f64) -> bool {
    evaluated <= limit * (1.0 + SATISFACTION_TOLERANCE) + f64::EPSILON
}

impl ErrorMetricsOfMemo {
    /// $\alpha^{\log}$ of one edge (concepts doc, section 2): `Ok` when
    /// `producer` may feed input `input_index_of_consumer` of `consumer`,
    /// else why not, for Pass 2's pruned list. `self` is
    /// [`ErrorMetricsOfMemo::of`] `memo`.
    /// - A readout's input (`SummaryEstimate`) from a sketch state: the
    ///   readout's error metric is one its output allows (the metric rule;
    ///   no rule where no query with a target reaches it; `NoGuarantee`
    ///   when, under such a query, the readout has no metric or its
    ///   guarantee's failure probability is unknown, as HLL's), and the
    ///   guarantee of the producer's θ for the readout's statistic, at the
    ///   required failure probability, is within the required error
    ///   ([`within_limit`]) and in its metric: the deployment's measurement
    ///   (`evidence`) when it has one, else the theoretical guarantee; a
    ///   merged state (any producer but a build) must keep the guarantee
    ///   (`MergedError::Exact` or `AsIfBuiltOverUnion`). A top-k's heap
    ///   holds its `k` whatever the requirement.
    /// - Any other input: the metric rule, when the producer is a sketch
    ///   readout: the consumer allows the readout's error metric on that
    ///   input. Only readouts introduce error, so an edge from any other
    ///   producer holds; a consumer reading an approximate value through it
    ///   is cut below, on the readout's edge.
    pub fn edge_meets_logical_requirements(
        &self,
        memo: &Memo,
        producer: CandidateForMemoGroupId,
        consumer: CandidateForMemoGroupId,
        input_index_of_consumer: usize,
        evidence: &dyn AccuracyEvidence,
    ) -> Result<(), RootPruneReason> {
        let consumer_candidate = memo.candidate(consumer);
        let input = input_index_of_consumer;
        let metric_rule = |producer_metric: AccuracyMetric, allowed: &[AccuracyMetric]| {
            if allowed.contains(&producer_metric) {
                Ok(())
            } else {
                Err(RootPruneReason::MetricRule {
                    consumer,
                    input,
                    producer_metric,
                    accepted_metrics: allowed.to_vec(),
                })
            }
        };
        let Operator::ASAP(ASAPOp::SummaryEstimate { .. }) = consumer_candidate.expression.operator
        else {
            let Some(producer_metric) = self.readout_error_metric(producer) else {
                return Ok(());
            };
            return match self.error_metrics_allowed_on_inputs(consumer) {
                Some(allowed) => metric_rule(producer_metric, &allowed),
                None => Ok(()),
            };
        };
        let producer_candidate = memo.candidate(producer);
        let (Some((summary_type, statistic)), Some(parameters)) = (
            sketch_readout(memo, consumer_candidate),
            producer_candidate.logical_properties.parameters(),
        ) else {
            // An exact state reads exactly.
            return Ok(());
        };
        let summary_type = &summary_type;
        if let (SketchStatistic::TopK { k }, Some(heap_size)) = (statistic, heap_size(parameters)) {
            if (heap_size as usize) < *k {
                return Err(RootPruneReason::HeapSmallerThanTopK {
                    consumer,
                    input,
                    k: *k,
                });
            }
        }
        let required = consumer_candidate.input_requirements[input].required_accuracy;
        let failure_probability = required.map_or(1.0, |required| required.failure_probability);
        let measured =
            evidence.measured_guarantee(summary_type, parameters, statistic, failure_probability);
        let allowed = self.error_metrics_allowed_on_inputs(consumer);
        // The theoretical guarantee before its failure-probability check,
        // computed once for both checks below.
        let theoretical = (measured.is_none() && (allowed.is_some() || required.is_some()))
            .then(|| {
                guarantee_before_failure_probability_check(
                    summary_type,
                    parameters,
                    statistic,
                    failure_probability,
                )
            })
            .flatten();
        if let Some(allowed) = allowed {
            // A query with a target reads this readout: its error needs a
            // metric and a failure probability a guarantee states.
            let no_guarantee = RootPruneReason::NoGuarantee { consumer, input };
            let metric = self
                .readout_error_metric(consumer)
                .ok_or(no_guarantee.clone())?;
            metric_rule(metric, &allowed)?;
            let evaluable = measured.is_some()
                || theoretical.as_ref().is_some_and(|guarantee| {
                    guarantee.spec.failure_probability.evaluate().is_some()
                });
            if !evaluable {
                return Err(no_guarantee);
            }
        }
        let Some(required) = required else {
            return Ok(());
        };
        let merged = !matches!(
            producer_candidate.expression.operator,
            Operator::ASAP(ASAPOp::SummaryAgg { .. } | ASAPOp::SummaryTumblingWindowChain { .. })
        );
        let keeps = !merged
            || matches!(
                SummaryTypeCapability::of(summary_type).merged_error(statistic),
                Some(MergedError::Exact | MergedError::AsIfBuiltOverUnion)
            );
        let guarantee = measured.or_else(|| {
            theoretical.filter(|guarantee| {
                holds_at_failure_probability(guarantee, required.failure_probability)
            })
        });
        let meets = guarantee.is_some_and(|guarantee| {
            guarantee.spec.accuracy_metric == required.metric
                && guarantee
                    .spec
                    .error
                    .evaluate()
                    .is_some_and(|error| within_limit(error, required.max_error))
        });
        if keeps && meets {
            Ok(())
        } else {
            Err(RootPruneReason::BelowRequiredAccuracy {
                consumer,
                input,
                required,
            })
        }
    }
}

/// A heap variant's heap size.
fn heap_size(parameters: &SketchParams) -> Option<u32> {
    match parameters {
        SketchParams::CmsWithHeap { heap_size, .. }
        | SketchParams::CountSketchWithHeap { heap_size, .. } => Some(*heap_size),
        _ => None,
    }
}

#[cfg(test)]
mod tests;

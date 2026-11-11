//! Whether a producer meets a consumer's logical requirements:
//! $\alpha^{\log}$ of a logical edge, the metrics each group's output may
//! be off in (the metric rule), a readout's metric, and the limit an
//! evaluated bound is compared against.

#[cfg(test)]
use asap_types::accuracy::AccuracyGuarantee;
use asap_types::accuracy::AccuracyMetric;
use asap_types::ir::operator::asap::summary_type_of_operator;
use asap_types::ir::schema::FieldDataType;
use asap_types::ir::{ASAPOp, Operator, QueryRoot};
use asap_types::stages::{CandidateForMemoGroupId, Memo, MemoGroupId, RootPruneReason};
use asap_types::summary_type::{
    parameter_list, MergedError, SketchParams, SketchStatistic, SummaryTypeCapability,
};

use crate::{error_propagation, theoretical_guarantee, AccuracyEvidence};

/// $\alpha^{\log}$ of one edge (concepts doc, section 2): `Ok` when
/// `producer` may feed input `input_index_of_consumer` of `consumer`, else
/// why not, for Pass 2's pruned list. `accepted` is
/// [`accepted_metrics`] of `memo`.
/// - A readout's input (`SummaryEstimate`) from a sketch state: the
///   readout's metric is one its output accepts (the metric rule; no rule
///   where no query with a target reaches it), and the guarantee of the
///   producer's θ for the readout's statistic, at the required failure
///   probability, is within the required error and in its metric: the
///   deployment's measurement (`evidence`) when it has one, else
///   [`theoretical_guarantee`]; a merged state (any producer but a build)
///   must keep the guarantee (`MergedError::Exact` or
///   `AsIfBuiltOverUnion`). A top-k's heap holds its `k` whatever the
///   requirement.
/// - Any other input: the metric rule, when the producer is a sketch
///   readout: the consumer accepts the readout's metric on that input.
///   Only readouts introduce error, so an edge from any other producer
///   holds; a consumer reading an approximate value through it is cut
///   below, on the readout's edge.
pub fn producer_satisfies_logical_requirements(
    memo: &Memo,
    accepted: &[Option<Vec<AccuracyMetric>>],
    producer: CandidateForMemoGroupId,
    consumer: CandidateForMemoGroupId,
    input_index_of_consumer: usize,
    evidence: &dyn AccuracyEvidence,
) -> Result<(), RootPruneReason> {
    let consumer_candidate = memo.candidate(consumer);
    let input = input_index_of_consumer;
    let metric_rule = |producer_metric: AccuracyMetric, accepted_metrics: Vec<AccuracyMetric>| {
        if accepted_metrics.contains(&producer_metric) {
            Ok(())
        } else {
            Err(RootPruneReason::MetricRule {
                consumer,
                input,
                producer_metric,
                accepted_metrics,
            })
        }
    };
    let operator = &consumer_candidate.expression.operator;
    let Operator::ASAP(ASAPOp::SummaryEstimate {
        summary_input,
        query: statistic,
    }) = operator
    else {
        let (Some(producer_metric), Some(accepted)) = (
            sketch_readout_metric(memo, producer),
            &accepted[consumer.group.0 as usize],
        ) else {
            return Ok(());
        };
        let on_input = accepted
            .iter()
            .copied()
            .filter(|&metric| error_propagation(memo, operator, metric).is_some())
            .collect();
        return metric_rule(producer_metric, on_input);
    };
    let summary_type =
        summary_type_of_operator(&memo.groups()[summary_input.0 as usize].expression);
    let producer_candidate = memo.candidate(producer);
    let (Some(summary_type @ FieldDataType::Sketch(_)), Some(parameters)) = (
        summary_type,
        producer_candidate.logical_properties.parameters(),
    ) else {
        // An exact state reads exactly.
        return Ok(());
    };
    if let (SketchStatistic::TopK { k }, Some(heap_size)) = (statistic, heap_size(parameters)) {
        if (heap_size as usize) < *k {
            return Err(RootPruneReason::HeapSmallerThanTopK {
                consumer,
                input,
                k: *k,
            });
        }
    }
    if let (Some(metric), Some(accepted)) = (
        readout_metric(summary_type, statistic),
        &accepted[consumer.group.0 as usize],
    ) {
        metric_rule(metric, accepted.clone())?;
    }
    let Some(required) = consumer_candidate.input_requirements[input].required_accuracy else {
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
    let guarantee = evidence
        .measured_guarantee(
            summary_type,
            parameters,
            statistic,
            required.failure_probability,
        )
        .or_else(|| {
            theoretical_guarantee(
                summary_type,
                parameters,
                statistic,
                required.failure_probability,
            )
        });
    let meets = guarantee.is_some_and(|guarantee| {
        guarantee.spec.accuracy_metric == required.metric
            && guarantee
                .spec
                .error
                .evaluate()
                .is_some_and(|error| error <= target_limit(required.max_error))
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

/// Per group, by group id, the metrics its output may be off in for the
/// queries with an accuracy target that reach it (concepts doc, section 4,
/// "Metric rule"): a query root accepts its target's metric (every metric
/// when the target names none); a candidate's input accepts the metrics
/// its output accepts that its error contract ([`error_propagation`])
/// carries; a group accepts the union over its consumers. `None` for a
/// group no such query reaches: no metric rule applies there (accuracy not
/// enforced).
pub fn accepted_metrics(memo: &Memo) -> Vec<Option<Vec<AccuracyMetric>>> {
    let mut accepted: Vec<Option<Vec<AccuracyMetric>>> = vec![None; memo.groups().len()];
    let mut pending: Vec<MemoGroupId> = Vec::new();
    for query in memo.queries() {
        let (Some(target), QueryRoot::Operator(root)) = (&query.accuracy_target, &query.root)
        else {
            continue;
        };
        let metrics: Vec<AccuracyMetric> = AccuracyMetric::ALL
            .into_iter()
            .filter(|&metric| target.accuracy_metric.is_none_or(|named| named == metric))
            .collect();
        if add_metrics(&mut accepted[root.0 as usize], &metrics) {
            pending.push(*root);
        }
    }
    while let Some(group) = pending.pop() {
        let metrics = accepted[group.0 as usize].clone().unwrap_or_default();
        for candidate in &memo.groups()[group.0 as usize].candidates {
            let operator = &candidate.expression.operator;
            let carried: Vec<AccuracyMetric> = metrics
                .iter()
                .copied()
                .filter(|&metric| error_propagation(memo, operator, metric).is_some())
                .collect();
            for &child in operator.children() {
                if add_metrics(&mut accepted[child.0 as usize], &carried) {
                    pending.push(child);
                }
            }
        }
    }
    accepted
}

/// Adds `metrics` to a group's accepted metrics (`None`: not yet reached);
/// whether they changed.
fn add_metrics(known: &mut Option<Vec<AccuracyMetric>>, metrics: &[AccuracyMetric]) -> bool {
    let mut changed = known.is_none();
    let known = known.get_or_insert_with(Vec::new);
    for &metric in metrics {
        if !known.contains(&metric) {
            known.push(metric);
            changed = true;
        }
    }
    changed
}

/// The metric of the error reading `statistic` from a state of
/// `summary_type` has: its first listed θ's guarantee's. `None` for an
/// exact state and for a summary type with no listed θ or no guarantee
/// for the statistic.
pub fn readout_metric(
    summary_type: &FieldDataType,
    statistic: &SketchStatistic,
) -> Option<AccuracyMetric> {
    let FieldDataType::Sketch(algorithm) = summary_type else {
        return None;
    };
    let parameters = parameter_list(*algorithm).first()?;
    Some(
        theoretical_guarantee(summary_type, parameters, statistic, 1.0)?
            .spec
            .accuracy_metric,
    )
}

/// A heap variant's heap size.
fn heap_size(parameters: &SketchParams) -> Option<u32> {
    match parameters {
        SketchParams::CmsWithHeap { heap_size, .. }
        | SketchParams::CountSketchWithHeap { heap_size, .. } => Some(*heap_size),
        _ => None,
    }
}

/// The metric of `producer`'s error when it is a readout of a sketch state
/// ([`readout_metric`]); `None` for any other candidate.
fn sketch_readout_metric(memo: &Memo, producer: CandidateForMemoGroupId) -> Option<AccuracyMetric> {
    let Operator::ASAP(ASAPOp::SummaryEstimate {
        summary_input,
        query,
    }) = &memo.candidate(producer).expression.operator
    else {
        return None;
    };
    readout_metric(
        summary_type_of_operator(&memo.groups()[summary_input.0 as usize].expression)?,
        query,
    )
}

/// Small relative tolerance for comparing an evaluated bound against a
/// target, so a parameter sized by `⌈·⌉` to *exactly* meet ε is not rejected
/// by floating-point noise.
const SATISFACTION_TOLERANCE: f64 = 1e-9;

/// The largest evaluated error (or failure probability) that meets a
/// required `limit`: `limit` widened by [`SATISFACTION_TOLERANCE`], so a
/// bound equal to it up to floating-point noise meets it.
pub fn target_limit(limit: f64) -> f64 {
    limit * (1.0 + SATISFACTION_TOLERANCE) + f64::EPSILON
}

/// Whether `guarantee`'s evaluated error is within `max_error` and, when
/// `failure_probability` is set, its evaluated failure probability within
/// it ([`target_limit`]); unknown values do not meet.
#[cfg(test)]
pub(crate) fn meets(
    guarantee: &AccuracyGuarantee,
    max_error: f64,
    failure_probability: Option<f64>,
) -> bool {
    let within = |value: Option<f64>, limit: f64| value.is_some_and(|v| v <= target_limit(limit));
    within(guarantee.spec.error.evaluate(), max_error)
        && failure_probability
            .is_none_or(|delta| within(guarantee.spec.failure_probability.evaluate(), delta))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A bound equal to the limit up to floating-point noise meets it; a
    /// bound one part in a million above it does not.
    #[test]
    fn target_limit_admits_only_floating_point_noise() {
        assert!(0.01 * (1.0 + 1e-12) <= target_limit(0.01));
        assert!(0.01 * (1.0 + 1e-6) > target_limit(0.01));
        assert!(0.0 <= target_limit(0.0));
    }
}

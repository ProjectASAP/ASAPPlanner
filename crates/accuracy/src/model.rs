//! The deployment's measured accuracy, and α^log of a logical edge.

use std::collections::HashMap;

use crate::{target_limit, theoretical_guarantee};
use asap_types::ir::operator::asap::summary_type_of_operator;
use asap_types::ir::properties::{AccuracyGuarantee, AccuracyMetric};
use asap_types::ir::schema::{
    FieldDataType, MergedError, SketchParams, SketchStatistic, SummaryTypeCapability,
};
use asap_types::ir::{ASAPOp, Operator, OperatorResultKind};
use asap_types::stages::{
    parameter_list, CandidateForMemoGroupId, LogicalRequirements, Memo, MemoGroupId,
    RootPruneReason,
};

/// The deployment's measured accuracy, per summary configuration. A
/// producer's guarantee for a readout is its measurement when there is one,
/// else [`theoretical_guarantee`](crate::estimators::theoretical_guarantee);
/// the source is decided before Pass 2 builds the logical edges (concepts
/// doc, section 2, "Where the guarantee comes from").
pub trait AccuracyEvidence {
    /// The measured error bound of reading `statistic` from a state of
    /// `summary_type` with parameters `parameters` at failure probability
    /// `failure_probability` (the guarantee's own failure probability);
    /// `None` when the deployment has no measurement for it, or when this θ
    /// cannot fail that rarely.
    fn measured_guarantee(
        &self,
        summary_type: &FieldDataType,
        parameters: &SketchParams,
        statistic: &SketchStatistic,
        failure_probability: f64,
    ) -> Option<AccuracyGuarantee>;
}

/// Evidence with no measurement: every guarantee is the theoretical one.
#[derive(Debug, Clone, Copy)]
pub struct NoMeasuredGuarantees;

impl AccuracyEvidence for NoMeasuredGuarantees {
    fn measured_guarantee(
        &self,
        _summary_type: &FieldDataType,
        _parameters: &SketchParams,
        _statistic: &SketchStatistic,
        _failure_probability: f64,
    ) -> Option<AccuracyGuarantee> {
        None
    }
}

/// $\alpha^{\log}$ of one edge (concepts doc, section 2): `Ok` when
/// `producer` may feed input `input_index_of_consumer` of `consumer`, else
/// why not, for Pass 2's pruned list.
/// - A readout's input (`SummaryEstimate`): every θ the producer's state
///   carries ([`carried_parameters`]: a build's own, a merge's those of the
///   builds below it) has a guarantee for the readout's statistic, at the
///   required failure probability, within the required error and in its
///   metric: the deployment's measurement (`evidence`) when it has one,
///   else [`theoretical_guarantee`]; a merge must keep the guarantee
///   (`MergedError::Exact` or `AsIfBuiltOverUnion`). A top-k's heap holds
///   its `k` whatever the requirement.
/// - Any other input: the metric rule, when a query with an accuracy target
///   reaches the consumer: the consumer's error contract
///   (`LogicalRequirements::error_propagation`) carries one of the metrics
///   the producer's output may be off in ([`output_metrics`]); an exact
///   producer always may.
pub fn producer_satisfies_logical_requirements(
    memo: &Memo,
    producer: CandidateForMemoGroupId,
    consumer: CandidateForMemoGroupId,
    input_index_of_consumer: usize,
    evidence: &dyn AccuracyEvidence,
) -> Result<(), RootPruneReason> {
    let consumer_candidate = memo.candidate(consumer);
    let input = input_index_of_consumer;
    match &consumer_candidate.expression.operator {
        Operator::ASAP(ASAPOp::SummaryEstimate {
            summary_input,
            query: statistic,
        }) => {
            let required = consumer_candidate.input_requirements[input].required_accuracy;
            let summary_type =
                summary_type_of_operator(&memo.groups()[summary_input.0 as usize].expression);
            for (parameters, merged) in carried_parameters(memo, producer) {
                let (Some(summary_type @ FieldDataType::Sketch(_)), Some(parameters)) =
                    (summary_type, parameters)
                else {
                    // An exact state reads exactly.
                    continue;
                };
                if let (SketchStatistic::TopK { k }, Some(heap_size)) =
                    (statistic, heap_size(parameters))
                {
                    if (heap_size as usize) < *k {
                        return Err(RootPruneReason::HeapSmallerThanTopK {
                            consumer,
                            input,
                            k: *k,
                        });
                    }
                }
                let Some(required) = required else {
                    continue;
                };
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
                if !(keeps && meets) {
                    return Err(RootPruneReason::BelowRequiredAccuracy {
                        consumer,
                        input,
                        required,
                    });
                }
            }
            Ok(())
        }
        operator => {
            let targeted = memo.groups()[consumer.group.0 as usize]
                .reached_by_queries()
                .iter()
                .any(|&query| memo.queries[query].accuracy_target.is_some());
            let metrics = output_metrics(memo, producer, &mut HashMap::new());
            if !targeted || metrics.is_empty() {
                return Ok(());
            }
            let consumer_carries: Vec<AccuracyMetric> = AccuracyMetric::ALL
                .into_iter()
                .filter(|&metric| {
                    LogicalRequirements::error_propagation(memo, operator, metric).is_some()
                })
                .collect();
            if metrics
                .iter()
                .any(|metric| consumer_carries.contains(metric))
            {
                Ok(())
            } else {
                Err(RootPruneReason::MetricRule {
                    consumer,
                    input,
                    producer_metric: metrics[0],
                    consumer_carries,
                })
            }
        }
    }
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

/// The θ a state candidate's rows carry, each with whether a merge combined
/// them: a build's own θ; for a merge, or a `Filter`, `TimeRange` or
/// `TimeShift` over states, those of every candidate of the groups it
/// reads. A merged state has its inputs' θ.
fn carried_parameters(
    memo: &Memo,
    producer: CandidateForMemoGroupId,
) -> Vec<(Option<&SketchParams>, bool)> {
    let candidate = memo.candidate(producer);
    let operator = &candidate.expression.operator;
    let merged = matches!(operator, Operator::ASAP(ASAPOp::SummaryMerge { .. }));
    match operator {
        Operator::ASAP(ASAPOp::SummaryAgg { .. } | ASAPOp::SummaryTumblingWindowChain { .. }) => {
            vec![(candidate.logical_properties.parameters(), false)]
        }
        _ => operator
            .children()
            .into_iter()
            .flat_map(|&group| {
                (0..memo.groups()[group.0 as usize].candidates.len()).flat_map(
                    move |index_in_group| {
                        carried_parameters(
                            memo,
                            CandidateForMemoGroupId {
                                group,
                                index_in_group,
                            },
                        )
                        .into_iter()
                        .map(move |(parameters, below)| (parameters, below || merged))
                    },
                )
            })
            .collect(),
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

/// The metrics `producer`'s output may be off in: a sketch readout's
/// ([`readout_metric`]); for any other value, the metrics its inputs'
/// producers may be off in that its error contract carries with a nonzero
/// factor. None for a state (its error enters at the readout above it) and
/// for an exact result. `by_group` caches each group's union over its
/// candidates.
fn output_metrics(
    memo: &Memo,
    producer: CandidateForMemoGroupId,
    by_group: &mut HashMap<MemoGroupId, Vec<AccuracyMetric>>,
) -> Vec<AccuracyMetric> {
    let operator = &memo.candidate(producer).expression.operator;
    if memo.groups()[producer.group.0 as usize]
        .group_properties
        .result_kind
        == OperatorResultKind::State
    {
        return Vec::new();
    }
    if let Operator::ASAP(ASAPOp::SummaryEstimate {
        summary_input,
        query,
    }) = operator
    {
        return summary_type_of_operator(&memo.groups()[summary_input.0 as usize].expression)
            .and_then(|summary_type| readout_metric(summary_type, query))
            .into_iter()
            .collect();
    }
    let mut metrics = Vec::new();
    for (input, &group) in operator.children().into_iter().enumerate() {
        for metric in group_output_metrics(memo, group, by_group) {
            let carried = LogicalRequirements::error_propagation(memo, operator, metric)
                .is_some_and(|propagation| propagation.factor_per_input[input] > 0.0);
            if carried && !metrics.contains(&metric) {
                metrics.push(metric);
            }
        }
    }
    metrics
}

/// [`output_metrics`] of every candidate of `group`, once per group.
fn group_output_metrics(
    memo: &Memo,
    group: MemoGroupId,
    by_group: &mut HashMap<MemoGroupId, Vec<AccuracyMetric>>,
) -> Vec<AccuracyMetric> {
    if let Some(metrics) = by_group.get(&group) {
        return metrics.clone();
    }
    // A group reached again through a cycle adds nothing new.
    by_group.insert(group, Vec::new());
    let mut metrics = Vec::new();
    for index_in_group in 0..memo.groups()[group.0 as usize].candidates.len() {
        let id = CandidateForMemoGroupId {
            group,
            index_in_group,
        };
        for metric in output_metrics(memo, id, by_group) {
            if !metrics.contains(&metric) {
                metrics.push(metric);
            }
        }
    }
    by_group.insert(group, metrics.clone());
    metrics
}

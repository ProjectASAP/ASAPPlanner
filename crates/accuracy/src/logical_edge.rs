//! $\alpha^{\log}$ of a logical edge: whether a producer meets a consumer's
//! logical requirements.

use asap_types::accuracy::AccuracyMetric;
use asap_types::ir::operator::asap::summary_type_of_operator;
use asap_types::ir::schema::FieldDataType;
use asap_types::ir::{ASAPOp, Operator};
use asap_types::stages::{CandidateForMemoGroupId, Memo, RootPruneReason};
use asap_types::summary_type::{MergedError, SketchParams, SketchStatistic, SummaryTypeCapability};

use crate::guarantee::guarantee_before_failure_probability_check;
use crate::metric_rule::metrics_carried;
use crate::{
    readout_metric, sketch_readout_metric, target_limit, theoretical_guarantee, AccuracyEvidence,
};

/// $\alpha^{\log}$ of one edge (concepts doc, section 2): `Ok` when
/// `producer` may feed input `input_index_of_consumer` of `consumer`, else
/// why not, for Pass 2's pruned list. `accepted` is
/// [`accepted_metrics`](crate::accepted_metrics) of `memo`.
/// - A readout's input (`SummaryEstimate`) from a sketch state: the
///   readout's metric is one its output accepts (the metric rule; no rule
///   where no query with a target reaches it; `NoGuarantee` when, under
///   such a query, the readout has no metric or its guarantee's failure
///   probability is unknown, as HLL's), and the guarantee of the
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
            sketch_readout_metric(memo, memo.candidate(producer)),
            &accepted[consumer.group.0 as usize],
        ) else {
            return Ok(());
        };
        return metric_rule(producer_metric, metrics_carried(memo, operator, accepted));
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
    let required = consumer_candidate.input_requirements[input].required_accuracy;
    let failure_probability = required.map_or(1.0, |required| required.failure_probability);
    let measured =
        evidence.measured_guarantee(summary_type, parameters, statistic, failure_probability);
    if let Some(accepted) = &accepted[consumer.group.0 as usize] {
        // A query with a target reads this readout: its error needs a
        // metric and a failure probability a guarantee states.
        let no_guarantee = RootPruneReason::NoGuarantee { consumer, input };
        let metric = readout_metric(summary_type, statistic).ok_or(no_guarantee.clone())?;
        metric_rule(metric, accepted.clone())?;
        let evaluable = measured.is_some()
            || guarantee_before_failure_probability_check(
                summary_type,
                parameters,
                statistic,
                failure_probability,
            )
            .is_some_and(|guarantee| guarantee.spec.failure_probability.evaluate().is_some());
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

/// A heap variant's heap size.
fn heap_size(parameters: &SketchParams) -> Option<u32> {
    match parameters {
        SketchParams::CmsWithHeap { heap_size, .. }
        | SketchParams::CountSketchWithHeap { heap_size, .. } => Some(*heap_size),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{readout_metric, NoMeasuredGuarantees};
    use asap_types::ir::operator::arguments::Reduction;
    use asap_types::ir::scalar::ColumnRef;
    use asap_types::stages::{CandidateForMemoGroup, OriginOfCandidateForMemoGroup};
    use asap_types::summary_type::{parameter_list, SketchAlgorithm, SummaryUpdate};
    use asap_types::test_support::memo_fixtures::scan;
    use asap_types::workload::DataWorkload;

    /// A distinct count read from HLL (relative error, failure probability
    /// unknown) or UnivMon (no guarantee for it) under a query with a
    /// relative target: the edge fails with `NoGuarantee`, its error would
    /// reach the root unbounded; with no target reaching it, it holds.
    /// HLL's readout metric is still relative.
    #[test]
    fn a_distinct_count_readout_without_an_evaluable_guarantee_serves_no_query_with_a_target() {
        assert_eq!(
            readout_metric(
                &FieldDataType::Sketch(SketchAlgorithm::Hll),
                &SketchStatistic::Cardinality
            ),
            Some(AccuracyMetric::Relative)
        );
        for algorithm in [SketchAlgorithm::Hll, SketchAlgorithm::UnivMon] {
            let mut memo = Memo::new(&DataWorkload::default());
            let events = memo.find_or_add_group(scan("events", &[], vec![])).unwrap();
            let build = Operator::ASAP(ASAPOp::SummaryAgg {
                child: events,
                summary_type: FieldDataType::Sketch(algorithm),
                input: SummaryUpdate::column(ColumnRef::SampleValue),
                reduction: Reduction::PerEntity,
                filter: None,
            });
            let state = memo.find_or_add_group(build.clone()).unwrap();
            let parameters = parameter_list(algorithm)[0].clone();
            let producer = memo
                .add_candidate(
                    state,
                    CandidateForMemoGroup::new(
                        build,
                        Some(parameters),
                        OriginOfCandidateForMemoGroup::Pass1,
                        Vec::new(),
                    ),
                )
                .unwrap();
            let readout = Operator::ASAP(ASAPOp::SummaryEstimate {
                summary_input: state,
                query: SketchStatistic::Cardinality,
            });
            let distinct = memo.find_or_add_group(readout.clone()).unwrap();
            let consumer = memo
                .add_candidate(
                    distinct,
                    CandidateForMemoGroup::new(
                        readout,
                        None,
                        OriginOfCandidateForMemoGroup::Pass1,
                        Vec::new(),
                    ),
                )
                .unwrap();
            let mut accepted = vec![None; memo.groups().len()];
            let edge = |accepted: &[Option<Vec<AccuracyMetric>>]| {
                producer_satisfies_logical_requirements(
                    &memo,
                    accepted,
                    producer,
                    consumer,
                    0,
                    &NoMeasuredGuarantees,
                )
            };
            assert!(edge(&accepted).is_ok(), "{algorithm:?}");
            accepted[distinct.0 as usize] = Some(vec![AccuracyMetric::Relative]);
            assert!(
                matches!(
                    edge(&accepted),
                    Err(RootPruneReason::NoGuarantee { input: 0, .. })
                ),
                "{algorithm:?}: {:?}",
                edge(&accepted)
            );
        }
    }
}

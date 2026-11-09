//! The deployment's measured accuracy, and α^log of a logical edge.

use crate::estimators::sketch_guarantee;
use crate::{target_limit, theoretical_guarantee};
use asap_types::ir::operator::asap::summary_type_of_operator;
use asap_types::ir::properties::{AccuracyGuarantee, AccuracyMetric};
use asap_types::ir::schema::{
    FieldDataType, MergedError, SketchAlgorithm, SketchParams, SketchStatistic,
    SummaryTypeCapability,
};
use asap_types::ir::{ASAPOp, Operator, QueryRoot};
use asap_types::stages::{
    parameter_list, CandidateForMemoGroupId, LogicalRequirements, Memo, MemoGroupId,
    RootPruneReason, ValueSign,
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
/// why not, for Pass 2's pruned list. `accepted` is
/// [`accepted_metrics`] of `memo`.
/// - A readout's input (`SummaryEstimate`) from a sketch state: the
///   readout's metric is one its output accepts (the metric rule; no rule
///   where no query with a target reaches it; `NoGuarantee` when the
///   summary type has no guarantee for the statistic), and the guarantee of the
///   producer's θ for the readout's statistic, at the required failure
///   probability, is within the required error and in its metric: the
///   deployment's measurement (`evidence`) when it has one, else
///   [`theoretical_guarantee`] when its sign condition holds for the
///   state's values ([`sign_condition_holds`]); a merged state (any producer but a build)
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
            .filter(|&metric| {
                LogicalRequirements::error_propagation(memo, operator, metric).is_some()
            })
            .collect();
        return metric_rule(producer_metric, on_input);
    };
    let state_group = &memo.groups()[summary_input.0 as usize];
    let summary_type = summary_type_of_operator(&state_group.expression);
    let state_sign = state_group.group_properties.state_column_sign();
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
    if let Some(accepted) = &accepted[consumer.group.0 as usize] {
        // A readout with no guarantee in any metric has an error without a
        // bound: no query with a target may read it.
        let Some(metric) = readout_metric(summary_type, statistic) else {
            return Err(RootPruneReason::NoGuarantee { consumer, input });
        };
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
            sign_condition_holds(summary_type, statistic, state_sign)
                .then(|| {
                    theoretical_guarantee(
                        summary_type,
                        parameters,
                        statistic,
                        required.failure_probability,
                        memo.max_distinct_per_evaluation(*summary_input),
                    )
                })
                .flatten()
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
/// its output accepts that its error contract
/// (`LogicalRequirements::error_propagation`) carries; a group accepts the
/// union over its consumers. `None` for a group no such query reaches: no
/// metric rule applies there (accuracy not enforced).
pub fn accepted_metrics(memo: &Memo) -> Vec<Option<Vec<AccuracyMetric>>> {
    let mut accepted: Vec<Option<Vec<AccuracyMetric>>> = vec![None; memo.groups().len()];
    let mut pending: Vec<MemoGroupId> = Vec::new();
    for query in &memo.queries {
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
                .filter(|&metric| {
                    LogicalRequirements::error_propagation(memo, operator, metric).is_some()
                })
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
    if !SummaryTypeCapability::of(summary_type).answers(statistic) {
        return None;
    }
    let parameters = parameter_list(*algorithm).first()?;
    Some(
        sketch_guarantee(algorithm, parameters, statistic)?
            .spec
            .accuracy_metric,
    )
}

/// Whether the theoretical guarantee of reading `statistic` from a state
/// of `summary_type` holds for states updated with values of sign `sign`
/// (concepts doc, section 2, "Sign conditions"): DDSketch's relative error
/// on a quantile needs values of one sign, `NonNegative` or `NonPositive`.
fn sign_condition_holds(
    summary_type: &FieldDataType,
    statistic: &SketchStatistic,
    sign: Option<ValueSign>,
) -> bool {
    match (summary_type, statistic) {
        (FieldDataType::Sketch(SketchAlgorithm::DDSketch), SketchStatistic::Quantile { .. }) => {
            matches!(sign, Some(ValueSign::NonNegative | ValueSign::NonPositive))
        }
        _ => true,
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

#[cfg(test)]
mod tests {
    use super::*;
    use asap_types::ir::operator::operator_properties::Reduction;
    use asap_types::ir::scalar::expr_ir::ColumnRef;
    use asap_types::ir::schema::{ExactKind, SummaryUpdate};
    use asap_types::stages::RequiredAccuracy;
    use asap_types::test_support::{
        add_candidate, group_with_own_candidate, series_schema, time_series_scan,
    };
    use asap_types::workload::SourceFacts;

    /// Decided 2026-11-09 (A6): the edge from an HLL build at precision 18
    /// into `count(distinct)`'s readout, which requires relative error 0.05
    /// at δ = 0.01, holds when the data bounds the scanned source at 9,636
    /// distinct items per evaluation, and is `BelowRequiredAccuracy` at
    /// 1,000,000 (outside the contract) or without a bound.
    #[test]
    fn a_distinct_count_readout_reads_the_scanned_source_s_bound() {
        for (bound, holds) in [(Some(9_636), true), (Some(1_000_000), false), (None, false)] {
            let facts = SourceFacts {
                max_distinct_per_evaluation: bound,
                ..SourceFacts::default()
            };
            let mut memo = Memo::new([("events".to_string(), facts)].into());
            let scan = group_with_own_candidate(
                &mut memo,
                time_series_scan("events", series_schema(&[]), vec![]),
            );
            let build = Operator::ASAP(ASAPOp::SummaryAgg {
                child: scan,
                summary_type: FieldDataType::Sketch(SketchAlgorithm::Hll),
                input: SummaryUpdate::column(ColumnRef::SampleValue),
                reduction: Reduction::PerEntity,
                filter: None,
            });
            let state = memo.find_or_add_group(build.clone()).unwrap();
            let producer = add_candidate(
                &mut memo,
                state,
                build,
                Some(SketchParams::Hll { precision: 18 }),
            )
            .unwrap();
            let readout = Operator::ASAP(ASAPOp::SummaryEstimate {
                summary_input: state,
                query: SketchStatistic::Cardinality,
            });
            let distinct = memo.find_or_add_group(readout.clone()).unwrap();
            let consumer = add_candidate(&mut memo, distinct, readout, None).unwrap();
            memo.set_input_requirements(
                consumer,
                vec![LogicalRequirements {
                    required_accuracy: Some(RequiredAccuracy {
                        metric: AccuracyMetric::Relative,
                        max_error: 0.05,
                        failure_probability: 0.01,
                    }),
                }],
            );
            let accepted = vec![None; memo.groups().len()];
            let result = producer_satisfies_logical_requirements(
                &memo,
                &accepted,
                producer,
                consumer,
                0,
                &NoMeasuredGuarantees,
            );
            assert_eq!(result.is_ok(), holds, "{bound:?}: {result:?}");
            if !holds {
                assert!(
                    matches!(result, Err(RootPruneReason::BelowRequiredAccuracy { .. })),
                    "{result:?}"
                );
            }
        }
    }

    /// A distinct count read from UnivMon, which has no guarantee for it
    /// in any metric, under a query with a relative target: the edge fails
    /// with `NoGuarantee` (its error would reach the root unbounded); with
    /// no target reaching it, it holds.
    #[test]
    fn a_readout_without_a_guarantee_serves_no_query_with_a_target() {
        let mut memo = Memo::default();
        let scan = group_with_own_candidate(
            &mut memo,
            time_series_scan("events", series_schema(&[]), vec![]),
        );
        let build = Operator::ASAP(ASAPOp::SummaryAgg {
            child: scan,
            summary_type: FieldDataType::Sketch(SketchAlgorithm::UnivMon),
            input: SummaryUpdate::column(ColumnRef::SampleValue),
            reduction: Reduction::PerEntity,
            filter: None,
        });
        let state = memo.find_or_add_group(build.clone()).unwrap();
        let parameters = parameter_list(SketchAlgorithm::UnivMon)[0].clone();
        let producer = add_candidate(&mut memo, state, build, Some(parameters)).unwrap();
        let readout = Operator::ASAP(ASAPOp::SummaryEstimate {
            summary_input: state,
            query: SketchStatistic::Cardinality,
        });
        let distinct = memo.find_or_add_group(readout.clone()).unwrap();
        let consumer = add_candidate(&mut memo, distinct, readout, None).unwrap();
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
        assert!(edge(&accepted).is_ok());
        accepted[distinct.0 as usize] = Some(vec![AccuracyMetric::Relative]);
        assert!(
            matches!(
                edge(&accepted),
                Err(RootPruneReason::NoGuarantee { input: 0, .. })
            ),
            "{:?}",
            edge(&accepted)
        );
    }

    /// A readout's metric is its guarantee's, whatever its failure
    /// probability or the source's distinct bound: HLL's distinct count
    /// still reads a relative error.
    #[test]
    fn readout_metric_is_the_guarantee_s_metric() {
        let quantile = SketchStatistic::Quantile { q: 0.5 };
        let sketch = |algorithm| FieldDataType::Sketch(algorithm);
        assert_eq!(
            readout_metric(&sketch(SketchAlgorithm::Kll), &quantile),
            Some(AccuracyMetric::Rank)
        );
        assert_eq!(
            readout_metric(&sketch(SketchAlgorithm::DDSketch), &quantile),
            Some(AccuracyMetric::Relative)
        );
        assert_eq!(
            readout_metric(&sketch(SketchAlgorithm::Hll), &SketchStatistic::Cardinality),
            Some(AccuracyMetric::Relative)
        );
        assert_eq!(
            readout_metric(&sketch(SketchAlgorithm::Hll), &quantile),
            None
        );
        assert_eq!(
            readout_metric(&FieldDataType::exact(ExactKind::Sum), &quantile),
            None
        );
    }
}

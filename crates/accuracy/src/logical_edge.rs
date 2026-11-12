//! $\alpha^{\log}$ of a logical edge: whether a producer meets a consumer's
//! logical requirements, and the comparison of an evaluated bound with a
//! required limit ([`target_limit`]).

use asap_types::accuracy::AccuracyMetric;
use asap_types::ir::{ASAPOp, Operator};
use asap_types::stages::{CandidateForMemoGroupId, Memo, RootPruneReason};
use asap_types::summary_type::{MergedError, SketchParams, SketchStatistic, SummaryTypeCapability};

use crate::guarantee::guarantee_before_failure_probability_check;
use crate::readout::sketch_readout;
use crate::{theoretical_guarantee, AccuracyEvidence};

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

/// $\alpha^{\log}$ of one edge (concepts doc, section 2): `Ok` when
/// `producer` may feed input `input_index_of_consumer` of `consumer`, else
/// why not, for Pass 2's pruned list. `readout_metric_of_candidate` is
/// [`readout_metric_of_candidate`](crate::readout_metric_of_candidate) of
/// `memo`, and `metrics_accepted_on_inputs` is
/// [`metrics_accepted_on_inputs`](crate::metrics_accepted_on_inputs) of
/// `consumer`.
/// - A readout's input (`SummaryEstimate`) from a sketch state: the
///   readout's metric is one its output accepts (the metric rule; no rule
///   where no query with a target reaches it; `NoGuarantee` when, under
///   such a query, the readout has no metric or its guarantee's failure
///   probability is unknown, as HLL's), and the guarantee of the
///   producer's θ for the readout's statistic, at the required failure
///   probability, is within the required error ([`target_limit`]) and in
///   its metric: the deployment's measurement (`evidence`) when it has
///   one, else [`theoretical_guarantee`]; a merged state (any producer but
///   a build) must keep the guarantee (`MergedError::Exact` or
///   `AsIfBuiltOverUnion`). A top-k's heap holds its `k` whatever the
///   requirement.
/// - Any other input: the metric rule, when the producer is a sketch
///   readout: the consumer accepts the readout's metric on that input.
///   Only readouts introduce error, so an edge from any other producer
///   holds; a consumer reading an approximate value through it is cut
///   below, on the readout's edge.
pub fn producer_satisfies_logical_requirements(
    memo: &Memo,
    readout_metric_of_candidate: &[Vec<Option<AccuracyMetric>>],
    metrics_accepted_on_inputs: Option<&[AccuracyMetric]>,
    producer: CandidateForMemoGroupId,
    consumer: CandidateForMemoGroupId,
    input_index_of_consumer: usize,
    evidence: &dyn AccuracyEvidence,
) -> Result<(), RootPruneReason> {
    let consumer_candidate = memo.candidate(consumer);
    let input = input_index_of_consumer;
    let readout_metric = |candidate: CandidateForMemoGroupId| {
        readout_metric_of_candidate[candidate.group.0 as usize][candidate.index_in_group]
    };
    let metric_rule = |producer_metric: AccuracyMetric, accepted_metrics: &[AccuracyMetric]| {
        if accepted_metrics.contains(&producer_metric) {
            Ok(())
        } else {
            Err(RootPruneReason::MetricRule {
                consumer,
                input,
                producer_metric,
                accepted_metrics: accepted_metrics.to_vec(),
            })
        }
    };
    let Operator::ASAP(ASAPOp::SummaryEstimate { .. }) = consumer_candidate.expression.operator
    else {
        let (Some(producer_metric), Some(accepted)) =
            (readout_metric(producer), metrics_accepted_on_inputs)
        else {
            return Ok(());
        };
        return metric_rule(producer_metric, accepted);
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
    if let Some(accepted) = metrics_accepted_on_inputs {
        // A query with a target reads this readout: its error needs a
        // metric and a failure probability a guarantee states.
        let no_guarantee = RootPruneReason::NoGuarantee { consumer, input };
        let metric = readout_metric(consumer).ok_or(no_guarantee.clone())?;
        metric_rule(metric, accepted)?;
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
    use crate::{readout_metric_of_candidate, NoMeasuredGuarantees};
    use asap_types::ir::operator::arguments::Reduction;
    use asap_types::ir::scalar::ColumnRef;
    use asap_types::ir::schema::FieldDataType;
    use asap_types::stages::{CandidateForMemoGroup, OriginOfCandidateForMemoGroup};
    use asap_types::summary_type::{parameter_list, SketchAlgorithm, SummaryUpdate};
    use asap_types::test_support::memo_fixtures::scan;
    use asap_types::workload::DataWorkload;

    /// A bound equal to the limit up to floating-point noise meets it; a
    /// bound one part in a million above it does not.
    #[test]
    fn target_limit_admits_only_floating_point_noise() {
        assert!(0.01 * (1.0 + 1e-12) <= target_limit(0.01));
        assert!(0.01 * (1.0 + 1e-6) > target_limit(0.01));
        assert!(0.0 <= target_limit(0.0));
    }

    /// A distinct count read from HLL (relative error, failure probability
    /// unknown) or UnivMon (no guarantee for it) under a query with a
    /// relative target: the edge fails with `NoGuarantee`, its error would
    /// reach the root unbounded; with no target reaching it, it holds.
    #[test]
    fn a_distinct_count_readout_without_an_evaluable_guarantee_serves_no_query_with_a_target() {
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
            let readout_metrics = readout_metric_of_candidate(&memo);
            let edge = |accepted: Option<&[AccuracyMetric]>| {
                producer_satisfies_logical_requirements(
                    &memo,
                    &readout_metrics,
                    accepted,
                    producer,
                    consumer,
                    0,
                    &NoMeasuredGuarantees,
                )
            };
            assert!(edge(None).is_ok(), "{algorithm:?}");
            let relative = Some(&[AccuracyMetric::Relative][..]);
            assert!(
                matches!(
                    edge(relative),
                    Err(RootPruneReason::NoGuarantee { input: 0, .. })
                ),
                "{algorithm:?}: {:?}",
                edge(relative)
            );
        }
    }

    /// A top-k heap smaller than k never serves the readout, whatever
    /// accuracy is required: the edge from each build into `topk(50)`'s
    /// readout fails with `HeapSmallerThanTopK` exactly for the heaps
    /// below 50 (sizes 10 and 20, with each width and depth).
    #[test]
    fn a_heap_smaller_than_k_fails_the_topk_readouts_edge() {
        use asap_types::test_support::memo_builders::{
            estimate, group, own_candidate, scan_group, sketch_state,
        };
        use asap_types::test_support::memo_fixtures::range;
        let mut memo = Memo::new(&DataWorkload::default());
        let input = scan_group(&mut memo, "x", &["job"], vec![]);
        let window = group(&mut memo, range(300_000, input));
        let state = sketch_state(
            &mut memo,
            window,
            SketchAlgorithm::CmsWithHeap,
            Reduction::by(vec![]),
        );
        let readout = estimate(&mut memo, state, SketchStatistic::TopK { k: 50 });
        let readout_metrics = readout_metric_of_candidate(&memo);
        let mut small = 0;
        for index_in_group in 0..memo.groups()[state.0 as usize].candidates.len() {
            let build = CandidateForMemoGroupId {
                group: state,
                index_in_group,
            };
            let Some(SketchParams::CmsWithHeap { heap_size, .. }) =
                memo.candidate(build).logical_properties.parameters()
            else {
                panic!("a heap build");
            };
            let edge = producer_satisfies_logical_requirements(
                &memo,
                &readout_metrics,
                None,
                build,
                own_candidate(readout),
                0,
                &NoMeasuredGuarantees,
            );
            if *heap_size < 50 {
                assert!(
                    matches!(
                        edge,
                        Err(RootPruneReason::HeapSmallerThanTopK { k: 50, .. })
                    ),
                    "{edge:?}"
                );
                small += 1;
            } else {
                assert!(edge.is_ok(), "{edge:?}");
            }
        }
        assert_eq!(small, 2 * 28);
    }
}

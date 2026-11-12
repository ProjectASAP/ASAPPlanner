//! The metric rule (concepts doc, section 4, "Metric rule"): the metrics
//! each group's output may be off in, and those each candidate accepts on
//! its inputs.

use asap_types::accuracy::AccuracyMetric;
use asap_types::ir::{ASAPOp, Operator};
use asap_types::stages::{CandidateForMemoGroupId, Memo, MemoGroupId};

use crate::metrics_carried;
use crate::readout_caps::caps;

/// Per group, by group id, the metrics its output may be off in for the
/// queries with an accuracy target that reach it, in
/// `AccuracyMetric::ALL` order: a query root accepts its target's metrics
/// (`AccuracyTarget::metrics`); a candidate's input accepts the metrics its
/// output accepts that its error contract carries ([`metrics_carried`]); a
/// group accepts the union over its consumers. These are the metrics of
/// the readout caps' walk (`readout_caps`), which also reaches a group
/// through inputs the output does not depend on. `None` for a group no
/// such query reaches: no metric rule applies there (accuracy not
/// enforced). `order` is the memo's topological order
/// (`Memo::topological_order`).
pub fn accepted_metrics(memo: &Memo, order: &[MemoGroupId]) -> Vec<Option<Vec<AccuracyMetric>>> {
    caps(memo, order)
        .into_iter()
        .map(|caps| {
            caps.map(|caps| {
                AccuracyMetric::ALL
                    .into_iter()
                    .filter(|metric| caps.contains_key(metric))
                    .collect()
            })
        })
        .collect()
}

/// The metrics `consumer` accepts an input's error in, the metric rule on
/// its input edges, from `accepted` ([`accepted_metrics`]): a readout
/// (`SummaryEstimate`) introduces its error, so it accepts those its own
/// output accepts; any other candidate those its output accepts that its
/// error contract carries ([`metrics_carried`]). `None` where no query
/// with a target reaches the consumer.
pub fn metrics_accepted_on_inputs(
    memo: &Memo,
    accepted: &[Option<Vec<AccuracyMetric>>],
    consumer: CandidateForMemoGroupId,
) -> Option<Vec<AccuracyMetric>> {
    let accepted = accepted[consumer.group.0 as usize].as_ref()?;
    let operator = &memo.candidate(consumer).expression.operator;
    Some(match operator {
        Operator::ASAP(ASAPOp::SummaryEstimate { .. }) => accepted.clone(),
        _ => metrics_carried(memo, operator, accepted),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        producer_satisfies_logical_requirements, readout_metric_of_candidate, NoMeasuredGuarantees,
    };
    use asap_types::accuracy::AccuracyTarget;
    use asap_types::ir::operator::arguments::Reduction;
    use asap_types::ir::operator::AggIntent;
    use asap_types::ir::scalar::expr_ir::ArithmeticOpKind;
    use asap_types::ir::scalar::{CompareOpKind, ScalarValue};
    use asap_types::ir::{ExprSemantics, NonASAPOp, Predicate};
    use asap_types::stages::RootPruneReason;
    use asap_types::summary_type::{SketchAlgorithm, SketchStatistic};
    use asap_types::test_support::memo_builders::{
        estimate, group, own_candidate, quantile_over_time, quantile_over_time_readouts, query,
        scan_group, QuantileReadouts,
    };
    use asap_types::test_support::memo_fixtures::{arithmetic, compare, range};
    use asap_types::workload::DataWorkload;

    fn accepted(memo: &Memo) -> Vec<Option<Vec<AccuracyMetric>>> {
        accepted_metrics(memo, &memo.topological_order().expect("acyclic"))
    }

    /// $\alpha^{\log}$ of the edge from `producer` into input `input` of
    /// `consumer`, with the metric rule of `memo`'s queries.
    fn edge(
        memo: &Memo,
        producer: CandidateForMemoGroupId,
        consumer: CandidateForMemoGroupId,
        input: usize,
    ) -> Result<(), RootPruneReason> {
        producer_satisfies_logical_requirements(
            memo,
            &readout_metric_of_candidate(memo),
            metrics_accepted_on_inputs(memo, &accepted(memo), consumer).as_deref(),
            producer,
            consumer,
            input,
            &NoMeasuredGuarantees,
        )
    }

    /// The edge on which the readout candidate `id` of a quantile group
    /// ([`QuantileReadouts`]) introduces its error into `consumer`'s input
    /// `input`: from `id` itself when it is the readout, else from the
    /// readout group the renaming `Project` `id` reads.
    fn readout_edge(
        memo: &Memo,
        id: CandidateForMemoGroupId,
        consumer: CandidateForMemoGroupId,
        input: usize,
    ) -> Result<(), RootPruneReason> {
        match &memo.candidate(id).expression.operator {
            Operator::NonASAP(NonASAPOp::Project { child, .. }) => {
                edge(memo, own_candidate(*child), id, 0)
            }
            _ => edge(memo, id, consumer, input),
        }
    }

    /// `quantile_over_time(q, m[5m])` per series of `m` by `job` with KLL
    /// and DDSketch readouts beside the exact quantile.
    fn quantile_readouts(memo: &mut Memo, q: f64) -> QuantileReadouts {
        let input = scan_group(memo, "m", &["job"], vec![]);
        let window = group(memo, range(300_000, input));
        quantile_over_time_readouts(
            memo,
            window,
            q,
            &[SketchAlgorithm::Kll, SketchAlgorithm::DDSketch],
        )
    }

    /// A ratio reads no rank error: with a relative target, the KLL
    /// readout's metric, rank, is accepted neither by the ratio nor at its
    /// own output, so its edge into the division fails the metric rule.
    #[test]
    fn a_ratio_reads_no_rank_error() {
        let mut memo = Memo::new(&DataWorkload::default());
        let (_, state, p99) = quantile_over_time(&mut memo, SketchAlgorithm::Kll, 0.99, 300);
        let p50 = estimate(&mut memo, state, SketchStatistic::Quantile { q: 0.5 });
        let ratio = group(&mut memo, arithmetic(ArithmeticOpKind::Div, p99, p50));
        let target = AccuracyTarget::max_error(0.05).accuracy_metric(AccuracyMetric::Relative);
        query(&mut memo, ratio, Some(target));
        let relative = Some(vec![AccuracyMetric::Relative]);
        assert_eq!(accepted(&memo)[p99.0 as usize], relative);
        assert_eq!(
            metrics_accepted_on_inputs(&memo, &accepted(&memo), own_candidate(ratio)),
            relative
        );
        assert!(matches!(
            edge(&memo, own_candidate(p99), own_candidate(ratio), 0),
            Err(RootPruneReason::MetricRule {
                producer_metric: AccuracyMetric::Rank,
                ..
            })
        ));
    }

    /// The metric rule cuts edges, not candidates.
    /// `avg by (job)(q0.5 / q0.5 + q0.9)` at absolute error 0.05, each
    /// quantile per series with KLL, DDSketch and exact candidates: the `+`
    /// carries absolute error only, so the division's inputs accept no
    /// metric and the edges from the DDSketch and KLL readouts into it are
    /// cut; the exact quantile's edge into the division and the division's
    /// edge into the `+` hold.
    #[test]
    fn the_metric_rule_cuts_the_readout_edges_and_keeps_the_exact_division() {
        let mut memo = Memo::new(&DataWorkload::default());
        let p50 = quantile_readouts(&mut memo, 0.5);
        let window = *memo.groups()[p50.result.0 as usize].expression.children()[0];
        let p90 = quantile_over_time_readouts(
            &mut memo,
            window,
            0.9,
            &[SketchAlgorithm::Kll, SketchAlgorithm::DDSketch],
        );
        let ratio = group(
            &mut memo,
            arithmetic(ArithmeticOpKind::Div, p50.result, p50.result),
        );
        let sum = group(
            &mut memo,
            arithmetic(ArithmeticOpKind::Add, ratio, p90.result),
        );
        let avg = group(
            &mut memo,
            Operator::NonASAP(NonASAPOp::Aggregate {
                child: sum,
                reduction: Reduction::by(vec![0]),
                measures: vec![AggIntent::Avg { col: None }],
                output_names: vec!["avg".into()],
                filters: vec![None],
                having: None,
            }),
        );
        let target = AccuracyTarget::max_error(0.05).accuracy_metric(AccuracyMetric::ABSOLUTE);
        query(&mut memo, avg, Some(target));
        let division = own_candidate(ratio);
        assert_eq!(
            metrics_accepted_on_inputs(&memo, &accepted(&memo), division),
            Some(vec![])
        );
        let result = edge(&memo, division, own_candidate(sum), 0);
        assert!(result.is_ok(), "{result:?}");
        for input in 0..2 {
            let result = edge(&memo, own_candidate(p50.result), division, input);
            assert!(result.is_ok(), "{result:?}");
            for &(algorithm, _, readout) in &p50.by_algorithm {
                assert!(
                    matches!(
                        readout_edge(&memo, readout, division, input),
                        Err(RootPruneReason::MetricRule { .. })
                    ),
                    "{algorithm:?}"
                );
            }
        }
    }

    /// A `Filter` whose predicate reads only label columns keeps or drops
    /// whole rows, so a value's error passes through it unchanged.
    /// `quantile_over_time(0.99, latency[5m])` behind `job = "api"`,
    /// relative error 0.05: the filter accepts relative error on its input
    /// and the DDSketch readout's edge holds.
    #[test]
    fn a_key_only_filter_keeps_the_ddsketch_readout_admissible() {
        let mut memo = Memo::new(&DataWorkload::default());
        let input = scan_group(&mut memo, "latency", &["job"], vec![]);
        let window = group(&mut memo, range(300_000, input));
        let p99 =
            quantile_over_time_readouts(&mut memo, window, 0.99, &[SketchAlgorithm::DDSketch]);
        let job = memo.groups()[p99.result.0 as usize]
            .group_properties
            .schema
            .fields
            .iter()
            .position(|field| field.name == "job")
            .expect("the result keeps the job label");
        let filtered = group(
            &mut memo,
            Operator::NonASAP(NonASAPOp::Filter {
                pred: Predicate(compare(
                    job,
                    CompareOpKind::Eq,
                    ScalarValue::Utf8("api".into()),
                    ExprSemantics::Promql,
                )),
                child: p99.result,
            }),
        );
        let target = AccuracyTarget::max_error(0.05).accuracy_metric(AccuracyMetric::Relative);
        query(&mut memo, filtered, Some(target));
        assert_eq!(
            metrics_accepted_on_inputs(&memo, &accepted(&memo), own_candidate(filtered)),
            Some(vec![AccuracyMetric::Relative])
        );
        let ddsketch_readout = p99.by_algorithm[0].2;
        let result = readout_edge(&memo, ddsketch_readout, own_candidate(filtered), 0);
        assert!(result.is_ok(), "{result:?}");
    }

    /// `avg` has an absolute contract only here; a relative bound on a
    /// mean needs the values' signs. So
    /// `avg(quantile_over_time(0.99, m[5m]))` with a relative target cuts
    /// the edges from the sketch readouts into the `avg`: it reads the
    /// exact quantile only.
    #[test]
    fn avg_of_readouts_under_a_relative_target_reads_exact_only() {
        let mut memo = Memo::new(&DataWorkload::default());
        let p99 = quantile_readouts(&mut memo, 0.99);
        let avg = group(
            &mut memo,
            Operator::NonASAP(NonASAPOp::Aggregate {
                child: p99.result,
                reduction: Reduction::by(vec![]),
                measures: vec![AggIntent::Avg { col: None }],
                output_names: vec!["avg".into()],
                filters: vec![None],
                having: None,
            }),
        );
        let target = AccuracyTarget::max_error(0.05).accuracy_metric(AccuracyMetric::Relative);
        query(&mut memo, avg, Some(target));
        for &(algorithm, _, readout) in &p99.by_algorithm {
            assert!(
                matches!(
                    readout_edge(&memo, readout, own_candidate(avg), 0),
                    Err(RootPruneReason::MetricRule { .. })
                ),
                "a sketch readout is admissible into avg: {algorithm:?}"
            );
        }
        let result = edge(&memo, own_candidate(p99.result), own_candidate(avg), 0);
        assert!(result.is_ok(), "{result:?}");
    }
}

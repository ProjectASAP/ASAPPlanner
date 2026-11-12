//! The error metrics of a memo (concepts doc, section 4, "Metric rule"):
//! each candidate's error contract per metric, the metrics each group's
//! output may be off in, each sketch readout's error metric; the metric
//! rule on edges ([`ErrorMetricsOfMemo::edge_meets_logical_requirements`],
//! in `logical_edge`) and per query
//! ([`ErrorMetricsOfMemo::exact_only_groups_of_query`]).

use std::collections::{BTreeMap, BTreeSet};

use asap_types::accuracy::{AccuracyMetric, AccuracyTarget};
use asap_types::ir::{QueryRoot, ScalarExpr};
use asap_types::stages::{CandidateForMemoGroupId, Memo, MemoGroupId};

use crate::error_contract::{error_contract, ErrorContract};
use crate::readout::sketch_readout_error_metric;
use crate::readout_caps::caps;

/// How many error metrics there are: `AccuracyMetric::ALL`'s length.
pub(crate) const METRIC_COUNT: usize = AccuracyMetric::ALL.len();

/// Per metric, in `AccuracyMetric::ALL` order: a candidate's error
/// contract in it; `None` = no contract.
pub(crate) type ErrorContractPerMetric = [Option<ErrorContract>; METRIC_COUNT];

/// The error metrics of one memo state, built once per memo state before
/// an edge pass; Stage 3 reads the same contracts.
pub struct ErrorMetricsOfMemo {
    /// [group][candidate][metric]: the candidate's error contract; `None` =
    /// no contract.
    pub(crate) error_contract: Vec<Vec<ErrorContractPerMetric>>,
    /// [group]: the error metrics its output may have, in
    /// `AccuracyMetric::ALL` order; `None` = no query with a target reaches
    /// it (no metric rule applies there).
    error_metrics_allowed_at_output: Vec<Option<Vec<AccuracyMetric>>>,
    /// [group][candidate]: the error metric of a sketch readout; `None` =
    /// not a sketch readout, or one whose summary type has no guarantee
    /// for the statistic it reads.
    readout_error_metric: Vec<Vec<Option<AccuracyMetric>>>,
}

impl ErrorMetricsOfMemo {
    /// The error metrics of `memo`, whose topological order is `order`
    /// (`Memo::topological_order`). A group's output may have the metrics
    /// the queries with a target reaching it allow: a query root allows
    /// its target's metrics ([`query_roots_with_target`]); a candidate's
    /// input those its output allows that its error contract passes to it;
    /// a group the union over its consumers. These are the metrics of the
    /// readout caps' walk (`readout_caps`), which also reaches a group
    /// through inputs the output does not depend on.
    pub fn of(memo: &Memo, order: &[MemoGroupId]) -> Self {
        let error_contract: Vec<Vec<ErrorContractPerMetric>> = memo
            .groups()
            .iter()
            .map(|group| {
                group
                    .candidates
                    .iter()
                    .map(|candidate| {
                        AccuracyMetric::ALL.map(|metric| {
                            error_contract(memo, &candidate.expression.operator, metric)
                        })
                    })
                    .collect()
            })
            .collect();
        let error_metrics_allowed_at_output = caps(memo, &error_contract, order)
            .into_iter()
            .map(|caps| {
                caps.map(|caps| {
                    AccuracyMetric::ALL
                        .into_iter()
                        .filter(|metric| caps.contains_key(metric))
                        .collect()
                })
            })
            .collect();
        let readout_error_metric = memo
            .groups()
            .iter()
            .map(|group| {
                group
                    .candidates
                    .iter()
                    .map(|candidate| sketch_readout_error_metric(memo, candidate))
                    .collect()
            })
            .collect();
        Self {
            error_contract,
            error_metrics_allowed_at_output,
            readout_error_metric,
        }
    }

    /// `candidate`'s error contract in `metric`; `None` when it has none.
    pub fn error_contract(
        &self,
        candidate: CandidateForMemoGroupId,
        metric: AccuracyMetric,
    ) -> Option<&ErrorContract> {
        self.error_contract[candidate.group.0 as usize][candidate.index_in_group]
            [metric_index(metric)]
        .as_ref()
    }

    /// The error metric of `candidate` when it is a sketch readout.
    pub(crate) fn readout_error_metric(
        &self,
        candidate: CandidateForMemoGroupId,
    ) -> Option<AccuracyMetric> {
        self.readout_error_metric[candidate.group.0 as usize][candidate.index_in_group]
    }

    /// The metrics among `metrics` that `consumer`'s error contract passes
    /// to its inputs: those an input of it may be off in when its output
    /// may be off in `metrics`.
    fn error_metrics_passed_to_inputs(
        &self,
        consumer: CandidateForMemoGroupId,
        metrics: &[AccuracyMetric],
    ) -> Vec<AccuracyMetric> {
        metrics
            .iter()
            .copied()
            .filter(|&metric| self.error_contract(consumer, metric).is_some())
            .collect()
    }

    /// The metrics `consumer` allows an input's error in, the metric rule
    /// on its input edges: those its group's output allows that its error
    /// contract passes to its inputs (a readout's contract passes every
    /// metric, so a readout allows its output's). `None` where no query
    /// with a target reaches the consumer.
    pub(crate) fn error_metrics_allowed_on_inputs(
        &self,
        consumer: CandidateForMemoGroupId,
    ) -> Option<Vec<AccuracyMetric>> {
        let allowed = self.error_metrics_allowed_at_output[consumer.group.0 as usize].as_ref()?;
        Some(self.error_metrics_passed_to_inputs(consumer, allowed))
    }

    /// Per query: the groups below its root it may read only exactly, the
    /// metric rule per query where the edges apply it per group. Walking
    /// down the live candidates (not in `dead`) from each root with a
    /// target, which allows its target's metrics
    /// ([`query_roots_with_target`]), a candidate passes to its inputs the
    /// metrics its error contract passes; a group keeps, per query, the
    /// metrics every path to it passes. A group is exact-only for a query
    /// when no metric reaches it (one path through a consumer without a
    /// contract, a comparison or a sum, suffices) or when it holds a sketch
    /// readout whose error metric is not among them (`quantile_over_time`
    /// at a relative target reads no KLL readout, whose error is a rank
    /// one); and so is every group below one.
    /// `exact_only_readout_groups_of_query` adds, per query, the readout
    /// groups the readout requirements list (a readout whose level another
    /// query gave, for a query whose failure probability has no per-value
    /// one). `order` is the memo's topological order.
    pub fn exact_only_groups_of_query(
        &self,
        memo: &Memo,
        order: &[MemoGroupId],
        dead: &BTreeSet<CandidateForMemoGroupId>,
        exact_only_readout_groups_of_query: &BTreeMap<usize, BTreeSet<MemoGroupId>>,
    ) -> Vec<BTreeSet<MemoGroupId>> {
        let live_candidates = |group: MemoGroupId| {
            (0..memo.groups()[group.0 as usize].candidates.len())
                .map(move |index_in_group| CandidateForMemoGroupId {
                    group,
                    index_in_group,
                })
                .filter(|candidate| !dead.contains(candidate))
        };
        let mut allowed: Vec<BTreeMap<usize, Vec<AccuracyMetric>>> =
            vec![BTreeMap::new(); memo.groups().len()];
        // Keeps, per query, the metrics every path to a group passes.
        fn keep_common(
            allowed: &mut [BTreeMap<usize, Vec<AccuracyMetric>>],
            group: MemoGroupId,
            query: usize,
            metrics: Vec<AccuracyMetric>,
        ) {
            allowed[group.0 as usize]
                .entry(query)
                .and_modify(|known| known.retain(|metric| metrics.contains(metric)))
                .or_insert(metrics);
        }
        for (query, root, _, metrics) in query_roots_with_target(memo) {
            keep_common(&mut allowed, root, query, metrics);
        }
        let mut exact_only = vec![BTreeSet::new(); memo.queries().len()];
        for (&query, groups) in exact_only_readout_groups_of_query {
            exact_only[query].extend(groups.iter().copied());
        }
        for &group in order.iter().rev() {
            let allowed_here = allowed[group.0 as usize].clone();
            for (&query, metrics) in &allowed_here {
                if metrics.is_empty() {
                    exact_only[query].insert(group);
                }
            }
            for consumer in live_candidates(group) {
                if let Some(metric) = self.readout_error_metric(consumer) {
                    for (&query, metrics) in &allowed_here {
                        if !metrics.contains(&metric) {
                            exact_only[query].insert(group);
                        }
                    }
                }
                let passed = self.error_metrics_passed_to_inputs(consumer, &AccuracyMetric::ALL);
                for &child in memo.candidate(consumer).expression.operator.children() {
                    for (&query, metrics) in &allowed_here {
                        let passed_here = metrics
                            .iter()
                            .copied()
                            .filter(|metric| passed.contains(metric))
                            .collect();
                        keep_common(&mut allowed, child, query, passed_here);
                    }
                }
            }
        }
        // Every group below an exact-only group is read exactly too.
        for &group in order.iter().rev() {
            for groups in &mut exact_only {
                if groups.contains(&group) {
                    for consumer in live_candidates(group) {
                        groups.extend(
                            memo.candidate(consumer)
                                .expression
                                .operator
                                .children()
                                .into_iter()
                                .copied(),
                        );
                    }
                }
            }
        }
        exact_only
    }
}

/// The index of `metric` in `AccuracyMetric::ALL`.
fn metric_index(metric: AccuracyMetric) -> usize {
    AccuracyMetric::ALL
        .iter()
        .position(|&listed| listed == metric)
        .expect("AccuracyMetric::ALL lists every metric")
}

/// Each query with a target, with the groups its root reads and the metrics
/// each allows: an operator root, `scalar(v)` and an SQL scalar subquery
/// (which return the value they read) allow the target's metrics
/// (`AccuracyTarget::metrics`); any other scalar expression's operands
/// allow none, as no error contract covers it, so they are read exactly.
pub(crate) fn query_roots_with_target(
    memo: &Memo,
) -> Vec<(usize, MemoGroupId, &AccuracyTarget, Vec<AccuracyMetric>)> {
    let mut roots = Vec::new();
    for (query, memo_query) in memo.queries().iter().enumerate() {
        let Some(target) = &memo_query.accuracy_target else {
            continue;
        };
        match &memo_query.root {
            QueryRoot::Operator(root)
            | QueryRoot::Scalar(
                ScalarExpr::PromqlScalarFromVector(root) | ScalarExpr::ScalarSubquery(root),
            ) => roots.push((query, *root, target, target.metrics())),
            QueryRoot::Scalar(expr) => {
                for &operand in expr.operator_refs() {
                    roots.push((query, operand, target, Vec::new()));
                }
            }
        }
    }
    roots
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::NoMeasuredGuarantees;
    use asap_types::accuracy::AccuracyTarget;
    use asap_types::ir::operator::arguments::Reduction;
    use asap_types::ir::operator::AggIntent;
    use asap_types::ir::scalar::expr_ir::ArithmeticOpKind;
    use asap_types::ir::scalar::{CompareOpKind, ScalarValue};
    use asap_types::ir::{ExprSemantics, NonASAPOp, Operator, Predicate};
    use asap_types::stages::RootPruneReason;
    use asap_types::summary_type::{SketchAlgorithm, SketchStatistic};
    use asap_types::test_support::memo_builders::{
        estimate, group, own_candidate, quantile_over_time, quantile_over_time_readouts, query,
        scan_group, QuantileReadout, QuantileReadouts,
    };
    use asap_types::test_support::memo_fixtures::{arithmetic, compare, range};
    use asap_types::workload::DataWorkload;

    fn error_metrics(memo: &Memo) -> ErrorMetricsOfMemo {
        ErrorMetricsOfMemo::of(memo, &memo.topological_order().expect("acyclic"))
    }

    /// The metrics `group`'s output may be off in.
    fn allowed_at_output(memo: &Memo, group: MemoGroupId) -> Option<Vec<AccuracyMetric>> {
        error_metrics(memo).error_metrics_allowed_at_output[group.0 as usize].clone()
    }

    /// The metrics `consumer` allows on its inputs.
    fn allowed_on_inputs(
        memo: &Memo,
        consumer: CandidateForMemoGroupId,
    ) -> Option<Vec<AccuracyMetric>> {
        error_metrics(memo).error_metrics_allowed_on_inputs(consumer)
    }

    /// $\alpha^{\log}$ of the edge from `producer` into input `input` of
    /// `consumer`, with the metric rule of `memo`'s queries.
    fn edge(
        memo: &Memo,
        producer: CandidateForMemoGroupId,
        consumer: CandidateForMemoGroupId,
        input: usize,
    ) -> Result<(), RootPruneReason> {
        error_metrics(memo).edge_meets_logical_requirements(
            memo,
            producer,
            consumer,
            input,
            &NoMeasuredGuarantees,
        )
    }

    /// The edge on which `readout` introduces its error into `consumer`'s
    /// input `input`: from the readout candidate itself into `consumer`
    /// when the quantile group reads it directly, else into the renaming
    /// `Project` that reads it.
    fn readout_edge(
        memo: &Memo,
        readout: &QuantileReadout,
        consumer: CandidateForMemoGroupId,
        input: usize,
    ) -> Result<(), RootPruneReason> {
        if readout.readout == readout.candidate_of_result {
            edge(memo, readout.readout, consumer, input)
        } else {
            edge(memo, readout.readout, readout.candidate_of_result, 0)
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
        assert_eq!(allowed_at_output(&memo, p99), relative);
        assert_eq!(allowed_on_inputs(&memo, own_candidate(ratio)), relative);
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
        assert_eq!(allowed_on_inputs(&memo, division), Some(vec![]));
        let result = edge(&memo, division, own_candidate(sum), 0);
        assert!(result.is_ok(), "{result:?}");
        for input in 0..2 {
            let result = edge(&memo, own_candidate(p50.result), division, input);
            assert!(result.is_ok(), "{result:?}");
            for readout in &p50.by_algorithm {
                assert!(
                    matches!(
                        readout_edge(&memo, readout, division, input),
                        Err(RootPruneReason::MetricRule { .. })
                    ),
                    "{:?}",
                    readout.algorithm
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
            allowed_on_inputs(&memo, own_candidate(filtered)),
            Some(vec![AccuracyMetric::Relative])
        );
        let result = readout_edge(&memo, &p99.by_algorithm[0], own_candidate(filtered), 0);
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
        for readout in &p99.by_algorithm {
            assert!(
                matches!(
                    readout_edge(&memo, readout, own_candidate(avg), 0),
                    Err(RootPruneReason::MetricRule { .. })
                ),
                "a sketch readout is admissible into avg: {:?}",
                readout.algorithm
            );
        }
        let result = edge(&memo, own_candidate(p99.result), own_candidate(avg), 0);
        assert!(result.is_ok(), "{result:?}");
    }
}

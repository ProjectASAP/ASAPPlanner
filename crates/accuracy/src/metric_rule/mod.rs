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
    /// ([`query_roots_with_target`]; an exact target none), a candidate passes to its inputs the
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
        for (query, root, target, metrics) in query_roots_with_target(memo) {
            // An exact target allows no error: the query reads every group
            // below its root exactly.
            let metrics = match target.is_exact() {
                true => Vec::new(),
                false => metrics,
            };
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
mod tests;

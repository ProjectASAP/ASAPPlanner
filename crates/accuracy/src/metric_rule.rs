//! The metric rule (concepts doc, section 4, "Metric rule"): the metrics
//! each group's output may be off in, and the metric of a readout's error.

use asap_types::accuracy::AccuracyMetric;
use asap_types::ir::operator::asap::summary_type_of_operator;
use asap_types::ir::schema::FieldDataType;
use asap_types::ir::{ASAPOp, Operator, QueryRoot};
use asap_types::stages::{CandidateForMemoGroup, Memo, MemoGroupId};
use asap_types::summary_type::{parameter_list, SketchStatistic};

use crate::error_propagation;
use crate::guarantee::guarantee_before_failure_probability_check;

/// Per group, by group id, the metrics its output may be off in for the
/// queries with an accuracy target that reach it: a query root accepts its
/// target's metric (every metric when the target names none); a
/// candidate's input accepts the metrics its output accepts that its error
/// contract carries ([`metrics_carried`]); a group accepts the union over
/// its consumers. `None` for a group no such query reaches: no metric rule
/// applies there (accuracy not enforced).
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
            let carried = metrics_carried(memo, operator, &metrics);
            for &child in operator.children() {
                if add_metrics(&mut accepted[child.0 as usize], &carried) {
                    pending.push(child);
                }
            }
        }
    }
    accepted
}

/// The metrics among `metrics` that `consumer`'s error contract
/// ([`error_propagation`]) carries to its inputs: those an input of it may
/// be off in when its output accepts `metrics`.
pub(crate) fn metrics_carried(
    memo: &Memo,
    consumer: &Operator<MemoGroupId>,
    metrics: &[AccuracyMetric],
) -> Vec<AccuracyMetric> {
    metrics
        .iter()
        .copied()
        .filter(|&metric| error_propagation(memo, consumer, metric).is_some())
        .collect()
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
/// `summary_type` has: its first listed θ's guarantee's, whatever that
/// guarantee's failure probability (HLL's distinct count reads a relative
/// error although its failure probability is unknown). `None` for an exact
/// state and for a summary type with no listed θ or no guarantee for the
/// statistic (UnivMon's distinct count).
pub fn readout_metric(
    summary_type: &FieldDataType,
    statistic: &SketchStatistic,
) -> Option<AccuracyMetric> {
    let FieldDataType::Sketch(algorithm) = summary_type else {
        return None;
    };
    let parameters = parameter_list(*algorithm).first()?;
    Some(
        guarantee_before_failure_probability_check(summary_type, parameters, statistic, 1.0)?
            .spec
            .accuracy_metric,
    )
}

/// The metric of `candidate`'s error when it is a readout
/// (`SummaryEstimate`) of a sketch state ([`readout_metric`]); `None` for
/// any other candidate.
pub fn sketch_readout_metric(
    memo: &Memo,
    candidate: &CandidateForMemoGroup,
) -> Option<AccuracyMetric> {
    let Operator::ASAP(ASAPOp::SummaryEstimate {
        summary_input,
        query,
    }) = &candidate.expression.operator
    else {
        return None;
    };
    readout_metric(
        summary_type_of_operator(&memo.groups()[summary_input.0 as usize].expression)?,
        query,
    )
}

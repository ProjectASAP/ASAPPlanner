//! Readout requirements (concepts doc, section 4, "Readout requirements"
//! and "Validity caps and pruning caps"): the caps the queries that reach
//! a readout pass down through their consumers' error contracts, and the
//! required-accuracy levels each readout gets from them.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use asap_types::accuracy::{AccuracyMetric, AccuracyTarget};
use asap_types::ir::Operator;
use asap_types::stages::{CandidateForMemoGroupId, Memo, MemoGroupId, RequiredAccuracy};

use crate::metric_rule::{query_roots_with_target, ErrorContractPerMetric};
use crate::ErrorMetricsOfMemo;

/// The error grid of the readouts of a query that combines several
/// ([`query_levels`]), loosest first.
const MAX_ERROR_LEVELS: [f64; 7] = [0.1, 0.05, 0.02, 0.01, 0.005, 0.002, 0.001];

/// The failure-probability grid of the readouts of a composed query: the
/// powers of ten from 1e-2 down to 1e-5, continued down to the first one at
/// or below a tenth of the query's per-value failure probability, so that up
/// to ten readouts each find a level and the levels sum within it (δ = 0.01
/// over 10 000 series is 1e-6 per value: two quantiles of a ratio read at
/// 1e-7 each).
fn failure_probability_levels(per_value_failure_probability: f64) -> Vec<f64> {
    let lowest = (per_value_failure_probability / 10.0).min(1e-5);
    let deepest_exponent = (-lowest.log10() - 1e-9).ceil().min(300.0) as i32;
    (2..=deepest_exponent)
        .map(|exponent| 1.0 / 10f64.powi(exponent))
        .collect()
}

/// Per group, by group id: `None` when no query with an accuracy target
/// reaches it; else per metric its output may be off in, per query, the
/// largest error in that metric the group's output may have for the query,
/// $\max_q \varepsilon_q / \lambda_{q,c,i}$ capped by the validity caps on
/// the way. A metric with no query has no cap: it reaches the group only
/// through inputs the output does not depend on ($\beta = 0$, a count's).
pub(crate) type Caps = Vec<Option<HashMap<AccuracyMetric, BTreeMap<usize, f64>>>>;

/// The required-accuracy levels one sketch readout gets
/// ([`readout_levels`]), with the queries reaching it whose failure
/// probability has no per-value one there.
#[derive(Debug, Clone, PartialEq)]
pub struct ReadoutRequiredAccuracy {
    /// One per readout candidate Pass 2 keeps: Pass 1's takes the first,
    /// a copy each further one.
    pub levels: Vec<RequiredAccuracy>,
    /// The queries, by index, whose target sets a failure probability
    /// without a per-value one at the readout (the values it covers or the
    /// readout group's values per evaluation unknown).
    pub queries_without_per_value_failure_probability: Vec<usize>,
}

/// The required-accuracy levels of the sketch readouts without a
/// requirement yet, per readout ([`ReadoutRequiredAccuracy`]), from
/// `error_metrics` of `memo` ([`ErrorMetricsOfMemo::of`]). `order` is the
/// memo's topological order (`Memo::topological_order`).
/// - A query whose path reads one readout group gives one level: its target
///   divided by the path's factor, the readout group's cap, at the target's
///   failure probability per readout value ([`query_levels`]).
/// - A composed query (several readout groups, such as a ratio) gives every
///   level of the grid [`MAX_ERROR_LEVELS`] × [`failure_probability_levels`]
///   not looser than the cap and the per-value failure probability (the cap
///   itself when no error level is), one δ level of 1 when the target sets
///   no failure probability.
/// - A readout group reached by several queries gets the union of their
///   levels, in query order; one no query with a target reaches in the
///   readout's metric gets none. Readouts of one group in one metric get
///   the same levels.
///
/// A query whose target sets a failure probability that has no per-value
/// one at the readout gives no level and is listed instead: Pass 2 prunes
/// a readout no query gives a level
/// (`RootPruneReason::ValuesPerEvaluationUnknown`), and lets a listed query
/// read the group only exactly when another query gives it a level.
pub fn readout_levels(
    memo: &Memo,
    error_metrics: &ErrorMetricsOfMemo,
    order: &[MemoGroupId],
) -> BTreeMap<CandidateForMemoGroupId, ReadoutRequiredAccuracy> {
    let caps = caps(memo, &error_metrics.error_contract, order);
    let mut readouts: Vec<(CandidateForMemoGroupId, AccuracyMetric)> = Vec::new();
    for (index, group) in memo.groups().iter().enumerate() {
        for (index_in_group, candidate) in group.candidates.iter().enumerate() {
            let readout = CandidateForMemoGroupId {
                group: MemoGroupId(index as u32),
                index_in_group,
            };
            if candidate
                .input_requirements
                .first()
                .is_some_and(|input| input.required_accuracy.is_none())
            {
                if let Some(metric) = error_metrics.readout_error_metric(readout) {
                    readouts.push((readout, metric));
                }
            }
        }
    }
    let readout_groups: BTreeSet<MemoGroupId> =
        readouts.iter().map(|(readout, _)| readout.group).collect();
    let composed = composed_queries(memo, order, &readout_groups);
    let mut levels_of_group: HashMap<(MemoGroupId, AccuracyMetric), ReadoutRequiredAccuracy> =
        HashMap::new();
    readouts
        .into_iter()
        .map(|(readout, metric)| {
            let group = readout.group;
            let levels = levels_of_group
                .entry((group, metric))
                .or_insert_with(|| group_levels(memo, &caps, &composed, group, metric))
                .clone();
            (readout, levels)
        })
        .collect()
}

/// The levels of readout group `group` in `metric` ([`readout_levels`]).
fn group_levels(
    memo: &Memo,
    caps: &Caps,
    composed: &BTreeSet<usize>,
    group: MemoGroupId,
    metric: AccuracyMetric,
) -> ReadoutRequiredAccuracy {
    let readout_values = memo.max_values_per_evaluation(group);
    let mut levels: Vec<RequiredAccuracy> = Vec::new();
    let mut queries_without_per_value_failure_probability = Vec::new();
    let group_caps = caps[group.0 as usize]
        .as_ref()
        .and_then(|caps| caps.get(&metric))
        .into_iter()
        .flatten();
    for (&query, &cap) in group_caps {
        let Some(target) = &memo.queries()[query].accuracy_target else {
            continue;
        };
        let Some(query_levels) = query_levels(
            target,
            memo.queries()[query].number_of_values_per_query_evaluation,
            readout_values,
            metric,
            cap,
            composed.contains(&query),
        ) else {
            queries_without_per_value_failure_probability.push(query);
            continue;
        };
        for level in query_levels {
            if !levels.contains(&level) {
                levels.push(level);
            }
        }
    }
    ReadoutRequiredAccuracy {
        levels,
        queries_without_per_value_failure_probability,
    }
}

/// The requirement levels one query gives a readout in `metric` whose cap
/// for it is `cap`; `None` when its target's failure probability has no
/// per-value one. A whole-result δ splits over the values one evaluation
/// returns, $n_q$; a reduction (`max`, `min`, `avg`, `sum`) fails when any
/// readout it reads does, so δ also splits over the readout group's values
/// per evaluation, `readout_values` (`Memo::max_values_per_evaluation`):
/// the per-value failure probability is
/// $\delta / \max(n_q, \text{readout values})$ (`max(quantile_over_time(0.9,
/// m[5m]))` over 1000 series at δ = 0.01: 1e-5; `SELECT count(DISTINCT
/// dst)`, one readout value: 0.01), and needs both known when the target
/// sets a δ.
fn query_levels(
    target: &AccuracyTarget,
    number_of_values_per_query_evaluation: Option<u64>,
    readout_values: Option<u64>,
    metric: AccuracyMetric,
    cap: f64,
    composed: bool,
) -> Option<Vec<RequiredAccuracy>> {
    let per_value_failure_probability = match target.failure_probability {
        None => 1.0,
        Some(delta) => {
            let values_covered = target
                .accuracy_scope
                .max_values_covered(number_of_values_per_query_evaluation)?;
            delta / values_covered.max(readout_values?).max(1) as f64
        }
    };
    let level = |max_error, failure_probability| RequiredAccuracy {
        metric,
        max_error,
        failure_probability,
    };
    if !composed {
        return Some(vec![level(cap, per_value_failure_probability)]);
    }
    let within = |grid: &[f64], limit: f64| -> Vec<f64> {
        let kept: Vec<f64> = grid.iter().copied().filter(|&x| x <= limit).collect();
        if kept.is_empty() {
            vec![limit]
        } else {
            kept
        }
    };
    let errors = within(&MAX_ERROR_LEVELS, cap);
    let failure_probabilities = if target.failure_probability.is_none() {
        vec![1.0]
    } else {
        within(
            &failure_probability_levels(per_value_failure_probability),
            per_value_failure_probability,
        )
    };
    Some(
        errors
            .iter()
            .flat_map(|&max_error| {
                failure_probabilities
                    .iter()
                    .map(move |&failure_probability| level(max_error, failure_probability))
            })
            .collect(),
    )
}

/// Each group's caps ([`Caps`]), one walk down from the query roots: each
/// query root group with a target is reached, with the target in each of
/// the metrics it allows ([`query_roots_with_target`]); then, consumers
/// before producers, every candidate of a reached group reaches its inputs
/// and passes them each metric its error contract (`error_contract`, per
/// group and candidate) has, with the caps
/// $\min(b_{c,i},\ \text{cap}/\beta_{c,i})$, keeping the largest per query
/// and metric (the bottleneck over paths); an input with $\beta_{c,i} = 0$
/// gets the metric but no cap through that candidate.
pub(crate) fn caps(
    memo: &Memo,
    error_contract: &[Vec<ErrorContractPerMetric>],
    order: &[MemoGroupId],
) -> Caps {
    let mut caps: Caps = vec![None; memo.groups().len()];
    for (query, root, target, metrics) in query_roots_with_target(memo) {
        let root_caps = caps[root.0 as usize].get_or_insert_with(HashMap::new);
        for metric in metrics {
            raise(root_caps, metric, query, target.max_error);
        }
    }
    for &group in order.iter().rev() {
        let Some(group_caps) = caps[group.0 as usize].clone() else {
            continue;
        };
        let candidates = &memo.groups()[group.0 as usize].candidates;
        for (consumer, contract_per_metric) in
            candidates.iter().zip(&error_contract[group.0 as usize])
        {
            let children = consumer.expression.operator.children();
            for &child in &children {
                caps[child.0 as usize].get_or_insert_with(HashMap::new);
            }
            for (metric, contract) in AccuracyMetric::ALL.into_iter().zip(contract_per_metric) {
                let (Some(caps_per_query), Some(contract)) = (group_caps.get(&metric), contract)
                else {
                    continue;
                };
                for (input, &child) in children.iter().enumerate() {
                    let child_caps = caps[child.0 as usize].get_or_insert_with(HashMap::new);
                    child_caps.entry(metric).or_default();
                    let factor = contract.error_factor_per_input[input];
                    // An input the output does not depend on (a count's,
                    // β = 0) gets no cap from it.
                    if factor > 0.0 {
                        let holds_up_to = contract.linear_bound_holds_up_to_per_input[input]
                            .unwrap_or(f64::INFINITY);
                        for (&query, &cap) in caps_per_query {
                            raise(child_caps, metric, query, holds_up_to.min(cap / factor));
                        }
                    }
                }
            }
        }
    }
    caps
}

/// The queries that combine several readouts: a candidate they reach has
/// two or more inputs below which a readout group lies (a ratio of two
/// quantiles). Readouts of one value by different summary types are
/// alternatives in one group's candidates, not a combination.
fn composed_queries(
    memo: &Memo,
    order: &[MemoGroupId],
    readout_groups: &BTreeSet<MemoGroupId>,
) -> BTreeSet<usize> {
    let mut reads_a_readout = vec![false; memo.groups().len()];
    let mut composed = BTreeSet::new();
    for &group in order {
        let candidates = &memo.groups()[group.0 as usize].candidates;
        let inputs_reading = |operator: &Operator<MemoGroupId>| {
            operator
                .children()
                .into_iter()
                .filter(|child| reads_a_readout[child.0 as usize])
                .count()
        };
        let most_inputs_reading = candidates
            .iter()
            .map(|candidate| inputs_reading(&candidate.expression.operator))
            .max()
            .unwrap_or(0);
        if most_inputs_reading > 1 {
            composed.extend(
                memo.groups()[group.0 as usize]
                    .reached_by_queries()
                    .iter()
                    .copied(),
            );
        }
        reads_a_readout[group.0 as usize] =
            readout_groups.contains(&group) || most_inputs_reading > 0;
    }
    composed
}

/// Keeps the larger of `cap` and the known cap of `query` in `metric`.
fn raise(
    caps: &mut HashMap<AccuracyMetric, BTreeMap<usize, f64>>,
    metric: AccuracyMetric,
    query: usize,
    cap: f64,
) {
    let known = caps.entry(metric).or_default().entry(query).or_insert(cap);
    *known = known.max(cap);
}

#[cfg(test)]
mod tests;

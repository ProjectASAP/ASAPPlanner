//! Step 3 (concepts doc, section 3, "Reach, caps and readout requirements";
//! section 4, "Readout requirements" and "Validity caps and pruning caps"):
//! each readout's required accuracy, from the caps the queries that reach
//! it pass down through their consumers' error contracts.

use std::collections::{BTreeMap, BTreeSet};

use crate::rules::Pass2RuleError;
use asap_accuracy::{error_propagation, readout_metric};
use asap_types::accuracy::{AccuracyMetric, AccuracyTarget};
use asap_types::ir::operator::asap::summary_type_of_operator;
use asap_types::ir::{ASAPOp, Operator, QueryRoot};
use asap_types::stages::{
    CandidateForMemoGroupId, LogicalRequirements, Memo, MemoGroupId, RequiredAccuracy,
};

/// The error grid of the readouts of a composed query.
const MAX_ERROR_LEVELS: [f64; 7] = [0.1, 0.05, 0.02, 0.01, 0.005, 0.002, 0.001];
/// The failure-probability grid of the readouts of a composed query: the
/// powers of ten from 1e-2 down to 1e-5, continued down to the first one at
/// or below a tenth of the query's per-value failure probability, so that up
/// to ten readouts each find a level and the levels sum within it (δ = 0.01
/// over 10 000 series is 1e-6 per value: two quantiles of a ratio read at
/// 1e-7 each).
fn failure_probability_levels(per_value: f64) -> Vec<f64> {
    let lowest = (per_value / 10.0).min(1e-5);
    let deepest_exponent = (-lowest.log10() - 1e-9).ceil().min(300.0) as i32;
    (2..=deepest_exponent)
        .map(|exponent| 1.0 / 10f64.powi(exponent))
        .collect()
}

/// Per group, by group id: per (query, index in `AccuracyMetric::ALL`), the
/// largest error in that metric the group's output may have for the query,
/// $\max_q \varepsilon_q / \lambda_{q,c,i}$ capped by the validity caps on
/// the way.
type Caps = Vec<BTreeMap<(usize, usize), f64>>;

/// Gives every sketch readout (`SummaryEstimate` over a sketch state) its
/// required accuracy, one readout candidate per requirement level: Pass 1's
/// candidate takes the first level and a copy is added per further level,
/// each written with `Memo::set_candidate_input_requirements`.
/// - A query whose path reads one readout group gives one level: its target
///   divided by the path's factor, the readout group's cap, at the target's
///   failure probability per value.
/// - A composed query (several readout groups, such as a ratio) gives every
///   level of the grid [`MAX_ERROR_LEVELS`] × [`failure_probability_levels`]
///   not looser than the cap and the per-value failure probability (the cap
///   itself when no error level is), one δ level of 1 when the target sets
///   no failure probability.
/// - A readout group reached by several queries gets the union of their
///   levels; one no query with a target reaches in the readout's metric
///   gets none.
///
/// Returns the readouts reached by a query whose whole-result failure
/// probability has no per-value one ($n_q$ unknown), each with that query:
/// Pass 2 prunes them (`RootPruneReason::ValuesPerEvaluationUnknown`).
pub(crate) fn set_readout_requirements(
    memo: &mut Memo,
) -> Result<BTreeMap<CandidateForMemoGroupId, usize>, Pass2RuleError> {
    let mut without_per_value_failure_probability = BTreeMap::new();
    let order = memo
        .topological_order()
        .ok_or(Pass2RuleError::CyclicDependencies)?;
    let caps = caps(memo, &order);
    let readout_groups: BTreeSet<MemoGroupId> = (0..memo.groups().len())
        .map(|index| MemoGroupId(index as u32))
        .filter(|&group| !sketch_readouts(memo, group).is_empty())
        .collect();
    let composed = composed_queries(memo, &order, &readout_groups);
    for &group in &readout_groups {
        for (index_in_group, metric) in sketch_readouts(memo, group) {
            let metric_index = AccuracyMetric::ALL
                .iter()
                .position(|known| *known == metric)
                .expect("every metric is listed");
            let mut levels: Vec<RequiredAccuracy> = Vec::new();
            let mut unknown_values_of = None;
            for (&(query, index), &cap) in &caps[group.0 as usize] {
                if index != metric_index {
                    continue;
                }
                let Some(target) = &memo.queries()[query].accuracy_target else {
                    continue;
                };
                let Some(query_levels) = query_levels(
                    target,
                    memo.queries()[query].number_of_values_per_query_evaluation,
                    metric,
                    cap,
                    composed.contains(&query),
                ) else {
                    unknown_values_of.get_or_insert(query);
                    continue;
                };
                for level in query_levels {
                    if !levels.contains(&level) {
                        levels.push(level);
                    }
                }
            }
            for readout in write_levels(memo, group, index_in_group, levels)? {
                if let Some(query) = unknown_values_of {
                    without_per_value_failure_probability.insert(readout, query);
                }
            }
        }
    }
    Ok(without_per_value_failure_probability)
}

/// The requirement levels one query gives a readout in `metric` whose cap
/// for it is `cap`; `None` when its target's failure probability has no
/// per-value one (a whole-result δ with $n_q$ unknown).
fn query_levels(
    target: &AccuracyTarget,
    number_of_values_per_query_evaluation: Option<u64>,
    metric: AccuracyMetric,
    cap: f64,
    composed: bool,
) -> Option<Vec<RequiredAccuracy>> {
    // The target's failure probability per value: a whole-result δ split
    // over the values one evaluation returns; 1 without a δ.
    let per_value = match target.failure_probability {
        None => 1.0,
        Some(delta) => {
            delta
                / target
                    .accuracy_scope
                    .max_values_covered(number_of_values_per_query_evaluation)?
                    .max(1) as f64
        }
    };
    let level = |max_error, failure_probability| RequiredAccuracy {
        metric,
        max_error,
        failure_probability,
    };
    if !composed {
        return Some(vec![level(cap, per_value)]);
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
        within(&failure_probability_levels(per_value), per_value)
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

/// Writes `levels` on readout `index_in_group` of `group`: the first on it,
/// each further one on a copy added to the group. No level leaves it
/// without a requirement. Returns the readout and its copies.
fn write_levels(
    memo: &mut Memo,
    group: MemoGroupId,
    index_in_group: usize,
    levels: Vec<RequiredAccuracy>,
) -> Result<Vec<CandidateForMemoGroupId>, Pass2RuleError> {
    let readout = CandidateForMemoGroupId {
        group,
        index_in_group,
    };
    let unset = memo.candidate(readout).clone();
    let mut written = vec![readout];
    for (n, level) in levels.into_iter().enumerate() {
        let id = if n == 0 {
            readout
        } else {
            memo.add_candidate(group, unset.clone())
                .map_err(Pass2RuleError::Candidate)?
        };
        if n > 0 {
            written.push(id);
        }
        memo.set_candidate_input_requirements(
            id,
            vec![LogicalRequirements {
                required_accuracy: Some(level),
            }],
        );
    }
    Ok(written)
}

/// The sketch readouts of `group` without a requirement yet, each with the
/// metric of its error ([`readout_metric`]).
fn sketch_readouts(memo: &Memo, group: MemoGroupId) -> Vec<(usize, AccuracyMetric)> {
    memo.groups()[group.0 as usize]
        .candidates
        .iter()
        .enumerate()
        .filter_map(|(index_in_group, candidate)| {
            let Operator::ASAP(ASAPOp::SummaryEstimate {
                summary_input,
                query,
            }) = &candidate.expression.operator
            else {
                return None;
            };
            if candidate.input_requirements[0].required_accuracy.is_some() {
                return None;
            }
            let summary_type =
                summary_type_of_operator(&memo.groups()[summary_input.0 as usize].expression)?;
            Some((index_in_group, readout_metric(summary_type, query)?))
        })
        .collect()
}

/// Each group's caps ([`Caps`]): each query root group's target, in the
/// target's metric (every metric when it names none), then, consumers
/// before producers, every candidate's caps passed to its inputs through
/// its error contract, $\min(b_{c,i},\ \text{cap}/\beta_{c,i})$, keeping
/// the largest per query and metric (the bottleneck over paths). A scalar
/// root's operands get none.
fn caps(memo: &Memo, order: &[MemoGroupId]) -> Caps {
    let mut caps: Caps = vec![BTreeMap::new(); memo.groups().len()];
    for (query, memo_query) in memo.queries().iter().enumerate() {
        let (Some(target), QueryRoot::Operator(group)) =
            (&memo_query.accuracy_target, &memo_query.root)
        else {
            continue;
        };
        for (metric_index, metric) in AccuracyMetric::ALL.iter().enumerate() {
            if target.accuracy_metric.is_none_or(|named| named == *metric) {
                raise(
                    &mut caps[group.0 as usize],
                    (query, metric_index),
                    target.max_error,
                );
            }
        }
    }
    for &group in order.iter().rev() {
        let group_caps = caps[group.0 as usize].clone();
        if group_caps.is_empty() {
            continue;
        }
        for consumer in &memo.groups()[group.0 as usize].candidates {
            let operator = &consumer.expression.operator;
            for (&(query, metric_index), &cap) in &group_caps {
                let Some(propagation) =
                    error_propagation(memo, operator, AccuracyMetric::ALL[metric_index])
                else {
                    continue;
                };
                for (input, &child) in operator.children().into_iter().enumerate() {
                    let factor = propagation.factor_per_input[input];
                    let validity =
                        propagation.validity_cap_per_input[input].unwrap_or(f64::INFINITY);
                    let passed = if factor > 0.0 {
                        validity.min(cap / factor)
                    } else {
                        validity
                    };
                    raise(&mut caps[child.0 as usize], (query, metric_index), passed);
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

/// Keeps the larger of `cap` and the known cap of `key`.
fn raise(caps: &mut BTreeMap<(usize, usize), f64>, key: (usize, usize), cap: f64) {
    let known = caps.entry(key).or_insert(cap);
    *known = known.max(cap);
}

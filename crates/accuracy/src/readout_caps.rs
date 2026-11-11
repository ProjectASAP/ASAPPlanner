//! Readout requirements (concepts doc, section 4, "Readout requirements"
//! and "Validity caps and pruning caps"): the caps the queries that reach
//! a readout pass down through their consumers' error contracts, and the
//! required-accuracy levels each readout gets from them.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use asap_types::accuracy::{AccuracyMetric, AccuracyTarget};
use asap_types::ir::{Operator, QueryRoot};
use asap_types::stages::{Memo, MemoGroupId, RequiredAccuracy};
use asap_types::summary_type::MAX_ERROR_LEVELS;

use crate::{error_propagation, sketch_readout_metric};

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

/// Per group, by group id: per metric, per query, the largest error in that
/// metric the group's output may have for the query,
/// $\max_q \varepsilon_q / \lambda_{q,c,i}$ capped by the validity caps on
/// the way.
type Caps = Vec<HashMap<AccuracyMetric, BTreeMap<usize, f64>>>;

/// The required-accuracy levels of the sketch readouts
/// ([`sketch_readout_metric`]) without a requirement yet, per readout
/// group and the readout's metric. `order` is the memo's topological order
/// (`Memo::topological_order`).
/// - A query whose path reads one readout group gives one level: its target
///   divided by the path's factor, the readout group's cap, at the target's
///   failure probability per value.
/// - A composed query (several readout groups, such as a ratio) gives every
///   level of the grid [`MAX_ERROR_LEVELS`] × [`failure_probability_levels`]
///   not looser than the cap and the per-value failure probability (the cap
///   itself when no error level is), one δ level of 1 when the target sets
///   no failure probability.
/// - A readout group reached by several queries gets the union of their
///   levels, in query order; one no query with a target reaches in the
///   readout's metric gets none.
///
/// `Err(query)` when no query gives a level while `query`, which reaches
/// the readout, has a whole-result failure probability without a per-value
/// one ($n_q$ unknown): Pass 2 prunes such a readout
/// (`RootPruneReason::ValuesPerEvaluationUnknown`). A readout another
/// query gives a level gets its levels.
pub fn readout_levels(
    memo: &Memo,
    order: &[MemoGroupId],
) -> HashMap<(MemoGroupId, AccuracyMetric), Result<Vec<RequiredAccuracy>, usize>> {
    let caps = caps(memo, order);
    let mut readout_metrics: BTreeMap<MemoGroupId, Vec<AccuracyMetric>> = BTreeMap::new();
    for (index, group) in memo.groups().iter().enumerate() {
        for candidate in &group.candidates {
            if candidate
                .input_requirements
                .first()
                .is_some_and(|input| input.required_accuracy.is_none())
            {
                if let Some(metric) = sketch_readout_metric(memo, candidate) {
                    let metrics = readout_metrics
                        .entry(MemoGroupId(index as u32))
                        .or_default();
                    if !metrics.contains(&metric) {
                        metrics.push(metric);
                    }
                }
            }
        }
    }
    let readout_groups: BTreeSet<MemoGroupId> = readout_metrics.keys().copied().collect();
    let composed = composed_queries(memo, order, &readout_groups);
    let mut levels_of_readouts = HashMap::new();
    for (group, metrics) in readout_metrics {
        for metric in metrics {
            let mut levels: Vec<RequiredAccuracy> = Vec::new();
            let mut unknown_values_of = None;
            let group_caps = caps[group.0 as usize].get(&metric).into_iter().flatten();
            for (&query, &cap) in group_caps {
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
            // Only a readout no query gives a level is pruned: one with a
            // level serves the queries that gave it.
            let levels = match unknown_values_of {
                Some(query) if levels.is_empty() => Err(query),
                _ => Ok(levels),
            };
            levels_of_readouts.insert((group, metric), levels);
        }
    }
    levels_of_readouts
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

/// Each group's caps ([`Caps`]): each query root group's target, in the
/// target's metric (every metric when it names none), then, consumers
/// before producers, every candidate's caps passed to its inputs through
/// its error contract, $\min(b_{c,i},\ \text{cap}/\beta_{c,i})$, keeping
/// the largest per query and metric (the bottleneck over paths). A scalar
/// root's operands get none.
fn caps(memo: &Memo, order: &[MemoGroupId]) -> Caps {
    let mut caps: Caps = vec![HashMap::new(); memo.groups().len()];
    for (query, memo_query) in memo.queries().iter().enumerate() {
        let (Some(target), QueryRoot::Operator(group)) =
            (&memo_query.accuracy_target, &memo_query.root)
        else {
            continue;
        };
        for metric in AccuracyMetric::ALL {
            if target.accuracy_metric.is_none_or(|named| named == metric) {
                raise(&mut caps[group.0 as usize], metric, query, target.max_error);
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
            for (&metric, caps_per_query) in &group_caps {
                let Some(propagation) = error_propagation(memo, operator, metric) else {
                    continue;
                };
                for (&query, &cap) in caps_per_query {
                    for (input, &child) in operator.children().into_iter().enumerate() {
                        let factor = propagation.factor_per_input[input];
                        let validity =
                            propagation.validity_cap_per_input[input].unwrap_or(f64::INFINITY);
                        let passed = if factor > 0.0 {
                            validity.min(cap / factor)
                        } else {
                            validity
                        };
                        raise(&mut caps[child.0 as usize], metric, query, passed);
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

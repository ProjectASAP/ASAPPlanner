//! Readout requirements (concepts doc, section 4, "Readout requirements"
//! and "Validity caps and pruning caps"): the caps the queries that reach
//! a readout pass down through their consumers' error contracts, and the
//! required-accuracy levels each readout gets from them.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use asap_types::accuracy::{AccuracyMetric, AccuracyTarget, ReadoutValueCounts};
use asap_types::ir::schema::FieldDataType;
use asap_types::ir::{ASAPOp, Operator};
use asap_types::stages::{CandidateForMemoGroupId, Memo, MemoGroupId, RequiredAccuracy};
use asap_types::summary_type::parameter_list;

use crate::estimate_dependence::{
    input_error_reaches_output, inputs_read_value_by_value, ranks_by_estimate,
    takes_each_value_from_one_input,
};
use crate::logical_edge::build_meets_level;
use crate::metric_rule::{query_roots_with_target, ErrorContractPerMetric};
use crate::readout::sketch_readout;
use crate::{AccuracyEvidence, ErrorMetricsOfMemo};

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
    /// without a per-value one at the readout: no bound on the readout
    /// values its covered output values depend on (the readout values per
    /// evaluation unknown, and so is the fan-in or the values covered).
    pub queries_without_per_value_failure_probability: Vec<usize>,
}

/// What [`readout_levels`] gives: each readout's levels, and the readout
/// values each query's δ is split over.
#[derive(Debug, Clone, PartialEq)]
pub struct ReadoutLevels {
    /// Per sketch readout without a requirement yet: its levels.
    pub of_readout: BTreeMap<CandidateForMemoGroupId, ReadoutRequiredAccuracy>,
    /// Per query, by index: the readout values (estimates) its output can
    /// depend on, `R` and the fan-in
    /// ([`ReadoutsReadByQueries::readout_values`]), which name the bound
    /// its δ is split by (`AccuracyTarget::failure_probability_bound`) and
    /// Stage 3's constraint (K) checks; `None` for a query without a
    /// target.
    ///
    /// Example: `q or r` over 100 series each: `R` 200, fan-in 1.
    pub readout_values_of_query: Vec<Option<ReadoutValueCounts>>,
}

/// The required-accuracy levels of the sketch readouts without a
/// requirement yet, per readout ([`ReadoutRequiredAccuracy`]), from
/// `error_metrics` of `memo` ([`ErrorMetricsOfMemo::of`]). `order` is the
/// memo's topological order (`Memo::topological_order`); `evidence` is the
/// deployment's measured accuracy the edges check builds against.
/// - A query whose path reads one readout group gives one level: its target
///   divided by the path's factor, the readout group's cap, at the target's
///   failure probability per readout value ([`query_levels`]: split over
///   the readout values the output values its scope covers depend on).
/// - A composed query (several readouts whose errors reach its output,
///   β > 0, such as a ratio; `q and r` and `q or r` combine none:
///   [`readouts_read_by_queries`]) gives every level of the grid
///   [`MAX_ERROR_LEVELS`] × [`failure_probability_levels`] not looser than
///   the cap and the per-value failure probability (the cap itself when no
///   error level is), one δ level of 1 when the target sets no failure
///   probability.
/// - A readout group reached by several queries gets the union of their
///   levels, in query order; one no query with a target reaches in the
///   readout's metric gets none. Readouts of one group in one metric get
///   the same levels.
/// - A level is dropped when the listed builds that meet it are exactly
///   those that meet a stricter level of the group ([`builds_meeting`]):
///   any producer a copy at the looser level accepts, the stricter copy
///   accepts too, so the looser copy only adds a choice of equal cost.
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
    evidence: &dyn AccuracyEvidence,
) -> ReadoutLevels {
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
    let readouts_read = readouts_read_by_queries(memo, error_metrics, order);
    let mut readouts_of_group: HashMap<
        (MemoGroupId, AccuracyMetric),
        Vec<CandidateForMemoGroupId>,
    > = HashMap::new();
    for &(readout, metric) in &readouts {
        readouts_of_group
            .entry((readout.group, metric))
            .or_default()
            .push(readout);
    }
    let mut levels_of_group: HashMap<(MemoGroupId, AccuracyMetric), ReadoutRequiredAccuracy> =
        HashMap::new();
    let readout_values_of_query = (0..memo.queries().len())
        .map(|query| readouts_read.readout_values.get(&query).copied())
        .collect();
    let of_readout = readouts
        .into_iter()
        .map(|(readout, metric)| {
            let group = readout.group;
            let levels = levels_of_group
                .entry((group, metric))
                .or_insert_with(|| {
                    let mut required = group_levels(memo, &caps, &readouts_read, group, metric);
                    required.levels = without_redundant_levels(
                        memo,
                        &readouts_of_group[&(group, metric)],
                        required.levels,
                        evidence,
                    );
                    required
                })
                .clone();
            (readout, levels)
        })
        .collect();
    ReadoutLevels {
        of_readout,
        readout_values_of_query,
    }
}

/// The levels of readout group `group` in `metric` ([`readout_levels`]).
fn group_levels(
    memo: &Memo,
    caps: &Caps,
    readouts_read: &ReadoutsReadByQueries,
    group: MemoGroupId,
    metric: AccuracyMetric,
) -> ReadoutRequiredAccuracy {
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
            readouts_read.readout_values[&query],
            metric,
            cap,
            readouts_read.composed.contains(&query),
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

/// `levels` of the readouts `readouts` of one group, without each level
/// the listed builds that meet it ([`builds_meeting`]) are exactly those
/// that meet a stricter level of `levels` (at most its error and failure
/// probability): DDSketch's relative error does not depend on δ, so of
/// (0.05, 0.01) and (0.05, 1e-4) only the second is kept; KLL's does, so
/// both are kept when a build meets the first only. Order is kept, and so
/// is every strictest level, so each dropped level's builds stay those of a
/// kept one.
fn without_redundant_levels(
    memo: &Memo,
    readouts: &[CandidateForMemoGroupId],
    levels: Vec<RequiredAccuracy>,
    evidence: &dyn AccuracyEvidence,
) -> Vec<RequiredAccuracy> {
    if levels.len() < 2 {
        return levels;
    }
    let builds: Vec<Vec<bool>> = levels
        .iter()
        .map(|&level| builds_meeting(memo, readouts, level, evidence))
        .collect();
    let stricter = |strict: &RequiredAccuracy, loose: &RequiredAccuracy| {
        strict != loose
            && strict.max_error <= loose.max_error
            && strict.failure_probability <= loose.failure_probability
    };
    levels
        .iter()
        .enumerate()
        .filter(|&(index, level)| {
            !levels.iter().enumerate().any(|(other, stricter_level)| {
                stricter(stricter_level, level) && builds[other] == builds[index]
            })
        })
        .map(|(_, &level)| level)
        .collect()
}

/// Per readout of `readouts` and listed θ of the summary type it reads
/// (`parameter_list`, the only θ a state candidate may have), in that
/// order: whether a state built with θ meets `level` for the readout's
/// statistic, as the logical edge checks it ([`build_meets_level`]).
fn builds_meeting(
    memo: &Memo,
    readouts: &[CandidateForMemoGroupId],
    level: RequiredAccuracy,
    evidence: &dyn AccuracyEvidence,
) -> Vec<bool> {
    readouts
        .iter()
        .filter_map(|&readout| {
            let candidate = memo.candidate(readout);
            let Operator::ASAP(ASAPOp::SummaryEstimate { summary_input, .. }) =
                candidate.expression.operator
            else {
                return None;
            };
            let (summary_type, statistic) = sketch_readout(memo, candidate)?;
            Some((summary_input, summary_type, statistic))
        })
        .flat_map(|(summary_input, summary_type, statistic)| {
            let FieldDataType::Sketch(algorithm) = summary_type else {
                unreachable!("a sketch readout reads a sketch state")
            };
            parameter_list(algorithm).iter().map(move |parameters| {
                build_meets_level(
                    memo,
                    summary_input,
                    &summary_type,
                    parameters,
                    statistic,
                    level,
                    evidence,
                )
            })
        })
        .collect()
}

/// The requirement levels one query gives a readout in `metric` whose cap
/// for it is `cap`; `None` when its target's failure probability has no
/// per-value one. The query's statement fails only when a readout value
/// one of the output values it covers depends on does, so δ splits over
/// those (`AccuracyTarget::failure_probability_per_readout_value`, over
/// `readout_values` of [`ReadoutsReadByQueries::readout_values`]).
/// - `max(quantile_over_time(0.9, m[5m]))` over 1000 series at δ = 0.01
///   returns one value but reads 1000 readout values: 1e-5.
/// - `max by (job) (q{host="h0"} or q{host="h1"})` with 3 series per side
///   reads 6 readout values, and a job's value may depend on all: δ / 6.
/// - A bare `q or r` copies each output value from one readout value:
///   per value δ, over a whole result of $n_q$ values δ / $n_q$.
/// - `SELECT count(DISTINCT dst)` reads one readout value: 0.01.
fn query_levels(
    target: &AccuracyTarget,
    number_of_values_per_query_evaluation: Option<u64>,
    readout_values: ReadoutValueCounts,
    metric: AccuracyMetric,
    cap: f64,
    composed: bool,
) -> Option<Vec<RequiredAccuracy>> {
    let per_value_failure_probability = target.failure_probability_per_readout_value(
        number_of_values_per_query_evaluation,
        readout_values,
    )?;
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
                    // An input the output does not depend on (a count's,
                    // β = 0) gets no cap from it.
                    if contract.input_error_reaches_output(input) {
                        let factor = contract.error_factor_per_input[input];
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

/// What each query with a target reads from the readouts below it
/// ([`readouts_read_by_queries`]).
struct ReadoutsReadByQueries {
    /// The queries that combine several readouts: a candidate they reach
    /// has two or more inputs below which lies a readout whose error
    /// reaches its output ($\beta > 0$ in a metric of its error contract,
    /// [`caps`] — a readout reaches the input through β > 0 at every step
    /// below): a ratio of two quantiles. `q and r` is not composed: its
    /// output copies `q`'s values and reads only which series `r` has
    /// ($\beta = 0$), so `q`'s readout keeps the single level of its cap.
    /// Neither is `q / count(r)`: the count reads no value of `r`. Nor is
    /// `q or r`: each output value is copied from `q` or from `r`, so its
    /// error is the larger of their errors, not their sum
    /// ([`takes_each_value_from_one_input`]), and each readout keeps the
    /// single level of its cap. Readouts of one value by different summary
    /// types are alternatives in one group's candidates, not a combination.
    composed: BTreeSet<usize>,
    /// Per query, by index: the readout values (estimates) its output can
    /// depend on, through inputs whose estimates reach their consumer's
    /// output (`ErrorMetricsOfMemo::input_estimates_reach_output`: β > 0,
    /// or values chosen by estimates), among alternative candidates of a
    /// group the largest. Per evaluation, summed over the readout groups
    /// below it (`Memo::max_estimates_per_evaluation` each: its values, or
    /// every item a top-k ranks); per output value, 1 at a readout (all of
    /// its estimates for a top-k, which ranks them), carried value by
    /// value through [`inputs_read_value_by_value`] operators (summed over
    /// inputs, or the largest where each value depends on one input,
    /// `ErrorMetricsOfMemo::each_value_depends_on_one_input`: `or` and a
    /// concatenation) and all of the inputs' per-evaluation values through
    /// any other operator (a reduction); a readout group also counts its
    /// other candidates (a reduction over readouts below it). `max by (job)
    /// (q{host="h0"} or q{host="h1"})` with 3 series per side: 6 and 6;
    /// `q or r` alone: 6 and 1; `q / count(r)`: `q`'s alone. A readout
    /// group reached on two paths (`q or q`) counts twice, which only
    /// tightens the split.
    readout_values: BTreeMap<usize, ReadoutValueCounts>,
}

/// The [`ReadoutsReadByQueries`] of `memo`, in one walk up `order` (the
/// memo's topological order) from its readout groups (those with a
/// candidate that is a sketch readout, `ErrorMetricsOfMemo::readout_error_metric`).
fn readouts_read_by_queries(
    memo: &Memo,
    error_metrics: &ErrorMetricsOfMemo,
    order: &[MemoGroupId],
) -> ReadoutsReadByQueries {
    let error_contract = &error_metrics.error_contract;
    let readout_groups: BTreeSet<MemoGroupId> = memo
        .groups()
        .iter()
        .enumerate()
        .filter(|(group, memo_group)| {
            (0..memo_group.candidates.len()).any(|index_in_group| {
                error_metrics
                    .readout_error_metric(CandidateForMemoGroupId {
                        group: MemoGroupId(*group as u32),
                        index_in_group,
                    })
                    .is_some()
            })
        })
        .map(|(group, _)| MemoGroupId(group as u32))
        .collect();
    // Per group: the readout values its output can depend on (`Some(0)`
    // when none, so it carries no readout error; `None` unknown).
    let none_read = ReadoutValueCounts {
        per_evaluation: Some(0),
        per_output_value: Some(0),
    };
    let mut readout_values_per_group = vec![none_read; memo.groups().len()];
    let mut composed = BTreeSet::new();
    for &group in order {
        let candidates = &memo.groups()[group.0 as usize].candidates;
        let mut most_inputs_reading_with_error = 0;
        let mut most_readout_values = none_read;
        for (index_in_group, (candidate, contract_per_metric)) in candidates
            .iter()
            .zip(&error_contract[group.0 as usize])
            .enumerate()
        {
            let id = CandidateForMemoGroupId {
                group,
                index_in_group,
            };
            let operator = &candidate.expression.operator;
            let children = operator.children();
            let reads_readouts = |input: usize| {
                readout_values_per_group[children[input].0 as usize].per_evaluation != Some(0)
            };
            let reads_with_error = |input: &usize| {
                reads_readouts(*input) && input_error_reaches_output(contract_per_metric, *input)
            };
            let reads_estimates = |input: &usize| {
                reads_readouts(*input)
                    && error_metrics.input_estimates_reach_output(memo, id, *input)
            };
            let inputs_reading_with_error = (0..children.len()).filter(reads_with_error).count();
            // Each output value comes from one input, so the output's error
            // is one input's, never a sum of several; its values still
            // depend on the readouts of every input.
            let inputs_combined = if takes_each_value_from_one_input(operator) {
                inputs_reading_with_error.min(1)
            } else {
                inputs_reading_with_error
            };
            most_inputs_reading_with_error = most_inputs_reading_with_error.max(inputs_combined);
            let read_of = |input: usize| readout_values_per_group[children[input].0 as usize];
            let per_evaluation = (0..children.len())
                .filter(reads_estimates)
                .try_fold(0u64, |total, input| {
                    total.checked_add(read_of(input).per_evaluation?)
                });
            // One output value reads one value of each of the first
            // `value_by_value` inputs, and every value of any other input.
            let per_output_value =
                inputs_read_value_by_value(operator).map_or(per_evaluation, |value_by_value| {
                    let mut per_input = (0..children.len()).filter(reads_estimates).map(|input| {
                        if input < value_by_value {
                            read_of(input).per_output_value
                        } else {
                            read_of(input).per_evaluation
                        }
                    });
                    if error_metrics.each_value_depends_on_one_input(memo, id) {
                        per_input.try_fold(0u64, |most, values| Some(most.max(values?)))
                    } else {
                        per_input.try_fold(0u64, |total, values| total.checked_add(values?))
                    }
                });
            most_readout_values = most_readout_values.larger(ReadoutValueCounts {
                per_evaluation,
                per_output_value,
            });
        }
        if most_inputs_reading_with_error > 1 {
            composed.extend(
                memo.groups()[group.0 as usize]
                    .reached_by_queries()
                    .iter()
                    .copied(),
            );
        }
        // A readout group's own readouts read its values per evaluation,
        // each output value one of them, or all of them for a top-k, whose
        // output keys are chosen by their estimates; any other candidate of
        // the group (a reduction over readouts below it) still counts.
        readout_values_per_group[group.0 as usize] = if readout_groups.contains(&group) {
            let per_evaluation = memo.max_estimates_per_evaluation(group);
            let ranks_by_estimate = candidates
                .iter()
                .any(|candidate| ranks_by_estimate(&candidate.expression.operator));
            let own = ReadoutValueCounts {
                per_evaluation,
                per_output_value: if ranks_by_estimate {
                    per_evaluation
                } else {
                    Some(1)
                },
            };
            own.larger(most_readout_values)
        } else {
            most_readout_values
        };
    }
    // A query of several roots (a scalar expression over them) reads every
    // root's values, one output value each root's output value.
    let mut readout_values: BTreeMap<usize, ReadoutValueCounts> = BTreeMap::new();
    for (query, root, _, _) in query_roots_with_target(memo) {
        let total = readout_values.entry(query).or_insert(none_read);
        *total = total.plus(readout_values_per_group[root.0 as usize]);
    }
    ReadoutsReadByQueries {
        composed,
        readout_values,
    }
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

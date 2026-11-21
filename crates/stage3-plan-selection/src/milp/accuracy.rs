//! The accuracy extension of the selection MILP
//! (`docs/design_docs/concepts/accuracy-models.md`, "Checking each query's
//! target in selection: constraint (K)"): constraint (K), per execution and
//! per error metric, over the error each execution adds and passes on
//! ([`AccuracyRules`], [`ErrorModel`]), and the same rule checked on one
//! selection ([`AccuracyRules::selection_meets_targets`]), which the greedy
//! upper bound needs to be a feasible plan of (K) and which checks the
//! solver's selection exactly. One composition rule,
//! [`AccuracyRules::compose`], gives both that check and the ceilings of
//! (K)'s big-M rows; [`accuracy_constraints`] writes the same rule as rows.

use std::collections::{BTreeMap, HashMap};
use std::hash::{Hash, Hasher};

use good_lp::{constraint, variable, Constraint, Expression, ProblemVariables, Variable};

use asap_accuracy::{
    inputs_read_value_by_value, query_roots_with_target, ranks_by_estimate,
    takes_each_value_from_one_input, tolerated_limit, ErrorContract, ErrorMetricsOfMemo,
};
use asap_types::accuracy::{AccuracyMetric, FailureProbabilityBound};
use asap_types::stages::{CandidateForMemoGroupId, MemoGroupId};

use super::equivalent_contexts::SharedVariables;
use super::model::SelectionInputs;
use crate::selectable::QueryId;

/// An execution's error as (K) composes it: its error bound per metric
/// (by [`AccuracyMetric::index`]) and two failure probabilities, each the
/// summed δ of estimates (one readout value in one evaluation) by the
/// union bound (`accuracy-models.md`, "Which estimates a result depends
/// on"). A failure probability is `None` when no declared fact bounds it
/// (a readout whose estimates per evaluation are unknown).
///
/// Example: a DDSketch readout over 1000 series required at relative 0.01
/// with δ = 1e-3 is 0.01 in `Relative`, 0 in every other metric, 1e-3 for
/// one value and 1000 × 1e-3 = 1 for all of them.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(super) struct ExecutionError {
    /// $E_{c,m}$ per metric.
    error_bound_per_metric: [f64; AccuracyMetric::COUNT],
    /// $P_c$: the largest, over its output values, of the summed δ of the
    /// estimates one value depends on.
    failure_probability_of_one_value: Option<f64>,
    /// $W_c$: the summed δ of every estimate any of its output values
    /// depends on.
    failure_probability_of_all_values: Option<f64>,
}

impl Default for ExecutionError {
    /// No error, and no estimate to fail: an exact execution's.
    fn default() -> Self {
        Self {
            error_bound_per_metric: [0.0; AccuracyMetric::COUNT],
            failure_probability_of_one_value: Some(0.0),
            failure_probability_of_all_values: Some(0.0),
        }
    }
}

/// Sums of failure probabilities, `None` (unbounded) when any term is.
fn sum_of(terms: impl IntoIterator<Item = Option<f64>>) -> Option<f64> {
    terms
        .into_iter()
        .try_fold(0.0, |total, term| Some(total + term?))
}

/// The largest of failure probabilities, `None` (unbounded) when any is.
fn largest_of(terms: impl IntoIterator<Item = Option<f64>>) -> Option<f64> {
    terms
        .into_iter()
        .try_fold(0.0, |largest: f64, term| Some(largest.max(term?)))
}

impl ExecutionError {
    /// The larger of `self` and `other` in each metric and in each failure
    /// probability, as the most any one of several producers passes on.
    fn largest(self, other: Self) -> Self {
        let mut largest = self;
        for (error, other) in largest
            .error_bound_per_metric
            .iter_mut()
            .zip(other.error_bound_per_metric)
        {
            *error = error.max(other);
        }
        largest.failure_probability_of_one_value = largest_of([
            self.failure_probability_of_one_value,
            other.failure_probability_of_one_value,
        ]);
        largest.failure_probability_of_all_values = largest_of([
            self.failure_probability_of_all_values,
            other.failure_probability_of_all_values,
        ]);
        largest
    }

    /// Whether it is 0 in every metric and in each failure probability.
    fn is_zero(&self) -> bool {
        self.failure_probability_of_one_value == Some(0.0)
            && self.failure_probability_of_all_values == Some(0.0)
            && self.error_bound_per_metric.iter().all(|&e| e == 0.0)
    }

    /// The failure probability `bound` checks: of all values for every
    /// estimate read, of one value for each covered value.
    fn failure_probability_checked_by(&self, bound: &FailureProbabilityBound) -> Option<f64> {
        match bound {
            FailureProbabilityBound::EveryEstimateRead { .. } => {
                self.failure_probability_of_all_values
            }
            FailureProbabilityBound::EachCoveredValue { .. } => {
                self.failure_probability_of_one_value
            }
        }
    }
}

/// How an execution's output values read its inputs, which decides how
/// (K) composes its error and failure probabilities (`asap_accuracy`'s
/// operator classes, the ones Pass 2's split counts by).
#[derive(Debug, Clone)]
struct HowInputsAreRead {
    /// Its first so many inputs are read value by value: each output value
    /// reads one value of each ([`inputs_read_value_by_value`]); every
    /// other input is read whole. `None`: all are read whole (a reduction).
    value_by_value: Option<usize>,
    /// Each output value is copied from one input (`or`, a concatenation:
    /// [`takes_each_value_from_one_input`]), so its error is the largest of
    /// the inputs', not their sum.
    each_value_from_one_input: bool,
    /// Each output value depends on the estimates of one input only
    /// (`ErrorMetricsOfMemo::each_value_depends_on_one_input`): the inputs
    /// read value by value add the largest of their one-value failure
    /// probabilities, not their sum. Not for `topk(3, q) or r`, whose
    /// values from `r` depend on the top-k's choice too.
    each_value_depends_on_one_input: bool,
    /// Per input: whether an estimate it depends on can make the output
    /// miss (`ErrorMetricsOfMemo::input_estimates_reach_output`), so its
    /// failure probabilities add to the output's. Not a count's input, nor
    /// the right side of `and` and `unless`, unless a top-k chooses its
    /// values.
    estimates_reach_output: Vec<bool>,
}

impl HowInputsAreRead {
    /// Whether input `input` is read value by value.
    fn reads_value_by_value(&self, input: usize) -> bool {
        self.value_by_value.is_some_and(|count| input < count)
    }

    /// Whether an estimate input `input` depends on can make the output
    /// miss ([`Self::estimates_reach_output`]).
    fn estimates_of_input_reach_output(&self, input: usize) -> bool {
        self.estimates_reach_output[input]
    }
}

/// What composing one execution's error from what its inputs read gives
/// ([`AccuracyRules::compose`]).
struct Composition {
    /// Its own error plus each input's error times the contract's factor,
    /// and its own failure probabilities plus its inputs' by the operator
    /// classes.
    error: ExecutionError,
    /// Whether every input's error passes the execution's metric rule (it
    /// has a contract in that metric) and the contract's validity cap.
    inputs_admitted: bool,
}

/// What (K) reads of the memo, built once per selection when some query
/// has an accuracy target: each candidate's error contracts, each
/// execution's own error, which executions a query with a target reaches,
/// and what each query root allows.
pub(super) struct AccuracyRules {
    /// The candidates' error contracts per metric
    /// (`ErrorMetricsOfMemo::error_contract`).
    error_metrics: ErrorMetricsOfMemo,
    /// Per execution, by position in `SelectableExecutions::executions`,
    /// its logical candidate.
    candidate_of_execution: Vec<CandidateForMemoGroupId>,
    /// Per execution, the error it adds: a readout's requirement
    /// (`CandidateForMemoGroup::required_accuracy`), its δ for one value
    /// (`R` times it for a top-k, which ranks every estimate) and `R`
    /// times its δ for all values (`Memo::max_estimates_per_evaluation`;
    /// 0 at δ = 0 whatever `R`); 0 for any other.
    own_error_per_execution: Vec<ExecutionError>,
    /// Per execution, how its output values read its inputs.
    how_inputs_are_read: Vec<HowInputsAreRead>,
    /// Per execution, whether a query with an accuracy target reaches its
    /// group.
    reached_by_query_with_target: Vec<bool>,
    /// Every execution, its producers' groups before its own (the memo's
    /// topological order): an order that composes each producer before
    /// its consumers.
    executions_producers_first: Vec<usize>,
    /// Per (query, output group its root reads), what its root check
    /// bounds the output by.
    roots: HashMap<(QueryId, MemoGroupId), RootLimits>,
}

/// What a root check of (K) bounds one query output by. The limits are
/// widened by [`tolerated_limit`] where they are compared, as the edge
/// check widens them. Two outputs with equal limits have the same root
/// rows, so they compare and hash by the limits' bits.
///
/// Example: `p99 / p50` at relative 0.03 with δ = 0.01 over a whole result
/// of 100 values, reading two readouts over 100 series each, allows 0.03
/// in `Relative`, 0 in every other metric, and checks each covered value:
/// its one-value failure probability at most 1e-4.
#[derive(Debug, Clone, Copy)]
pub(super) struct RootLimits {
    /// Per metric (by [`AccuracyMetric::index`]), the error the output may
    /// have: the target's ε in a metric the root allows
    /// ([`query_roots_with_target`]), 0 in any other.
    tolerated_error_per_metric: [f64; AccuracyMetric::COUNT],
    /// The union bound the query's failure probability is checked by, with
    /// its limit (the bound Pass 2 split δ by,
    /// `LogicalASAPMemoCandidates::failure_probability_bound`);
    /// `None` without a δ, or with neither bound known (Pass 2 then gives
    /// the query's readouts no level, and (X) makes it read them exactly).
    failure_probability_bound: Option<FailureProbabilityBound>,
}

impl RootLimits {
    /// The limits' bits, which equality, hashing and order compare: the
    /// error per metric, and the bound's kind and limit (the rows read no
    /// more of it).
    fn bits(&self) -> ([u64; AccuracyMetric::COUNT], Option<(bool, u64)>) {
        (
            self.tolerated_error_per_metric.map(f64::to_bits),
            self.failure_probability_bound.map(|bound| {
                (
                    matches!(bound, FailureProbabilityBound::EachCoveredValue { .. }),
                    bound.limit().to_bits(),
                )
            }),
        )
    }
}

impl PartialEq for RootLimits {
    fn eq(&self, other: &Self) -> bool {
        self.bits() == other.bits()
    }
}

impl Eq for RootLimits {}

impl Hash for RootLimits {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.bits().hash(state);
    }
}

impl PartialOrd for RootLimits {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for RootLimits {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.bits().cmp(&other.bits())
    }
}

impl AccuracyRules {
    /// The rules over `inputs`; `None` when no query has an accuracy
    /// target, so (K) has no rows.
    /// Whether execution `b`'s own error is no more than `a`'s in every
    /// metric and failure probability (an unbounded one, `None`, is more
    /// than any bound), both of one memo group.
    pub(super) fn own_error_no_more(&self, b: usize, a: usize) -> bool {
        let (eb, ea) = (
            &self.own_error_per_execution[b],
            &self.own_error_per_execution[a],
        );
        let no_more = |x: Option<f64>, y: Option<f64>| match (x, y) {
            (_, None) => true,
            (None, Some(_)) => false,
            (Some(x), Some(y)) => x <= y,
        };
        eb.error_bound_per_metric
            .iter()
            .zip(&ea.error_bound_per_metric)
            .all(|(x, y)| x <= y)
            && no_more(
                eb.failure_probability_of_one_value,
                ea.failure_probability_of_one_value,
            )
            && no_more(
                eb.failure_probability_of_all_values,
                ea.failure_probability_of_all_values,
            )
            && self.candidate_of_execution[b].group == self.candidate_of_execution[a].group
    }

    pub(super) fn of(inputs: &SelectionInputs<'_>) -> Option<Self> {
        let physical = inputs.physical;
        let memo = &physical.logical_candidates.memo;
        if memo
            .queries()
            .iter()
            .all(|query| query.accuracy_target.is_none())
        {
            return None;
        }
        // Pass 2 rejects a cyclic memo (`Pass2RuleError::CyclicDependencies`).
        let order = memo
            .topological_order()
            .expect("Stage 3 reads Pass 2's acyclic memo");
        let candidate_of_execution: Vec<CandidateForMemoGroupId> = inputs
            .selectable
            .executions()
            .iter()
            .map(|execution| {
                physical.physical_candidates[execution.candidate.0].logical_candidate_id
            })
            .collect();
        let own_error_per_execution = candidate_of_execution
            .iter()
            .map(|&id| {
                let mut own = ExecutionError::default();
                let candidate = memo.candidate(id);
                if let Some(required) = candidate.required_accuracy() {
                    own.error_bound_per_metric[required.metric.index()] = required.max_error;
                    let delta = required.failure_probability;
                    // At δ = 0 no estimate can fail, however many there are.
                    let of_every_estimate = if delta == 0.0 {
                        Some(0.0)
                    } else {
                        memo.max_estimates_per_evaluation(id.group)
                            .map(|estimates| estimates as f64 * delta)
                    };
                    own.failure_probability_of_one_value =
                        if ranks_by_estimate(&candidate.expression.operator) {
                            of_every_estimate
                        } else {
                            Some(delta)
                        };
                    own.failure_probability_of_all_values = of_every_estimate;
                }
                own
            })
            .collect();
        let error_metrics = ErrorMetricsOfMemo::of(memo, &order);
        let how_inputs_are_read = candidate_of_execution
            .iter()
            .map(|&id| {
                let operator = &memo.candidate(id).expression.operator;
                HowInputsAreRead {
                    value_by_value: inputs_read_value_by_value(operator),
                    each_value_from_one_input: takes_each_value_from_one_input(operator),
                    each_value_depends_on_one_input: error_metrics
                        .each_value_depends_on_one_input(memo, id),
                    estimates_reach_output: (0..operator.children().len())
                        .map(|input| error_metrics.input_estimates_reach_output(memo, id, input))
                        .collect(),
                }
            })
            .collect();
        let reached_by_query_with_target = candidate_of_execution
            .iter()
            .map(|id| {
                memo.groups()[id.group.0 as usize]
                    .reached_by_queries()
                    .iter()
                    .any(|&query| memo.queries()[query].accuracy_target.is_some())
            })
            .collect();
        let mut position_in_order = vec![0; memo.groups().len()];
        for (position, group) in order.iter().enumerate() {
            position_in_order[group.0 as usize] = position;
        }
        let mut executions_producers_first: Vec<usize> =
            (0..candidate_of_execution.len()).collect();
        executions_producers_first
            .sort_by_key(|&c| position_in_order[candidate_of_execution[c].group.0 as usize]);
        let roots = query_roots_with_target(memo)
            .into_iter()
            .map(|(query, root, target, allowed_metrics)| {
                let failure_probability_bound =
                    physical.logical_candidates.failure_probability_bound(query);
                let mut tolerated_error_per_metric = [0.0; AccuracyMetric::COUNT];
                for metric in allowed_metrics {
                    tolerated_error_per_metric[metric.index()] = tolerated_limit(target.max_error);
                }
                let limits = RootLimits {
                    tolerated_error_per_metric,
                    failure_probability_bound,
                };
                ((QueryId(query), root), limits)
            })
            .collect();
        Some(Self {
            error_metrics,
            candidate_of_execution,
            own_error_per_execution,
            how_inputs_are_read,
            reached_by_query_with_target,
            executions_producers_first,
            roots,
        })
    }

    /// What the root check of `query`'s output reading `group` bounds it
    /// by; `None` when the query has no target, so (K) checks nothing
    /// there.
    pub(super) fn root_limits(&self, query: QueryId, group: MemoGroupId) -> Option<&RootLimits> {
        self.roots.get(&(query, group))
    }

    /// Execution `c`'s error contract in metric `m` (by
    /// [`AccuracyMetric::index`]); `None` where it has none (the metric
    /// rule).
    fn contract(&self, c: usize, m: usize) -> Option<&ErrorContract> {
        self.error_metrics
            .error_contract(self.candidate_of_execution[c], AccuracyMetric::ALL[m])
    }

    /// Execution `c`'s error composed from `inputs_read`, the error read on
    /// each of its inputs (input index, error), by (K)'s rule
    /// (`accuracy-models.md`, "Checking each query's target in
    /// selection"): its own error
    /// plus, per metric, $\beta_{c,i,m}$ times each input's error in that
    /// metric where the input's error reaches the output ($\beta > 0$); the
    /// largest of those instead when each output value is copied from one
    /// input (`or`, a concatenation), whose error is that input's. Its
    /// failure probabilities add, over the inputs whose error reaches
    /// the output in some metric (a union bound over estimates):
    /// - of one value, its own plus the one-value failure probability of
    ///   each input read value by value (the largest of them when each
    ///   value is copied from one input) plus the all-values one of each
    ///   input read whole;
    /// - of all values, its own plus each input's all-values one.
    ///
    /// An input's error in a metric `c` has no contract for breaks the
    /// metric rule, and one above the contract's validity cap $b_{c,i}$
    /// breaks the cap: either leaves the inputs not admitted, and adds
    /// nothing in that metric for the metric rule.
    ///
    /// Examples: `p99 / p50`'s contract passes each operand's relative
    /// error with factor 1.111 and caps the denominator at 0.1: reading
    /// 0.02 and 0.02 composes 0.044, admitted; reading a denominator at
    /// 0.2 is not admitted. `max` over a readout of 1000 series at δ 1e-5
    /// reads it whole: 0.01 for one value and for all. `q or r` over
    /// readouts at δ 1e-4 of 100 series each: 1e-4 for one value, 0.02
    /// for all.
    fn compose(
        &self,
        c: usize,
        inputs_read: impl IntoIterator<Item = (usize, ExecutionError)>,
    ) -> Composition {
        let own = self.own_error_per_execution[c];
        let mut composed = Composition {
            error: own,
            inputs_admitted: true,
        };
        let how = &self.how_inputs_are_read[c];
        let mut one_value_of_inputs_read_value_by_value = Vec::new();
        let mut all_values_of_inputs_read_whole = Vec::new();
        let mut all_values_of_inputs = Vec::new();
        for (i, read) in inputs_read {
            if how.estimates_of_input_reach_output(i) {
                all_values_of_inputs.push(read.failure_probability_of_all_values);
                if how.reads_value_by_value(i) {
                    one_value_of_inputs_read_value_by_value
                        .push(read.failure_probability_of_one_value);
                } else {
                    all_values_of_inputs_read_whole.push(read.failure_probability_of_all_values);
                }
            }
            for (m, &error) in read.error_bound_per_metric.iter().enumerate() {
                if error <= 0.0 {
                    continue;
                }
                let Some(contract) = self.contract(c, m) else {
                    composed.inputs_admitted = false;
                    continue;
                };
                if !contract.input_error_reaches_output(i) {
                    continue;
                }
                if contract.linear_bound_holds_up_to_per_input[i]
                    .is_some_and(|holds_up_to| error > tolerated_limit(holds_up_to))
                {
                    composed.inputs_admitted = false;
                }
                let of_input = contract.error_factor_per_input[i] * error;
                let composed_error = &mut composed.error.error_bound_per_metric[m];
                if how.each_value_from_one_input {
                    // Each output value's error is one input's.
                    *composed_error = composed_error.max(own.error_bound_per_metric[m] + of_input);
                } else {
                    *composed_error += of_input;
                }
            }
        }
        let of_inputs_read_value_by_value = if how.each_value_depends_on_one_input {
            largest_of(one_value_of_inputs_read_value_by_value)
        } else {
            sum_of(one_value_of_inputs_read_value_by_value)
        };
        composed.error.failure_probability_of_one_value = sum_of([
            own.failure_probability_of_one_value,
            of_inputs_read_value_by_value,
            sum_of(all_values_of_inputs_read_whole),
        ]);
        composed.error.failure_probability_of_all_values = sum_of([
            own.failure_probability_of_all_values,
            sum_of(all_values_of_inputs),
        ]);
        composed
    }

    /// Whether the selection `edges` (by position in
    /// `SelectableExecutions::edges`) with the outputs `answering` (query,
    /// execution answering it, in `SelectableExecutions::answering` order)
    /// meets (K), exactly in f64: each execution a query with a target
    /// reaches, composed ([`Self::compose`]) from the producers selected
    /// on its inputs, admits its inputs' errors and, at each output of a
    /// query with a target, is within the root's error limits; and each
    /// query's failure probability its bound checks, summed over its
    /// outputs (a query with several roots reads them all), is within its
    /// limit. `Err` names the first query (in `answering` order) whose
    /// output misses its target.
    ///
    /// Example: p99 / p50 at relative 0.03 over two readouts at 0.02 does
    /// not meet it (1.111 · 0.04 = 0.044), so it is `Err` with the ratio's
    /// query; over two at 0.01 it does.
    pub(super) fn selection_meets_targets(
        &self,
        inputs: &SelectionInputs<'_>,
        edges: &[usize],
        answering: &[(QueryId, usize)],
    ) -> Result<(), QueryId> {
        let selectable = inputs.selectable;
        let count = selectable.executions().len();
        let mut reads: Vec<Vec<(usize, usize)>> = vec![Vec::new(); count];
        for &position in edges {
            let edge = selectable.edges()[position];
            let input = inputs.physical.edge(edge.edge).input_index_of_consumer;
            reads[edge.consumer].push((input, edge.producer));
        }
        let mut composed: Vec<Option<ExecutionError>> = vec![None; count];
        let mut on_walk = vec![false; count];
        let memo = &inputs.physical.logical_candidates.memo;
        // Per query with a δ, in `answering` order: its bound and the
        // failure probability it checks, summed over the outputs so far
        // (`None` once one is unbounded).
        let mut failure_probability_of_query: Vec<(QueryId, FailureProbabilityBound, Option<f64>)> =
            Vec::new();
        for (&(_, group), &(query, v)) in memo.output_groups().iter().zip(answering) {
            // Compose `v` and every execution below it, producers first.
            let mut pending: Vec<(usize, bool)> = vec![(v, false)];
            while let Some((c, producers_done)) = pending.pop() {
                if composed[c].is_some() {
                    continue;
                }
                if !producers_done {
                    if on_walk[c] {
                        // A cycle, which (E) forbids.
                        return Err(query);
                    }
                    on_walk[c] = true;
                    pending.push((c, true));
                    pending.extend(
                        reads[c]
                            .iter()
                            .filter(|&&(_, u)| composed[u].is_none())
                            .map(|&(_, u)| (u, false)),
                    );
                    continue;
                }
                if !self.reached_by_query_with_target[c] {
                    // No query with a target reaches it: (K) has no rows
                    // for it, nor for its producers.
                    composed[c] = Some(ExecutionError::default());
                    continue;
                }
                let Some(inputs_read) = reads[c]
                    .iter()
                    .map(|&(i, u)| Some((i, composed[u]?)))
                    .collect::<Option<Vec<_>>>()
                else {
                    return Err(query);
                };
                let composition = self.compose(c, inputs_read);
                if !composition.inputs_admitted {
                    return Err(query);
                }
                composed[c] = Some(composition.error);
            }
            let Some(limits) = self.root_limits(query, group) else {
                continue;
            };
            let error = composed[v].expect("each answering execution is composed");
            if error
                .error_bound_per_metric
                .iter()
                .zip(limits.tolerated_error_per_metric)
                .any(|(&error, limit)| error > limit)
            {
                return Err(query);
            }
            if let Some(bound) = limits.failure_probability_bound {
                let checked = error.failure_probability_checked_by(&bound);
                match failure_probability_of_query
                    .iter_mut()
                    .find(|(known, _, _)| *known == query)
                {
                    Some((_, _, total)) => *total = sum_of([*total, checked]),
                    None => failure_probability_of_query.push((query, bound, checked)),
                }
            }
        }
        for (query, bound, total) in failure_probability_of_query {
            if total.is_none_or(|total| total > tolerated_limit(bound.limit())) {
                return Err(query);
            }
        }
        Ok(())
    }
}

/// What (K) is built over, per execution (by position in
/// `SelectableExecutions::executions`), before any variable: its
/// selectable input edges and upper bounds on the error and failure
/// probability any selection gives it (the big-M of (K)).
///
/// Only an execution that is its own original (not in a copy of an
/// equivalent context, [`SharedVariables`]) and that a query with an
/// accuracy target reaches has rows: a copy's rows would repeat its
/// original's on the same variables. An execution whose inputs carry no
/// error under any selection (a readout, whose state is exact) has a
/// constant error, its own, and no variable: its consumers read it as a
/// constant times $y$ ([`ErrorTerm`]).
///
/// Example: B = p99 / p50 at relative 0.03, both read from DDSketch
/// readouts required at relative 0.02 or 0.01: each readout's own error is
/// its requirement, the ratio adds none and passes relative error on with
/// factor 1/(1 − 0.1) per input, so its ceiling is 1.111 (0.02 + 0.02).
pub(super) struct ErrorModel<'a> {
    /// The rules (K) applies.
    rules: &'a AccuracyRules,
    /// Whether (K) has rows for the execution.
    has_rows: Vec<bool>,
    /// Per input, the selectable edges into the execution that have a
    /// $y$: (position in `SelectableExecutions::edges`, producer
    /// execution); empty for an execution without rows.
    selectable_edges_per_input: Vec<Vec<Vec<(usize, usize)>>>,
    /// $M$ (and $M^P$, $M^W$): the largest error and failure probabilities
    /// any selection gives the execution; a failure probability is `None`
    /// when some selection leaves it unbounded.
    ceiling: Vec<ExecutionError>,
    /// Whether some query root above the execution checks its failure
    /// probability of one value ($P$) and of all values ($W$), through the
    /// rows it reads them by: a root checking each covered value reads $P$,
    /// one checking every estimate read $W$; $P$ reads $P$ of an input read
    /// value by value and $W$ of one read whole, $W$ reads $W$. (K) has a
    /// variable only for a failure probability some root checks.
    checked_failure_probabilities: Vec<(bool, bool)>,
    /// Whether the execution's error is constant: its inputs carry none
    /// under any selection, so its ceilings are its own error.
    error_is_constant: Vec<bool>,
}

/// An execution's error in one metric, or one of its failure
/// probabilities, as (K)'s rows read it: a constant (a readout's own
/// requirement) or a variable with its ceiling.
#[derive(Clone, Copy)]
enum ErrorTerm {
    Constant(f64),
    Variable { variable: Variable, ceiling: f64 },
}

impl<'a> ErrorModel<'a> {
    /// The model over `inputs` and `rules`, with `y` the edge variables
    /// kept and `shared` the copies of equivalent contexts.
    pub(super) fn new(
        inputs: &SelectionInputs<'_>,
        rules: &'a AccuracyRules,
        y: &BTreeMap<usize, Variable>,
        shared: &SharedVariables,
    ) -> Self {
        let selectable = inputs.selectable;
        let count = selectable.executions().len();
        let has_rows: Vec<bool> = (0..count)
            .map(|index| {
                rules.reached_by_query_with_target[index]
                    && shared.original_of_execution[index] == index
            })
            .collect();
        let selectable_edges_of = |index: usize| -> Vec<Vec<(usize, usize)>> {
            if !has_rows[index] {
                return Vec::new();
            }
            (0..selectable.input_count(index))
                .map(|input| {
                    selectable
                        .edges_into(index, input)
                        .iter()
                        .filter(|position| y.contains_key(position))
                        .map(|&position| (position, selectable.edges()[position].producer))
                        .collect()
                })
                .collect()
        };
        let mut model = Self {
            rules,
            selectable_edges_per_input: (0..count).map(selectable_edges_of).collect(),
            has_rows,
            ceiling: vec![ExecutionError::default(); count],
            checked_failure_probabilities: vec![(false, false); count],
            error_is_constant: vec![false; count],
        };
        model.set_ceilings();
        model.set_checked_failure_probabilities(inputs, shared);
        model.error_is_constant = (0..count)
            .map(|c| {
                model.has_rows[c]
                    && model.selectable_edges_per_input[c]
                        .iter()
                        .flatten()
                        .all(|&(_, u)| model.ceiling[u].is_zero())
            })
            .collect();
        model
    }

    /// Execution `u`'s error in metric `m` (by [`AccuracyMetric::index`]),
    /// `None` when it is 0 under every selection.
    fn error_bound(&self, accuracy: &AccuracyVariables, u: usize, m: usize) -> Option<ErrorTerm> {
        let ceiling = self.ceiling[u].error_bound_per_metric[m];
        match (ceiling > 0.0, self.error_is_constant[u]) {
            (false, _) => None,
            (true, true) => Some(ErrorTerm::Constant(ceiling)),
            (true, false) => Some(ErrorTerm::Variable {
                variable: accuracy.error_bound[&(u, m)],
                ceiling,
            }),
        }
    }

    /// Execution `u`'s failure probability of one value ($P_u$); `Ok(None)`
    /// when it is 0 under every selection, `Err` when some selection
    /// leaves it unbounded.
    fn failure_probability_of_one_value(
        &self,
        accuracy: &AccuracyVariables,
        u: usize,
    ) -> Result<Option<ErrorTerm>, Unbounded> {
        self.failure_probability_term(
            self.ceiling[u].failure_probability_of_one_value,
            u,
            &accuracy.failure_probability_of_one_value,
        )
    }

    /// Execution `u`'s failure probability of all values ($W_u$), as
    /// [`Self::failure_probability_of_one_value`].
    fn failure_probability_of_all_values(
        &self,
        accuracy: &AccuracyVariables,
        u: usize,
    ) -> Result<Option<ErrorTerm>, Unbounded> {
        self.failure_probability_term(
            self.ceiling[u].failure_probability_of_all_values,
            u,
            &accuracy.failure_probability_of_all_values,
        )
    }

    /// The failure probability `variables` holds for `u`, whose ceiling is
    /// `ceiling`, as a term.
    fn failure_probability_term(
        &self,
        ceiling: Option<f64>,
        u: usize,
        variables: &HashMap<usize, Variable>,
    ) -> Result<Option<ErrorTerm>, Unbounded> {
        let ceiling = ceiling.ok_or(Unbounded)?;
        Ok(match (ceiling > 0.0, self.error_is_constant[u]) {
            (false, _) => None,
            (true, true) => Some(ErrorTerm::Constant(ceiling)),
            (true, false) => Some(ErrorTerm::Variable {
                variable: variables[&u],
                ceiling,
            }),
        })
    }

    /// The failure probability `bound` checks of execution `v` (of all
    /// values for every estimate read, of one value for each covered
    /// value).
    fn failure_probability_checked_by(
        &self,
        accuracy: &AccuracyVariables,
        v: usize,
        bound: &FailureProbabilityBound,
    ) -> Result<Option<ErrorTerm>, Unbounded> {
        match bound {
            FailureProbabilityBound::EveryEstimateRead { .. } => {
                self.failure_probability_of_all_values(accuracy, v)
            }
            FailureProbabilityBound::EachCoveredValue { .. } => {
                self.failure_probability_of_one_value(accuracy, v)
            }
        }
    }

    /// [`Self::checked_failure_probabilities`], in one pass from each
    /// original query's answering executions down, consumers first.
    fn set_checked_failure_probabilities(
        &mut self,
        inputs: &SelectionInputs<'_>,
        shared: &SharedVariables,
    ) {
        let rules = self.rules;
        let memo = &inputs.physical.logical_candidates.memo;
        for (&(query, group), (query_id, executions)) in memo
            .output_groups()
            .iter()
            .zip(inputs.selectable.answering())
        {
            if shared.original_of_query[query] != *query_id {
                continue;
            }
            let Some(bound) = rules
                .root_limits(*query_id, group)
                .and_then(|limits| limits.failure_probability_bound)
            else {
                continue;
            };
            for &v in executions {
                let checked = &mut self.checked_failure_probabilities[v];
                match bound {
                    FailureProbabilityBound::EveryEstimateRead { .. } => checked.1 = true,
                    FailureProbabilityBound::EachCoveredValue { .. } => checked.0 = true,
                }
            }
        }
        for &c in rules.executions_producers_first.iter().rev() {
            let (one_value, all_values) = self.checked_failure_probabilities[c];
            if !self.has_rows[c] || !(one_value || all_values) {
                continue;
            }
            let how = &rules.how_inputs_are_read[c];
            for (i, edges) in self.selectable_edges_per_input[c].iter().enumerate() {
                if !how.estimates_of_input_reach_output(i) {
                    continue;
                }
                let value_by_value = how.reads_value_by_value(i);
                for &(_, u) in edges {
                    let checked = &mut self.checked_failure_probabilities[u];
                    checked.0 |= one_value && value_by_value;
                    checked.1 |= all_values || (one_value && !value_by_value);
                }
            }
        }
    }

    /// The ceilings, in one pass over the executions with rows, producers
    /// first ([`AccuracyRules::executions_producers_first`]): each
    /// execution composed ([`AccuracyRules::compose`]) from the largest
    /// ceiling of the producers selectable on each input. Every selectable
    /// edge reads a group the memo orders before the consumer's (the memo
    /// is acyclic, so no group reads itself), so each producer's ceiling is
    /// final when its consumer reads it.
    fn set_ceilings(&mut self) {
        let rules = self.rules;
        for &c in &rules.executions_producers_first {
            if !self.has_rows[c] {
                continue;
            }
            let largest_per_input: Vec<(usize, ExecutionError)> = self.selectable_edges_per_input
                [c]
                .iter()
                .enumerate()
                .map(|(i, edges)| {
                    let largest =
                        edges
                            .iter()
                            .fold(ExecutionError::default(), |largest, &(_, u)| {
                                debug_assert_ne!(
                                    rules.candidate_of_execution[u].group,
                                    rules.candidate_of_execution[c].group,
                                    "a selectable edge reads another group"
                                );
                                largest.largest(self.ceiling[u])
                            });
                    (i, largest)
                })
                .collect();
            // A ceiling bounds every selection, admitted or not.
            self.ceiling[c] = rules.compose(c, largest_per_input).error;
        }
    }
}

/// A failure probability no declared fact bounds under some selection
/// (a readout whose values per evaluation are unknown, below): (K) has no
/// variable for it, and no output may be answered by a bound reading it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Unbounded;

/// The accuracy variables of constraint (K), all continuous, in
/// each only for an execution with rows
/// ([`ErrorModel`]) whose error is not constant and whose ceiling is above
/// 0 (and, for a failure probability, bounded and checked by some query
/// root above: [`ErrorModel::checked_failure_probabilities`]), by
/// execution position.
pub(super) struct AccuracyVariables {
    /// $E_{\kappa,c,m}$: the execution's error bound in metric $m$ (by
    /// [`AccuracyMetric::index`]).
    error_bound: HashMap<(usize, usize), Variable>,
    /// $P_{\kappa,c}$: its failure probability of one value.
    failure_probability_of_one_value: HashMap<usize, Variable>,
    /// $W_{\kappa,c}$: its failure probability of all values.
    failure_probability_of_all_values: HashMap<usize, Variable>,
}

impl AccuracyVariables {
    /// $E$, $P$ and $W$ over `model`, added to `problem`.
    pub(super) fn new(model: &ErrorModel<'_>, problem: &mut ProblemVariables) -> Self {
        let mut variables = Self {
            error_bound: HashMap::new(),
            failure_probability_of_one_value: HashMap::new(),
            failure_probability_of_all_values: HashMap::new(),
        };
        for c in
            (0..model.has_rows.len()).filter(|&c| model.has_rows[c] && !model.error_is_constant[c])
        {
            for (m, &ceiling) in model.ceiling[c].error_bound_per_metric.iter().enumerate() {
                if ceiling > 0.0 {
                    variables
                        .error_bound
                        .insert((c, m), problem.add(variable().min(0)));
                }
            }
            let ceiling = model.ceiling[c];
            let (one_value_checked, all_values_checked) = model.checked_failure_probabilities[c];
            for (failure_probability, checked, of) in [
                (
                    ceiling.failure_probability_of_one_value,
                    one_value_checked,
                    &mut variables.failure_probability_of_one_value,
                ),
                (
                    ceiling.failure_probability_of_all_values,
                    all_values_checked,
                    &mut variables.failure_probability_of_all_values,
                ),
            ] {
                if checked && failure_probability.is_some_and(|ceiling| ceiling > 0.0) {
                    of.insert(c, problem.add(variable().min(0)));
                }
            }
        }
        variables
    }

    /// The number of $E$, $P$ and $W$ variables.
    #[cfg(test)]
    pub(super) fn variable_count(&self) -> usize {
        self.error_bound.len()
            + self.failure_probability_of_one_value.len()
            + self.failure_probability_of_all_values.len()
    }
}

/// (K) (`accuracy-models.md`, "Checking each query's target in
/// selection"), per execution $c$ with rows ([`ErrorModel`]):
/// - $E_{c,m} \ge \varepsilon_c[m = m_c] + \sum_i \beta_{c,i,m}
///   F_{c,i,m}$ (one row per input instead of the sum when each output
///   value is copied from one input, `or` and a concatenation) and $F_{c,i,m} \ge E_{u,m} - M_{u,m}(1 - y_{u,c,i})$ per
///   selectable edge, $F_{c,i,m} \le b_{c,i}$ where the contract's linear
///   bound holds only up to $b_{c,i}$ (a ratio's denominator), and
///   $E_{u,m} \le M_{u,m}(1 - y_{u,c,i})$ when $c$ has no contract in $m$
///   (the metric rule on the selected path); an input whose error does not
///   reach the output ($\beta = 0$, a count's) passes none;
/// - failure probabilities, over the inputs whose error reaches the output
///   in some metric, with $HP_{c,i} \ge P_u - M^P_u(1 - y_{u,c,i})$ and
///   $HW_{c,i} \ge W_u - M^W_u(1 - y_{u,c,i})$ (a union bound over
///   estimates, [`AccuracyRules::compose`]):
///   $P_c \ge P^{own}_c + \sum_{i\ \text{value by value}} HP_{c,i} +
///   \sum_{i\ \text{whole}} HW_{c,i}$, one row per input read value by
///   value instead of their sum when each value is copied from one input
///   (`or`), and $W_c \ge W^{own}_c + \sum_i HW_{c,i}$;
/// - per query $q$ with a target, output group $g$ its root reads and
///   execution $v$ answering it, $E_{v,m} \le \varepsilon_q[m \in
///   \text{allowed}] + M(1 - z_{q,v})$, the metrics $g$ allows at the root
///   ([`query_roots_with_target`]: the target's at an operator root, none
///   for the operands of a scalar expression no contract covers);
/// - per query $q$ with a δ, one failure row by the bound
///   `AccuracyTarget::failure_probability_bound` names: $W_v \le \delta_q$
///   (every estimate read) or $P_v \le \delta_q / V$ (each covered value)
///   $+ M(1 - z_{q,v})$; a query with several roots sums the term its
///   answering executions give per root, $T_{q,g} \ge X_v - M_v(1 -
///   z_{q,v})$, in one row $\sum_g T_{q,g} \le$ the limit. An output whose
///   term is unbounded under some selection is not answered through that
///   execution. No row without a bound ([`RootLimits`], each limit widened
///   by [`tolerated_limit`]).
///
/// It is [`AccuracyRules::compose`]'s rule over variables: the same own
/// error, factors, metric rule and validity caps.
///
/// $F$, $HP$, $HW$ and $T$ are added to `problem` here. A copy of an equivalent context
/// and its queries have no rows: they share their original's variables,
/// and the copy's rows would be its original's
/// ([`super::equivalent_contexts::copies_in_equivalent_contexts`] pairs
/// queries only with equal [`RootLimits`] per output).
pub(super) fn accuracy_constraints(
    inputs: &SelectionInputs<'_>,
    model: &ErrorModel<'_>,
    accuracy: &AccuracyVariables,
    y: &BTreeMap<usize, Variable>,
    z: &BTreeMap<(QueryId, usize), Variable>,
    shared: &SharedVariables,
    problem: &mut ProblemVariables,
) -> Vec<Constraint> {
    let rules = model.rules;
    let mut constraints = Vec::new();
    for c in (0..model.has_rows.len()).filter(|&c| model.has_rows[c] && !model.error_is_constant[c])
    {
        // β_{c,i,m} F_{c,i,m} per metric and input: summed, or each alone
        // when every output value is copied from one input.
        let mut error_of_inputs: [Vec<Expression>; AccuracyMetric::COUNT] = Default::default();
        // HP_{c,i} of each input read value by value, Σ HW_{c,i} of the
        // inputs read whole, and Σ_i HW_{c,i}.
        let has_one_value = accuracy.failure_probability_of_one_value.contains_key(&c);
        let has_all_values = accuracy.failure_probability_of_all_values.contains_key(&c);
        let how = &rules.how_inputs_are_read[c];
        let mut one_value_of_inputs_read_value_by_value: Vec<Expression> = Vec::new();
        let mut all_values_of_inputs_read_whole = Expression::default();
        let mut all_values_of_inputs = Expression::default();
        for (i, edges) in model.selectable_edges_per_input[c].iter().enumerate() {
            for (m, error_of_input) in error_of_inputs.iter_mut().enumerate() {
                let read: Vec<(Variable, ErrorTerm)> = edges
                    .iter()
                    .filter_map(|&(position, u)| {
                        Some((y[&position], model.error_bound(accuracy, u, m)?))
                    })
                    .collect();
                if read.is_empty() {
                    continue;
                }
                let Some(contract) = rules.contract(c, m) else {
                    // The metric rule: no error in m reaches c.
                    for (y, error) in read {
                        constraints.push(match error {
                            ErrorTerm::Constant(_) => constraint!(y <= 0),
                            ErrorTerm::Variable { variable, ceiling } => {
                                constraint!(variable <= ceiling * (1 - y))
                            }
                        });
                    }
                    continue;
                };
                if !contract.input_error_reaches_output(i) {
                    continue;
                }
                let of_input = chosen_producers(&read, &mut constraints, problem);
                if let Some(holds_up_to) = contract.linear_bound_holds_up_to_per_input[i] {
                    let holds_up_to = tolerated_limit(holds_up_to);
                    constraints.push(constraint!(of_input.clone() <= holds_up_to));
                }
                error_of_input.push(contract.error_factor_per_input[i] * of_input);
            }
            // Only an input whose estimates reach the output can make it
            // miss: a count's input and the right of `and` add nothing,
            // unless a top-k chooses their values.
            if !how.estimates_of_input_reach_output(i) {
                continue;
            }
            let value_by_value = how.reads_value_by_value(i);
            // A variable's ceiling is bounded only when every producer's
            // is, so each term read here is bounded.
            let read_of = |of_all_values: bool| {
                edges
                    .iter()
                    .filter_map(|&(position, u)| {
                        let term = if of_all_values {
                            model.failure_probability_of_all_values(accuracy, u)
                        } else {
                            model.failure_probability_of_one_value(accuracy, u)
                        };
                        let term = term.expect("a bounded ceiling has bounded producers")?;
                        Some((y[&position], term))
                    })
                    .collect::<Vec<(Variable, ErrorTerm)>>()
            };
            if has_one_value && value_by_value {
                let read = read_of(false);
                if !read.is_empty() {
                    one_value_of_inputs_read_value_by_value.push(chosen_producers(
                        &read,
                        &mut constraints,
                        problem,
                    ));
                }
            }
            if has_all_values || (has_one_value && !value_by_value) {
                let read = read_of(true);
                if !read.is_empty() {
                    let of_input = chosen_producers(&read, &mut constraints, problem);
                    if !value_by_value {
                        all_values_of_inputs_read_whole += of_input.clone();
                    }
                    all_values_of_inputs += of_input;
                }
            }
        }
        let own = rules.own_error_per_execution[c];
        for (m, of_inputs) in error_of_inputs.into_iter().enumerate() {
            if let Some(&error_bound) = accuracy.error_bound.get(&(c, m)) {
                let own = own.error_bound_per_metric[m];
                if how.each_value_from_one_input && !of_inputs.is_empty() {
                    for of_input in of_inputs {
                        constraints.push(constraint!(error_bound >= own + of_input));
                    }
                } else {
                    let of_inputs: Expression = of_inputs.into_iter().sum();
                    constraints.push(constraint!(error_bound >= own + of_inputs));
                }
            }
        }
        if let Some(&of_one_value) = accuracy.failure_probability_of_one_value.get(&c) {
            let own = own
                .failure_probability_of_one_value
                .expect("a bounded ceiling has a bounded own term");
            let whole = all_values_of_inputs_read_whole;
            if how.each_value_depends_on_one_input
                && !one_value_of_inputs_read_value_by_value.is_empty()
            {
                for of_input in one_value_of_inputs_read_value_by_value {
                    constraints.push(constraint!(of_one_value >= own + of_input + whole.clone()));
                }
            } else {
                let value_by_value: Expression =
                    one_value_of_inputs_read_value_by_value.into_iter().sum();
                constraints.push(constraint!(of_one_value >= own + value_by_value + whole));
            }
        }
        if let Some(&of_all_values) = accuracy.failure_probability_of_all_values.get(&c) {
            let own = own
                .failure_probability_of_all_values
                .expect("a bounded ceiling has a bounded own term");
            constraints.push(constraint!(of_all_values >= own + all_values_of_inputs));
        }
    }
    let memo = &inputs.physical.logical_candidates.memo;
    let output_groups = memo.output_groups();
    let mut outputs_per_query = vec![0usize; memo.queries().len()];
    for &(query, _) in &output_groups {
        outputs_per_query[query] += 1;
    }
    // Per query with a bound and several outputs: the bound and, per
    // output, what its answering execution gives.
    let mut several_roots: BTreeMap<QueryId, (FailureProbabilityBound, Expression)> =
        BTreeMap::new();
    for (&(query, group), (query_id, executions)) in
        output_groups.iter().zip(inputs.selectable.answering())
    {
        debug_assert_eq!(query, query_id.0, "answering is in output_groups order");
        if shared.original_of_query[query] != *query_id {
            continue;
        }
        let Some(limits) = rules.root_limits(*query_id, group) else {
            continue;
        };
        let outputs_of_query = outputs_per_query[query];
        let mut checked_per_execution: Vec<(Variable, ErrorTerm)> = Vec::new();
        for &v in executions {
            let z = z[&(*query_id, v)];
            for (m, &limit) in limits.tolerated_error_per_metric.iter().enumerate() {
                constraints.extend(
                    model
                        .error_bound(accuracy, v, m)
                        .and_then(|error| at_most_when_answering(error, limit, z)),
                );
            }
            let Some(bound) = limits.failure_probability_bound else {
                continue;
            };
            match model.failure_probability_checked_by(accuracy, v, &bound) {
                Err(Unbounded) => constraints.push(constraint!(z <= 0)),
                Ok(None) => {}
                Ok(Some(term)) if outputs_of_query == 1 => {
                    constraints.extend(at_most_when_answering(
                        term,
                        tolerated_limit(bound.limit()),
                        z,
                    ));
                }
                Ok(Some(term)) => checked_per_execution.push((z, term)),
            }
        }
        if let (Some(bound), true) = (limits.failure_probability_bound, outputs_of_query > 1) {
            let of_output = if checked_per_execution.is_empty() {
                Expression::default()
            } else {
                chosen_producers(&checked_per_execution, &mut constraints, problem)
            };
            let (_, total) = several_roots
                .entry(*query_id)
                .or_insert_with(|| (bound, Expression::default()));
            *total += of_output;
        }
    }
    for (bound, total) in several_roots.into_values() {
        constraints.push(constraint!(total <= tolerated_limit(bound.limit())));
    }
    constraints
}

/// The error (or failure probability) one input of a consumer reads from
/// the producer selected on it, `read` listing each producer's with its
/// $y$: $\sum_u e_u y_u$ over the producers with a constant error (at most
/// one $y$ of an input is 1, constraint (A)), and over the others a
/// variable $F \ge E_u - M_u(1 - y_u)$, also at least that sum.
fn chosen_producers(
    read: &[(Variable, ErrorTerm)],
    constraints: &mut Vec<Constraint>,
    problem: &mut ProblemVariables,
) -> Expression {
    let constant: Expression = read
        .iter()
        .filter_map(|&(y, term)| match term {
            ErrorTerm::Constant(value) => Some(value * y),
            ErrorTerm::Variable { .. } => None,
        })
        .sum();
    if read
        .iter()
        .all(|(_, term)| matches!(term, ErrorTerm::Constant(_)))
    {
        return constant;
    }
    let of_input = problem.add(variable().min(0));
    constraints.push(constraint!(of_input >= constant));
    for &(y, term) in read {
        if let ErrorTerm::Variable { variable, ceiling } = term {
            constraints.push(constraint!(of_input >= variable - ceiling * (1 - y)));
        }
    }
    of_input.into()
}

/// The root row bounding `term` by `limit` when $z = 1$: none when the
/// ceiling is within it; $z = 0$ for a constant above it.
fn at_most_when_answering(term: ErrorTerm, limit: f64, z: Variable) -> Option<Constraint> {
    match term {
        ErrorTerm::Constant(value) => (value > limit).then(|| constraint!(z <= 0)),
        ErrorTerm::Variable { variable, ceiling } => {
            (limit < ceiling).then(|| constraint!(variable <= limit + (ceiling - limit) * (1 - z)))
        }
    }
}

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

use asap_accuracy::{query_roots_with_target, tolerated_limit, ErrorContract, ErrorMetricsOfMemo};
use asap_types::accuracy::AccuracyMetric;
use asap_types::stages::{CandidateForMemoGroupId, MemoGroupId};

use super::equivalent_contexts::SharedVariables;
use super::model::SelectionInputs;
use crate::selectable::QueryId;

/// An execution's error as (K) composes it: its error bound per metric
/// (by [`AccuracyMetric::index`]) and its failure probability.
///
/// Example: a DDSketch readout required at relative 0.01 with δ = 1e-3
/// is 0.01 in `Relative`, 0 in every other metric, and 1e-3.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub(super) struct ExecutionError {
    /// $E_{c,m}$ per metric.
    error_bound_per_metric: [f64; AccuracyMetric::COUNT],
    /// $D_c$.
    failure_probability: f64,
}

impl ExecutionError {
    /// The larger of `self` and `other` in each metric and in failure
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
        largest.failure_probability = largest.failure_probability.max(other.failure_probability);
        largest
    }

    /// Whether it is 0 in every metric and in failure probability.
    fn is_zero(&self) -> bool {
        self.failure_probability == 0.0 && self.error_bound_per_metric.iter().all(|&e| e == 0.0)
    }
}

/// What composing one execution's error from what its inputs read gives
/// ([`AccuracyRules::compose`]).
struct Composition {
    /// Its own error plus each input's error times the contract's factor,
    /// and its own failure probability plus each input's.
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
    /// (`CandidateForMemoGroup::required_accuracy`), 0 for any other.
    own_error_per_execution: Vec<ExecutionError>,
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

/// What a root check of (K) bounds one query output by, each limit
/// already widened by [`tolerated_limit`] as the edge check widens it.
/// Two outputs with equal limits have the same root rows, so they compare
/// and hash by the limits' bits.
///
/// Example: `p99 / p50` at relative 0.03 with δ = 0.01 over a whole result
/// of 100 values allows 0.03 (widened) in `Relative`, 0 in every other
/// metric, and 1e-4 (widened) failure probability per output value.
#[derive(Debug, Clone, Copy)]
pub(super) struct RootLimits {
    /// Per metric (by [`AccuracyMetric::index`]), the error the output may
    /// have: the target's ε in a metric the root allows
    /// ([`query_roots_with_target`]), 0 in any other.
    tolerated_error_per_metric: [f64; AccuracyMetric::COUNT],
    /// The failure probability each output value may have
    /// (`AccuracyTarget::failure_probability_per_output_value`); `None`
    /// without a δ, or with one over the whole result and $n_q$ unknown
    /// (Pass 2 then gives that query's readouts no level).
    tolerated_failure_probability_per_output_value: Option<f64>,
}

impl RootLimits {
    /// The limits' bits, which equality, hashing and order compare.
    fn bits(&self) -> ([u64; AccuracyMetric::COUNT], Option<u64>) {
        (
            self.tolerated_error_per_metric.map(f64::to_bits),
            self.tolerated_failure_probability_per_output_value
                .map(f64::to_bits),
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
                if let Some(required) = memo.candidate(id).required_accuracy() {
                    own.error_bound_per_metric[required.metric.index()] = required.max_error;
                    own.failure_probability = required.failure_probability;
                }
                own
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
                let failure_probability = target.failure_probability_per_output_value(
                    memo.queries()[query].number_of_values_per_query_evaluation,
                );
                let mut tolerated_error_per_metric = [0.0; AccuracyMetric::COUNT];
                for metric in allowed_metrics {
                    tolerated_error_per_metric[metric.index()] = tolerated_limit(target.max_error);
                }
                let limits = RootLimits {
                    tolerated_error_per_metric,
                    tolerated_failure_probability_per_output_value: failure_probability
                        .map(tolerated_limit),
                };
                ((QueryId(query), root), limits)
            })
            .collect();
        Some(Self {
            error_metrics: ErrorMetricsOfMemo::of(memo, &order),
            candidate_of_execution,
            own_error_per_execution,
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
    /// metric where the input's error reaches the output ($\beta > 0$),
    /// and its own failure probability plus every input's (a union bound).
    /// An input's error in a metric `c` has no contract for breaks the
    /// metric rule, and one above the contract's validity cap $b_{c,i}$
    /// breaks the cap: either leaves the inputs not admitted, and adds
    /// nothing in that metric for the metric rule.
    ///
    /// Example: `p99 / p50`'s contract passes each operand's relative
    /// error with factor 1.111 and caps the denominator at 0.1: reading
    /// 0.02 and 0.02 composes 0.044, admitted; reading a denominator at
    /// 0.2 is not admitted.
    fn compose(
        &self,
        c: usize,
        inputs_read: impl IntoIterator<Item = (usize, ExecutionError)>,
    ) -> Composition {
        let mut composed = Composition {
            error: self.own_error_per_execution[c],
            inputs_admitted: true,
        };
        for (i, read) in inputs_read {
            composed.error.failure_probability += read.failure_probability;
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
                composed.error.error_bound_per_metric[m] +=
                    contract.error_factor_per_input[i] * error;
            }
        }
        composed
    }

    /// Whether the selection `edges` (by position in
    /// `SelectableExecutions::edges`) with the outputs `answering` (query,
    /// execution answering it, in `SelectableExecutions::answering` order)
    /// meets (K), exactly in f64: each execution a query with a target
    /// reaches, composed ([`Self::compose`]) from the producers selected
    /// on its inputs, admits its inputs' errors and, at each output of a
    /// query with a target, is within the root's limits. `Err` names the
    /// first query (in `answering` order) whose output misses its target.
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
                || limits
                    .tolerated_failure_probability_per_output_value
                    .is_some_and(|limit| error.failure_probability > limit)
            {
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
    /// $M$: the largest error and failure probability any selection gives
    /// the execution.
    ceiling: Vec<ExecutionError>,
    /// Whether the execution's error is constant: its inputs carry none
    /// under any selection, so its ceilings are its own error.
    error_is_constant: Vec<bool>,
}

/// An execution's error in one metric, or its failure probability, as
/// (K)'s rows read it: a constant (a readout's
/// own requirement) or a variable with its ceiling.
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
            error_is_constant: vec![false; count],
        };
        model.set_ceilings();
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

    /// Execution `u`'s failure probability; `None` when it is 0
    /// under every selection.
    fn failure_probability(&self, accuracy: &AccuracyVariables, u: usize) -> Option<ErrorTerm> {
        let ceiling = self.ceiling[u].failure_probability;
        match (ceiling > 0.0, self.error_is_constant[u]) {
            (false, _) => None,
            (true, true) => Some(ErrorTerm::Constant(ceiling)),
            (true, false) => Some(ErrorTerm::Variable {
                variable: accuracy.failure_probability[&u],
                ceiling,
            }),
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

/// The accuracy variables of constraint (K), all continuous, in
/// each only for an execution with rows
/// ([`ErrorModel`]) whose error is not constant and whose ceiling is above
/// 0, by execution position.
pub(super) struct AccuracyVariables {
    /// $E_{\kappa,c,m}$: the execution's error bound in metric $m$ (by
    /// [`AccuracyMetric::index`]).
    error_bound: HashMap<(usize, usize), Variable>,
    /// $D_{\kappa,c}$: its failure probability.
    failure_probability: HashMap<usize, Variable>,
}

impl AccuracyVariables {
    /// $E$ and $D$ over `model`, added to `problem`.
    pub(super) fn new(model: &ErrorModel<'_>, problem: &mut ProblemVariables) -> Self {
        let mut variables = Self {
            error_bound: HashMap::new(),
            failure_probability: HashMap::new(),
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
            if model.ceiling[c].failure_probability > 0.0 {
                variables
                    .failure_probability
                    .insert(c, problem.add(variable().min(0)));
            }
        }
        variables
    }

    /// The number of $E$ and $D$ variables.
    #[cfg(test)]
    pub(super) fn variable_count(&self) -> usize {
        self.error_bound.len() + self.failure_probability.len()
    }
}

/// (K) (`accuracy-models.md`, "Checking each query's target in
/// selection"), per execution $c$ with rows ([`ErrorModel`]):
/// - $E_{c,m} \ge \varepsilon_c[m = m_c] + \sum_i \beta_{c,i,m}
///   F_{c,i,m}$ and $F_{c,i,m} \ge E_{u,m} - M_{u,m}(1 - y_{u,c,i})$ per
///   selectable edge, $F_{c,i,m} \le b_{c,i}$ where the contract's linear
///   bound holds only up to $b_{c,i}$ (a ratio's denominator), and
///   $E_{u,m} \le M_{u,m}(1 - y_{u,c,i})$ when $c$ has no contract in $m$
///   (the metric rule on the selected path); an input whose error does not
///   reach the output ($\beta = 0$, a count's) passes none;
/// - $D_c \ge \delta_c + \sum_i H_{c,i}$, $H_{c,i} \ge D_u - M_u(1 -
///   y_{u,c,i})$ (a union bound);
/// - per query $q$ with a target, output group $g$ its root reads and
///   execution $v$ answering it, $E_{v,m} \le \varepsilon_q[m \in
///   \text{allowed}] + M(1 - z_{q,v})$, the metrics $g$ allows at the root
///   ([`query_roots_with_target`]: the target's at an operator root, none
///   for the operands of a scalar expression no contract covers), and
///   $D_v \le \delta_q + M(1 - z_{q,v})$ with $\delta_q$ per output value
///   (`AccuracyTarget::failure_probability_per_output_value`); no $D$ row
///   without one ([`RootLimits`], each limit widened by
///   [`tolerated_limit`]).
///
/// It is [`AccuracyRules::compose`]'s rule over variables: the same own
/// error, factors, metric rule and validity caps.
///
/// $F$ and $H$ are added to `problem` here. A copy of an equivalent context
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
        // Σ_i β_{c,i,m} F_{c,i,m} per metric, and Σ_i H_{c,i}.
        let mut error_of_inputs: [Expression; AccuracyMetric::COUNT] = Default::default();
        let mut failure_probability_of_inputs = Expression::default();
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
                *error_of_input += contract.error_factor_per_input[i] * of_input;
            }
            // H_{c,i} counts the failure of the one producer selected on
            // input i, not of each readout value an output value depends
            // on: `max(q)` over 1,000 series reads 1,000 readout values,
            // and D here adds the readout's δ once. That is sound only
            // because Pass 2's levels already split the query's δ over
            // the readout values its output depends on
            // (`readout_caps::query_levels`), so each readout's δ is the
            // query's over 1,000; the root row checks the composed sum
            // against the per-output-value δ.
            let read: Vec<(Variable, ErrorTerm)> = edges
                .iter()
                .filter_map(|&(position, u)| {
                    Some((y[&position], model.failure_probability(accuracy, u)?))
                })
                .collect();
            if !read.is_empty() {
                failure_probability_of_inputs += chosen_producers(&read, &mut constraints, problem);
            }
        }
        let own = rules.own_error_per_execution[c];
        for (m, of_inputs) in error_of_inputs.into_iter().enumerate() {
            if let Some(&error_bound) = accuracy.error_bound.get(&(c, m)) {
                let own = own.error_bound_per_metric[m];
                constraints.push(constraint!(error_bound >= own + of_inputs));
            }
        }
        if let Some(&failure_probability) = accuracy.failure_probability.get(&c) {
            let own = own.failure_probability;
            constraints.push(constraint!(
                failure_probability >= own + failure_probability_of_inputs
            ));
        }
    }
    let memo = &inputs.physical.logical_candidates.memo;
    for (&(query, group), (query_id, executions)) in memo
        .output_groups()
        .iter()
        .zip(inputs.selectable.answering())
    {
        debug_assert_eq!(query, query_id.0, "answering is in output_groups order");
        if shared.original_of_query[query] != *query_id {
            continue;
        }
        let Some(limits) = rules.root_limits(*query_id, group) else {
            continue;
        };
        for &v in executions {
            let z = z[&(*query_id, v)];
            for (m, &limit) in limits.tolerated_error_per_metric.iter().enumerate() {
                constraints.extend(
                    model
                        .error_bound(accuracy, v, m)
                        .and_then(|error| at_most_when_answering(error, limit, z)),
                );
            }
            if let (Some(limit), Some(failure_probability)) = (
                limits.tolerated_failure_probability_per_output_value,
                model.failure_probability(accuracy, v),
            ) {
                constraints.extend(at_most_when_answering(failure_probability, limit, z));
            }
        }
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

//! The accuracy extension of the selection MILP
//! (`docs/design_docs/concepts/memo-groups-properties-requirements.md`,
//! section 4, "Query composition and root checks"): constraint (K), per
//! execution and per error metric, over the error each execution adds and
//! passes on ([`AccuracyRules`], [`ErrorModel`]), and the same rule
//! checked on one selection ([`AccuracyRules::selection_meets_targets`]),
//! which the greedy upper bound needs to be a feasible plan of (K).

use std::collections::{BTreeMap, HashMap};

use good_lp::{constraint, variable, Constraint, Expression, ProblemVariables, Variable};

use asap_accuracy::{query_roots_with_target, tolerated_limit, ErrorContract, ErrorMetricsOfMemo};
use asap_types::accuracy::AccuracyMetric;
use asap_types::stages::{CandidateForMemoGroupId, MemoGroupId, RequiredAccuracy};

use super::equivalent_contexts::SharedVariables;
use super::model::SelectionInputs;
use crate::selectable::QueryId;

/// The number of error metrics, `AccuracyMetric::ALL`'s length.
const METRIC_COUNT: usize = AccuracyMetric::ALL.len();

/// What (K) reads of the memo, built once per selection when some query
/// has an accuracy target: each candidate's error contracts, which
/// executions a query with a target reaches, and what each query root
/// allows.
pub(super) struct AccuracyRules {
    /// The candidates' error contracts per metric
    /// (`ErrorMetricsOfMemo::error_contract`).
    error_metrics: ErrorMetricsOfMemo,
    /// Per execution, by position in `SelectableExecutions::executions`,
    /// its logical candidate.
    candidate_of_execution: Vec<CandidateForMemoGroupId>,
    /// Per execution, whether a query with an accuracy target reaches its
    /// group.
    targeted: Vec<bool>,
    /// Per (query, output group its root reads), what its root check
    /// bounds the output by.
    roots: HashMap<(usize, MemoGroupId), RootLimits>,
}

/// What a root check of (K) bounds one query output by.
struct RootLimits {
    /// The target's ε.
    max_error: f64,
    /// The metrics allowed at the root ([`query_roots_with_target`]).
    allowed_metrics: Vec<AccuracyMetric>,
    /// $\delta_q$ per value (`AccuracyTarget::failure_probability_per_value`);
    /// `None` without a δ, or with one over the whole result and $n_q$
    /// unknown (Pass 2 gives that query's readouts no level).
    failure_probability_per_value: Option<f64>,
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
        let targeted = candidate_of_execution
            .iter()
            .map(|id| {
                memo.groups()[id.group.0 as usize]
                    .reached_by_queries()
                    .iter()
                    .any(|&query| memo.queries()[query].accuracy_target.is_some())
            })
            .collect();
        let roots = query_roots_with_target(memo)
            .into_iter()
            .map(|(query, root, target, allowed_metrics)| {
                let failure_probability_per_value =
                    target
                        .failure_probability
                        .and(target.failure_probability_per_value(
                            memo.queries()[query].number_of_values_per_query_evaluation,
                        ));
                let limits = RootLimits {
                    max_error: target.max_error,
                    allowed_metrics,
                    failure_probability_per_value,
                };
                ((query, root), limits)
            })
            .collect();
        Some(Self {
            error_metrics: ErrorMetricsOfMemo::of(memo, &order),
            candidate_of_execution,
            targeted,
            roots,
        })
    }

    /// Execution `c`'s error contract in metric `m` (by position in
    /// `AccuracyMetric::ALL`); `None` where it has none (the metric rule).
    fn contract(&self, c: usize, m: usize) -> Option<&ErrorContract> {
        self.error_metrics
            .error_contract(self.candidate_of_execution[c], AccuracyMetric::ALL[m])
    }

    /// Whether the selection `edges` (by position in
    /// `SelectableExecutions::edges`) with the outputs `answering` (query,
    /// execution answering it, in `SelectableExecutions::answering` order)
    /// meets (K): the composed error of each execution a query with a
    /// target reaches, by the rule of [`accuracy_constraints`] on the
    /// selected edges, passes its consumers' metric rule and validity caps
    /// and, at each output of a query with a target, the root's limits.
    ///
    /// Example: p99 / p50 at relative 0.03 over two readouts at 0.02 does
    /// not meet it (1.111 · 0.04 = 0.044); over two at 0.01 it does.
    pub(super) fn selection_meets_targets(
        &self,
        inputs: &SelectionInputs<'_>,
        edges: &[usize],
        answering: &[(QueryId, usize)],
    ) -> bool {
        let selectable = inputs.selectable;
        let count = selectable.executions().len();
        let mut reads: Vec<Vec<(usize, usize)>> = vec![Vec::new(); count];
        for &position in edges {
            let edge = selectable.edges()[position];
            let input = inputs.physical.edge(edge.edge).input_index_of_consumer;
            reads[edge.consumer].push((input, edge.producer));
        }
        // Composed (E per metric, D) per execution, producers first.
        let mut composed: Vec<Option<([f64; METRIC_COUNT], f64)>> = vec![None; count];
        let mut on_walk = vec![false; count];
        let mut pending: Vec<(usize, bool)> = answering.iter().map(|&(_, v)| (v, false)).collect();
        while let Some((c, producers_done)) = pending.pop() {
            if composed[c].is_some() {
                continue;
            }
            if !producers_done {
                if on_walk[c] {
                    // A cycle, which (E) forbids.
                    return false;
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
            if !self.targeted[c] {
                // No query with a target reaches it: (K) has no rows for
                // it, nor for its producers.
                composed[c] = Some(([0.0; METRIC_COUNT], 0.0));
                continue;
            }
            let (mut error_bound, mut failure_probability) = own_error_of(own_error(inputs, c));
            for &(i, u) in &reads[c] {
                let Some((producer_error, producer_failure)) = composed[u] else {
                    return false;
                };
                failure_probability += producer_failure;
                for (m, &error) in producer_error.iter().enumerate() {
                    if error <= 0.0 {
                        continue;
                    }
                    let Some(contract) = self.contract(c, m) else {
                        return false;
                    };
                    if !contract.input_error_reaches_output(i) {
                        continue;
                    }
                    if contract.linear_bound_holds_up_to_per_input[i]
                        .is_some_and(|holds_up_to| error > tolerated_limit(holds_up_to))
                    {
                        return false;
                    }
                    error_bound[m] += contract.error_factor_per_input[i] * error;
                }
            }
            composed[c] = Some((error_bound, failure_probability));
        }
        let memo = &inputs.physical.logical_candidates.memo;
        for (&(query, group), &(_, v)) in memo.output_groups().iter().zip(answering) {
            let Some(limits) = self.roots.get(&(query, group)) else {
                continue;
            };
            let (error_bound, failure_probability) =
                composed[v].expect("each answering execution is composed");
            for (m, &error) in error_bound.iter().enumerate() {
                let limit = match limits.allowed_metrics.contains(&AccuracyMetric::ALL[m]) {
                    true => tolerated_limit(limits.max_error),
                    false => 0.0,
                };
                if error > limit {
                    return false;
                }
            }
            if limits
                .failure_probability_per_value
                .is_some_and(|limit| failure_probability > tolerated_limit(limit))
            {
                return false;
            }
        }
        true
    }
}

/// $(\varepsilon_c, \delta_c)$ in metric $m_c$ of the execution at
/// position `index`: a readout's requirement, the error it adds; `None` for
/// any other candidate.
fn own_error(inputs: &SelectionInputs<'_>, index: usize) -> Option<RequiredAccuracy> {
    let physical = inputs.physical;
    let execution = inputs.selectable.executions()[index];
    let id = physical.physical_candidates[execution.candidate.0].logical_candidate_id;
    physical
        .logical_candidates
        .memo
        .candidate(id)
        .input_requirements
        .iter()
        .find_map(|requirements| requirements.required_accuracy)
}

/// `own` as an error per metric (by position in `AccuracyMetric::ALL`)
/// and a failure probability; 0 when `None`.
fn own_error_of(own: Option<RequiredAccuracy>) -> ([f64; METRIC_COUNT], f64) {
    let mut error_bound = [0.0; METRIC_COUNT];
    let Some(own) = own else {
        return (error_bound, 0.0);
    };
    let m = AccuracyMetric::ALL
        .iter()
        .position(|&listed| listed == own.metric)
        .expect("AccuracyMetric::ALL lists every metric");
    error_bound[m] = own.max_error;
    (error_bound, own.failure_probability)
}

/// What (K) is built over, per execution (by position in
/// `SelectableExecutions::executions`), before any variable: its
/// selectable input edges and upper bounds on the error and failure
/// probability any selection gives it (the big-M of (K)).
///
/// Only an execution that is its own original (not in a copy of an
/// equivalent context, [`SharedVariables`]) and that a query with an
/// accuracy target reaches has rows: a copy's rows would repeat its
/// original's on the same variables.
///
/// Example: B = p99 / p50 at relative 0.03, both read from DDSketch
/// readouts required at relative 0.02 or 0.01: each readout's own error is
/// its requirement, the ratio adds none and passes relative error on with
/// factor 1/(1 − 0.1) per input, so its ceiling is 1.111 (0.02 + 0.02).
pub(super) struct ErrorModel<'a> {
    /// The rules (K) applies; `None` when no query has a target.
    rules: Option<&'a AccuracyRules>,
    /// Whether (K) has rows for the execution.
    has_rows: Vec<bool>,
    /// Per input, the selectable edges into the execution that have a
    /// $y$: (position in `SelectableExecutions::edges`, producer
    /// execution); empty for an execution without rows.
    inputs: Vec<Vec<Vec<(usize, usize)>>>,
    /// $M$ per metric: the largest $E_{c,m}$ any selection gives.
    error_bound_ceiling: Vec<[f64; METRIC_COUNT]>,
    /// The largest $D_c$ any selection gives.
    failure_probability_ceiling: Vec<f64>,
}

impl<'a> ErrorModel<'a> {
    /// The model over `inputs` and `rules`, with `y` the edge variables
    /// kept and `shared` the copies of equivalent contexts.
    pub(super) fn new(
        inputs: &SelectionInputs<'_>,
        rules: Option<&'a AccuracyRules>,
        y: &BTreeMap<usize, Variable>,
        shared: &SharedVariables,
    ) -> Self {
        let selectable = inputs.selectable;
        let count = selectable.executions().len();
        let has_rows: Vec<bool> = (0..count)
            .map(|index| {
                rules.is_some_and(|rules| rules.targeted[index])
                    && shared.original_of_execution[index] == index
            })
            .collect();
        let inputs_of = |index: usize| -> Vec<Vec<(usize, usize)>> {
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
            inputs: (0..count).map(inputs_of).collect(),
            has_rows,
            error_bound_ceiling: vec![[0.0; METRIC_COUNT]; count],
            failure_probability_ceiling: vec![0.0; count],
        };
        model.set_ceilings(inputs);
        model
    }

    /// The ceilings, by rounds of the composition rule of (K) over the
    /// selectable edges until none grows: at most one round per execution,
    /// the longest path a selection without cycles (constraint (E)) has.
    fn set_ceilings(&mut self, inputs: &SelectionInputs<'_>) {
        let Some(rules) = self.rules else {
            return;
        };
        let with_rows: Vec<usize> = (0..self.has_rows.len())
            .filter(|&c| self.has_rows[c])
            .collect();
        let own: HashMap<usize, ([f64; METRIC_COUNT], f64)> = with_rows
            .iter()
            .map(|&c| (c, own_error_of(own_error(inputs, c))))
            .collect();
        for _ in 0..=with_rows.len() {
            let mut grew = false;
            for &c in &with_rows {
                let (mut error_bound, mut failure_probability) = own[&c];
                for (i, edges) in self.inputs[c].iter().enumerate() {
                    for (m, bound) in error_bound.iter_mut().enumerate() {
                        if let Some(contract) = rules.contract(c, m) {
                            let largest = edges
                                .iter()
                                .map(|&(_, u)| self.error_bound_ceiling[u][m])
                                .fold(0.0, f64::max);
                            *bound += contract.error_factor_per_input[i] * largest;
                        }
                    }
                    failure_probability += edges
                        .iter()
                        .map(|&(_, u)| self.failure_probability_ceiling[u])
                        .fold(0.0, f64::max);
                }
                for (ceiling, bound) in self.error_bound_ceiling[c].iter_mut().zip(error_bound) {
                    if bound > *ceiling {
                        *ceiling = bound;
                        grew = true;
                    }
                }
                if failure_probability > self.failure_probability_ceiling[c] {
                    self.failure_probability_ceiling[c] = failure_probability;
                    grew = true;
                }
            }
            if !grew {
                break;
            }
        }
    }
}

/// The accuracy variables of constraint (K), all continuous, each only for
/// an execution with rows ([`ErrorModel`]) whose ceiling is above 0, by
/// execution position.
pub(super) struct AccuracyVariables {
    /// $E_{\kappa,c,m}$: the execution's error bound in metric $m$ (by
    /// position in `AccuracyMetric::ALL`).
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
        for c in (0..model.has_rows.len()).filter(|&c| model.has_rows[c]) {
            for m in 0..METRIC_COUNT {
                if model.error_bound_ceiling[c][m] > 0.0 {
                    variables
                        .error_bound
                        .insert((c, m), problem.add(variable().min(0)));
                }
            }
            if model.failure_probability_ceiling[c] > 0.0 {
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

    /// Whether (K) has no variable: no query with an accuracy target reads
    /// an error.
    pub(super) fn is_empty(&self) -> bool {
        self.error_bound.is_empty() && self.failure_probability.is_empty()
    }
}

/// (K) (section 4, "Query composition and root checks"), per execution
/// $c$ with rows ([`ErrorModel`]):
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
///   $D_v \le \delta_q + M(1 - z_{q,v})$ with $\delta_q$ per value
///   (`AccuracyTarget::failure_probability_per_value`); no $D$ row without
///   a per-value $\delta_q$. Each limit is widened by [`tolerated_limit`],
///   as the edge check widens it.
///
/// $F$ and $H$ are added to `problem` here. A copy of an equivalent context
/// and its queries have no rows: they share their original's variables,
/// and the copy's rows would be its original's
/// ([`super::equivalent_contexts::copies_in_equivalent_contexts`] pairs
/// queries only with equal targets, $n_q$ and metrics allowed per output).
pub(super) fn accuracy_constraints(
    inputs: &SelectionInputs<'_>,
    model: &ErrorModel<'_>,
    accuracy: &AccuracyVariables,
    y: &BTreeMap<usize, Variable>,
    z: &BTreeMap<(QueryId, usize), Variable>,
    shared: &SharedVariables,
    problem: &mut ProblemVariables,
) -> Vec<Constraint> {
    let Some(rules) = model.rules else {
        return Vec::new();
    };
    let mut constraints = Vec::new();
    for c in (0..model.has_rows.len()).filter(|&c| model.has_rows[c]) {
        // Σ_i β_{c,i,m} F_{c,i,m} per metric, and Σ_i H_{c,i}.
        let mut error_of_inputs: [Expression; METRIC_COUNT] = Default::default();
        let mut failure_probability_of_inputs = Expression::default();
        for (i, edges) in model.inputs[c].iter().enumerate() {
            for (m, error_of_input) in error_of_inputs.iter_mut().enumerate() {
                let read: Vec<(Variable, usize, Variable)> = edges
                    .iter()
                    .filter_map(|&(position, u)| {
                        Some((y[&position], u, *accuracy.error_bound.get(&(u, m))?))
                    })
                    .collect();
                if read.is_empty() {
                    continue;
                }
                let Some(contract) = rules.contract(c, m) else {
                    // The metric rule: no error in m reaches c.
                    for (y, u, error_bound) in read {
                        let ceiling = model.error_bound_ceiling[u][m];
                        constraints.push(constraint!(error_bound <= ceiling * (1 - y)));
                    }
                    continue;
                };
                if !contract.input_error_reaches_output(i) {
                    continue;
                }
                let of_input = problem.add(variable().min(0));
                for (y, u, error_bound) in read {
                    let ceiling = model.error_bound_ceiling[u][m];
                    constraints.push(constraint!(of_input >= error_bound - ceiling * (1 - y)));
                }
                if let Some(holds_up_to) = contract.linear_bound_holds_up_to_per_input[i] {
                    let holds_up_to = tolerated_limit(holds_up_to);
                    constraints.push(constraint!(of_input <= holds_up_to));
                }
                *error_of_input += contract.error_factor_per_input[i] * of_input;
            }
            let read: Vec<(Variable, usize, Variable)> = edges
                .iter()
                .filter_map(|&(position, u)| {
                    Some((y[&position], u, *accuracy.failure_probability.get(&u)?))
                })
                .collect();
            if !read.is_empty() {
                let of_input = problem.add(variable().min(0));
                for (y, u, failure_probability) in read {
                    let ceiling = model.failure_probability_ceiling[u];
                    constraints.push(constraint!(
                        of_input >= failure_probability - ceiling * (1 - y)
                    ));
                }
                failure_probability_of_inputs += of_input;
            }
        }
        let (own_error_bound, own_failure_probability) = own_error_of(own_error(inputs, c));
        for (m, of_inputs) in error_of_inputs.into_iter().enumerate() {
            if let Some(&error_bound) = accuracy.error_bound.get(&(c, m)) {
                let own = own_error_bound[m];
                constraints.push(constraint!(error_bound >= own + of_inputs));
            }
        }
        if let Some(&failure_probability) = accuracy.failure_probability.get(&c) {
            constraints.push(constraint!(
                failure_probability >= own_failure_probability + failure_probability_of_inputs
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
        let Some(limits) = rules.roots.get(&(query, group)) else {
            continue;
        };
        for &v in executions {
            let z = z[&(*query_id, v)];
            for (m, metric) in AccuracyMetric::ALL.iter().enumerate() {
                let Some(&error_bound) = accuracy.error_bound.get(&(v, m)) else {
                    continue;
                };
                let limit = match limits.allowed_metrics.contains(metric) {
                    true => tolerated_limit(limits.max_error),
                    false => 0.0,
                };
                let ceiling = model.error_bound_ceiling[v][m];
                if limit < ceiling {
                    constraints.push(constraint!(
                        error_bound <= limit + (ceiling - limit) * (1 - z)
                    ));
                }
            }
            if let (Some(limit), Some(&failure_probability)) = (
                limits.failure_probability_per_value,
                accuracy.failure_probability.get(&v),
            ) {
                let limit = tolerated_limit(limit);
                let ceiling = model.failure_probability_ceiling[v];
                if limit < ceiling {
                    constraints.push(constraint!(
                        failure_probability <= limit + (ceiling - limit) * (1 - z)
                    ));
                }
            }
        }
    }
    constraints
}

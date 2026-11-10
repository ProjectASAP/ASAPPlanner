//! Stage 3's selection MILP, named after the formulation (concepts doc,
//! sections 3–5).
//!
//! | Family | Variables | Constraints |
//! |---|---|---|
//! | baseline (section 3) over execution contexts (section 5) | $x_{\kappa,v}$ (with $x^{\mathrm{mat}}_v = x_{\mathrm{Materialized}(v),v}$), $y_{\kappa,u,c,i}$, $z_{q,v}$, $t$, $h_v$ ([`SelectionVariables`]) | (A) [`input_satisfaction_constraints`], (B) [`producer_selection_constraints`], (C)/(D) [`query_coverage_constraints`], (E) [`acyclicity_constraints`], (F) [`utilization_constraints`], one consumer per context of a query-time result that is not materialized [`unshared_result_constraints`], (R) [`retention_constraints`]; (G), (H): $y$ exists only on admissible edges where the producer delivers on the consumer's context |
//! | accuracy (section 4), per execution | $E_{\kappa,c,m}$, $D_{\kappa,c}$ ([`AccuracyVariables`]); $F_{\kappa,c,i,m}$, $H_{\kappa,c,i}$ in [`accuracy_constraints`] | (K) [`accuracy_constraints`], over [`ErrorModel`] |

use std::collections::HashMap;
use std::time::Instant;

use good_lp::solvers::highs::highs;
use good_lp::{
    constraint, variable, Constraint, Expression, ProblemVariables, SolverModel, Variable,
};

use asap_accuracy::target_limit;
use asap_types::ir::properties::AccuracyMetric;
use asap_types::ir::QueryRoot;
use asap_types::physical::{ExecutionTiming, Lifetime};
use asap_types::stages::{
    ErrorPropagation, LogicalRequirements, PhysicalASAPMemoCandidates, PhysicalCandidateId,
    PhysicalEdgeId, RequiredAccuracy,
};
use asap_types::workload::QueryWorkloadEntry;

use crate::cost::{edge_read_back_ms, output_retention_need_ms, ExecutionCosts};
use crate::milp::execution_contexts::{Execution, ExecutionContexts};

/// A query, by index in the workload.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct QueryId(pub usize);

/// What the MILP is built over.
pub(crate) struct SelectionInputs<'a> {
    pub(crate) physical: &'a PhysicalASAPMemoCandidates,
    /// Each query's workload entry, for the retention needs of (R).
    pub(crate) entries: &'a [QueryWorkloadEntry],
    /// The executions $x$ and edges $y$ are over.
    pub(crate) contexts: &'a ExecutionContexts,
    /// The objective's coefficients: $x$ is 0 for an execution it could
    /// not cost, $y$ exists only on an edge it costs, $h_v$ only for a
    /// candidate with a retention cost.
    pub(crate) costs: &'a ExecutionCosts,
    /// Each query output reference (a query, or one operator a scalar query
    /// reads), with the executions that may answer it: $z_{q,v}$ exists
    /// only for them.
    pub(crate) query_outputs: &'a [(QueryId, Vec<Execution>)],
}

/// The variables (sections 3 and 5).
pub(crate) struct SelectionVariables {
    /// $x_{\kappa,v}$, by position in `ExecutionContexts::executions`.
    x: Vec<Variable>,
    /// $y_{\kappa,u,c,i}$, by position in `ExecutionContexts::edges`; `None`
    /// on an edge that could not be costed.
    y: Vec<Option<Variable>>,
    /// $z_{q,v}$, for $v \in G_q$, by the execution answering $q$.
    z: HashMap<(QueryId, Execution), Variable>,
    /// $t$, by position in `ExecutionContexts::executions`: only for
    /// executions on a cycle of selectable edges (a strongly connected
    /// component); `None` elsewhere.
    t: Vec<Option<Variable>>,
    /// $h_v$ in seconds, for a candidate with a retention cost.
    h: HashMap<PhysicalCandidateId, Variable>,
}

/// $x$, $y$, $z$, $t$ and $h$ over `inputs`, added to `problem`.
fn selection_variables(
    inputs: &SelectionInputs<'_>,
    problem: &mut ProblemVariables,
) -> SelectionVariables {
    let x = inputs
        .costs
        .per_execution
        .iter()
        .map(|cost| match cost.is_ok() {
            true => problem.add(variable().binary()),
            false => problem.add(variable().min(0).max(0)),
        })
        .collect();
    let y = inputs
        .costs
        .per_edge
        .iter()
        .map(|cost| cost.map(|_| problem.add(variable().binary())))
        .collect();
    let mut z = HashMap::new();
    for (query, executions) in inputs.query_outputs {
        for &execution in executions {
            z.entry((*query, execution))
                .or_insert_with(|| problem.add(variable().binary()));
        }
    }
    // t only inside a strongly connected component of the selectable
    // edges, bounded by its size.
    let t = component_sizes(inputs)
        .into_iter()
        .map(|size| (size > 1).then(|| problem.add(variable().min(0).max((size - 1) as f64))))
        .collect();
    let mut retaining: Vec<PhysicalCandidateId> = inputs
        .costs
        .retention_cost_per_second
        .keys()
        .copied()
        .collect();
    retaining.sort_unstable();
    let h = retaining
        .into_iter()
        .map(|v| (v, problem.add(variable().min(0))))
        .collect();
    SelectionVariables { x, y, z, t, h }
}

/// The selectable edges ($y$ exists), by position in
/// `ExecutionContexts::edges`, with their (consumer, edge, producer).
fn selectable<'a>(
    inputs: &'a SelectionInputs<'_>,
    variables: &'a SelectionVariables,
) -> impl Iterator<Item = (usize, (Execution, PhysicalEdgeId, Execution), Variable)> + 'a {
    inputs
        .contexts
        .edges()
        .iter()
        .zip(&variables.y)
        .enumerate()
        .filter_map(|(k, (&edge, y))| Some((k, edge, (*y)?)))
}

/// (A) $\sum_{u \in P_{c,i}} y_{\kappa,u,c,i} = x_{\kappa,c}$, per execution
/// of $c$ and input $i$.
fn input_satisfaction_constraints(
    inputs: &SelectionInputs<'_>,
    variables: &SelectionVariables,
) -> Vec<Constraint> {
    let physical = inputs.physical;
    let mut producers: HashMap<(usize, usize), Expression> = HashMap::new();
    for (_, (consumer, edge, _), y) in selectable(inputs, variables) {
        *producers
            .entry((
                inputs.contexts.index_of(consumer),
                physical.edge(edge).input_index_of_consumer,
            ))
            .or_default() += y;
    }
    let mut constraints = Vec::new();
    for (index, execution) in inputs.contexts.executions().iter().enumerate() {
        for i in 0..crate::cost::input_count(physical, execution.candidate) {
            let chosen = producers.remove(&(index, i)).unwrap_or_default();
            constraints.push(constraint!(chosen == variables.x[index]));
        }
    }
    constraints
}

/// (B) $y_{\kappa,u,c,i} \le x$ of $u$'s execution it reads.
fn producer_selection_constraints(
    inputs: &SelectionInputs<'_>,
    variables: &SelectionVariables,
) -> Vec<Constraint> {
    selectable(inputs, variables)
        .map(|(_, (_, _, producer), y)| {
            constraint!(y <= variables.x[inputs.contexts.index_of(producer)])
        })
        .collect()
}

/// (C) $\sum_{v \in G_q} z_{q,v} = 1$; (D) $z_{q,v} \le x$ of $v$'s
/// execution answering $q$.
fn query_coverage_constraints(
    inputs: &SelectionInputs<'_>,
    variables: &SelectionVariables,
) -> Vec<Constraint> {
    let mut constraints = Vec::new();
    for (query, executions) in inputs.query_outputs {
        let covered: Expression = executions
            .iter()
            .map(|&execution| variables.z[&(*query, execution)])
            .sum();
        constraints.push(constraint!(covered == 1));
    }
    for (&(_, execution), &z) in sorted_by_key(&variables.z) {
        constraints.push(constraint!(
            z <= variables.x[inputs.contexts.index_of(execution)]
        ));
    }
    constraints
}

/// (E) $t_u + 1 \le t_c + n(1 - y_{\kappa,u,c,i})$ between the producer's
/// and the consumer's executions, only on edges inside a strongly connected
/// component of the selectable edges, $n$ its size.
fn acyclicity_constraints(
    inputs: &SelectionInputs<'_>,
    variables: &SelectionVariables,
) -> Vec<Constraint> {
    let components = selectable_components(inputs);
    let sizes = component_sizes(inputs);
    selectable(inputs, variables)
        .filter_map(|(_, (consumer, _, producer), y)| {
            let (u, c) = (
                inputs.contexts.index_of(producer),
                inputs.contexts.index_of(consumer),
            );
            if components[u] != components[c] {
                return None;
            }
            let (t_u, t_c) = (variables.t[u]?, variables.t[c]?);
            let n = sizes[u] as f64;
            Some(constraint!(t_u + 1 <= t_c + n * (1 - y)))
        })
        .collect()
}

/// (F) $x_{\kappa,v} \le \sum y + \sum_q z_{q,v}$ over the edges and query
/// outputs reading that execution.
fn utilization_constraints(
    inputs: &SelectionInputs<'_>,
    variables: &SelectionVariables,
) -> Vec<Constraint> {
    uses_of(inputs, variables)
        .into_iter()
        .enumerate()
        .map(|(index, used)| constraint!(variables.x[index] <= used))
        .collect()
}

/// A query-time result that is not materialized ($\ell$ = None) has at most
/// one consumer per context: $\sum y + \sum_q z_{q,v} \le x_{\kappa,v}$.
/// Consumers in one context that share a result read a `OneEvaluation` or
/// a materialized producer (section 5). An ingestion-time producer is
/// exempt: there is no `OneEvaluation` at ingestion time, and the
/// consumers of one ingestion run share its rows.
fn unshared_result_constraints(
    inputs: &SelectionInputs<'_>,
    variables: &SelectionVariables,
) -> Vec<Constraint> {
    uses_of(inputs, variables)
        .into_iter()
        .enumerate()
        .filter(|(index, _)| {
            let v = inputs.contexts.executions()[*index].candidate;
            let properties = inputs.physical.physical_candidates[v.0].physical_properties;
            properties.lifetime() == Lifetime::None
                && properties.timing() == ExecutionTiming::QueryTime
        })
        .map(|(index, used)| constraint!(used <= variables.x[index]))
        .collect()
}

/// (R) $h_v \ge \operatorname{need}_{\kappa,v,c,i}\, y_{\kappa,v,c,i}$ and
/// $h_v \ge \operatorname{span}_q\, z_{q,v}$, for a candidate $v$ with $h_v$:
/// how far back its rows are read through each selected edge (the reading
/// path's offset plus range, `crate::cost::read_back_ms`) and by each
/// query reading it directly (its range-evaluation span), a constant per
/// edge, so (R) is linear. Query latency is ignored: a result counts as read
/// when its evaluation starts.
fn retention_constraints(
    inputs: &SelectionInputs<'_>,
    variables: &SelectionVariables,
) -> Vec<Constraint> {
    let mut constraints = Vec::new();
    for (_, (_, edge, producer), y) in selectable(inputs, variables) {
        let Some(&h) = variables.h.get(&producer.candidate) else {
            continue;
        };
        let need_ms = edge_read_back_ms(inputs.physical, inputs.entries, edge, true)
            .expect("a candidate with h_v is read within a bound");
        constraints.push(constraint!(h >= seconds(need_ms) * y));
    }
    for (&(query, execution), &z) in sorted_by_key(&variables.z) {
        if let Some(&h) = variables.h.get(&execution.candidate) {
            let need_ms = output_retention_need_ms(&inputs.entries[query.0]);
            constraints.push(constraint!(h >= seconds(need_ms) * z));
        }
    }
    constraints
}

/// `ms` in seconds, the unit of $h_v$ in the MILP.
fn seconds(ms: u64) -> f64 {
    ms as f64 / 1_000.0
}

/// Per execution, its selected edges to consumers and query outputs:
/// $\sum y + \sum_q z_{q,v}$.
fn uses_of(inputs: &SelectionInputs<'_>, variables: &SelectionVariables) -> Vec<Expression> {
    let mut uses: Vec<Expression> = vec![Expression::default(); variables.x.len()];
    for (_, (_, _, producer), y) in selectable(inputs, variables) {
        uses[inputs.contexts.index_of(producer)] += y;
    }
    for (&(_, execution), &z) in sorted_by_key(&variables.z) {
        uses[inputs.contexts.index_of(execution)] += z;
    }
    uses
}

/// `z` in key order, so the model is the same in every run.
fn sorted_by_key(
    z: &HashMap<(QueryId, Execution), Variable>,
) -> Vec<(&(QueryId, Execution), &Variable)> {
    let mut z: Vec<_> = z.iter().collect();
    z.sort_unstable_by_key(|(key, _)| **key);
    z
}

/// Each execution's strongly connected component of the selectable edges.
fn selectable_components(inputs: &SelectionInputs<'_>) -> Vec<usize> {
    let contexts = inputs.contexts;
    strongly_connected_components(
        contexts.executions().len(),
        contexts
            .edges()
            .iter()
            .zip(&inputs.costs.per_edge)
            .filter(|(_, cost)| cost.is_some())
            .map(|(&(consumer, _, producer), _)| {
                (contexts.index_of(producer), contexts.index_of(consumer))
            }),
    )
}

/// The size of each execution's [`selectable_components`] component: 1
/// for an execution on no cycle.
fn component_sizes(inputs: &SelectionInputs<'_>) -> Vec<usize> {
    let components = selectable_components(inputs);
    let mut size: HashMap<usize, usize> = HashMap::new();
    for &component in &components {
        *size.entry(component).or_default() += 1;
    }
    components.iter().map(|component| size[component]).collect()
}

/// Each node's strongly connected component of the graph with `arcs`
/// (Kosaraju), as a component number.
fn strongly_connected_components(
    node_count: usize,
    arcs: impl Iterator<Item = (usize, usize)>,
) -> Vec<usize> {
    let mut forward = vec![Vec::new(); node_count];
    let mut backward = vec![Vec::new(); node_count];
    for (from, to) in arcs {
        forward[from].push(to);
        backward[to].push(from);
    }
    // Nodes by finishing time of a depth-first search over `forward`.
    let mut finished = Vec::with_capacity(node_count);
    let mut visited = vec![false; node_count];
    for start in 0..node_count {
        if std::mem::replace(&mut visited[start], true) {
            continue;
        }
        let mut stack = vec![(start, 0)];
        while let Some((node, next)) = stack.pop() {
            if let Some(&to) = forward[node].get(next) {
                stack.push((node, next + 1));
                if !std::mem::replace(&mut visited[to], true) {
                    stack.push((to, 0));
                }
            } else {
                finished.push(node);
            }
        }
    }
    let mut component = vec![usize::MAX; node_count];
    for (number, &start) in finished.iter().rev().enumerate() {
        if component[start] != usize::MAX {
            continue;
        }
        component[start] = number;
        let mut stack = vec![start];
        while let Some(node) = stack.pop() {
            for &from in &backward[node] {
                if component[from] == usize::MAX {
                    component[from] = number;
                    stack.push(from);
                }
            }
        }
    }
    component
}

/// The MILP's optimum: the executions with $x = 1$, the edges with
/// $y = 1$, the execution answering each query output ($z = 1$) and $h_v$
/// of each selected candidate that has it, and the MILP's size and time.
#[derive(Debug, Clone)]
pub(crate) struct SolvedSelection {
    /// The selected executions, ascending.
    pub(crate) executions: Vec<Execution>,
    /// The selected edges as (consumer, edge, producer), ascending.
    pub(crate) edges: Vec<(Execution, PhysicalEdgeId, Execution)>,
    /// The execution answering each of `SelectionInputs::query_outputs`, in
    /// its order.
    pub(crate) outputs: Vec<Execution>,
    /// $h_v$ in ms of each selected candidate with $h_v$.
    pub(crate) retention_ms: HashMap<PhysicalCandidateId, u64>,
    /// The objective's value, in cost per second (unscaled).
    pub(crate) objective_per_second: f64,
    pub(crate) variables: usize,
    pub(crate) constraints: usize,
    pub(crate) construction_ms: u64,
    pub(crate) solve_ms: u64,
}

/// Why the MILP has no optimum.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum SelectionMilpError {
    /// No plan meets the constraints.
    Infeasible,
    /// The solver failed.
    Solver(String),
}

/// Builds and solves the MILP: minimize
/// $\sum_v W^{\mathrm{mat}}_v x^{\mathrm{mat}}_v + \sum_{\kappa,v}
/// \rho_\kappa c_{\kappa,v} x_{\kappa,v} + \sum \rho_\kappa c_{\kappa,u,c,i}
/// y_{\kappa,u,c,i} + \sum_v \pi_v h_v$ under (A)–(F),
/// [`unshared_result_constraints`], (R) and (K), with HiGHS at gap 0 on one
/// thread, the coefficients scaled so the largest is 1,000 (HiGHS's
/// tolerances are absolute).
pub(crate) fn solve_selection(
    inputs: &SelectionInputs<'_>,
) -> Result<SolvedSelection, SelectionMilpError> {
    let started = Instant::now();
    let mut problem = ProblemVariables::new();
    let variables = selection_variables(inputs, &mut problem);
    let error_model = ErrorModel::new(inputs);
    let accuracy = accuracy_variables(&error_model, &mut problem);
    let accuracy_constraints =
        accuracy_constraints(inputs, &variables, &error_model, &accuracy, &mut problem);
    let has_accuracy_constraints = !accuracy_constraints.is_empty();
    let constraints: Vec<Constraint> = [
        input_satisfaction_constraints(inputs, &variables),
        producer_selection_constraints(inputs, &variables),
        query_coverage_constraints(inputs, &variables),
        acyclicity_constraints(inputs, &variables),
        utilization_constraints(inputs, &variables),
        unshared_result_constraints(inputs, &variables),
        retention_constraints(inputs, &variables),
        accuracy_constraints,
    ]
    .into_iter()
    .flatten()
    .collect();
    let costs = inputs.costs;
    // π_v per second of retention, the unit of h.
    let retention_cost = |v: &PhysicalCandidateId| costs.retention_cost_per_second[v] * 1_000.0;
    let largest = costs
        .per_execution
        .iter()
        .flatten()
        .chain(costs.per_edge.iter().flatten())
        .copied()
        .chain(costs.retention_cost_per_second.keys().map(retention_cost))
        .fold(0.0_f64, |largest, cost| largest.max(cost.abs()));
    let scale = match largest > 0.0 && largest.is_finite() {
        true => 1_000.0 / largest,
        false => 1.0,
    };
    let mut retaining: Vec<(PhysicalCandidateId, Variable)> =
        variables.h.iter().map(|(&v, &h)| (v, h)).collect();
    retaining.sort_unstable_by_key(|(v, _)| *v);
    // The objective's terms per second, unscaled: (cost, variable).
    let terms: Vec<(f64, Variable)> = costs
        .per_execution
        .iter()
        .zip(&variables.x)
        .filter_map(|(cost, &x)| Some((*cost.as_ref().ok()?, x)))
        .chain(
            costs
                .per_edge
                .iter()
                .zip(&variables.y)
                .filter_map(|(cost, y)| Some(((*cost)?, (*y)?))),
        )
        .chain(retaining.iter().map(|(v, h)| (retention_cost(v), *h)))
        .collect();
    let objective: Expression = terms.iter().map(|&(cost, v)| cost * scale * v).sum();
    let variable_count = problem.len();
    // HiGHS's columns are the variables in the order they were added.
    let column_of: HashMap<Variable, usize> = problem
        .iter_variables_with_def()
        .enumerate()
        .map(|(column, (variable, _))| (variable, column))
        .collect();
    let constraint_count = constraints.len();
    let mut model = problem
        .minimise(objective)
        .using(highs)
        .with_all(constraints)
        .try_into_inner()
        .map_err(|error| SelectionMilpError::Solver(error.to_string()))?;
    model.set_option("mip_rel_gap", 0.0);
    model.set_option("mip_abs_gap", 0.0);
    model.set_option("output_flag", false);
    model.set_option("threads", 1);
    if has_accuracy_constraints {
        // Errors and failure probabilities are small (1e-3, 1e-7): the
        // default feasibility tolerance 1e-7 would let (K) pass a bound
        // above its limit.
        model.set_option("primal_feasibility_tolerance", 1e-10);
        model.set_option("mip_feasibility_tolerance", 1e-10);
    }
    let construction_ms = started.elapsed().as_millis() as u64;
    let started = Instant::now();
    let solved = model
        .try_solve()
        .map_err(|status| SelectionMilpError::Solver(format!("{status:?}")))?;
    let solve_ms = started.elapsed().as_millis() as u64;
    use highs::{HighsModelStatus, HighsSolutionStatus};
    let columns: Vec<f64> = match solved.status() {
        // No constraint: no query reads an operator (literal-only scalar
        // roots), so nothing is selected.
        HighsModelStatus::ModelEmpty => vec![0.0; variable_count],
        HighsModelStatus::Optimal
            if solved.primal_solution_status() == HighsSolutionStatus::Feasible =>
        {
            solved.get_solution().columns().to_vec()
        }
        HighsModelStatus::Infeasible | HighsModelStatus::UnboundedOrInfeasible => {
            return Err(SelectionMilpError::Infeasible)
        }
        other => return Err(SelectionMilpError::Solver(format!("{other:?}"))),
    };
    let value = |variable: Variable| columns[column_of[&variable]];
    let chosen = |variable: Variable| value(variable) > 0.5;
    let executions: Vec<Execution> = inputs
        .contexts
        .executions()
        .iter()
        .zip(&variables.x)
        .filter(|(_, &x)| chosen(x))
        .map(|(&execution, _)| execution)
        .collect();
    let objective_per_second = terms
        .iter()
        .map(|&(cost, variable)| cost * value(variable))
        .sum();
    let retention_ms = retaining
        .iter()
        .filter(|(v, _)| executions.iter().any(|execution| execution.candidate == *v))
        .map(|&(v, h)| (v, (value(h) * 1_000.0).round().max(0.0) as u64))
        .collect();
    Ok(SolvedSelection {
        executions,
        edges: selectable(inputs, &variables)
            .filter(|&(_, _, y)| chosen(y))
            .map(|(_, edge, _)| edge)
            .collect(),
        outputs: inputs
            .query_outputs
            .iter()
            .map(|(query, executions)| {
                executions
                    .iter()
                    .copied()
                    .find(|&execution| chosen(variables.z[&(*query, execution)]))
                    .expect("(C): one output per query output reference")
            })
            .collect(),
        retention_ms,
        objective_per_second,
        variables: variable_count,
        constraints: constraint_count,
        construction_ms,
        solve_ms,
    })
}

/// What (K) is built over, per execution (by position in
/// `ExecutionContexts::executions`), before any variable: each execution's
/// own error and error contract, and upper bounds on the error and failure
/// probability it may carry, the big-M of (K).
struct ErrorModel {
    /// Whether a query with an accuracy target reaches the execution's
    /// group: (K) has rows only for such executions.
    targeted: Vec<bool>,
    /// $(\varepsilon_c, \delta_c)$ in metric $m_c$: a readout's
    /// requirement, its own error; `None` for any other candidate.
    own_error: Vec<Option<RequiredAccuracy>>,
    /// Per metric of `AccuracyMetric::ALL`, the candidate's error contract
    /// (`LogicalRequirements::error_propagation`); `None` where it has none
    /// (the metric rule).
    contract: Vec<[Option<ErrorPropagation>; METRIC_COUNT]>,
    /// The selectable edges into each execution: (input, position in
    /// `ExecutionContexts::edges`, producer execution).
    inputs: Vec<Vec<(usize, usize, usize)>>,
    /// $M$ per metric: the largest $E_{c,m}$ any selection gives.
    error_bound_ceiling: Vec<[f64; METRIC_COUNT]>,
    /// The largest $D_c$ any selection gives.
    failure_probability_ceiling: Vec<f64>,
}

/// The number of metrics, `AccuracyMetric::ALL`.
const METRIC_COUNT: usize = AccuracyMetric::ALL.len();

impl ErrorModel {
    fn new(inputs: &SelectionInputs<'_>) -> Self {
        let physical = inputs.physical;
        let memo = &physical.logical_candidates.memo;
        let executions = inputs.contexts.executions();
        let targeted_query: Vec<bool> = memo
            .queries
            .iter()
            .map(|query| query.accuracy_target.is_some())
            .collect();
        let mut model = Self {
            targeted: Vec::with_capacity(executions.len()),
            own_error: Vec::with_capacity(executions.len()),
            contract: Vec::with_capacity(executions.len()),
            inputs: vec![Vec::new(); executions.len()],
            error_bound_ceiling: vec![[0.0; METRIC_COUNT]; executions.len()],
            failure_probability_ceiling: vec![0.0; executions.len()],
        };
        for execution in executions {
            let id = physical.physical_candidates[execution.candidate.0].logical_candidate_id;
            let candidate = memo.candidate(id);
            let operator = &candidate.expression.operator;
            model.targeted.push(
                memo.groups()[id.group.0 as usize]
                    .reached_by_queries()
                    .iter()
                    .any(|&query| targeted_query[query]),
            );
            let own = candidate
                .input_requirements
                .iter()
                .find_map(|requirements| requirements.required_accuracy);
            model.own_error.push(own);
            model.contract.push(
                AccuracyMetric::ALL
                    .map(|metric| LogicalRequirements::error_propagation(memo, operator, metric)),
            );
        }
        for (position, (&(consumer, edge, producer), cost)) in inputs
            .contexts
            .edges()
            .iter()
            .zip(&inputs.costs.per_edge)
            .enumerate()
        {
            if cost.is_some() {
                model.inputs[inputs.contexts.index_of(consumer)].push((
                    physical.edge(edge).input_index_of_consumer,
                    position,
                    inputs.contexts.index_of(producer),
                ));
            }
        }
        model.set_ceilings();
        model
    }

    /// The ceilings, by rounds of the composition rule of (K) over the
    /// selectable edges until none grows: at most one round per execution,
    /// the longest path a selection without cycles (constraint (E)) has.
    fn set_ceilings(&mut self) {
        for _ in 0..=self.targeted.len() {
            let mut grew = false;
            for c in (0..self.targeted.len()).filter(|&c| self.targeted[c]) {
                let mut error_bound = [0.0; METRIC_COUNT];
                let mut failure_probability = 0.0;
                if let Some(own) = self.own_error[c] {
                    error_bound[metric_index(own.metric)] = own.max_error;
                    failure_probability = own.failure_probability;
                }
                for i in 0..self.input_count(c) {
                    let producers = || self.edges_on(c, i).map(|(_, u)| u);
                    for (m, bound) in error_bound.iter_mut().enumerate() {
                        if let Some(contract) = &self.contract[c][m] {
                            let largest = producers()
                                .map(|u| self.error_bound_ceiling[u][m])
                                .fold(0.0, f64::max);
                            *bound += contract.factor_per_input[i] * largest;
                        }
                    }
                    failure_probability += producers()
                        .map(|u| self.failure_probability_ceiling[u])
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

    /// The number of inputs of execution `c`'s candidate.
    fn input_count(&self, c: usize) -> usize {
        self.inputs[c]
            .iter()
            .map(|&(i, _, _)| i + 1)
            .max()
            .unwrap_or(0)
    }

    /// The selectable edges on input `i` of execution `c`: (position in
    /// `ExecutionContexts::edges`, producer execution).
    fn edges_on(&self, c: usize, i: usize) -> impl Iterator<Item = (usize, usize)> + '_ {
        self.inputs[c]
            .iter()
            .filter(move |&&(input, _, _)| input == i)
            .map(|&(_, position, u)| (position, u))
    }
}

/// `metric`'s position in `AccuracyMetric::ALL`.
fn metric_index(metric: AccuracyMetric) -> usize {
    AccuracyMetric::ALL
        .iter()
        .position(|known| *known == metric)
        .expect("every metric is listed")
}

/// The accuracy variables of constraint (K) (section 4), all continuous,
/// each only for an execution a query with an accuracy target reaches and
/// whose ceiling is above 0, by execution position.
struct AccuracyVariables {
    /// $E_{\kappa,c,m}$: the execution's error bound in metric $m$ (by
    /// position in `AccuracyMetric::ALL`).
    error_bound: HashMap<(usize, usize), Variable>,
    /// $D_{\kappa,c}$: its failure probability.
    failure_probability: HashMap<usize, Variable>,
}

/// $E$ and $D$ over `model`, added to `problem`.
fn accuracy_variables(model: &ErrorModel, problem: &mut ProblemVariables) -> AccuracyVariables {
    let mut variables = AccuracyVariables {
        error_bound: HashMap::new(),
        failure_probability: HashMap::new(),
    };
    for c in (0..model.targeted.len()).filter(|&c| model.targeted[c]) {
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

/// (K) (section 4, "Query composition and root checks"), per execution
/// $c$ a query with an accuracy target reaches:
/// - $E_{c,m} \ge \varepsilon_c[m = m_c] + \sum_i \beta_{c,i,m}
///   F_{c,i,m}$, $F_{c,i,m} \ge E_{u,m} - M_{u,m}(1 - y_{u,c,i})$ per
///   selectable edge, and $E_{u,m} \le M_{u,m}(1 - y_{u,c,i})$ when $c$'s
///   contract has no metric $m$ (the metric rule on the selected path);
/// - $D_c \ge \delta_c + \sum_i H_{c,i}$, $H_{c,i} \ge D_u - M_u(1 -
///   y_{u,c,i})$ (a union bound);
/// - per query $q$ with a target and execution $v$ answering it, $E_{v,m}
///   \le \varepsilon_q[m \text{ accepted}] + M(1 - z_{q,v})$ (a target
///   accepts its metric, every metric when it names none; a scalar root's
///   operand none, as Pass 2 gives its readouts no requirement), $D_v \le
///   \delta_q + M(1 - z_{q,v})$ with $\delta_q$ per value, each limit
///   widened by `target_limit`. A readout no target reaches in an accepted
///   metric has no edge into a consumer that is reached (Pass 2's metric
///   rule cuts it), so its error never reaches a root.
fn accuracy_constraints(
    inputs: &SelectionInputs<'_>,
    selection: &SelectionVariables,
    model: &ErrorModel,
    accuracy: &AccuracyVariables,
    problem: &mut ProblemVariables,
) -> Vec<Constraint> {
    let mut constraints = Vec::new();
    for c in (0..model.targeted.len()).filter(|&c| model.targeted[c]) {
        // Σ_i β_{c,i,m} F_{c,i,m} per metric, and Σ_i H_{c,i}.
        let mut error_of_inputs: [Expression; METRIC_COUNT] = Default::default();
        let mut failure_probability_of_inputs = Expression::default();
        for i in 0..model.input_count(c) {
            let edges = || {
                model
                    .edges_on(c, i)
                    .filter_map(|(position, u)| Some((selection.y[position]?, u)))
            };
            for (m, error_of_input) in error_of_inputs.iter_mut().enumerate() {
                let read: Vec<(Variable, usize, Variable)> = edges()
                    .filter_map(|(y, u)| Some((y, u, *accuracy.error_bound.get(&(u, m))?)))
                    .collect();
                if read.is_empty() {
                    continue;
                }
                let Some(contract) = &model.contract[c][m] else {
                    for (y, u, error_bound) in read {
                        let ceiling = model.error_bound_ceiling[u][m];
                        constraints.push(constraint!(error_bound <= ceiling * (1 - y)));
                    }
                    continue;
                };
                let factor = contract.factor_per_input[i];
                if factor == 0.0 {
                    continue;
                }
                let of_input = problem.add(variable().min(0));
                for (y, u, error_bound) in read {
                    let ceiling = model.error_bound_ceiling[u][m];
                    constraints.push(constraint!(of_input >= error_bound - ceiling * (1 - y)));
                }
                *error_of_input += factor * of_input;
            }
            let read: Vec<(Variable, usize, Variable)> = edges()
                .filter_map(|(y, u)| Some((y, u, *accuracy.failure_probability.get(&u)?)))
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
        let own = model.own_error[c];
        for (m, of_inputs) in error_of_inputs.into_iter().enumerate() {
            if let Some(&error_bound) = accuracy.error_bound.get(&(c, m)) {
                let own = own
                    .filter(|own| metric_index(own.metric) == m)
                    .map_or(0.0, |own| own.max_error);
                constraints.push(constraint!(error_bound >= own + of_inputs));
            }
        }
        if let Some(&failure_probability) = accuracy.failure_probability.get(&c) {
            let own = own.map_or(0.0, |own| own.failure_probability);
            constraints.push(constraint!(
                failure_probability >= own + failure_probability_of_inputs
            ));
        }
    }
    let queries = &inputs.physical.logical_candidates.memo.queries;
    for (query, executions) in inputs.query_outputs {
        let memo_query = &queries[query.0];
        let Some(target) = &memo_query.accuracy_target else {
            continue;
        };
        let operator_root = matches!(memo_query.root, QueryRoot::Operator(_));
        // No limit without a δ; with a whole-result δ and $n_q$ unknown,
        // Pass 2 already pruned the query's readouts.
        let failure_probability_limit = target
            .failure_probability
            .and(target.failure_probability_per_value(memo_query.values_per_evaluation))
            .filter(|_| operator_root);
        for &execution in executions {
            let v = inputs.contexts.index_of(execution);
            let z = selection.z[&(*query, execution)];
            for (m, metric) in AccuracyMetric::ALL.iter().enumerate() {
                let Some(&error_bound) = accuracy.error_bound.get(&(v, m)) else {
                    continue;
                };
                let accepted =
                    operator_root && target.accuracy_metric.is_none_or(|named| named == *metric);
                let limit = if accepted {
                    target_limit(target.max_error)
                } else {
                    0.0
                };
                let ceiling = model.error_bound_ceiling[v][m];
                if limit < ceiling {
                    constraints.push(constraint!(
                        error_bound <= limit + (ceiling - limit) * (1 - z)
                    ));
                }
            }
            if let (Some(limit), Some(&failure_probability)) = (
                failure_probability_limit,
                accuracy.failure_probability.get(&v),
            ) {
                let limit = target_limit(limit);
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

#[cfg(test)]
mod tests;

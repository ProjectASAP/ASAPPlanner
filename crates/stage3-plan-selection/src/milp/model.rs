//! Stage 3's selection MILP, named after the formulation (concepts doc,
//! sections 3–5).
//!
//! | Family | Variables | Constraints |
//! |---|---|---|
//! | baseline (section 3) | $x_v$, $y_{u,c,i}$, $z_{q,v}$, $t_v$ ([`SelectionVariables`]) | (A) [`input_satisfaction_constraints`], (B) [`producer_selection_constraints`], (C)/(D) [`query_coverage_constraints`], (E) [`acyclicity_constraints`], (F) [`utilization_constraints`], one consumer of a query-time result that is not materialized [`unshared_result_constraints`]; (G), (H): $y$ exists only on admissible edges |
//! | execution contexts (section 5) | $x^{\mathrm{mat}}_v$, $x_{\kappa,v}$, $y_{\kappa,u,c,i}$, $h_v$ ([`ExecutionContextVariables`]) | (R) [`retention_constraints`] |
//! | accuracy (section 4) | $E_{c,m}$, $F_{c,i,m}$, $D_c$, $H_{c,i}$ ([`AccuracyVariables`]) | (K) [`accuracy_constraints`] |
//!
//! The baseline is solved by [`solve_selection`]; the extensions' bodies
//! are `todo!()` until execution contexts and constraint (K) are added.

use std::collections::HashMap;
use std::time::Instant;

use good_lp::solvers::highs::highs;
use good_lp::{
    constraint, variable, Constraint, Expression, ProblemVariables, SolverModel, Variable,
};

use asap_types::accuracy::AccuracyMetric;
use asap_types::ir::properties::ExecutionTiming;
use asap_types::stages::Lifetime;
use asap_types::stages::{
    ExecutionContextId, PhysicalASAPMemoCandidates, PhysicalCandidateId, PhysicalEdgeId,
};

use crate::cost::{input_count, SelectionCosts};

/// A query, by index in the workload.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct QueryId(pub usize);

/// What the baseline MILP is built over.
pub(crate) struct SelectionInputs<'a> {
    pub(crate) physical: &'a PhysicalASAPMemoCandidates,
    /// $w_v$ and $c_{u,c,i}$: $y$ exists only on the edges
    /// [`SelectionCosts::edge`] costs, $x_v$ is 0 for a candidate it could
    /// not cost.
    pub(crate) costs: &'a SelectionCosts,
    /// Each query output reference (a query, or one operator a scalar query
    /// reads), with the candidates that may answer it: $z_{q,v}$ exists
    /// only for them.
    pub(crate) query_outputs: &'a [(QueryId, Vec<PhysicalCandidateId>)],
}

/// The baseline variables (section 3).
pub(crate) struct SelectionVariables {
    /// $x_v$, by [`PhysicalCandidateId`].
    x: Vec<Variable>,
    /// $y_{u,c,i}$, on admissible edges only.
    y: HashMap<PhysicalEdgeId, Variable>,
    /// $z_{q,v}$, for $v \in G_q$.
    z: HashMap<(QueryId, PhysicalCandidateId), Variable>,
    /// $t_v$, by [`PhysicalCandidateId`]: only for candidates on a cycle of
    /// admissible edges (a strongly connected component); `None` elsewhere.
    t: Vec<Option<Variable>>,
}

/// $x$, $y$, $z$ and $t$ over `inputs`, added to `problem`.
fn selection_variables(
    inputs: &SelectionInputs<'_>,
    problem: &mut ProblemVariables,
) -> SelectionVariables {
    let count = inputs.physical.physical_candidates.len();
    let x = (0..count)
        .map(|v| match inputs.costs.candidate[v].is_ok() {
            true => problem.add(variable().binary()),
            false => problem.add(variable().min(0).max(0)),
        })
        .collect();
    let mut edges: Vec<PhysicalEdgeId> = inputs.costs.edge.keys().copied().collect();
    edges.sort_unstable();
    let y = edges
        .into_iter()
        .map(|edge| (edge, problem.add(variable().binary())))
        .collect();
    let mut z = HashMap::new();
    for (query, candidates) in inputs.query_outputs {
        for &v in candidates {
            z.entry((*query, v))
                .or_insert_with(|| problem.add(variable().binary()));
        }
    }
    // t_v only inside a strongly connected component of the selectable
    // edges, bounded by its size.
    let t = component_sizes(inputs)
        .into_iter()
        .map(|size| (size > 1).then(|| problem.add(variable().min(0).max((size - 1) as f64))))
        .collect();
    SelectionVariables { x, y, z, t }
}

/// (A) $\sum_{u \in P_{c,i}} y_{u,c,i} = x_c$.
fn input_satisfaction_constraints(
    inputs: &SelectionInputs<'_>,
    variables: &SelectionVariables,
) -> Vec<Constraint> {
    let physical = inputs.physical;
    let mut constraints = Vec::new();
    for c in (0..physical.physical_candidates.len()).map(PhysicalCandidateId) {
        for i in 0..input_count(physical, c) {
            let producers: Expression = physical
                .admissible_edges_into(c, i)
                .iter()
                .filter_map(|edge| variables.y.get(edge))
                .sum();
            constraints.push(constraint!(producers == variables.x[c.0]));
        }
    }
    constraints
}

/// (B) $y_{u,c,i} \le x_u$.
fn producer_selection_constraints(
    inputs: &SelectionInputs<'_>,
    variables: &SelectionVariables,
) -> Vec<Constraint> {
    sorted(&variables.y)
        .map(|(edge, y)| {
            let producer = inputs.physical.edge(edge).producer_candidate_id;
            constraint!(y <= variables.x[producer.0])
        })
        .collect()
}

/// (C) $\sum_{v \in G_q} z_{q,v} = 1$; (D) $z_{q,v} \le x_v$.
fn query_coverage_constraints(
    inputs: &SelectionInputs<'_>,
    variables: &SelectionVariables,
) -> Vec<Constraint> {
    let mut constraints = Vec::new();
    for (query, candidates) in inputs.query_outputs {
        let covered: Expression = candidates.iter().map(|&v| variables.z[&(*query, v)]).sum();
        constraints.push(constraint!(covered == 1));
    }
    for (&(_, v), &z) in sorted_by_key(&variables.z) {
        constraints.push(constraint!(z <= variables.x[v.0]));
    }
    constraints
}

/// (E) $t_u + 1 \le t_c + n(1 - y_{u,c,i})$, only on edges inside a strongly
/// connected component of the admissible edges, $n$ its size.
fn acyclicity_constraints(
    inputs: &SelectionInputs<'_>,
    variables: &SelectionVariables,
) -> Vec<Constraint> {
    let components = selectable_components(inputs);
    let sizes = component_sizes(inputs);
    sorted(&variables.y)
        .filter_map(|(edge, y)| {
            let edge = inputs.physical.edge(edge);
            let (u, c) = (edge.producer_candidate_id.0, edge.consumer_candidate_id.0);
            if components[u] != components[c] {
                return None;
            }
            let (t_u, t_c) = (variables.t[u]?, variables.t[c]?);
            let n = sizes[u] as f64;
            Some(constraint!(t_u + 1 <= t_c + n * (1 - y)))
        })
        .collect()
}

/// (F) $x_v \le \sum_{(v,c,i)} y_{v,c,i} + \sum_q z_{q,v}$.
fn utilization_constraints(
    inputs: &SelectionInputs<'_>,
    variables: &SelectionVariables,
) -> Vec<Constraint> {
    let uses = uses_of(inputs, variables);
    uses.into_iter()
        .enumerate()
        .map(|(v, used)| constraint!(variables.x[v] <= used))
        .collect()
}

/// A query-time result that is not materialized ($\ell$ = None) has at most
/// one consumer: $\sum_{(v,c,i)} y_{v,c,i} + \sum_q z_{q,v} \le x_v$.
/// Consumers that share a result read a `OneEvaluation` or a materialized
/// producer (section 5, per execution context). An ingestion-time
/// producer is exempt: there is no `OneEvaluation` at ingestion time, and
/// the consumers of one ingestion run share its rows.
fn unshared_result_constraints(
    inputs: &SelectionInputs<'_>,
    variables: &SelectionVariables,
) -> Vec<Constraint> {
    let uses = uses_of(inputs, variables);
    uses.into_iter()
        .enumerate()
        .filter(|(v, _)| {
            let properties = inputs.physical.physical_candidates[*v].physical_properties;
            properties.lifetime() == Lifetime::None
                && properties.timing() == ExecutionTiming::QueryTime
        })
        .map(|(v, used)| constraint!(used <= variables.x[v]))
        .collect()
}

/// Per candidate, its selected edges to consumers and query outputs:
/// $\sum_{(v,c,i)} y_{v,c,i} + \sum_q z_{q,v}$.
fn uses_of(inputs: &SelectionInputs<'_>, variables: &SelectionVariables) -> Vec<Expression> {
    let mut uses: Vec<Expression> = (0..inputs.physical.physical_candidates.len())
        .map(|v| {
            inputs
                .physical
                .admissible_edges_out_of(PhysicalCandidateId(v))
                .iter()
                .filter_map(|edge| variables.y.get(edge))
                .sum()
        })
        .collect();
    for (&(_, v), &z) in sorted_by_key(&variables.z) {
        uses[v.0] += z;
    }
    uses
}

/// `y` in edge order, so the model is the same in every run.
fn sorted(
    y: &HashMap<PhysicalEdgeId, Variable>,
) -> impl Iterator<Item = (PhysicalEdgeId, Variable)> {
    let mut y: Vec<(PhysicalEdgeId, Variable)> = y.iter().map(|(&e, &v)| (e, v)).collect();
    y.sort_unstable_by_key(|(edge, _)| *edge);
    y.into_iter()
}

/// `z` in key order.
fn sorted_by_key(
    z: &HashMap<(QueryId, PhysicalCandidateId), Variable>,
) -> Vec<(&(QueryId, PhysicalCandidateId), &Variable)> {
    let mut z: Vec<_> = z.iter().collect();
    z.sort_unstable_by_key(|(key, _)| **key);
    z
}

/// Each candidate's strongly connected component of the selectable edges.
fn selectable_components(inputs: &SelectionInputs<'_>) -> Vec<usize> {
    strongly_connected_components(
        inputs.physical.physical_candidates.len(),
        inputs.costs.edge.keys().map(|&edge| {
            let edge = inputs.physical.edge(edge);
            (edge.producer_candidate_id.0, edge.consumer_candidate_id.0)
        }),
    )
}

/// The size of each candidate's [`selectable_components`] component: 1
/// for a candidate on no cycle.
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

/// The baseline MILP's optimum: $\{x = 1\}$, $\{y = 1\}$ and $\{z = 1\}$,
/// and the MILP's size and time.
#[derive(Debug, Clone)]
pub(crate) struct SolvedSelection {
    /// The selected candidates, ascending.
    pub(crate) candidates: Vec<PhysicalCandidateId>,
    /// The selected edges, ascending.
    pub(crate) edges: Vec<PhysicalEdgeId>,
    /// The candidate answering each of `SelectionInputs::query_outputs`, in
    /// its order.
    pub(crate) outputs: Vec<PhysicalCandidateId>,
    pub(crate) variables: usize,
    pub(crate) constraints: usize,
    pub(crate) construction_ms: u64,
    pub(crate) solve_ms: u64,
}

/// Why the baseline MILP has no optimum.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum SelectionMilpError {
    /// No plan meets constraints (A)–(F).
    Infeasible,
    /// The solver failed.
    Solver(String),
}

/// Builds and solves the baseline MILP: minimize
/// $\sum_v w_v x_v + \sum_{(u,c,i)} c_{u,c,i} y_{u,c,i}$ under (A)–(F) and
/// [`unshared_result_constraints`], with HiGHS at gap 0 on one thread, the
/// coefficients scaled so the largest is 1,000 (HiGHS's tolerances are
/// absolute).
pub(crate) fn solve_selection(
    inputs: &SelectionInputs<'_>,
) -> Result<SolvedSelection, SelectionMilpError> {
    let started = Instant::now();
    let mut problem = ProblemVariables::new();
    let variables = selection_variables(inputs, &mut problem);
    let constraints: Vec<Constraint> = [
        input_satisfaction_constraints(inputs, &variables),
        producer_selection_constraints(inputs, &variables),
        query_coverage_constraints(inputs, &variables),
        acyclicity_constraints(inputs, &variables),
        utilization_constraints(inputs, &variables),
        unshared_result_constraints(inputs, &variables),
    ]
    .into_iter()
    .flatten()
    .collect();
    let largest = inputs
        .costs
        .candidate
        .iter()
        .flatten()
        .chain(inputs.costs.edge.values())
        .fold(0.0_f64, |largest, &cost| largest.max(cost.abs()));
    let scale = match largest > 0.0 && largest.is_finite() {
        true => 1_000.0 / largest,
        false => 1.0,
    };
    let candidate_terms = inputs
        .costs
        .candidate
        .iter()
        .zip(&variables.x)
        .filter_map(|(cost, &x)| cost.as_ref().ok().map(|cost| cost * scale * x));
    let edge_terms = sorted(&variables.y).map(|(edge, y)| inputs.costs.edge[&edge] * scale * y);
    let objective: Expression = candidate_terms.chain(edge_terms).sum();
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
    let chosen = |variable: Variable| columns[column_of[&variable]] > 0.5;
    Ok(SolvedSelection {
        candidates: (0..variables.x.len())
            .filter(|&v| chosen(variables.x[v]))
            .map(PhysicalCandidateId)
            .collect(),
        edges: sorted(&variables.y)
            .filter(|&(_, y)| chosen(y))
            .map(|(edge, _)| edge)
            .collect(),
        outputs: inputs
            .query_outputs
            .iter()
            .map(|(query, candidates)| {
                candidates
                    .iter()
                    .copied()
                    .find(|&v| chosen(variables.z[&(*query, v)]))
                    .expect("(C): one output per query output reference")
            })
            .collect(),
        variables: variable_count,
        constraints: constraint_count,
        construction_ms,
        solve_ms,
    })
}

/// The execution-context variables (section 5), which replace $x$ and the
/// unindexed $y$ of the baseline.
#[allow(dead_code)] // Read once execution contexts are selected.
struct ExecutionContextVariables {
    /// $x^{\mathrm{mat}}_v$, for an `AcrossEvaluations` candidate.
    x_materialized: HashMap<PhysicalCandidateId, Variable>,
    /// $x_{\kappa,v}$, for a candidate with $\ell$ = None or `OneEvaluation`.
    x_in_execution_context: HashMap<(ExecutionContextId, PhysicalCandidateId), Variable>,
    /// $y_{\kappa,u,c,i}$, indexed by the consumer's context.
    y_in_execution_context: HashMap<(ExecutionContextId, PhysicalEdgeId), Variable>,
    /// $h_v$, for an `AcrossEvaluations` producer.
    retention_amount: HashMap<PhysicalCandidateId, Variable>,
}

/// The accuracy variables of constraint (K) (section 4), all continuous.
#[allow(dead_code)] // Read once constraint (K) is added.
struct AccuracyVariables {
    /// $E_{c,m}$: candidate $c$'s error bound in metric $m$.
    error_bound: HashMap<(PhysicalCandidateId, AccuracyMetric), Variable>,
    /// $F_{c,i,m}$: the chosen producer's error bound in metric $m$ on input
    /// $i$ of $c$.
    error_bound_of_input: HashMap<(PhysicalCandidateId, usize, AccuracyMetric), Variable>,
    /// $D_c$, by [`PhysicalCandidateId`].
    failure_probability: Vec<Variable>,
    /// $H_{c,i}$: the chosen producer's failure probability on input $i$ of $c$.
    failure_probability_of_input: HashMap<(PhysicalCandidateId, usize), Variable>,
}

/// (R) $h_v \ge \operatorname{need}_{\kappa,v,c,i}\, y_{\kappa,v,c,i}$: for a
/// time-indexed producer (raw data, a chain, results stored per evaluation
/// time) the reading path's offset plus range, a constant per edge, so (R)
/// is linear. Query latency is ignored: a result counts as read when its
/// evaluation starts.
#[allow(dead_code)] // Called once execution contexts are selected.
fn retention_constraints(
    physical: &PhysicalASAPMemoCandidates,
    variables: &ExecutionContextVariables,
) -> Vec<Constraint> {
    let _ = (physical, variables);
    todo!("#1207 PR 7: constraint (R)")
}

/// (K): $E_{c,m}$ per candidate and metric and $D_c$ per candidate, big-M on
/// selected edges, the metric rule on the selected path, and the root
/// checks $E_{v,m_q} \le \varepsilon_q + M_q(1 - z_{q,v})$,
/// $D_v \le \delta_q + (1 - z_{q,v})$.
#[allow(dead_code)] // Called once constraint (K) is added.
fn accuracy_constraints(
    physical: &PhysicalASAPMemoCandidates,
    selection: &SelectionVariables,
    accuracy: &AccuracyVariables,
) -> Vec<Constraint> {
    let _ = (physical, selection, accuracy);
    todo!("#1207 PR 8: constraint (K)")
}

#[cfg(test)]
mod tests;

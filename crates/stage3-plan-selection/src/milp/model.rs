//! Stage 3's selection MILP, named after the formulation (concepts doc,
//! sections 3–5).
//!
//! | Family | Variables | Constraints |
//! |---|---|---|
//! | baseline (section 3) | $x_v$, $y_{u,c,i}$, $z_{q,v}$, $t_v$ ([`SelectionVariables`]) | (A) [`input_satisfaction_constraints`], (B) [`producer_selection_constraints`], (C)/(D) [`query_coverage_constraints`], (E) [`acyclicity_constraints`], (F) [`utilization_constraints`]; (G), (H): $y$ exists only on admissible edges |
//! | execution contexts (section 5) | $x^{\mathrm{mat}}_v$, $x_{\kappa,v}$, $y_{\kappa,u,c,i}$, $h_v$ ([`ExecutionContextVariables`]) | (R) [`retention_constraints`] |
//! | accuracy (section 4) | $E_c$, $F_{c,i}$, $D_c$, $H_{c,i}$ ([`AccuracyVariables`]) | (K) [`accuracy_constraints`] |
//!
//! The baseline (section 3) is built and solved by [`solve_selection`]:
//! $x_v$ and $y_{u,c,i}$ binary, $z_{q,v}$ binary per query output group,
//! $t_v$ continuous in $[0, |\mathcal C| - 1]$, the objective
//! $\min \sum_v w_v x_v$ with $w_v$ from [`crate::candidate_cost`]. The
//! execution-context and accuracy families are `todo!()` until #1207 PRs 7
//! (contexts) and 8 (accuracy); nothing calls them yet, hence their
//! `dead_code` allowances.

use std::collections::{BTreeSet, HashMap};
use std::time::Instant;

use good_lp::solvers::highs::highs;
use good_lp::{
    constraint, variable, Constraint, Expression, ProblemVariables, SolverModel, Variable,
};

use asap_types::physical::{ExecutionTiming, Lifetime};
use asap_types::stages::{
    output_references, DagOfSelectedCandidates, ExecutionContext, ExecutionContextId, MemoGroupId,
    PhysicalASAPMemoCandidates, PhysicalCandidateId, PhysicalEdge, PhysicalEdgeId, SelectedEdge,
    SelectedNodeId,
};
use asap_types::workload::QueryWorkloadEntry;

/// A query, by index in the workload.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct QueryId(pub usize);

/// The baseline variables (section 3).
pub(crate) struct SelectionVariables {
    /// $x_v$, by [`PhysicalCandidateId`].
    x: Vec<Variable>,
    /// $y_{u,c,i}$, on admissible edges only.
    y: HashMap<PhysicalEdgeId, Variable>,
    /// $z_{q,v}$, for $v \in G_q$.
    z: HashMap<(QueryId, PhysicalCandidateId), Variable>,
    /// $t_v$, by [`PhysicalCandidateId`].
    t: Vec<Variable>,
}

/// What the baseline MILP is built over: Stage 2's candidates and
/// admissible edges, each candidate's $w_v$ (`None`: not costable, so
/// $x_v = 0$), each query's entry, and indexes built once
/// ([`Self::new`]).
pub(crate) struct SelectionInputs<'a> {
    pub(crate) physical: &'a PhysicalASAPMemoCandidates,
    /// $w_v$ per second, by [`PhysicalCandidateId`].
    pub(crate) cost_per_second: &'a [Option<f64>],
    /// Each query's workload entry, whose recurrence and range evaluation
    /// are its evaluation context.
    pub(crate) entries: &'a [QueryWorkloadEntry],
    /// $\mathcal E$, by [`PhysicalEdgeId`].
    edges: Vec<PhysicalEdge>,
    /// The physical candidates of each memo group, ascending.
    candidates_of_group: HashMap<MemoGroupId, Vec<PhysicalCandidateId>>,
    /// Each query's output groups $G_q$, each pair once, in
    /// output-reference order.
    query_output_groups: Vec<(QueryId, MemoGroupId)>,
}

impl<'a> SelectionInputs<'a> {
    pub(crate) fn new(
        physical: &'a PhysicalASAPMemoCandidates,
        cost_per_second: &'a [Option<f64>],
        entries: &'a [QueryWorkloadEntry],
    ) -> Self {
        let mut candidates_of_group: HashMap<MemoGroupId, Vec<PhysicalCandidateId>> =
            HashMap::new();
        for (id, candidate) in physical.physical_candidates.iter().enumerate() {
            candidates_of_group
                .entry(candidate.logical_candidate_id.group)
                .or_default()
                .push(PhysicalCandidateId(id));
        }
        let mut seen = std::collections::HashSet::new();
        let query_output_groups = output_references(&physical.query_roots)
            .into_iter()
            .map(|(query, group)| (QueryId(query), group))
            .filter(|pair| seen.insert(*pair))
            .collect();
        Self {
            physical,
            cost_per_second,
            entries,
            edges: physical.edges().map(|(_, edge)| edge).collect(),
            candidates_of_group,
            query_output_groups,
        }
    }

    /// $\mathcal E$, by [`PhysicalEdgeId`].
    fn edges(&self) -> &[PhysicalEdge] {
        &self.edges
    }

    /// $|\mathcal I(c)|$: the inputs of candidate `c`.
    fn input_count(&self, c: PhysicalCandidateId) -> usize {
        self.physical
            .logical_candidates
            .memo
            .candidate(self.physical.physical_candidates[c.0].logical_candidate_id)
            .expression
            .operator
            .children()
            .len()
    }

    /// Whether edge `edge`'s producer delivers on every evaluation context
    /// its consumer serves: a producer materialized across evaluations read
    /// by a query-time consumer must deliver on the evaluation of each
    /// query reaching the consumer's group (`PhysicalCandidate::delivers_on`).
    /// The baseline has no execution contexts, so it requires every one;
    /// with contexts (#1207 PR 7) the check is per context $\kappa$ of
    /// $y_{\kappa,u,c,i}$. A consumer at ingestion time is not checked
    /// here (its pipeline context is #1207 PR 7's).
    fn delivers_to_consumer(&self, edge: &PhysicalEdge) -> bool {
        let producer = &self.physical.physical_candidates[edge.producer_candidate_id.0];
        let consumer = &self.physical.physical_candidates[edge.consumer_candidate_id.0];
        if !matches!(
            producer.physical_properties.lifetime(),
            Lifetime::AcrossEvaluations(_)
        ) || consumer.physical_properties.timing() != ExecutionTiming::QueryTime
        {
            return true;
        }
        let memo = &self.physical.logical_candidates.memo;
        memo.groups()[consumer.logical_candidate_id.group.0 as usize]
            .queries
            .iter()
            .filter_map(|&query| self.entries.get(query))
            .all(|entry| {
                producer.delivers_on(
                    memo,
                    &ExecutionContext::Evaluation {
                        recurrence: entry.recurrence.clone(),
                        range_evaluation: entry.range_evaluation,
                    },
                )
            })
    }

    /// The physical candidates of memo group `group`, ascending.
    fn candidates_of_group(&self, group: MemoGroupId) -> Vec<PhysicalCandidateId> {
        self.candidates_of_group
            .get(&group)
            .cloned()
            .unwrap_or_default()
    }

    /// Each query's output groups $G_q$, each pair once, in output-reference
    /// order: a scalar query reads one per operator reference.
    fn query_output_groups(&self) -> Vec<(QueryId, MemoGroupId)> {
        self.query_output_groups.clone()
    }
}

/// The baseline MILP's variables over `inputs` (constraint (I), domains):
/// $x_v$ binary, fixed at 0 for a candidate that cannot be costed; $y$
/// binary on each admissible edge ((G) and (H)) whose producer and consumer
/// can be costed and whose producer delivers on its consumer's evaluations
/// ([`SelectionInputs::delivers_to_consumer`]); $z_{q,v}$ binary for each costable $v \in G_q$;
/// $t_v \in [0, |\mathcal C| - 1]$ continuous.
fn selection_variables(
    inputs: &SelectionInputs<'_>,
    problem: &mut ProblemVariables,
) -> SelectionVariables {
    let count = inputs.physical.physical_candidates.len();
    let costable = |id: PhysicalCandidateId| inputs.cost_per_second[id.0].is_some();
    let x = (0..count)
        .map(|id| match costable(PhysicalCandidateId(id)) {
            true => problem.add(variable().binary()),
            false => problem.add(variable().min(0).max(0)),
        })
        .collect();
    let mut y = HashMap::new();
    for (id, edge) in inputs.physical.edges() {
        if edge.satisfies_logical_requirement
            && edge.satisfies_physical_requirement
            && inputs.delivers_to_consumer(&edge)
            && costable(edge.producer_candidate_id)
            && costable(edge.consumer_candidate_id)
        {
            y.insert(id, problem.add(variable().binary()));
        }
    }
    let mut z = HashMap::new();
    for (query, group) in inputs.query_output_groups() {
        for v in inputs.candidates_of_group(group) {
            if costable(v) {
                z.entry((query, v))
                    .or_insert_with(|| problem.add(variable().binary()));
            }
        }
    }
    let rank_bound = count.saturating_sub(1) as f64;
    let t = (0..count)
        .map(|_| problem.add(variable().min(0).max(rank_bound)))
        .collect();
    SelectionVariables { x, y, z, t }
}

/// (A) $\sum_{u \in P_{c,i}} y_{u,c,i} = x_c$: every input of a selected
/// consumer has exactly one selected producer, and an unselected consumer
/// none.
fn input_satisfaction_constraints(
    inputs: &SelectionInputs<'_>,
    variables: &SelectionVariables,
) -> Vec<Constraint> {
    let mut constraints = Vec::new();
    for c in (0..inputs.physical.physical_candidates.len()).map(PhysicalCandidateId) {
        for i in 0..inputs.input_count(c) {
            let producers: Expression = inputs
                .physical
                .admissible_edges_into(c, i)
                .iter()
                .filter_map(|edge| variables.y.get(edge))
                .sum();
            constraints.push(constraint!(producers == variables.x[c.0]));
        }
    }
    constraints
}

/// (B) $y_{u,c,i} \le x_u$: a chosen producer is selected.
fn producer_selection_constraints(
    inputs: &SelectionInputs<'_>,
    variables: &SelectionVariables,
) -> Vec<Constraint> {
    let edges = inputs.edges();
    let mut by_edge: Vec<(&PhysicalEdgeId, &Variable)> = variables.y.iter().collect();
    by_edge.sort_by_key(|(edge, _)| **edge);
    by_edge
        .into_iter()
        .map(|(edge, &y)| {
            let producer = edges[edge.0].producer_candidate_id;
            constraint!(y <= variables.x[producer.0])
        })
        .collect()
}

/// (C) $\sum_{v \in G_q} z_{q,v} = 1$, for each output group of each query;
/// (D) $z_{q,v} \le x_v$.
fn query_coverage_constraints(
    inputs: &SelectionInputs<'_>,
    variables: &SelectionVariables,
) -> Vec<Constraint> {
    let mut constraints = Vec::new();
    for (query, group) in inputs.query_output_groups() {
        let outputs: Vec<(PhysicalCandidateId, Variable)> = inputs
            .candidates_of_group(group)
            .into_iter()
            .filter_map(|v| variables.z.get(&(query, v)).map(|&z| (v, z)))
            .collect();
        let covered: Expression = outputs.iter().map(|&(_, z)| z).sum();
        constraints.push(constraint!(covered == 1));
        for (v, z) in outputs {
            constraints.push(constraint!(z <= variables.x[v.0]));
        }
    }
    constraints
}

/// (E) $t_u + 1 \le t_c + |\mathcal C|(1 - y_{u,c,i})$: no selected cycle.
/// Only edges inside a strongly connected component of the admissible
/// edges can close a cycle, so (E) is written for those only (a producer
/// reading itself included, which (E) forbids).
fn acyclicity_constraints(
    inputs: &SelectionInputs<'_>,
    variables: &SelectionVariables,
) -> Vec<Constraint> {
    let edges = inputs.edges();
    let component = strongly_connected_components(
        inputs.physical.physical_candidates.len(),
        variables.y.keys().map(|edge| {
            (
                edges[edge.0].producer_candidate_id.0,
                edges[edge.0].consumer_candidate_id.0,
            )
        }),
    );
    let candidate_count = inputs.physical.physical_candidates.len() as f64;
    let mut on_cycles: Vec<(&PhysicalEdgeId, &Variable)> = variables
        .y
        .iter()
        .filter(|(edge, _)| {
            let edge = &edges[edge.0];
            component[edge.producer_candidate_id.0] == component[edge.consumer_candidate_id.0]
        })
        .collect();
    on_cycles.sort_by_key(|(edge, _)| **edge);
    on_cycles
        .into_iter()
        .map(|(edge, &y)| {
            let edge = &edges[edge.0];
            let t_u = variables.t[edge.producer_candidate_id.0];
            let t_c = variables.t[edge.consumer_candidate_id.0];
            constraint!(t_u + 1 <= t_c + candidate_count * (1 - y))
        })
        .collect()
}

/// (F) $x_v \le \sum_{(v,c,i)} y_{v,c,i} + \sum_q z_{q,v}$: a selected
/// candidate supplies a selected input or a query output.
fn utilization_constraints(
    inputs: &SelectionInputs<'_>,
    variables: &SelectionVariables,
) -> Vec<Constraint> {
    let mut outputs_of: HashMap<PhysicalCandidateId, Expression> = HashMap::new();
    for (&(_, v), &z) in &variables.z {
        *outputs_of.entry(v).or_default() += z;
    }
    (0..inputs.physical.physical_candidates.len())
        .map(PhysicalCandidateId)
        .map(|v| {
            let consumers: Expression = inputs
                .physical
                .admissible_edges_out_of(v)
                .iter()
                .filter_map(|edge| variables.y.get(edge))
                .sum();
            let used = consumers + outputs_of.remove(&v).unwrap_or_default();
            constraint!(variables.x[v.0] <= used)
        })
        .collect()
}

/// The component of each of `node_count` nodes in the graph of `arcs`
/// (`(from, to)`), numbered so equal numbers are one strongly connected
/// component (Kosaraju's algorithm, iterative).
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
        if visited[start] {
            continue;
        }
        visited[start] = true;
        let mut stack = vec![(start, 0)];
        while let Some((node, next)) = stack.pop() {
            if let Some(&to) = forward[node].get(next) {
                stack.push((node, next + 1));
                if !visited[to] {
                    visited[to] = true;
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

/// The solved baseline: the selected DAG and what the solve took.
#[derive(Debug, Clone)]
pub(crate) struct SolvedSelection {
    pub(crate) selected_dag: DagOfSelectedCandidates,
    /// $\sum_v w_v x_v$ at the optimum, per second.
    pub(crate) objective_per_second: f64,
    /// The solver's lower bound on it; NaN when it reports none.
    pub(crate) lower_bound_per_second: f64,
    pub(crate) variable_count: usize,
    pub(crate) constraint_count: usize,
    pub(crate) construction_ms: u64,
    pub(crate) solver_ms: u64,
}

/// Why the baseline MILP selected nothing.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum SelectionMilpError {
    /// No assignment meets (A)–(I): some query output has no candidate
    /// whose inputs all have costable admissible producers.
    Infeasible,
    /// HiGHS failed.
    Solver(String),
}

/// Build the baseline MILP over `inputs` ((A)–(I), $\min \sum_v w_v x_v$),
/// solve it to proven optimality with HiGHS, and decode $\{x = 1\}$,
/// $\{y = 1\}$ and $\{z = 1\}$ into a [`DagOfSelectedCandidates`]: one node
/// per selected candidate, in candidate order; one edge per selected $y$;
/// each output reference's node the selected $z$ of its query and group.
pub(crate) fn solve_selection(
    inputs: &SelectionInputs<'_>,
) -> Result<SolvedSelection, SelectionMilpError> {
    // (C) cannot hold for a query output group without a costable
    // candidate.
    if inputs.query_output_groups().into_iter().any(|(_, group)| {
        inputs
            .candidates_of_group(group)
            .into_iter()
            .all(|v| inputs.cost_per_second[v.0].is_none())
    }) {
        return Err(SelectionMilpError::Infeasible);
    }
    let started = Instant::now();
    let mut problem = ProblemVariables::new();
    let variables = selection_variables(inputs, &mut problem);
    let constraints: Vec<Constraint> = [
        input_satisfaction_constraints(inputs, &variables),
        producer_selection_constraints(inputs, &variables),
        query_coverage_constraints(inputs, &variables),
        acyclicity_constraints(inputs, &variables),
        utilization_constraints(inputs, &variables),
    ]
    .into_iter()
    .flatten()
    .collect();
    // Scaled so the largest cost is 1,000: HiGHS's tolerances are absolute.
    let largest = inputs
        .cost_per_second
        .iter()
        .flatten()
        .fold(0.0_f64, |largest, &cost| largest.max(cost));
    let scale = if largest > 0.0 && largest.is_finite() {
        1_000.0 / largest
    } else {
        1.0
    };
    let objective: Expression = inputs
        .cost_per_second
        .iter()
        .zip(&variables.x)
        .filter_map(|(cost, &x)| cost.map(|cost| cost * scale * x))
        .sum();
    let variable_count = problem.len();
    // HiGHS's columns are the variables in the order they were added.
    let column_of: HashMap<Variable, usize> = problem
        .iter_variables_with_def()
        .enumerate()
        .map(|(column, (variable, _))| (variable, column))
        .collect();
    let constraint_count = constraints.len();
    let mut model = problem
        .minimise(objective.clone())
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
    let solver_ms = started.elapsed().as_millis() as u64;
    use highs::{HighsModelStatus, HighsSolutionStatus};
    if solved.status() == HighsModelStatus::ModelEmpty {
        // No constraint: no query reads an operator (literal-only scalar
        // roots), so nothing is selected.
        return Ok(SolvedSelection {
            selected_dag: decode(inputs, &variables, |_| false),
            objective_per_second: 0.0,
            lower_bound_per_second: 0.0,
            variable_count,
            constraint_count,
            construction_ms,
            solver_ms,
        });
    }
    match solved.status() {
        HighsModelStatus::Optimal => {}
        HighsModelStatus::Infeasible | HighsModelStatus::UnboundedOrInfeasible => {
            return Err(SelectionMilpError::Infeasible)
        }
        other => return Err(SelectionMilpError::Solver(format!("{other:?}"))),
    }
    if solved.primal_solution_status() != HighsSolutionStatus::Feasible {
        return Err(SelectionMilpError::Solver(
            "HiGHS reported an optimal status without a feasible solution".into(),
        ));
    }
    let lower_bound_per_second = solved
        .double_info_value(c"mip_dual_bound")
        .unwrap_or(f64::NAN)
        / scale;
    let columns = solved.get_solution().columns().to_vec();
    let chosen = |variable: Variable| columns[column_of[&variable]] > 0.5;
    let objective_per_second: f64 = inputs
        .cost_per_second
        .iter()
        .zip(&variables.x)
        .filter(|(_, &x)| chosen(x))
        .filter_map(|(cost, _)| *cost)
        .sum();
    let selected_dag = decode(inputs, &variables, chosen);
    Ok(SolvedSelection {
        selected_dag,
        objective_per_second,
        lower_bound_per_second,
        variable_count,
        constraint_count,
        construction_ms,
        solver_ms,
    })
}

/// $\{x = 1\}$, $\{y = 1\}$ and $\{z = 1\}$ as a [`DagOfSelectedCandidates`]
/// (`decode.rs` in the formulation): one node per selected candidate, in
/// candidate order; each selected edge between their nodes; each output
/// reference's node the candidate its query's $z$ selects in its group.
fn decode(
    inputs: &SelectionInputs<'_>,
    variables: &SelectionVariables,
    chosen: impl Fn(Variable) -> bool,
) -> DagOfSelectedCandidates {
    let candidate_of_node: Vec<PhysicalCandidateId> = (0..variables.x.len())
        .filter(|&v| chosen(variables.x[v]))
        .map(PhysicalCandidateId)
        .collect();
    let node_of: HashMap<PhysicalCandidateId, SelectedNodeId> = candidate_of_node
        .iter()
        .enumerate()
        .map(|(node, &candidate)| (candidate, SelectedNodeId(node)))
        .collect();
    let edges = inputs.edges();
    let mut selected_edges: BTreeSet<(usize, usize, usize)> = BTreeSet::new();
    for (edge, &y) in &variables.y {
        if chosen(y) {
            let edge = &edges[edge.0];
            if let (Some(producer), Some(consumer)) = (
                node_of.get(&edge.producer_candidate_id),
                node_of.get(&edge.consumer_candidate_id),
            ) {
                selected_edges.insert((consumer.0, edge.input_index_of_consumer, producer.0));
            }
        }
    }
    let node_of_output_reference = output_references(&inputs.physical.query_roots)
        .into_iter()
        .map(|(query, group)| {
            inputs
                .candidates_of_group(group)
                .into_iter()
                .find(|v| {
                    variables
                        .z
                        .get(&(QueryId(query), *v))
                        .is_some_and(|&z| chosen(z))
                })
                .and_then(|v| node_of.get(&v).copied())
                .unwrap_or(SelectedNodeId(usize::MAX))
        })
        .collect();
    DagOfSelectedCandidates {
        candidate_of_node,
        edges: selected_edges
            .into_iter()
            .map(
                |(consumer, input_index_of_consumer, producer)| SelectedEdge {
                    producer: SelectedNodeId(producer),
                    consumer: SelectedNodeId(consumer),
                    input_index_of_consumer,
                },
            )
            .collect(),
        node_of_output_reference,
    }
}

#[allow(dead_code)] // #1207 PR 7.
/// The execution-context variables (section 5), which replace $x$ and the
/// unindexed $y$ of the baseline.
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

#[allow(dead_code)] // #1207 PR 8.
/// The accuracy variables of constraint (K) (section 4), all continuous.
struct AccuracyVariables {
    /// $E_c$, by [`PhysicalCandidateId`].
    error_bound: Vec<Variable>,
    /// $F_{c,i}$: the chosen producer's error bound on input $i$ of $c$.
    error_bound_of_input: HashMap<(PhysicalCandidateId, usize), Variable>,
    /// $D_c$, by [`PhysicalCandidateId`].
    failure_probability: Vec<Variable>,
    /// $H_{c,i}$: the chosen producer's failure probability on input $i$ of $c$.
    failure_probability_of_input: HashMap<(PhysicalCandidateId, usize), Variable>,
}

#[allow(dead_code)] // #1207 PR 7.
/// (R) $h_v \ge \operatorname{need}_{\kappa,v,c,i}\, y_{\kappa,v,c,i}$.
///
/// Assumptions (decided 2026-11-06): an edge's need is the time horizon
/// the queries it serves read, a constant per edge (a 5m range over 10 s
/// windows needs 30 windows), so (R) is linear; query latency is ignored
/// until the latency design: a result counts as read when its evaluation
/// starts.
fn retention_constraints(
    physical: &PhysicalASAPMemoCandidates,
    variables: &ExecutionContextVariables,
) -> Vec<Constraint> {
    let _ = (physical, variables);
    todo!("#1207 PR 7: constraint (R)")
}

#[allow(dead_code)] // #1207 PR 8.
/// (K): $E_c$ and $D_c$ per candidate, big-M on selected edges, and the
/// root checks $E_v \le \varepsilon_q + M_q(1 - z_{q,v})$,
/// $D_v \le \delta_q + (1 - z_{q,v})$.
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

//! Stage 3's selection MILP, named after the formulation (concepts doc,
//! sections 3–5).
//!
//! | Family | Variables | Constraints |
//! |---|---|---|
//! | baseline (section 3) | $z_{q,v}$, $t_v$ ([`SelectionVariables`]) | (A) [`input_satisfaction_constraints`], (B) [`producer_selection_constraints`], (C)/(D) [`query_coverage_constraints`], (E) [`acyclicity_constraints`], (F) [`utilization_constraints`]; (G), (H): $y$ exists only on admissible edges |
//! | execution contexts (section 5) | $x^{\mathrm{mat}}_v$, $x_{\kappa,v}$, $y_{\kappa,u,c,i}$, $h_v$ ([`ExecutionContextVariables`]) | (R) [`retention_constraints`] |
//! | accuracy (section 4) | $E_c$, $F_{c,i}$, $D_c$, $H_{c,i}$ ([`AccuracyVariables`]) | (K) [`accuracy_constraints`] |
//!
//! The execution-context variables replace the baseline's $x_v$ and
//! unindexed $y_{u,c,i}$: a candidate with $\ell$ = None or `OneEvaluation`
//! runs once per context $\kappa$ that selects it ($x_{\kappa,v}$), an
//! `AcrossEvaluations` candidate once ($x^{\mathrm{mat}}_v$), and every edge
//! is selected per context of its consumer ($y_{\kappa,u,c,i}$), an
//! `AcrossEvaluations` consumer's being its computation context
//! ([`crate::milp::execution_contexts`]). Constraints (A)–(F) hold per
//! execution ([`Execution`]): $x$ below is $x_{\kappa,v}$ or
//! $x^{\mathrm{mat}}_v$, whichever runs $v$ for a consumer in $\kappa$.
//!
//! [`solve_selection`] builds and solves it: $x$, $y$ and $z$ binary, $t$
//! continuous in $[0, |\text{executions}| - 1]$ on cyclic components only,
//! $h_v \ge 0$ continuous, and the objective (section 5)
//! $$\min \sum_v W^{\mathrm{mat}}_v x^{\mathrm{mat}}_v + \sum_{\kappa,v}
//! \rho_\kappa c_{\kappa,v} x_{\kappa,v} + \sum_v \pi_v h_v$$
//! with the costs from [`crate::execution_cost`]. The accuracy family is
//! `todo!()` until #1207 PR 8; nothing calls it yet, hence its `dead_code`
//! allowances.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::time::Instant;

use good_lp::solvers::highs::highs;
use good_lp::{
    constraint, variable, Constraint, Expression, ProblemVariables, SolverModel, Variable,
};

use asap_types::stages::{
    output_references, DagOfSelectedCandidates, ExecutionContext, ExecutionContextId, MemoGroupId,
    PhysicalASAPMemoCandidates, PhysicalCandidateId, PhysicalEdge, PhysicalEdgeId, SelectedEdge,
    SelectedNodeId,
};

use crate::execution_cost::ExecutionCosts;
use crate::milp::execution_contexts::{Execution, ExecutionContexts, TumblingWindowChainGeometry};

/// A query, by index in the workload.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct QueryId(pub usize);

/// The baseline variables (section 3) the execution contexts keep.
pub(crate) struct SelectionVariables {
    /// $z_{q,v}$, for $v \in G_q$ whose execution in $q$'s context can
    /// give its output.
    z: HashMap<(QueryId, PhysicalCandidateId), Variable>,
    /// $t$, per execution on a cyclic component of the selectable edges.
    t: HashMap<Execution, Variable>,
}

/// The execution-context variables (section 5), which replace $x$ and the
/// unindexed $y$ of the baseline.
pub(crate) struct ExecutionContextVariables {
    /// $x^{\mathrm{mat}}_v$, for an `AcrossEvaluations` candidate.
    x_materialized: HashMap<PhysicalCandidateId, Variable>,
    /// $x_{\kappa,v}$, for a candidate with $\ell$ = None or `OneEvaluation`.
    x_in_execution_context: HashMap<(ExecutionContextId, PhysicalCandidateId), Variable>,
    /// $y_{\kappa,u,c,i}$, indexed by the consumer's context.
    y_in_execution_context: HashMap<(ExecutionContextId, PhysicalEdgeId), Variable>,
    /// $h_v$, for an `AcrossEvaluations` producer: the tumbling windows it
    /// retains.
    retention_amount: HashMap<PhysicalCandidateId, Variable>,
}

impl ExecutionContextVariables {
    /// $x$ of `execution`, when it can be costed.
    fn x(&self, execution: Execution) -> Option<Variable> {
        match execution {
            Execution::Materialized(v) => self.x_materialized.get(&v).copied(),
            Execution::InContext(context, v) => {
                self.x_in_execution_context.get(&(context, v)).copied()
            }
        }
    }
}

/// What the MILP is built over: Stage 2's candidates and admissible edges,
/// the workload's execution contexts with each execution and edge they
/// allow, the objective's costs (an execution without one is left out),
/// and indexes built once ([`Self::new`]).
pub(crate) struct SelectionInputs<'a> {
    pub(crate) physical: &'a PhysicalASAPMemoCandidates,
    pub(crate) contexts: &'a ExecutionContexts,
    pub(crate) costs: &'a ExecutionCosts,
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
        contexts: &'a ExecutionContexts,
        costs: &'a ExecutionCosts,
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
            contexts,
            costs,
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

    /// The producer and consumer executions of edge `edge` selected in the
    /// consumer's context `context`.
    fn executions_of_edge(
        &self,
        context: ExecutionContextId,
        edge: PhysicalEdgeId,
    ) -> (Execution, Execution) {
        let edge = &self.edges[edge.0];
        (
            ExecutionContexts::execution_in(self.physical, context, edge.producer_candidate_id),
            ExecutionContexts::execution_in(self.physical, context, edge.consumer_candidate_id),
        )
    }

    /// The objective's cost per second of `execution`
    /// ($\rho_\kappa c_{\kappa,v}$ or $W^{\mathrm{mat}}_v$), when it can be
    /// costed.
    fn cost_per_second(&self, execution: Execution) -> Option<f64> {
        self.costs.cost_per_second.get(&execution).copied()
    }

    /// $\operatorname{need}_{\kappa,v,c,i}$ of `AcrossEvaluations` producer
    /// `v` for a consumer in `context`, in tumbling windows: what the
    /// queries evaluated in `context` read of it, their range-evaluation
    /// span included ([`TumblingWindowChainGeometry::retention_need_windows`]).
    /// Query latency is ignored (decided 2026-11-06): a result counts as
    /// read when its evaluation starts.
    fn retention_need_windows(&self, context: ExecutionContextId, v: PhysicalCandidateId) -> u64 {
        let span_ms = match &self.contexts.contexts()[context.0] {
            ExecutionContext::Evaluation {
                range_evaluation: Some(range),
                ..
            } => range.span_ms,
            _ => 0,
        };
        TumblingWindowChainGeometry::of(&self.physical.logical_candidates.memo, v, self.physical)
            .map_or(0, |chain| chain.retention_need_windows(span_ms))
    }

    /// The physical candidates of memo group `group`, ascending.
    fn candidates_of_group(&self, group: MemoGroupId) -> &[PhysicalCandidateId] {
        self.candidates_of_group
            .get(&group)
            .map_or(&[], Vec::as_slice)
    }

    /// Each query's output groups $G_q$, each pair once, in output-reference
    /// order: a scalar query reads one per operator reference.
    fn query_output_groups(&self) -> &[(QueryId, MemoGroupId)] {
        &self.query_output_groups
    }

    /// The execution of `v` that gives query `query`'s output, when it can
    /// be costed.
    fn output_execution(&self, query: QueryId, v: PhysicalCandidateId) -> Option<Execution> {
        self.contexts
            .output_execution(self.physical, query.0, v)
            .filter(|&execution| self.cost_per_second(execution).is_some())
    }
}

/// The MILP's variables over `inputs` (constraint (I), domains): $x$
/// binary per costed execution; $y_{\kappa,u,c,i}$ binary per edge the
/// contexts allow ((G) and (H): admissible; an `AcrossEvaluations`
/// producer delivers on $\kappa$) whose producer and consumer executions
/// are costed; $z_{q,v}$ binary per costed output execution; $h_v \ge 0$
/// per costed `AcrossEvaluations` candidate; $t \in [0, n - 1]$ continuous
/// per execution on a cyclic component of the $y$ edges, $n$ the executions.
fn selection_variables(
    inputs: &SelectionInputs<'_>,
    problem: &mut ProblemVariables,
) -> (SelectionVariables, ExecutionContextVariables) {
    let mut x_materialized = HashMap::new();
    let mut x_in_execution_context = HashMap::new();
    let mut retention_amount = HashMap::new();
    for &execution in inputs.contexts.executions() {
        if inputs.cost_per_second(execution).is_none() {
            continue;
        }
        match execution {
            Execution::Materialized(v) => {
                x_materialized.insert(v, problem.add(variable().binary()));
                retention_amount.insert(v, problem.add(variable().min(0)));
            }
            Execution::InContext(context, v) => {
                x_in_execution_context.insert((context, v), problem.add(variable().binary()));
            }
        }
    }
    let mut context_variables = ExecutionContextVariables {
        x_materialized,
        x_in_execution_context,
        y_in_execution_context: HashMap::new(),
        retention_amount,
    };
    for &(context, edge) in inputs.contexts.edges() {
        let (producer, consumer) = inputs.executions_of_edge(context, edge);
        if context_variables.x(producer).is_some() && context_variables.x(consumer).is_some() {
            context_variables
                .y_in_execution_context
                .insert((context, edge), problem.add(variable().binary()));
        }
    }
    let mut z = HashMap::new();
    for &(query, group) in inputs.query_output_groups() {
        for &v in inputs.candidates_of_group(group) {
            if inputs.output_execution(query, v).is_some() {
                z.entry((query, v))
                    .or_insert_with(|| problem.add(variable().binary()));
            }
        }
    }
    let executions: Vec<Execution> = inputs.contexts.executions().to_vec();
    let position: HashMap<Execution, usize> = executions
        .iter()
        .enumerate()
        .map(|(position, &execution)| (execution, position))
        .collect();
    let component = strongly_connected_components(
        executions.len(),
        sorted(&context_variables.y_in_execution_context)
            .into_iter()
            .map(|(&(context, edge), _)| {
                let (producer, consumer) = inputs.executions_of_edge(context, edge);
                (position[&producer], position[&consumer])
            }),
    );
    let rank_bound = executions.len().saturating_sub(1) as f64;
    let mut t = HashMap::new();
    for (&(context, edge), _) in sorted(&context_variables.y_in_execution_context) {
        let (producer, consumer) = inputs.executions_of_edge(context, edge);
        if component[position[&producer]] == component[position[&consumer]] {
            for execution in [producer, consumer] {
                t.entry(execution)
                    .or_insert_with(|| problem.add(variable().min(0).max(rank_bound)));
            }
        }
    }
    (SelectionVariables { z, t }, context_variables)
}

/// `map`'s entries in key order, so constraints come out in the same order
/// every run.
fn sorted<K: Ord, V>(map: &HashMap<K, V>) -> Vec<(&K, &V)> {
    let mut entries: Vec<(&K, &V)> = map.iter().collect();
    entries.sort_by(|a, b| a.0.cmp(b.0));
    entries
}

/// (A) $\sum_{u \in P_{c,i}} y_{\kappa,u,c,i} = x_c$: every input of a
/// selected consumer execution has exactly one selected producer, read in
/// the consumer's context, and an unselected one none.
fn input_satisfaction_constraints(
    inputs: &SelectionInputs<'_>,
    variables: &ExecutionContextVariables,
) -> Vec<Constraint> {
    let mut producers: BTreeMap<(Execution, usize), Expression> = BTreeMap::new();
    for (&(context, edge), &y) in sorted(&variables.y_in_execution_context) {
        let (_, consumer) = inputs.executions_of_edge(context, edge);
        *producers
            .entry((consumer, inputs.edges()[edge.0].input_index_of_consumer))
            .or_default() += y;
    }
    let mut constraints = Vec::new();
    for &consumer in inputs.contexts.executions() {
        let Some(x) = variables.x(consumer) else {
            continue;
        };
        for input in 0..inputs.input_count(consumer.candidate()) {
            let chosen = producers.remove(&(consumer, input)).unwrap_or_default();
            constraints.push(constraint!(chosen == x));
        }
    }
    constraints
}

/// (B) $y_{\kappa,u,c,i} \le x_{\kappa,u}$ for a None or `OneEvaluation`
/// producer (read only within $\kappa$), $y_{\kappa,u,c,i} \le
/// x^{\mathrm{mat}}_u$ for an `AcrossEvaluations` one: a chosen producer is
/// selected.
fn producer_selection_constraints(
    inputs: &SelectionInputs<'_>,
    variables: &ExecutionContextVariables,
) -> Vec<Constraint> {
    sorted(&variables.y_in_execution_context)
        .into_iter()
        .map(|(&(context, edge), &y)| {
            let (producer, _) = inputs.executions_of_edge(context, edge);
            let x = variables
                .x(producer)
                .expect("y only between costed executions");
            constraint!(y <= x)
        })
        .collect()
}

/// (C) $\sum_{v \in G_q} z_{q,v} = 1$, for each output group of each query;
/// (D) $z_{q,v} \le x$ of $v$'s execution in $q$'s context.
fn query_coverage_constraints(
    inputs: &SelectionInputs<'_>,
    selection: &SelectionVariables,
    variables: &ExecutionContextVariables,
) -> Vec<Constraint> {
    let mut constraints = Vec::new();
    for &(query, group) in inputs.query_output_groups() {
        let outputs: Vec<(PhysicalCandidateId, Variable)> = inputs
            .candidates_of_group(group)
            .iter()
            .filter_map(|&v| selection.z.get(&(query, v)).map(|&z| (v, z)))
            .collect();
        let covered: Expression = outputs.iter().map(|&(_, z)| z).sum();
        constraints.push(constraint!(covered == 1));
        for (v, z) in outputs {
            let execution = inputs
                .output_execution(query, v)
                .expect("z only on costed output executions");
            let x = variables.x(execution).expect("costed");
            constraints.push(constraint!(z <= x));
        }
    }
    constraints
}

/// (E) $t_u + 1 \le t_c + n(1 - y_{\kappa,u,c,i})$ over executions ($n$
/// the executions): no selected cycle. Only edges inside a strongly
/// connected component of the selectable edges can close a cycle, so (E)
/// is written for those only (an execution reading itself included, which
/// (E) forbids).
fn acyclicity_constraints(
    inputs: &SelectionInputs<'_>,
    selection: &SelectionVariables,
    variables: &ExecutionContextVariables,
) -> Vec<Constraint> {
    let execution_count = inputs.contexts.executions().len() as f64;
    sorted(&variables.y_in_execution_context)
        .into_iter()
        .filter_map(|(&(context, edge), &y)| {
            let (producer, consumer) = inputs.executions_of_edge(context, edge);
            let t_u = *selection.t.get(&producer)?;
            let t_c = *selection.t.get(&consumer)?;
            Some(constraint!(t_u + 1 <= t_c + execution_count * (1 - y)))
        })
        .collect()
}

/// (F) $x \le \sum y + \sum_q z_{q,v}$ per execution: a selected execution
/// supplies a selected input (in its context, or, for an
/// `AcrossEvaluations` candidate, in any context) or a query output.
fn utilization_constraints(
    inputs: &SelectionInputs<'_>,
    selection: &SelectionVariables,
    variables: &ExecutionContextVariables,
) -> Vec<Constraint> {
    let mut used: BTreeMap<Execution, Expression> = BTreeMap::new();
    for (&(context, edge), &y) in sorted(&variables.y_in_execution_context) {
        let (producer, _) = inputs.executions_of_edge(context, edge);
        *used.entry(producer).or_default() += y;
    }
    for (&(query, v), &z) in sorted(&selection.z) {
        let execution = inputs
            .output_execution(query, v)
            .expect("z only on costed output executions");
        *used.entry(execution).or_default() += z;
    }
    inputs
        .contexts
        .executions()
        .iter()
        .filter_map(|&execution| {
            let x = variables.x(execution)?;
            let used = used.remove(&execution).unwrap_or_default();
            Some(constraint!(x <= used))
        })
        .collect()
}

/// (R) $h_v \ge \operatorname{need}_{\kappa,v,c,i}\, y_{\kappa,v,c,i}$, per
/// edge out of an `AcrossEvaluations` producer $v$, and
/// $h_v \ge \operatorname{need}_{\kappa_q,v}\, z_{q,v}$ for a query whose
/// output $v$ is: it retains as many tumbling windows as the most
/// demanding selected consumer reads
/// ([`SelectionInputs::retention_need_windows`]).
///
/// Assumptions (decided 2026-11-06): an edge's need is the time horizon
/// the queries it serves read, a constant per edge (a 5m range over 10 s
/// windows needs 30 windows), so (R) is linear; query latency is ignored
/// until the latency design: a result counts as read when its evaluation
/// starts.
fn retention_constraints(
    inputs: &SelectionInputs<'_>,
    selection: &SelectionVariables,
    variables: &ExecutionContextVariables,
) -> Vec<Constraint> {
    let edges = sorted(&variables.y_in_execution_context)
        .into_iter()
        .filter_map(|(&(context, edge), &y)| {
            let Execution::Materialized(v) = inputs.executions_of_edge(context, edge).0 else {
                return None;
            };
            let h = variables.retention_amount[&v];
            let need = inputs.retention_need_windows(context, v) as f64;
            Some(constraint!(h >= need * y))
        });
    // A query reading `v` itself is its consumer too, in its own context.
    let outputs = sorted(&selection.z)
        .into_iter()
        .filter_map(|(&(query, v), &z)| {
            let Some(Execution::Materialized(_)) = inputs.output_execution(query, v) else {
                return None;
            };
            let context = inputs.contexts.context_of_query(query.0)?;
            let h = variables.retention_amount[&v];
            let need = inputs.retention_need_windows(context, v) as f64;
            Some(constraint!(h >= need * z))
        });
    edges.chain(outputs).collect()
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

/// The solved selection: the selected DAG and what the solve took.
#[derive(Debug, Clone)]
pub(crate) struct SolvedSelection {
    pub(crate) selected_dag: DagOfSelectedCandidates,
    /// The execution behind each node of `selected_dag`, by
    /// [`SelectedNodeId`].
    pub(crate) execution_of_node: Vec<Execution>,
    /// The objective at the optimum, per second: the selected executions'
    /// $W^{\mathrm{mat}}_v$ and $\rho_\kappa c_{\kappa,v}$, plus $\pi_v h_v$
    /// with each $h_v$ the largest need of its selected edges.
    pub(crate) objective_per_second: f64,
    /// The solver's lower bound on it; NaN when it reports none.
    pub(crate) lower_bound_per_second: f64,
    pub(crate) variable_count: usize,
    pub(crate) constraint_count: usize,
    pub(crate) construction_ms: u64,
    pub(crate) solver_ms: u64,
}

/// Why the MILP selected nothing.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum SelectionMilpError {
    /// No assignment meets the constraints: some query output has no
    /// execution whose inputs all have costable producers it can read.
    Infeasible,
    /// HiGHS failed.
    Solver(String),
}

/// Build the MILP over `inputs` ((A)–(F) and (R) per execution, the
/// objective of section 5), solve it to proven optimality with HiGHS, and
/// decode $\{x = 1\}$, $\{y = 1\}$ and $\{z = 1\}$ into a
/// [`DagOfSelectedCandidates`]: one node per selected execution, in
/// execution order, so a candidate run in two contexts is two nodes; one
/// edge per selected $y$; each output reference's node the execution of
/// the candidate its query's $z$ selects.
pub(crate) fn solve_selection(
    inputs: &SelectionInputs<'_>,
) -> Result<SolvedSelection, SelectionMilpError> {
    // (C) cannot hold for a query output group without a costable output
    // execution.
    if inputs.query_output_groups().iter().any(|&(query, group)| {
        inputs
            .candidates_of_group(group)
            .iter()
            .all(|&v| inputs.output_execution(query, v).is_none())
    }) {
        return Err(SelectionMilpError::Infeasible);
    }
    let started = Instant::now();
    let mut problem = ProblemVariables::new();
    let (selection, variables) = selection_variables(inputs, &mut problem);
    let constraints: Vec<Constraint> = [
        input_satisfaction_constraints(inputs, &variables),
        producer_selection_constraints(inputs, &variables),
        query_coverage_constraints(inputs, &selection, &variables),
        acyclicity_constraints(inputs, &selection, &variables),
        utilization_constraints(inputs, &selection, &variables),
        retention_constraints(inputs, &selection, &variables),
    ]
    .into_iter()
    .flatten()
    .collect();
    // The objective's coefficients, per second, in execution order.
    let mut terms: Vec<(f64, Variable)> = Vec::new();
    for &execution in inputs.contexts.executions() {
        if let (Some(cost), Some(x)) = (inputs.cost_per_second(execution), variables.x(execution)) {
            terms.push((cost, x));
        }
        if let Execution::Materialized(v) = execution {
            if let (Some(&cost), Some(&h)) = (
                inputs.costs.retention_cost_per_window_per_second.get(&v),
                variables.retention_amount.get(&v),
            ) {
                terms.push((cost, h));
            }
        }
    }
    // Scaled so the largest coefficient is 1,000: HiGHS's tolerances are
    // absolute.
    let largest = terms
        .iter()
        .fold(0.0_f64, |largest, &(cost, _)| largest.max(cost));
    let scale = if largest > 0.0 && largest.is_finite() {
        1_000.0 / largest
    } else {
        1.0
    };
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
        let (selected_dag, execution_of_node) = decode(inputs, &selection, &variables, |_| false);
        return Ok(SolvedSelection {
            selected_dag,
            execution_of_node,
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
    let (selected_dag, execution_of_node) = decode(inputs, &selection, &variables, chosen);
    let objective_per_second = objective_of(inputs, &selected_dag, &execution_of_node);
    Ok(SolvedSelection {
        selected_dag,
        execution_of_node,
        objective_per_second,
        lower_bound_per_second,
        variable_count,
        constraint_count,
        construction_ms,
        solver_ms,
    })
}

/// The objective of a decoded selection, per second: each selected
/// execution's cost ($W^{\mathrm{mat}}_v$ or $\rho_\kappa c_{\kappa,v}$)
/// and $\pi_v h_v$ of each selected `AcrossEvaluations` candidate, $h_v$
/// the largest need of its selected edges and of the queries it gives the
/// output of, which minimizes $\pi_v h_v$ under (R).
pub(crate) fn objective_of(
    inputs: &SelectionInputs<'_>,
    selected_dag: &DagOfSelectedCandidates,
    execution_of_node: &[Execution],
) -> f64 {
    let mut retained: BTreeMap<PhysicalCandidateId, u64> = BTreeMap::new();
    for edge in &selected_dag.edges {
        if let (Execution::Materialized(v), Some(context)) = (
            execution_of_node[edge.producer.0],
            inputs
                .contexts
                .input_context(execution_of_node[edge.consumer.0]),
        ) {
            let need = inputs.retention_need_windows(context, v);
            let windows = retained.entry(v).or_default();
            *windows = (*windows).max(need);
        }
    }
    for ((query, _), node) in output_references(&inputs.physical.query_roots)
        .into_iter()
        .zip(&selected_dag.node_of_output_reference)
    {
        if let (Some(Execution::Materialized(v)), Some(context)) = (
            execution_of_node.get(node.0).copied(),
            inputs.contexts.context_of_query(query),
        ) {
            let need = inputs.retention_need_windows(context, v);
            let windows = retained.entry(v).or_default();
            *windows = (*windows).max(need);
        }
    }
    let executions: f64 = execution_of_node
        .iter()
        .filter_map(|&execution| inputs.cost_per_second(execution))
        .sum();
    let retention: f64 = retained
        .into_iter()
        .map(|(v, windows)| {
            inputs
                .costs
                .retention_cost_per_window_per_second
                .get(&v)
                .map_or(0.0, |cost| cost * windows as f64)
        })
        .sum();
    executions + retention
}

/// $\{x = 1\}$, $\{y = 1\}$ and $\{z = 1\}$ as a [`DagOfSelectedCandidates`]
/// (`decode.rs` in the formulation), with the execution behind each node:
/// one node per selected execution, in execution order; each selected edge
/// between the nodes of its producer and consumer executions; each output
/// reference's node the execution of the candidate its query's $z$ selects
/// in its group.
fn decode(
    inputs: &SelectionInputs<'_>,
    selection: &SelectionVariables,
    variables: &ExecutionContextVariables,
    chosen: impl Fn(Variable) -> bool,
) -> (DagOfSelectedCandidates, Vec<Execution>) {
    let execution_of_node: Vec<Execution> = inputs
        .contexts
        .executions()
        .iter()
        .copied()
        .filter(|&execution| variables.x(execution).is_some_and(&chosen))
        .collect();
    let node_of: HashMap<Execution, SelectedNodeId> = execution_of_node
        .iter()
        .enumerate()
        .map(|(node, &execution)| (execution, SelectedNodeId(node)))
        .collect();
    let mut selected_edges: BTreeSet<(usize, usize, usize)> = BTreeSet::new();
    for (&(context, edge), &y) in &variables.y_in_execution_context {
        if chosen(y) {
            let (producer, consumer) = inputs.executions_of_edge(context, edge);
            if let (Some(producer), Some(consumer)) =
                (node_of.get(&producer), node_of.get(&consumer))
            {
                selected_edges.insert((
                    consumer.0,
                    inputs.edges()[edge.0].input_index_of_consumer,
                    producer.0,
                ));
            }
        }
    }
    let node_of_output_reference = output_references(&inputs.physical.query_roots)
        .into_iter()
        .map(|(query, group)| {
            let query = QueryId(query);
            inputs
                .candidates_of_group(group)
                .iter()
                .find(|v| selection.z.get(&(query, **v)).is_some_and(|&z| chosen(z)))
                .and_then(|&v| inputs.output_execution(query, v))
                .and_then(|execution| node_of.get(&execution).copied())
                .unwrap_or(SelectedNodeId(usize::MAX))
        })
        .collect();
    let selected_dag = DagOfSelectedCandidates {
        candidate_of_node: execution_of_node
            .iter()
            .map(|execution| execution.candidate())
            .collect(),
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
    };
    (selected_dag, execution_of_node)
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

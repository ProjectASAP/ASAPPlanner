//! Stage 3's selection MILP, named after the formulation
//! (`docs/design_docs/concepts/memo-groups-properties-requirements.md`,
//! sections 3–5).
//!
//! | Family | Variables | Constraints |
//! |---|---|---|
//! | baseline (section 3) over execution contexts (section 5) | $x_{\kappa,v}$ (with $x^{\mathrm{mat}}_v = x_{\mathrm{Materialized}(v),v}$), $y_{\kappa,u,c,i}$, $z_{q,v}$, $t$, $r_s$ ([`SelectionVariables`]) | (A) [`input_satisfaction_constraints`], (B) [`producer_selection_constraints`], (C)/(D) [`query_coverage_constraints`], (E) [`acyclicity_constraints`], (F) and one consumer per context of a query-time result that is not materialized [`use_constraints`], each source's raw data on disk once [`raw_data_disk_constraints`]; (G), (H): $y$ exists only on admissible edges where the producer delivers on the consumer's context |
//! | retention (section 5) | $h_v$ | (R) [`super::execution_contexts::retention_constraints`] |
//! | accuracy (section 4) | $E_{c,m}$, $F_{c,i,m}$, $D_c$, $H_{c,i}$ (`AccuracyVariables`) | (K) `accuracy_constraints` |
//!
//! [`solve_selection`] solves it without (K), whose variables and
//! constraint are in [`super::accuracy`], `todo!()` until it is added. The
//! strongly connected components (E) needs are in [`super::graph`]; the
//! bound that leaves out executions before scaling in
//! [`super::upper_bound`].

use std::collections::{BTreeMap, HashMap};
use std::time::Instant;

use good_lp::solvers::highs::highs;
use good_lp::{
    constraint, variable, Constraint, Expression, ProblemVariables, SolverModel, Variable,
};

use asap_types::ir::operator::Source;
use asap_types::ir::properties::ExecutionTiming;
use asap_types::stages::{
    Lifetime, PhysicalASAPMemoCandidates, PhysicalCandidateId, PhysicalEdgeId,
};
use asap_types::workload::QueryWorkloadEntry;

use super::execution_contexts::{least_retention_ms, retention_constraints, retention_variables};
use super::graph::Components;
use super::upper_bound::dearer_than_a_feasible_plan;
use crate::cost::ExecutionCosts;
use crate::selectable::{Execution, ExecutionContexts};

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
    /// $y_{\kappa,u,c,i}$, by position in `ExecutionContexts::edges`, on
    /// the edges costed between executions kept, in edge order.
    y: BTreeMap<usize, Variable>,
    /// $z_{q,v}$, by the execution answering $q$, in key order.
    z: BTreeMap<(QueryId, Execution), Variable>,
    /// $t$, by position in `ExecutionContexts::executions`: only for
    /// executions on a cycle of selectable edges (a strongly connected
    /// component); `None` elsewhere.
    t: Vec<Option<Variable>>,
    /// $h_v$ in seconds, for a candidate with a retention cost.
    h: BTreeMap<PhysicalCandidateId, Variable>,
    /// Per raw-data source a query-time scan keeps on disk at a cost: the
    /// cost per second of keeping it, at least each selected scan's
    /// ([`raw_data_disk_constraints`]).
    raw_data_on_disk: BTreeMap<Source, Variable>,
}

/// $x$, $y$, $z$, $t$, $h$ and $r$ over `inputs`, added to `problem`: $x$
/// fixed at 0 for an execution that could not be costed or is `left_out`,
/// and no $y$ on an edge of one.
fn selection_variables(
    inputs: &SelectionInputs<'_>,
    left_out: &[bool],
    components: &Components,
    problem: &mut ProblemVariables,
) -> SelectionVariables {
    let contexts = inputs.contexts;
    let x = inputs
        .costs
        .per_execution
        .iter()
        .zip(left_out)
        .map(|(cost, &left_out)| match cost.is_ok() && !left_out {
            true => problem.add(variable().binary()),
            false => problem.add(variable().min(0).max(0)),
        })
        .collect();
    let y = contexts
        .edges()
        .iter()
        .zip(&inputs.costs.per_edge)
        .enumerate()
        .filter(|(_, (&(consumer, _, producer), cost))| {
            cost.is_some()
                && !left_out[contexts.index_of(consumer)]
                && !left_out[contexts.index_of(producer)]
        })
        .map(|(position, _)| (position, problem.add(variable().binary())))
        .collect();
    let mut z = BTreeMap::new();
    for (query, executions) in inputs.query_outputs {
        for &execution in executions {
            z.entry((*query, execution))
                .or_insert_with(|| problem.add(variable().binary()));
        }
    }
    // t only inside a strongly connected component of the selectable
    // edges, bounded by its size.
    let t = components
        .size
        .iter()
        .map(|&size| (size > 1).then(|| problem.add(variable().min(0).max((size - 1) as f64))))
        .collect();
    let h = retention_variables(inputs, problem);
    let mut raw_data_on_disk = BTreeMap::new();
    for source in inputs.costs.raw_data_source.values() {
        raw_data_on_disk
            .entry(source.clone())
            .or_insert_with(|| problem.add(variable().min(0)));
    }
    SelectionVariables {
        x,
        y,
        z,
        t,
        h,
        raw_data_on_disk,
    }
}

/// (A) $\sum_{u \in P_{c,i}} y_{\kappa,u,c,i} = x_{\kappa,c}$, per execution
/// of $c$ and input $i$.
fn input_satisfaction_constraints(
    inputs: &SelectionInputs<'_>,
    variables: &SelectionVariables,
) -> Vec<Constraint> {
    let physical = inputs.physical;
    let contexts = inputs.contexts;
    let mut producers: HashMap<(usize, usize), Expression> = HashMap::new();
    for (&position, &y) in &variables.y {
        let (consumer, edge, _) = contexts.edges()[position];
        *producers
            .entry((
                contexts.index_of(consumer),
                physical.edge(edge).input_index_of_consumer,
            ))
            .or_default() += y;
    }
    let mut constraints = Vec::new();
    for (index, execution) in contexts.executions().iter().enumerate() {
        for i in 0..physical.input_count(execution.candidate) {
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
    let contexts = inputs.contexts;
    variables
        .y
        .iter()
        .map(|(&position, &y)| {
            let (_, _, producer) = contexts.edges()[position];
            constraint!(y <= variables.x[contexts.index_of(producer)])
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
    for (&(_, execution), &z) in &variables.z {
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
    components: &Components,
) -> Vec<Constraint> {
    let contexts = inputs.contexts;
    variables
        .y
        .iter()
        .filter_map(|(&position, &y)| {
            let (consumer, _, producer) = contexts.edges()[position];
            let (u, c) = (contexts.index_of(producer), contexts.index_of(consumer));
            if components.of[u] != components.of[c] {
                return None;
            }
            let (t_u, t_c) = (variables.t[u]?, variables.t[c]?);
            let n = components.size[u] as f64;
            Some(constraint!(t_u + 1 <= t_c + n * (1 - y)))
        })
        .collect()
}

/// (F) $x_{\kappa,v} \le \sum y + \sum_q z_{q,v}$ over the edges and query
/// outputs reading that execution; with it, a query-time result that is not
/// materialized ($\ell$ = None) has at most one consumer per context,
/// $\sum y + \sum_q z_{q,v} \le x_{\kappa,v}$, so for such an execution the
/// two are one row, $\sum y + \sum_q z_{q,v} = x_{\kappa,v}$. Consumers in
/// one context that share a result read a `OneEvaluation` or a materialized
/// producer (section 5). An ingestion-time producer is exempt from the
/// one-consumer rule: there is no `OneEvaluation` at ingestion time, and the
/// consumers of one ingestion run share its rows.
fn use_constraints(
    inputs: &SelectionInputs<'_>,
    variables: &SelectionVariables,
) -> Vec<Constraint> {
    uses_of(inputs, variables)
        .into_iter()
        .enumerate()
        .map(|(index, used)| {
            let v = inputs.contexts.executions()[index].candidate;
            let properties = inputs.physical.physical_candidates[v.0].physical_properties;
            match properties.lifetime() == Lifetime::None
                && properties.timing() == ExecutionTiming::QueryTime
            {
                true => constraint!(used == variables.x[index]),
                false => constraint!(variables.x[index] <= used),
            }
        })
        .collect()
}

/// The raw data of each source is kept on disk once, for the selected scan
/// that keeps the most: $r_s \ge \pi_v h_v$ for each query-time `Scan` $v$
/// of source $s$ with $h_v$, and $r_s \ge d_v x_{\kappa,v}$ for each
/// execution of one without, $d_v$ its cost per second of the raw data it
/// keeps retaining everything.
fn raw_data_disk_constraints(
    inputs: &SelectionInputs<'_>,
    variables: &SelectionVariables,
) -> Vec<Constraint> {
    let costs = inputs.costs;
    let mut scans: Vec<(&PhysicalCandidateId, &Source)> = costs.raw_data_source.iter().collect();
    scans.sort_unstable_by_key(|(v, _)| **v);
    let mut constraints = Vec::new();
    for (v, source) in scans {
        let r = variables.raw_data_on_disk[source];
        if let Some(&h) = variables.h.get(v) {
            let per_second_retained = costs.retention_cost_per_second[v] * 1_000.0;
            constraints.push(constraint!(r >= per_second_retained * h));
        } else if let Some(&d) = costs.unbounded_raw_data_on_disk_cost_per_second.get(v) {
            for (index, execution) in inputs.contexts.executions().iter().enumerate() {
                if execution.candidate == *v {
                    constraints.push(constraint!(r >= d * variables.x[index]));
                }
            }
        }
    }
    constraints
}

/// Per execution, its selected edges to consumers and query outputs:
/// $\sum y + \sum_q z_{q,v}$.
fn uses_of(inputs: &SelectionInputs<'_>, variables: &SelectionVariables) -> Vec<Expression> {
    let contexts = inputs.contexts;
    let mut uses: Vec<Expression> = vec![Expression::default(); variables.x.len()];
    for (&position, &y) in &variables.y {
        let (_, _, producer) = contexts.edges()[position];
        uses[contexts.index_of(producer)] += y;
    }
    for (&(_, execution), &z) in &variables.z {
        uses[contexts.index_of(execution)] += z;
    }
    uses
}

/// The MILP's optimum: the executions with $x = 1$, the edges with
/// $y = 1$, the execution answering each query output ($z = 1$) and the
/// retention of each selected candidate with $h_v$, and the MILP's size
/// and time.
#[derive(Debug, Clone)]
pub(crate) struct SolvedSelection {
    /// The selected executions, ascending.
    pub(crate) executions: Vec<Execution>,
    /// The selected edges as (consumer, edge, producer), ascending.
    pub(crate) edges: Vec<(Execution, PhysicalEdgeId, Execution)>,
    /// The execution answering each of `SelectionInputs::query_outputs`, in
    /// its order.
    pub(crate) outputs: Vec<Execution>,
    /// $h_v$ in ms of each selected candidate with $h_v$: the least (R)
    /// allows for the selection.
    pub(crate) retention_ms: HashMap<PhysicalCandidateId, u64>,
    /// The optimum, unscaled: the selected plan's cost per second.
    pub(crate) objective_cost_per_second: f64,
    /// The MILP's variables and constraint rows.
    pub(crate) variable_count: usize,
    pub(crate) constraint_count: usize,
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
/// y_{\kappa,u,c,i} + \sum_v \pi_v h_v + \sum_s r_s$ ($\pi_v h_v$ of a
/// query-time scan is in $r_s$) under (A)–(F), [`use_constraints`],
/// [`raw_data_disk_constraints`] and (R), with HiGHS at gap 0 on one
/// thread. The executions dearer than a feasible plan are left out first
/// ([`dearer_than_a_feasible_plan`]), then the coefficients are scaled so
/// the largest left is 1,000 (HiGHS's tolerances are absolute).
pub(crate) fn solve_selection(
    inputs: &SelectionInputs<'_>,
) -> Result<SolvedSelection, SelectionMilpError> {
    let started = Instant::now();
    let left_out = dearer_than_a_feasible_plan(inputs);
    let components = Components::of(inputs);
    let mut problem = ProblemVariables::new();
    let variables = selection_variables(inputs, &left_out, &components, &mut problem);
    let constraints: Vec<Constraint> = [
        input_satisfaction_constraints(inputs, &variables),
        producer_selection_constraints(inputs, &variables),
        query_coverage_constraints(inputs, &variables),
        acyclicity_constraints(inputs, &variables, &components),
        use_constraints(inputs, &variables),
        raw_data_disk_constraints(inputs, &variables),
        retention_constraints(inputs, &variables.h, &variables.y, &variables.z),
    ]
    .into_iter()
    .flatten()
    .collect();
    let costs = inputs.costs;
    // π_v per second of retention, the unit of h.
    let retention_cost = |v: &PhysicalCandidateId| costs.retention_cost_per_second[v] * 1_000.0;
    // The objective's terms per second, unscaled: (cost, variable). A
    // query-time scan's retention is in its source's r_s.
    let terms: Vec<(f64, Variable)> = costs
        .per_execution
        .iter()
        .zip(&variables.x)
        .zip(&left_out)
        .filter(|(_, &left_out)| !left_out)
        .filter_map(|((cost, &x), _)| Some((*cost.as_ref().ok()?, x)))
        .chain(
            variables
                .y
                .iter()
                .map(|(&position, &y)| (costs.per_edge[position].expect("a costed edge"), y)),
        )
        .chain(
            variables
                .h
                .iter()
                .filter(|(v, _)| !costs.raw_data_source.contains_key(v))
                .map(|(v, &h)| (retention_cost(v), h)),
        )
        .chain(variables.raw_data_on_disk.values().map(|&r| (1.0, r)))
        .collect();
    // Scaled by the largest coefficient left, the retention and raw-data
    // bounds included.
    let largest = terms
        .iter()
        .map(|(cost, _)| *cost)
        .chain(costs.raw_data_source.keys().filter_map(|v| {
            costs
                .retention_cost_per_second
                .get(v)
                .map(|_| retention_cost(v))
        }))
        .chain(
            costs
                .unbounded_raw_data_on_disk_cost_per_second
                .values()
                .copied(),
        )
        .fold(0.0_f64, |largest, cost| largest.max(cost.abs()));
    let scale = match largest > 0.0 && largest.is_finite() {
        true => 1_000.0 / largest,
        false => 1.0,
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
    let contexts = inputs.contexts;
    let selected: Vec<usize> = (0..variables.x.len())
        .filter(|&index| chosen(variables.x[index]))
        .collect();
    let selected_edges: Vec<usize> = variables
        .y
        .iter()
        .filter(|&(_, &y)| chosen(y))
        .map(|(&position, _)| position)
        .collect();
    let outputs: Vec<Execution> = inputs
        .query_outputs
        .iter()
        .map(|(query, executions)| {
            executions
                .iter()
                .copied()
                .find(|&execution| chosen(variables.z[&(*query, execution)]))
                .expect("(C): one output per query output reference")
        })
        .collect();
    let answering: Vec<(QueryId, Execution)> = inputs
        .query_outputs
        .iter()
        .zip(&outputs)
        .map(|((query, _), &execution)| (*query, execution))
        .collect();
    let (cost_per_second, retention_ms) =
        selection_cost_per_second(inputs, &selected, &selected_edges, &answering);
    let objective_cost_per_second = cost_per_second + costs.request_overhead_cost_per_second;
    Ok(SolvedSelection {
        executions: selected
            .iter()
            .map(|&index| contexts.executions()[index])
            .collect(),
        edges: selected_edges
            .iter()
            .map(|&position| contexts.edges()[position])
            .collect(),
        outputs,
        retention_ms,
        objective_cost_per_second,
        variable_count,
        constraint_count,
        construction_ms,
        solve_ms,
    })
}

/// The cost per second of a selection, without the request overhead every
/// plan pays: its executions (`selected`, by position in
/// `ExecutionContexts::executions`) and edges (`edges`, by position in
/// `ExecutionContexts::edges`), the retention they and the query outputs
/// `answering` need ([`least_retention_ms`]), and each source's raw data
/// on disk at its largest selected scan; with that retention.
pub(crate) fn selection_cost_per_second(
    inputs: &SelectionInputs<'_>,
    selected: &[usize],
    edges: &[usize],
    answering: &[(QueryId, Execution)],
) -> (f64, HashMap<PhysicalCandidateId, u64>) {
    let costs = inputs.costs;
    let retention_ms = least_retention_ms(inputs, edges.iter().copied(), answering.iter().copied());
    let mut raw_data_on_disk: BTreeMap<&Source, f64> = BTreeMap::new();
    let mut retention_cost_per_second = 0.0;
    for (&v, &ms) in &retention_ms {
        let cost = costs.retention_cost_per_second[&v] * ms as f64;
        match costs.raw_data_source.get(&v) {
            Some(source) => {
                let kept = raw_data_on_disk.entry(source).or_default();
                *kept = kept.max(cost);
            }
            None => retention_cost_per_second += cost,
        }
    }
    for &index in selected {
        let v = inputs.contexts.executions()[index].candidate;
        if let (Some(source), Some(&cost)) = (
            costs.raw_data_source.get(&v),
            costs.unbounded_raw_data_on_disk_cost_per_second.get(&v),
        ) {
            let kept = raw_data_on_disk.entry(source).or_default();
            *kept = kept.max(cost);
        }
    }
    let cost = selected
        .iter()
        .filter_map(|&index| costs.per_execution[index].as_ref().ok())
        .sum::<f64>()
        + edges
            .iter()
            .filter_map(|&position| costs.per_edge[position])
            .sum::<f64>()
        + retention_cost_per_second
        + raw_data_on_disk.values().sum::<f64>();
    (cost, retention_ms)
}

#[cfg(test)]
mod tests;

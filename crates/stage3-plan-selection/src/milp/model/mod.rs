//! Stage 3's selection MILP, named after the formulation
//! (`docs/design_docs/concepts/memo-groups-properties-requirements.md`,
//! sections 3–5).
//!
//! | Family | Variables | Constraints |
//! |---|---|---|
//! | baseline (section 3) over execution contexts (section 5) | $x_{\kappa,v}$ (with $x^{\mathrm{mat}}_v = x_{\mathrm{Materialized}(v),v}$), $y_{\kappa,u,c,i}$, $z_{q,v}$, $t$, $r_s$ ([`SelectionVariables`]) | (A) [`input_satisfaction_constraints`], (B) [`producer_selection_constraints`], (C)/(D) [`query_coverage_constraints`], (E) [`acyclicity_constraints`], (F) and one consumer per context of a query-time result that is not materialized [`use_constraints`], each source's raw data on disk once [`raw_data_disk_rows`]; (G), (H): $y$ exists only on admissible edges where the producer delivers on the consumer's context |
//! | retention (section 5) | $h_v$ | (R) [`super::retention::retention_constraints`] |
//! | exact-only reads per query | $a_{q,e}$ | (X) [`super::exact_only::exact_only_constraints`] |
//! | accuracy (section 4) | $E_{c,m}$, $F_{c,i,m}$, $D_c$, $H_{c,i}$ (`AccuracyVariables`) | (K) `accuracy_constraints` |
//!
//! [`solve_selection`] solves it without (K), whose variables and
//! constraint are in [`super::accuracy`], `todo!()` until it is added. The
//! strongly connected components (E) needs are in [`super::graph`]; the
//! executions left out before scaling, those dearer than a feasible plan
//! and those their `OneEvaluation` twin dominates, in
//! [`super::upper_bound`] and [`super::dominance`].

use std::collections::{BTreeMap, HashMap};
use std::time::Instant;

use good_lp::solvers::highs::highs;
use good_lp::{
    constraint, variable, Constraint, Expression, ProblemVariables, SolverModel, Variable,
};

use asap_types::ir::operator::Source;
use asap_types::stages::{PhysicalASAPMemoCandidates, PhysicalCandidateId, PhysicalEdgeId};
use asap_types::workload::QueryWorkloadEntry;

use super::dominance::dominated_by_one_evaluation_twin;
use super::exact_only::{exact_only_constraints, exact_only_variables};
use super::graph::Components;
use super::retention::{least_retention_ms, retention_constraints, retention_variables};
use super::upper_bound::dearer_than_a_feasible_plan;
use crate::cost::ExecutionCosts;
use crate::selectable::{Execution, QueryId, SelectableExecutions};

/// What the MILP is built over.
pub(crate) struct SelectionInputs<'a> {
    pub(crate) physical: &'a PhysicalASAPMemoCandidates,
    /// Each query's workload entry, for the retention needs of (R).
    pub(crate) entries: &'a [QueryWorkloadEntry],
    /// The executions $x$, edges $y$ and query outputs $z$ are over:
    /// $z_{q,v}$ exists only for the executions
    /// `SelectableExecutions::answering` lists.
    pub(crate) selectable: &'a SelectableExecutions,
    /// The objective's coefficients: $x$ is 0 for an execution it could
    /// not cost, $y$ exists only on an edge it costs, $h_v$ only for a
    /// candidate with a retention cost.
    pub(crate) costs: &'a ExecutionCosts,
}

/// The variables (sections 3 and 5). Executions are by position in
/// `SelectableExecutions::executions`, edges by position in
/// `SelectableExecutions::edges`.
pub(crate) struct SelectionVariables {
    /// $x_{\kappa,v}$, by execution.
    x: Vec<Variable>,
    /// $y_{\kappa,u,c,i}$, by edge, on the edges costed between executions
    /// kept, in edge order.
    y: BTreeMap<usize, Variable>,
    /// $z_{q,v}$, by query and the execution answering it, in key order.
    z: BTreeMap<(QueryId, usize), Variable>,
    /// $t$, by execution: only for executions on a cycle of selectable
    /// edges (a strongly connected component); `None` elsewhere.
    t: Vec<Option<Variable>>,
    /// $h_v$ in seconds retained, for a candidate with a retention cost and
    /// an execution kept ([`retention_variables`]).
    h: BTreeMap<PhysicalCandidateId, Variable>,
    /// Per raw-data source a query-time scan keeps on disk at a cost: the
    /// cost per second of keeping it, at least each selected scan's,
    /// scaled by the objective's scale ([`raw_data_disk_rows`]).
    raw_data_on_disk: BTreeMap<Source, Variable>,
    /// $a_{q,e}$: query $q$'s selected path reads execution $e$, where
    /// (X) needs it ([`exact_only_variables`]).
    read_by_query: BTreeMap<(QueryId, usize), Variable>,
}

/// $x$, $y$, $z$, $t$, $h$ and $r$ over `inputs`, added to `problem`: $x$
/// fixed at 0 for an execution that is not `kept` (not costed, or dearer
/// than a feasible plan), and no $y$ on an edge of one.
fn selection_variables(
    inputs: &SelectionInputs<'_>,
    kept: &[bool],
    components: &Components,
    problem: &mut ProblemVariables,
) -> SelectionVariables {
    let selectable = inputs.selectable;
    let x = kept
        .iter()
        .map(|&kept| match kept {
            true => problem.add(variable().binary()),
            false => problem.add(variable().min(0).max(0)),
        })
        .collect();
    let y = selectable
        .edges()
        .iter()
        .zip(&inputs.costs.per_edge)
        .enumerate()
        .filter(|(_, (edge, cost))| cost.is_some() && kept[edge.consumer] && kept[edge.producer])
        .map(|(position, _)| (position, problem.add(variable().binary())))
        .collect();
    let mut z = BTreeMap::new();
    for (query, executions) in selectable.answering() {
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
    let h = retention_variables(inputs, kept, problem);
    let mut raw_data_on_disk = BTreeMap::new();
    for source in inputs.costs.raw_data_source.values() {
        raw_data_on_disk
            .entry(source.clone())
            .or_insert_with(|| problem.add(variable().min(0)));
    }
    let read_by_query = exact_only_variables(inputs, &y, problem);
    SelectionVariables {
        x,
        y,
        z,
        t,
        h,
        raw_data_on_disk,
        read_by_query,
    }
}

/// (A) $\sum_{u \in P_{c,i}} y_{\kappa,u,c,i} = x_{\kappa,c}$, per execution
/// of $c$ and input $i$.
fn input_satisfaction_constraints(
    inputs: &SelectionInputs<'_>,
    variables: &SelectionVariables,
) -> Vec<Constraint> {
    let selectable = inputs.selectable;
    let mut constraints = Vec::new();
    for (index, &x) in variables.x.iter().enumerate() {
        for i in 0..selectable.input_count(index) {
            let chosen: Expression = selectable
                .edges_into(index, i)
                .iter()
                .filter_map(|position| variables.y.get(position))
                .sum();
            constraints.push(constraint!(chosen == x));
        }
    }
    constraints
}

/// (B) $y_{\kappa,u,c,i} \le x$ of $u$'s execution it reads, except for
/// a producer with one reader per context ([`has_one_reader`]), whose (F) row
/// implies it.
fn producer_selection_constraints(
    inputs: &SelectionInputs<'_>,
    variables: &SelectionVariables,
) -> Vec<Constraint> {
    variables
        .y
        .iter()
        .filter_map(|(&position, &y)| {
            let producer = inputs.selectable.edges()[position].producer;
            (!has_one_reader(inputs, producer)).then(|| constraint!(y <= variables.x[producer]))
        })
        .collect()
}

/// (C) $\sum_{v \in G_q} z_{q,v} = 1$; (D) $z_{q,v} \le x$ of $v$'s
/// execution answering $q$, except for one with one reader per context
/// ([`has_one_reader`]), whose (F) row implies it.
fn query_coverage_constraints(
    inputs: &SelectionInputs<'_>,
    variables: &SelectionVariables,
) -> Vec<Constraint> {
    let mut constraints = Vec::new();
    for (query, executions) in inputs.selectable.answering() {
        let covered: Expression = executions
            .iter()
            .map(|&execution| variables.z[&(*query, execution)])
            .sum();
        constraints.push(constraint!(covered == 1));
    }
    for (&(_, execution), &z) in &variables.z {
        if !has_one_reader(inputs, execution) {
            constraints.push(constraint!(z <= variables.x[execution]));
        }
    }
    constraints
}

/// Whether the execution at position `index` has one reader per context
/// (`PhysicalProperties::has_one_reader_per_context`): its (F) row is the equality
/// $\sum y + \sum_q z_{q,v} = x_{\kappa,v}$ ([`use_constraints`]), which
/// with $y, z \ge 0$ implies (B) and (D) for it, so those rows are left
/// out.
pub(super) fn has_one_reader(inputs: &SelectionInputs<'_>, index: usize) -> bool {
    let v = inputs.selectable.executions()[index].candidate;
    inputs.physical.physical_candidates[v.0]
        .physical_properties
        .has_one_reader_per_context()
}

/// (E) $t_u + 1 \le t_c + n(1 - y_{\kappa,u,c,i})$ between the producer's
/// and the consumer's executions, only on edges inside a strongly connected
/// component of the selectable edges, $n$ its size.
fn acyclicity_constraints(
    inputs: &SelectionInputs<'_>,
    variables: &SelectionVariables,
    components: &Components,
) -> Vec<Constraint> {
    variables
        .y
        .iter()
        .filter_map(|(&position, &y)| {
            let edge = inputs.selectable.edges()[position];
            let (c, u) = (edge.consumer, edge.producer);
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
/// outputs reading that execution; with it, a candidate with one reader
/// per context (`PhysicalProperties::has_one_reader_per_context`: a query-time result that
/// is not materialized) has $\sum y + \sum_q z_{q,v} \le x_{\kappa,v}$, so
/// for such an execution the two are one row, $\sum y + \sum_q z_{q,v} =
/// x_{\kappa,v}$. An ingestion-time producer is exempt from the
/// one-consumer rule: there is no `OneEvaluation` at ingestion time, and
/// the consumers of one ingestion run share its rows.
fn use_constraints(
    inputs: &SelectionInputs<'_>,
    variables: &SelectionVariables,
) -> Vec<Constraint> {
    uses_of(inputs, variables)
        .into_iter()
        .enumerate()
        .map(|(index, used)| match has_one_reader(inputs, index) {
            true => constraint!(used == variables.x[index]),
            false => constraint!(variables.x[index] <= used),
        })
        .collect()
}

/// The raw data of each source is kept on disk once, for the selected scan
/// that keeps the most: $r_s \ge \pi_v h_v$ for each query-time `Scan` $v$
/// of source $s$ with $h_v$, and $r_s \ge d_v x_{\kappa,v}$ for each kept
/// execution of one without, $d_v$ its cost per second of the raw data it
/// keeps retaining everything: each row as ($r_s$, its coefficient, the
/// variable it multiplies), unscaled.
fn raw_data_disk_rows(
    inputs: &SelectionInputs<'_>,
    variables: &SelectionVariables,
    kept: &[bool],
) -> Vec<(Variable, f64, Variable)> {
    let costs = inputs.costs;
    let mut scans: Vec<(&PhysicalCandidateId, &Source)> = costs.raw_data_source.iter().collect();
    scans.sort_unstable_by_key(|(v, _)| **v);
    let mut executions_of: HashMap<PhysicalCandidateId, Vec<usize>> = HashMap::new();
    for (index, execution) in inputs.selectable.executions().iter().enumerate() {
        if kept[index] {
            executions_of
                .entry(execution.candidate)
                .or_default()
                .push(index);
        }
    }
    let mut rows = Vec::new();
    for (v, source) in scans {
        let r = variables.raw_data_on_disk[source];
        if let Some(&h) = variables.h.get(v) {
            rows.push((r, costs.retention_cost_per_retained_second[v], h));
        } else if let Some(&d) = costs.unbounded_raw_data_on_disk_cost_per_second.get(v) {
            for &index in executions_of.get(v).into_iter().flatten() {
                rows.push((r, d, variables.x[index]));
            }
        }
    }
    rows
}

/// Per execution, its selected edges to consumers and query outputs:
/// $\sum y + \sum_q z_{q,v}$.
fn uses_of(inputs: &SelectionInputs<'_>, variables: &SelectionVariables) -> Vec<Expression> {
    let mut uses: Vec<Expression> = vec![Expression::default(); variables.x.len()];
    for (&position, &y) in &variables.y {
        uses[inputs.selectable.edges()[position].producer] += y;
    }
    for (&(_, execution), &z) in &variables.z {
        uses[execution] += z;
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
    /// The selected edges, ascending.
    pub(crate) edges: Vec<SelectedEdge>,
    /// The execution answering each of `SelectableExecutions::answering`,
    /// in its order.
    pub(crate) outputs: Vec<Execution>,
    /// $h_v$ in ms of each selected candidate with $h_v$: the least (R)
    /// allows for the selection.
    pub(crate) retention_ms: HashMap<PhysicalCandidateId, u64>,
    /// The optimum, unscaled: the selected plan's cost per second, less
    /// the request overhead every plan pays.
    pub(crate) cost_per_second_without_request_overhead: f64,
    /// The MILP's variables and constraint rows.
    pub(crate) variable_count: usize,
    pub(crate) constraint_count: usize,
    pub(crate) construction_ms: u64,
    pub(crate) solve_ms: u64,
}

/// A selected $y_{\kappa,u,c,i}$: edge `edge` from execution `producer`
/// into execution `consumer`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct SelectedEdge {
    pub(crate) consumer: Execution,
    pub(crate) edge: PhysicalEdgeId,
    pub(crate) producer: Execution,
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
/// [`raw_data_disk_rows`], (R) and (X), with HiGHS at gap 0 on one
/// thread. The executions dearer than a feasible plan
/// ([`dearer_than_a_feasible_plan`]) or dominated by their `OneEvaluation`
/// twin ([`dominated_by_one_evaluation_twin`]) are left out first, with
/// the $h_v$ of a candidate whose
/// executions are all left out, then the coefficients kept are scaled so
/// the largest is 1,000 (HiGHS's tolerances are absolute).
pub(crate) fn solve_selection(
    inputs: &SelectionInputs<'_>,
) -> Result<SolvedSelection, SelectionMilpError> {
    let started = Instant::now();
    let costs = inputs.costs;
    let kept: Vec<bool> = dearer_than_a_feasible_plan(inputs)
        .iter()
        .zip(dominated_by_one_evaluation_twin(inputs))
        .zip(&costs.per_execution)
        .map(|((&dearer, dominated), cost)| cost.is_ok() && !dearer && !dominated)
        .collect();
    let components = Components::of(inputs);
    let mut problem = ProblemVariables::new();
    let variables = selection_variables(inputs, &kept, &components, &mut problem);
    // The objective's terms per second, unscaled: (cost, variable). A
    // query-time scan's retention is in its source's r_s.
    let terms: Vec<(f64, Variable)> = costs
        .per_execution
        .iter()
        .zip(&variables.x)
        .zip(&kept)
        .filter(|(_, &kept)| kept)
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
                .map(|(v, &h)| (costs.retention_cost_per_retained_second[v], h)),
        )
        .collect();
    // Scaled by the largest coefficient kept, the raw-data rows' included.
    let raw_data_rows = raw_data_disk_rows(inputs, &variables, &kept);
    let largest = terms
        .iter()
        .map(|(cost, _)| *cost)
        .chain(raw_data_rows.iter().map(|&(_, coefficient, _)| coefficient))
        .fold(0.0_f64, |largest, cost| largest.max(cost.abs()));
    let scale = match largest > 0.0 && largest.is_finite() {
        true => 1_000.0 / largest,
        false => 1.0,
    };
    let constraints: Vec<Constraint> = [
        input_satisfaction_constraints(inputs, &variables),
        producer_selection_constraints(inputs, &variables),
        query_coverage_constraints(inputs, &variables),
        acyclicity_constraints(inputs, &variables, &components),
        use_constraints(inputs, &variables),
        // The objective's scale is in the raw-data rows, so $r_s$ is the
        // scaled cost and its objective coefficient is 1.
        raw_data_rows
            .iter()
            .map(|&(r, coefficient, variable)| constraint!(r >= coefficient * scale * variable))
            .collect(),
        retention_constraints(
            inputs,
            &variables.h,
            &variables.x,
            &variables.y,
            &variables.z,
        ),
        exact_only_constraints(inputs, &variables.read_by_query, &variables.y, &variables.z),
    ]
    .into_iter()
    .flatten()
    .collect();
    // The raw data on disk is scaled by its rows ([`raw_data_disk_rows`]).
    let objective: Expression = terms
        .iter()
        .map(|&(cost, v)| cost * scale * v)
        .chain(
            variables
                .raw_data_on_disk
                .values()
                .map(|&r| Expression::from(r)),
        )
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
    let selectable = inputs.selectable;
    let executions = selectable.executions();
    let selected: Vec<usize> = (0..variables.x.len())
        .filter(|&index| chosen(variables.x[index]))
        .collect();
    let selected_edges: Vec<usize> = variables
        .y
        .iter()
        .filter(|&(_, &y)| chosen(y))
        .map(|(&position, _)| position)
        .collect();
    let answering: Vec<(QueryId, usize)> = selectable
        .answering()
        .iter()
        .map(|(query, answering)| {
            let execution = answering
                .iter()
                .copied()
                .find(|&execution| chosen(variables.z[&(*query, execution)]))
                .expect("(C): one output per query output reference");
            (*query, execution)
        })
        .collect();
    let (cost_per_second_without_request_overhead, retention_ms) =
        selection_cost_per_second(inputs, &selected, &selected_edges, &answering);
    Ok(SolvedSelection {
        executions: selected.iter().map(|&index| executions[index]).collect(),
        edges: selected_edges
            .iter()
            .map(|&position| {
                let edge = selectable.edges()[position];
                let (consumer, producer) = (edge.consumer, edge.producer);
                SelectedEdge {
                    consumer: executions[consumer],
                    edge: edge.edge,
                    producer: executions[producer],
                }
            })
            .collect(),
        outputs: answering
            .iter()
            .map(|&(_, execution)| executions[execution])
            .collect(),
        retention_ms,
        cost_per_second_without_request_overhead,
        variable_count,
        constraint_count,
        construction_ms,
        solve_ms,
    })
}

/// The cost per second of a selection, less the request overhead every
/// plan pays: its executions (`selected`, by position in
/// `SelectableExecutions::executions`) and edges (`edges`, by position in
/// `SelectableExecutions::edges`), the retention they and the query
/// outputs `answering` (by execution position) need
/// ([`least_retention_ms`]), and each source's raw data on disk at its
/// largest selected scan; with that retention.
pub(crate) fn selection_cost_per_second(
    inputs: &SelectionInputs<'_>,
    selected: &[usize],
    edges: &[usize],
    answering: &[(QueryId, usize)],
) -> (f64, HashMap<PhysicalCandidateId, u64>) {
    let costs = inputs.costs;
    let retention_ms = least_retention_ms(inputs, edges.iter().copied(), answering.iter().copied());
    let mut raw_data_on_disk: BTreeMap<&Source, f64> = BTreeMap::new();
    let mut retention_cost_per_second = 0.0;
    for (&v, &ms) in &retention_ms {
        let cost = costs.retention_cost_per_second(v, ms);
        match costs.raw_data_source.get(&v) {
            Some(source) => {
                let kept = raw_data_on_disk.entry(source).or_default();
                *kept = kept.max(cost);
            }
            None => retention_cost_per_second += cost,
        }
    }
    for &index in selected {
        let v = inputs.selectable.executions()[index].candidate;
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

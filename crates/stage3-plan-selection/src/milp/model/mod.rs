//! Stage 3's selection MILP, named after the formulation
//! (`docs/design_docs/concepts/memo-groups-properties-requirements.md`,
//! sections 3–5).
//!
//! | Family | Variables | Constraints |
//! |---|---|---|
//! | baseline (section 3) | $x_v$, $y_{u,c,i}$, $z_{q,v}$, $t_v$, $r_s$ ([`SelectionVariables`]) | (A) [`input_satisfaction_constraints`], (B) [`producer_selection_constraints`], (C)/(D) [`query_coverage_constraints`], (E) [`acyclicity_constraints`], (F) and one consumer of a query-time result that is not materialized [`use_constraints`], each source's raw data on disk once [`raw_data_disk_constraints`]; (G), (H): $y$ exists only on admissible edges |
//! | execution contexts (section 5) | $x^{\mathrm{mat}}_v$, $x_{\kappa,v}$, $y_{\kappa,u,c,i}$, $h_v$ (`ExecutionContextVariables`) | (R) `retention_constraints` |
//! | accuracy (section 4) | $E_{c,m}$, $F_{c,i,m}$, $D_c$, $H_{c,i}$ (`AccuracyVariables`) | (K) `accuracy_constraints` |
//!
//! The baseline is solved by [`solve_selection`]; the extensions'
//! variables and constraints are in [`super::execution_contexts`] and
//! [`super::accuracy`], `todo!()` until they are added. The strongly
//! connected components (E) needs are in [`super::graph`]; the bound that
//! leaves out candidates before scaling in [`super::upper_bound`].

use std::collections::{BTreeMap, HashMap};
use std::time::Instant;

use good_lp::solvers::highs::highs;
use good_lp::{
    constraint, variable, Constraint, Expression, ProblemVariables, SolverModel, Variable,
};

use asap_types::ir::operator::Source;
use asap_types::ir::properties::ExecutionTiming;
use asap_types::stages::Lifetime;
use asap_types::stages::{PhysicalASAPMemoCandidates, PhysicalCandidateId, PhysicalEdgeId};

use super::graph::Components;
use super::upper_bound::dearer_than_a_feasible_plan;
use crate::cost::SelectionCosts;

/// A query, by index in the workload.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct QueryId(pub usize);

/// What the baseline MILP is built over.
pub(crate) struct SelectionInputs<'a> {
    pub(crate) physical: &'a PhysicalASAPMemoCandidates,
    /// $w_v$ and $c_{u,c,i}$: $y$ exists only on the edges
    /// [`SelectionCosts::edge_cost_change_per_second`] costs, $x_v$ is 0 for a
    /// candidate it could not cost.
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
    /// $y_{u,c,i}$, on admissible edges only, in edge order.
    y: BTreeMap<PhysicalEdgeId, Variable>,
    /// $z_{q,v}$, for $v \in G_q$, in key order.
    z: BTreeMap<(QueryId, PhysicalCandidateId), Variable>,
    /// $t_v$, by [`PhysicalCandidateId`]: only for candidates on a cycle of
    /// admissible edges (a strongly connected component); `None` elsewhere.
    t: Vec<Option<Variable>>,
    /// Per raw-data source a query-time scan keeps on disk at a cost: the
    /// cost per second of keeping it, at least each selected scan's
    /// ([`raw_data_disk_constraints`]).
    raw_data_on_disk: BTreeMap<Source, Variable>,
}

/// $x$, $y$, $z$, $t$ and $r$ over `inputs`, added to `problem`: $x_v$
/// fixed at 0 for a candidate that could not be costed or is `left_out`,
/// and no $y$ on an edge of one.
fn selection_variables(
    inputs: &SelectionInputs<'_>,
    left_out: &[bool],
    components: &Components,
    problem: &mut ProblemVariables,
) -> SelectionVariables {
    let count = inputs.physical.physical_candidates.len();
    let x = (0..count)
        .map(
            |v| match inputs.costs.candidate_cost_per_second[v].is_ok() && !left_out[v] {
                true => problem.add(variable().binary()),
                false => problem.add(variable().min(0).max(0)),
            },
        )
        .collect();
    let mut edges: Vec<PhysicalEdgeId> = inputs
        .costs
        .edge_cost_change_per_second
        .keys()
        .copied()
        .filter(|&edge| {
            let edge = inputs.physical.edge(edge);
            !left_out[edge.producer_candidate_id.0] && !left_out[edge.consumer_candidate_id.0]
        })
        .collect();
    edges.sort_unstable();
    let y = edges
        .into_iter()
        .map(|edge| (edge, problem.add(variable().binary())))
        .collect();
    let mut z = BTreeMap::new();
    for (query, candidates) in inputs.query_outputs {
        for &v in candidates {
            z.entry((*query, v))
                .or_insert_with(|| problem.add(variable().binary()));
        }
    }
    // t_v only inside a strongly connected component of the selectable
    // edges, bounded by its size.
    let t = components
        .size
        .iter()
        .map(|&size| (size > 1).then(|| problem.add(variable().min(0).max((size - 1) as f64))))
        .collect();
    let mut raw_data_on_disk = BTreeMap::new();
    for (source, _) in inputs.costs.raw_data_on_disk.iter().flatten() {
        raw_data_on_disk
            .entry(source.clone())
            .or_insert_with(|| problem.add(variable().min(0)));
    }
    SelectionVariables {
        x,
        y,
        z,
        t,
        raw_data_on_disk,
    }
}

/// (A) $\sum_{u \in P_{c,i}} y_{u,c,i} = x_c$.
fn input_satisfaction_constraints(
    inputs: &SelectionInputs<'_>,
    variables: &SelectionVariables,
) -> Vec<Constraint> {
    let physical = inputs.physical;
    let mut constraints = Vec::new();
    for c in (0..physical.physical_candidates.len()).map(PhysicalCandidateId) {
        for i in 0..physical.input_count(c) {
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
    variables
        .y
        .iter()
        .map(|(&edge, &y)| {
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
    for (&(_, v), &z) in &variables.z {
        constraints.push(constraint!(z <= variables.x[v.0]));
    }
    constraints
}

/// (E) $t_u + 1 \le t_c + n(1 - y_{u,c,i})$, only on edges inside a strongly
/// connected component of the admissible edges, $n$ its size.
fn acyclicity_constraints(
    inputs: &SelectionInputs<'_>,
    variables: &SelectionVariables,
    components: &Components,
) -> Vec<Constraint> {
    variables
        .y
        .iter()
        .filter_map(|(&edge, &y)| {
            let edge = inputs.physical.edge(edge);
            let (u, c) = (edge.producer_candidate_id.0, edge.consumer_candidate_id.0);
            if components.of[u] != components.of[c] {
                return None;
            }
            let (t_u, t_c) = (variables.t[u]?, variables.t[c]?);
            let n = components.size[u] as f64;
            Some(constraint!(t_u + 1 <= t_c + n * (1 - y)))
        })
        .collect()
}

/// (F) $x_v \le \sum_{(v,c,i)} y_{v,c,i} + \sum_q z_{q,v}$ for every
/// candidate; with it, a query-time result that is not materialized
/// ($\ell$ = None) has at most one consumer, $\sum_{(v,c,i)} y_{v,c,i} +
/// \sum_q z_{q,v} \le x_v$, so for such a candidate the two are one row,
/// $\sum_{(v,c,i)} y_{v,c,i} + \sum_q z_{q,v} = x_v$. Consumers that share a
/// result read a `OneEvaluation` or a materialized producer (section 5, per
/// execution context). An ingestion-time producer is exempt from the
/// one-consumer rule: there is no `OneEvaluation` at ingestion time, and the
/// consumers of one ingestion run share its rows.
fn use_constraints(
    inputs: &SelectionInputs<'_>,
    variables: &SelectionVariables,
) -> Vec<Constraint> {
    uses_of(inputs, variables)
        .into_iter()
        .enumerate()
        .map(|(v, used)| {
            let properties = inputs.physical.physical_candidates[v].physical_properties;
            match properties.lifetime() == Lifetime::None
                && properties.timing() == ExecutionTiming::QueryTime
            {
                true => constraint!(used == variables.x[v]),
                false => constraint!(variables.x[v] <= used),
            }
        })
        .collect()
}

/// The raw data of each source is kept on disk once, for the selected scan
/// that keeps the most: $r_s \ge d_v x_v$ for each query-time `Scan` $v$ of
/// source $s$, $d_v$ its cost per second of the raw data it keeps.
fn raw_data_disk_constraints(
    inputs: &SelectionInputs<'_>,
    variables: &SelectionVariables,
) -> Vec<Constraint> {
    inputs
        .costs
        .raw_data_on_disk
        .iter()
        .enumerate()
        .filter_map(|(v, on_disk)| {
            let (source, cost) = on_disk.as_ref()?;
            Some(constraint!(
                variables.raw_data_on_disk[source] >= *cost * variables.x[v]
            ))
        })
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
    for (&(_, v), &z) in &variables.z {
        uses[v.0] += z;
    }
    uses
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
    /// The optimum, unscaled: the selected plan's cost per second.
    pub(crate) objective_cost_per_second: f64,
    /// The MILP's variables and constraint rows.
    pub(crate) variable_count: usize,
    pub(crate) constraint_count: usize,
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
/// $\sum_v w_v x_v + \sum_{(u,c,i)} c_{u,c,i} y_{u,c,i} + \sum_s r_s$ under
/// (A)–(F), the one-consumer rule ([`use_constraints`]) and
/// [`raw_data_disk_constraints`], with HiGHS at gap 0 on one thread. The
/// candidates dearer than a feasible plan are left out first
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
    ]
    .into_iter()
    .flatten()
    .collect();
    // Scaled by the largest coefficient of a candidate the bound keeps.
    let kept = |v: &usize| !left_out[*v];
    let largest = (0..left_out.len())
        .filter(kept)
        .filter_map(|v| inputs.costs.candidate_cost_per_second[v].as_ref().ok())
        .chain(
            variables
                .y
                .keys()
                .map(|edge| &inputs.costs.edge_cost_change_per_second[edge]),
        )
        .chain(
            (0..left_out.len())
                .filter(kept)
                .filter_map(|v| inputs.costs.raw_data_on_disk[v].as_ref())
                .map(|(_, cost)| cost),
        )
        .fold(0.0_f64, |largest, &cost| largest.max(cost.abs()));
    let scale = match largest > 0.0 && largest.is_finite() {
        true => 1_000.0 / largest,
        false => 1.0,
    };
    let candidate_terms = (0..left_out.len()).filter(kept).filter_map(|v| {
        let cost = inputs.costs.candidate_cost_per_second[v].as_ref().ok()?;
        Some(cost * scale * variables.x[v])
    });
    let edge_terms = variables
        .y
        .iter()
        .map(|(edge, &y)| inputs.costs.edge_cost_change_per_second[edge] * scale * y);
    let raw_data_terms = variables
        .raw_data_on_disk
        .values()
        .map(|&on_disk| scale * on_disk);
    let objective: Expression = candidate_terms
        .chain(edge_terms)
        .chain(raw_data_terms)
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
    // The optimum, unscaled: the selected candidates' and edges' costs,
    // each source's raw data on disk at its largest selected scan, and the
    // request overhead every plan pays.
    let mut raw_data_on_disk: BTreeMap<&Source, f64> = BTreeMap::new();
    for (source, cost) in (0..variables.x.len())
        .filter(|&v| chosen(variables.x[v]))
        .filter_map(|v| inputs.costs.raw_data_on_disk[v].as_ref())
    {
        let kept = raw_data_on_disk.entry(source).or_default();
        *kept = kept.max(*cost);
    }
    let objective_cost_per_second = (0..variables.x.len())
        .filter(|&v| chosen(variables.x[v]))
        .filter_map(|v| inputs.costs.candidate_cost_per_second[v].as_ref().ok())
        .sum::<f64>()
        + variables
            .y
            .iter()
            .filter(|&(_, &y)| chosen(y))
            .map(|(edge, _)| inputs.costs.edge_cost_change_per_second[edge])
            .sum::<f64>()
        + raw_data_on_disk.values().sum::<f64>()
        + inputs.costs.request_overhead_cost_per_second;
    Ok(SolvedSelection {
        objective_cost_per_second,
        candidates: (0..variables.x.len())
            .filter(|&v| chosen(variables.x[v]))
            .map(PhysicalCandidateId)
            .collect(),
        edges: variables
            .y
            .iter()
            .filter(|&(_, &y)| chosen(y))
            .map(|(&edge, _)| edge)
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
        variable_count,
        constraint_count,
        construction_ms,
        solve_ms,
    })
}

#[cfg(test)]
mod tests;

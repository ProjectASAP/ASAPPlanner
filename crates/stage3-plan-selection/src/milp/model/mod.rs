//! Stage 3's selection MILP, named after the formulation
//! (`docs/design_docs/concepts/memo-groups-properties-requirements.md`,
//! sections 3–5).
//!
//! | Family | Variables | Constraints |
//! |---|---|---|
//! | baseline (section 3) over execution contexts (section 5) | $x_{\kappa,v}$ (with $x^{\mathrm{mat}}_v = x_{\mathrm{Materialized}(v),v}$), $y_{\kappa,u,c,i}$, $z_{q,v}$, $t$, $r_s$ ([`SelectionVariables`]) | (A) [`input_satisfaction_constraints`], (B) [`producer_selection_constraints`], (C)/(D) [`query_coverage_constraints`], (E) [`acyclicity_constraints`], (F) and one consumer per context of a query-time result that is not materialized [`use_constraints`], each source's raw data on disk once [`selection_cost_terms`]; (G), (H): $y$ exists only on admissible edges where the producer delivers on the consumer's context |
//! | retention (section 5) | $h_v$ | (R) [`super::retention::retention_constraints`] |
//! | result and answer caches (`cost-terms-from-calibration.md`, section 2) | $t$, $b$, $H$, $w_{q,e}$ | [`super::kept_results::kept_result_terms`] |
//! | resident windows under a window cache (`cost-terms-from-calibration.md`, section 1) | $r_v$ | (W) [`super::retention::resident_window_terms`] |
//! | exact-only reads per query | $a_{q,e}$ | (X) [`super::exact_only::exact_only_constraints`] |
//! | accuracy (`accuracy-models.md`, constraint (K)), per execution | $E_{\kappa,c,m}$, $D_{\kappa,c}$ ([`AccuracyVariables`]); $F_{\kappa,c,i,m}$, $H_{\kappa,c,i}$ | (K) [`accuracy_constraints`], over [`ErrorModel`] |
//!
//! The strongly connected components (E) needs are in [`super::graph`]; the
//! executions left out before scaling, those dearer than a feasible plan
//! and those their lifetime twin dominates (`None` by `OneEvaluation`, or
//! a `OneEvaluation` read at most once by `None`), in
//! [`super::upper_bound`] and [`super::dominance`].

use std::collections::{BTreeMap, HashMap, HashSet};
use std::time::Instant;

use good_lp::solvers::highs::highs;
use good_lp::{
    constraint, variable, Constraint, Expression, ProblemVariables, SolverModel, Variable,
};

use asap_types::accuracy::FailureProbabilityBound;
use asap_types::ir::operator::Source;
use asap_types::stages::{PhysicalASAPMemoCandidates, PhysicalCandidateId, PhysicalEdgeId};
use asap_types::workload::QueryWorkloadEntry;

use super::accuracy::{accuracy_constraints, AccuracyRules, AccuracyVariables, ErrorModel};
use super::dominance::dominated_by_lifetime_twin;
use super::equivalent_contexts::{copies_in_equivalent_contexts, SharedVariables};
use super::exact_only::{exact_only_constraints, exact_only_variables};
use super::graph::Components;
use super::kept_results::{kept_result_terms, kept_results_quantity, KeptResultVariables};
use super::retention::{
    least_retention_ms, resident_window_terms, resident_windows_quantity, retention_constraints,
    retention_variables,
};
use super::upper_bound::dearer_than_a_feasible_plan;
use crate::cost::ExecutionCosts;
use crate::selectable::{Execution, QueryId, SelectableExecutions};

pub(crate) mod cluster;

/// What the MILP is built over.
#[derive(Clone, Copy)]
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
    pub(super) x: Vec<Variable>,
    /// $y_{\kappa,u,c,i}$, by edge, on the edges costed between executions
    /// kept, in edge order.
    pub(super) y: BTreeMap<usize, Variable>,
    /// $z_{q,v}$, by query and the execution answering it, in key order.
    pub(super) z: BTreeMap<(QueryId, usize), Variable>,
    /// $t$, by execution: only for executions on a cycle of selectable
    /// edges (a strongly connected component); `None` elsewhere.
    t: Vec<Option<Variable>>,
    /// $h_v$ in seconds retained, for a candidate with a retention cost and
    /// an execution kept ([`retention_variables`]).
    pub(super) h: BTreeMap<PhysicalCandidateId, Variable>,
    /// Per raw-data source a query-time scan keeps on disk at a cost: the
    /// cost per second of keeping it, at least each selected scan's,
    /// scaled by the objective's scale ([`selection_cost_terms`]).
    raw_data_on_disk: BTreeMap<Source, Variable>,
    /// $a_{q,e}$: query $q$'s selected path reads execution $e$, where
    /// (X) needs it ([`exact_only_variables`]).
    read_by_query: BTreeMap<(QueryId, usize), Variable>,
}

/// $x$, $y$, $z$, $t$, $h$ and $r$ over `inputs`, added to `problem`: $x$
/// fixed at 0 for an execution that is not `kept` (not costed, or dearer
/// than a feasible plan), and no $y$ on an edge of one. The executions,
/// edges and queries of a copy of an equivalent evaluation context take the
/// original's $x$, $y$ and $z$ (`shared`, [`copies_in_equivalent_contexts`]),
/// so their costs add up on one variable.
fn selection_variables(
    inputs: &SelectionInputs<'_>,
    kept: &[bool],
    shared: &SharedVariables,
    components: &Components,
    problem: &mut ProblemVariables,
) -> SelectionVariables {
    let selectable = inputs.selectable;
    let SharedVariables {
        original_of_execution,
        original_of_edge,
        original_of_query,
    } = shared;
    let mut x: Vec<Variable> = Vec::with_capacity(kept.len());
    for (index, &kept) in kept.iter().enumerate() {
        let variable = match (original_of_execution[index], kept) {
            (original, _) if original != index => x[original],
            (_, true) => problem.add(variable().binary()),
            (_, false) => problem.add(variable().min(0).max(0)),
        };
        x.push(variable);
    }
    let mut y: BTreeMap<usize, Variable> = BTreeMap::new();
    for (position, (edge, cost)) in selectable
        .edges()
        .iter()
        .zip(&inputs.costs.per_edge)
        .enumerate()
    {
        if cost.is_some() && kept[edge.consumer] && kept[edge.producer] {
            let variable = match original_of_edge[position] {
                original if original != position => y[&original],
                _ => problem.add(variable().binary()),
            };
            y.insert(position, variable);
        }
    }
    let mut z = BTreeMap::new();
    #[cfg(debug_assertions)]
    let answering_pairs: std::collections::BTreeSet<(QueryId, usize)> = selectable
        .answering()
        .iter()
        .flat_map(|(query, executions)| {
            executions.iter().map(move |&execution| (*query, execution))
        })
        .collect();
    for (query, executions) in selectable.answering() {
        for &execution in executions {
            let original = (original_of_query[query.0], original_of_execution[execution]);
            // A shared z is that of a real (query, answering execution)
            // pair: a copy's query pairs with an original query of the
            // same output executions.
            #[cfg(debug_assertions)]
            debug_assert!(
                answering_pairs.contains(&original),
                "z of {query:?} at execution {execution} shares {original:?}, not an answering pair"
            );
            let variable = *z
                .entry(original)
                .or_insert_with(|| problem.add(variable().binary()));
            z.insert((*query, execution), variable);
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

/// One row that keeps a source's raw data on disk, unscaled: ($r_s$,
/// coefficient, variable) for $r_s \ge$ coefficient × variable.
pub(super) type RawDataRow = (Variable, f64, Variable);

/// `costs` over the selection, unscaled: the terms (coefficient,
/// variable) of each kept execution's share ($x$), each edge's ($y$) and
/// each retention share ($h_v$) of a candidate that is not a query-time
/// scan, and the rows ($r_s$, coefficient, variable) that keep each
/// source's raw data on disk once, for the selected scan that keeps the
/// most: $r_s \ge \pi_v h_v$ for each query-time `Scan` $v$ of source $s$
/// with $h_v$ (and $\pi_v > 0$), $r_s \ge d_v x_{\kappa,v}$ for each kept
/// execution of one without, $d_v$ its cost per second of the raw data it
/// keeps retaining everything, and $r_s \ge d_e w_e$ for each read $e$
/// (edge $y$ or output $z$) of a scan whose raw data is not linear in its
/// retention ([`ExecutionCosts::raw_data_reads`]). `raw_data_on_disk_of`
/// gives source $s$'s $r_s$: cost model 1's one objective holds it in
/// [`SelectionVariables`]; cost model 3 adds one per quantity.
pub(super) fn selection_cost_terms<'c>(
    selectable: &SelectableExecutions,
    costs: &'c ExecutionCosts,
    variables: &SelectionVariables,
    kept: &[bool],
    mut raw_data_on_disk_of: impl FnMut(&'c Source) -> Variable,
) -> (Vec<(f64, Variable)>, Vec<RawDataRow>) {
    let mut terms: Vec<(f64, Variable)> = Vec::new();
    for (index, &x) in variables.x.iter().enumerate() {
        if kept[index] {
            terms.push((
                costs.per_execution[index].as_ref().copied().unwrap_or(0.0),
                x,
            ));
        }
    }
    for (&position, &y) in &variables.y {
        terms.push((costs.per_edge[position].unwrap_or(0.0), y));
    }
    let retention_share = |v: &PhysicalCandidateId| {
        costs
            .retention_cost_per_retained_second
            .get(v)
            .copied()
            .unwrap_or(0.0)
    };
    for (v, &h) in &variables.h {
        if !costs.raw_data_source.contains_key(v) {
            terms.push((retention_share(v), h));
        }
    }
    let mut executions_of: HashMap<PhysicalCandidateId, Vec<usize>> = HashMap::new();
    for (index, execution) in selectable.executions().iter().enumerate() {
        if kept[index] {
            executions_of
                .entry(execution.candidate)
                .or_default()
                .push(index);
        }
    }
    let mut raw_data_rows = Vec::new();
    let mut scans: Vec<(&PhysicalCandidateId, &Source)> = costs.raw_data_source.iter().collect();
    scans.sort_unstable_by_key(|(v, _)| **v);
    for (v, source) in scans {
        let r = raw_data_on_disk_of(source);
        if let Some(&h) = variables.h.get(v) {
            let per_retained_second = retention_share(v);
            if per_retained_second > 0.0 {
                raw_data_rows.push((r, per_retained_second, h));
            }
        } else if let Some(&d) = costs.unbounded_raw_data_on_disk_cost_per_second.get(v) {
            for &index in executions_of.get(v).into_iter().flatten() {
                raw_data_rows.push((r, d, variables.x[index]));
            }
        }
    }
    for read in &costs.raw_data_reads {
        if let (Some(reader), true) = (
            read.reader
                .variable(read.execution, &variables.y, &variables.z),
            read.raw_data_kept > 0.0,
        ) {
            raw_data_rows.push((
                raw_data_on_disk_of(&read.source),
                read.raw_data_kept,
                reader,
            ));
        }
    }
    (terms, raw_data_rows)
}

/// What every solve of the selection MILP builds first: the variables
/// ([`selection_variables`]) and, when some query has an accuracy target,
/// (K)'s variables and rows, added to `problem`.
pub(super) struct SelectionModel {
    pub(super) variables: SelectionVariables,
    /// The strongly connected components of the selectable edges, for (E).
    pub(super) components: Components,
    /// (K)'s rows; empty without accuracy targets.
    pub(super) accuracy_rows: Vec<Constraint>,
    /// How many variables `problem` held before (K)'s were added.
    pub(super) selection_variable_count: usize,
}

/// [`SelectionModel`] over the executions `kept`, with (K) under `rules`.
pub(super) fn selection_model(
    inputs: &SelectionInputs<'_>,
    kept: &[bool],
    rules: Option<&AccuracyRules>,
    problem: &mut ProblemVariables,
) -> SelectionModel {
    let components = Components::of(inputs);
    let shared = copies_in_equivalent_contexts(inputs, kept, rules);
    let variables = selection_variables(inputs, kept, &shared, &components, problem);
    let selection_variable_count = problem.len();
    let accuracy_rows = rules
        .map(|rules| {
            let error_model = ErrorModel::new(inputs, rules, &variables.y, &shared);
            let accuracy = AccuracyVariables::new(&error_model, problem);
            accuracy_constraints(
                inputs,
                &error_model,
                &accuracy,
                &variables.y,
                &variables.z,
                &shared,
                problem,
            )
        })
        .unwrap_or_default();
    SelectionModel {
        variables,
        components,
        accuracy_rows,
        selection_variable_count,
    }
}

/// The rows of the selection's structure, in order: (A), (B), (C)/(D),
/// (E), (F), (R) and (X).
pub(super) fn structure_constraints(
    inputs: &SelectionInputs<'_>,
    variables: &SelectionVariables,
    components: &Components,
) -> Vec<Constraint> {
    [
        input_satisfaction_constraints(inputs, variables),
        producer_selection_constraints(inputs, variables),
        query_coverage_constraints(inputs, variables),
        acyclicity_constraints(inputs, variables, components),
        use_constraints(inputs, variables),
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
    .collect()
}

/// HiGHS's column of each variable of `problem`: the variables in the
/// order they were added.
pub(super) fn column_of(problem: &ProblemVariables) -> HashMap<Variable, usize> {
    problem
        .iter_variables_with_def()
        .enumerate()
        .map(|(column, (variable, _))| (variable, column))
        .collect()
}

/// The column values of a solved MILP of `variable_count` variables: all 0
/// for an empty model (no constraint: no query reads an operator, as with
/// literal-only scalar roots, so nothing is selected), and the solution of
/// a feasible plan at an optimum or at a node limit; otherwise HiGHS's
/// status, for the caller's error.
pub(super) fn solution_columns(
    solved: &highs::SolvedModel,
    variable_count: usize,
) -> Result<Vec<f64>, highs::HighsModelStatus> {
    use highs::{HighsModelStatus, HighsSolutionStatus};
    match solved.status() {
        HighsModelStatus::ModelEmpty => Ok(vec![0.0; variable_count]),
        HighsModelStatus::Optimal | HighsModelStatus::ReachedSolutionLimit
            if solved.primal_solution_status() == HighsSolutionStatus::Feasible =>
        {
            Ok(solved.get_solution().columns().to_vec())
        }
        other => Err(other),
    }
}

/// The executions ($x$) and edges ($y$) a solution selects, by position,
/// ascending; `chosen` says whether a variable is 1 in it.
pub(super) fn selected_executions_and_edges(
    variables: &SelectionVariables,
    chosen: impl Fn(Variable) -> bool,
) -> (Vec<usize>, Vec<usize>) {
    let selected: Vec<usize> = (0..variables.x.len())
        .filter(|&index| chosen(variables.x[index]))
        .collect();
    let edges: Vec<usize> = variables
        .y
        .iter()
        .filter(|&(_, &y)| chosen(y))
        .map(|(&position, _)| position)
        .collect();
    (selected, edges)
}

/// The execution answering each query output of
/// `SelectableExecutions::answering` in a solution, in its order;
/// `chosen` says whether a variable is 1 in it.
pub(super) fn answering_executions(
    selectable: &SelectableExecutions,
    variables: &SelectionVariables,
    chosen: impl Fn(Variable) -> bool,
) -> Vec<(QueryId, usize)> {
    selectable
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
        .collect()
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
    /// The MILP's own optimum, unscaled, less the request overhead, under
    /// cost model 1: in debug builds `crate::select_plan` checks it against
    /// `asap_cost::plan_cost` of the selected plan. `None` under cost model
    /// 3, whose objective is latency.
    pub(crate) milp_optimum_per_second: Option<f64>,
    /// The MILP's variables and constraint rows.
    pub(crate) variable_count: usize,
    pub(crate) constraint_count: usize,
    pub(crate) construction_ms: u64,
    /// The time of every HiGHS solve, summed.
    pub(crate) solve_ms: u64,
    /// How many times HiGHS solved the MILP: 1, or 2 when the selection at
    /// HiGHS's default tolerances missed an accuracy target when checked
    /// exactly and the MILP was solved again at
    /// [`LEAST_HIGHS_FEASIBILITY_TOLERANCE`].
    pub(crate) solve_count: usize,
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
    /// The solver's selection misses `query`'s accuracy target when
    /// checked exactly: its limits, such as a failure probability of
    /// 1e-12 for each covered value (δ = 0.01 over 1e10 values), are below the
    /// solver's least feasibility tolerance,
    /// [`LEAST_HIGHS_FEASIBILITY_TOLERANCE`], so (K) cannot be trusted.
    TargetBelowSolverPrecision {
        query: QueryId,
        /// The union bound the target's δ is checked by, with its limit
        /// (`AccuracyTarget::failure_probability_bound`); `None` without
        /// one.
        failure_probability_bound: Option<FailureProbabilityBound>,
    },
}

/// The smallest primal and MIP feasibility tolerance HiGHS accepts, 1e-10
/// (absolute; HiGHS's default is 1e-7). A second solve uses it when the
/// selection at the default tolerances misses an accuracy target: a
/// per-value δ of 1e-6 (δ = 0.01 over 1e4 values) may be passed by up to
/// 1e-7 at the default, but only by 1e-10 here. With the objective scaled
/// to [`LARGEST_SCALED_OBJECTIVE_COEFFICIENT`], a raw-data row's activity
/// can reach about 1e6, where one rounding step is about 1.2e-10, so this
/// tolerance is at the limit of double precision on those rows.
const LEAST_HIGHS_FEASIBILITY_TOLERANCE: f64 = 1e-10;

/// The objective's largest coefficient once scaled, 1e6. HiGHS ends its
/// search when no branch can beat the best plan found by more than its MIP
/// feasibility tolerance (absolute, 1e-6), so plans within 1e-6 of each
/// other after scaling tie, and which one it returns depends on its search
/// path (row order, build). At 1e6 that margin is 1e-12 of the largest
/// coefficient. Example: two quantiles over the same 5 minutes, raw data
/// retained, have three plans within 5.5e-10 of each other (relative);
/// scaled to 1,000 their differences (6e-7) were ties, and two machines
/// returned different plans; at 1e6 every row order returns the cheapest.
const LARGEST_SCALED_OBJECTIVE_COEFFICIENT: f64 = 1e6;

/// The factor that scales an objective whose largest absolute coefficient
/// is `largest` to [`LARGEST_SCALED_OBJECTIVE_COEFFICIENT`]; 1 when
/// `largest` is 0 or not finite.
pub(super) fn objective_scale(largest: f64) -> f64 {
    match largest > 0.0 && largest.is_finite() {
        true => LARGEST_SCALED_OBJECTIVE_COEFFICIENT / largest,
        false => 1.0,
    }
}

/// Builds and solves the MILP: minimize
/// $\sum_v W^{\mathrm{mat}}_v x^{\mathrm{mat}}_v + \sum_{\kappa,v}
/// \rho_\kappa c_{\kappa,v} x_{\kappa,v} + \sum \rho_\kappa c_{\kappa,u,c,i}
/// y_{\kappa,u,c,i} + \sum_v \pi_v h_v + \sum_s r_s$ ($\pi_v h_v$ of a
/// query-time scan is in $r_s$) under (A)–(F), [`use_constraints`],
/// the raw-data rows of [`selection_cost_terms`], (R), (X) and (K), with HiGHS at gap 0 on one
/// thread. The executions dearer than a feasible plan
/// ([`dearer_than_a_feasible_plan`]) or dominated by their lifetime twin
/// ([`dominated_by_lifetime_twin`]) are left out first (and a
/// copy of an equivalent context shares its original's variables,
/// [`copies_in_equivalent_contexts`]), with
/// the $h_v$ of a candidate whose
/// executions are all left out, then the coefficients kept are scaled so
/// the largest is [`LARGEST_SCALED_OBJECTIVE_COEFFICIENT`] (HiGHS's
/// tolerances are absolute). HiGHS solves
/// at its default tolerances and the selection is then checked exactly
/// ([`AccuracyRules::selection_meets_targets`]); a selection that misses a
/// target is solved once more at [`LEAST_HIGHS_FEASIBILITY_TOLERANCE`],
/// and if that one misses too, or finds no plan at all, the error is
/// [`SelectionMilpError::TargetBelowSolverPrecision`].
pub(crate) fn solve_selection(
    inputs: &SelectionInputs<'_>,
) -> Result<SolvedSelection, SelectionMilpError> {
    let started = Instant::now();
    let costs = inputs.costs;
    let rules = AccuracyRules::of(inputs);
    let kept: Vec<bool> = dearer_than_a_feasible_plan(inputs, rules.as_ref())
        .iter()
        .zip(dominated_by_lifetime_twin(inputs))
        .zip(&costs.per_execution)
        .map(|((&dearer, dominated), cost)| cost.is_ok() && !dearer && !dominated)
        .collect();
    let mut problem = ProblemVariables::new();
    // (K), only when some query has an accuracy target.
    let SelectionModel {
        variables,
        components,
        accuracy_rows,
        ..
    } = selection_model(inputs, &kept, rules.as_ref(), &mut problem);
    // The windows cached views keep resident in memory, $r_v$ and (W).
    let mut resident_rows = Vec::new();
    let resident_terms = resident_window_terms(
        costs,
        inputs.selectable,
        &variables.y,
        &variables.z,
        &mut problem,
        &mut resident_rows,
    );
    // The data plane's result and answer caches.
    let kept_terms = kept_result_terms(
        inputs,
        costs,
        &KeptResultVariables {
            x: &variables.x,
            z: &variables.z,
            h: &variables.h,
        },
        &kept,
        &mut problem,
        &mut resident_rows,
    );
    // Cost model 1's objective is `costs` itself: every y is on a costed
    // edge and every h_v has a retention share (`selection_cost_terms`
    // reads a missing one as 0, which only cost model 3's quantities have).
    assert!(
        variables
            .y
            .keys()
            .all(|&position| costs.per_edge[position].is_some()),
        "a costed edge"
    );
    assert!(
        variables
            .h
            .keys()
            .all(|v| costs.retention_cost_per_retained_second.contains_key(v)),
        "a retention share for each h_v"
    );
    // The objective's terms per second, unscaled: (cost, variable). A
    // query-time scan's retention is in its source's r_s.
    let (mut terms, raw_data_rows) =
        selection_cost_terms(inputs.selectable, costs, &variables, &kept, |source| {
            variables.raw_data_on_disk[source]
        });
    terms.extend(resident_terms);
    terms.extend(kept_terms);
    // Scaled by the largest coefficient kept, the raw-data rows' included.
    let largest = terms
        .iter()
        .map(|(cost, _)| *cost)
        .chain(raw_data_rows.iter().map(|&(_, coefficient, _)| coefficient))
        .fold(0.0_f64, |largest, cost| largest.max(cost.abs()));
    let scale = objective_scale(largest);
    let constraints: Vec<Constraint> = [
        structure_constraints(inputs, &variables, &components),
        // The objective's scale is in the raw-data rows, so $r_s$ is the
        // scaled cost and its objective coefficient is 1.
        raw_data_rows
            .iter()
            .map(|&(r, coefficient, variable)| constraint!(r >= coefficient * scale * variable))
            .collect(),
        resident_rows,
        accuracy_rows,
    ]
    .into_iter()
    .flatten()
    .collect();
    // The raw data on disk is scaled by its rows ([`selection_cost_terms`]).
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
    let column_of = column_of(&problem);
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
    let selectable = inputs.selectable;
    let executions = selectable.executions();
    let mut solve_ms = 0;
    let mut solve_count = 0;
    // The query the first solve's selection missed, checked exactly: the
    // error names it if the second solve finds no plan at all.
    let mut missed_at_default_tolerance: Option<QueryId> = None;
    let below_solver_precision = |query: QueryId| SelectionMilpError::TargetBelowSolverPrecision {
        query,
        failure_probability_bound: inputs
            .physical
            .logical_candidates
            .failure_probability_bound(query.0),
    };
    // The MILP's optimum, unscaled, to check against `plan_cost`.
    let mut optimum_per_second = 0.0;
    let (selected, selected_edges, answering) = loop {
        let started = Instant::now();
        let solved = model
            .try_solve()
            .map_err(|status| SelectionMilpError::Solver(format!("{status:?}")))?;
        solve_ms += started.elapsed().as_millis() as u64;
        solve_count += 1;
        use highs::HighsModelStatus;
        // No node or solution limit is set, so a feasible plan is an
        // optimum.
        let columns: Vec<f64> = match solution_columns(&solved, variable_count) {
            Ok(columns) => columns,
            // At LEAST_HIGHS_FEASIBILITY_TOLERANCE no plan meets every
            // target at the tighter tolerance; the first selection missed
            // this query by less than 1e-7.
            Err(HighsModelStatus::Infeasible | HighsModelStatus::UnboundedOrInfeasible) => {
                return Err(match missed_at_default_tolerance {
                    Some(query) => below_solver_precision(query),
                    None => SelectionMilpError::Infeasible,
                })
            }
            Err(other) => return Err(SelectionMilpError::Solver(format!("{other:?}"))),
        };
        if solved.status() != HighsModelStatus::ModelEmpty {
            optimum_per_second = solved.objective_value() / scale;
        }
        let chosen = |variable: Variable| columns[column_of[&variable]] > 0.5;
        let (selected, selected_edges) = selected_executions_and_edges(&variables, chosen);
        let answering = answering_executions(selectable, &variables, chosen);
        // (K) holds only to the solver's tolerance: at the default 1e-7, a
        // per-value δ of 1e-6 may be missed by a tenth, and at 1e-10 a δ
        // near or below 1e-10 by orders of magnitude. The exact check sends
        // a selection that misses a target to a second solve at
        // LEAST_HIGHS_FEASIBILITY_TOLERANCE, and refuses one that misses
        // after it rather than return a plan that may not meet its target.
        let Some(Err(query)) = rules
            .as_ref()
            .map(|rules| rules.selection_meets_targets(inputs, &selected_edges, &answering))
        else {
            break (selected, selected_edges, answering);
        };
        if solve_count == 2 {
            return Err(below_solver_precision(query));
        }
        missed_at_default_tolerance = Some(query);
        // Solving the same HiGHS model again offers the first selection as
        // a starting solution; HiGHS checks it at the tighter tolerance and
        // drops it when it misses (as here), so it cannot bias the result.
        model = solved.into();
        model.set_option(
            "primal_feasibility_tolerance",
            LEAST_HIGHS_FEASIBILITY_TOLERANCE,
        );
        model.set_option(
            "mip_feasibility_tolerance",
            LEAST_HIGHS_FEASIBILITY_TOLERANCE,
        );
    };
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
        milp_optimum_per_second: Some(optimum_per_second),
        variable_count,
        constraint_count,
        construction_ms,
        solve_ms,
        solve_count,
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
    let selected_edges: HashSet<usize> = edges.iter().copied().collect();
    let selected_outputs: HashSet<(QueryId, usize)> = answering.iter().copied().collect();
    for read in &costs.raw_data_reads {
        if read
            .reader
            .is_selected(read.execution, &selected_edges, &selected_outputs)
        {
            let kept = raw_data_on_disk.entry(&read.source).or_default();
            *kept = kept.max(read.raw_data_kept);
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
        + raw_data_on_disk.values().sum::<f64>()
        + resident_windows_quantity(costs, inputs.selectable, edges, answering)
        + kept_results_quantity(inputs, costs, selected, answering, &retention_ms);
    (cost, retention_ms)
}

#[cfg(test)]
mod tests;

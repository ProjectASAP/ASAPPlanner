//! Cost model 3's `lowest_mean_latency`
//! (`docs/design_docs/proposals/stage3-lowest-mean-latency.md`, section
//! 2.5): the selection MILP's variables and constraints (A)–(F), (R), (X)
//! and (K), with the cluster's limits as rows and the frequency-weighted
//! mean query latency as the objective; ties by peak memory, then disk, in
//! lexicographic solves.
//!
//! Each query $q$ has its latency $L_q = F_q + E_q$: $F_q$, its freshness
//! wait, at least the allowed lateness and at least $f_G$ of every refresh
//! group $G$ whose ingestion-time execution it reads ($f_G$ = lateness +
//! queueing + the group's refresh wall time, linear in the selection;
//! big-M on the read); $E_q$, the query-time work of the executions it
//! reads ($a_{q,e}$, each once) and the producer-dependent part of the
//! edges it reads ($w_{q,i}$), plus its fixed cost per request and step.
//! Before the MILP, an execution whose least possible latency (its own
//! work, the fastest choice beneath it, and the least freshness of the
//! refresh groups it may read) is over the `max_latency_ms` of every query
//! that may read it is left out (compared in f64, away from the solver's
//! tolerances); the remaining bounds are rows.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::time::Instant;

use good_lp::solvers::highs::highs;
use good_lp::{
    constraint, variable, Constraint, Expression, IntoAffineExpression, ProblemVariables,
    SolverModel, Variable,
};

use asap_cost::plan::execution_demand::evaluation_rate;
use asap_cost::{ClusterLimits, CostInputs};
use asap_types::ir::properties::ExecutionTiming;
use asap_types::plan::materialized_view::RefreshWindow;
use asap_types::stages::PhysicalCandidateId;
use asap_types::workload::{greatest_common_divisor, DataWorkload};

use super::super::accuracy::{accuracy_constraints, AccuracyRules, AccuracyVariables, ErrorModel};
use super::super::dominance::dominated_by_one_evaluation_twin;
use super::super::equivalent_contexts::copies_in_equivalent_contexts;
use super::super::exact_only::exact_only_constraints;
use super::super::graph::Components;
use super::super::retention::retention_constraints;
use super::{
    acyclicity_constraints, input_satisfaction_constraints, producer_selection_constraints,
    query_coverage_constraints, selection_cost_per_second, selection_variables, use_constraints,
    SelectedEdge, SelectionInputs, SelectionMilpError, SelectionVariables, SolvedSelection,
};
use crate::cost::ExecutionCosts;
use crate::selectable::QueryId;

/// The largest objective coefficient once scaled, as cost model 1's.
const LARGEST_SCALED_COEFFICIENT: f64 = 1e6;

/// The relative slack a lexicographic solve keeps on an earlier optimum.
const LEXICOGRAPHIC_SLACK: f64 = 1e-9;

/// What cost model 3 selects over: the selection inputs (their costs are
/// cost model 1's, for the structure: which executions and edges can be
/// costed, and retention), and each quantity the rows read
/// ([`crate::cost::Quantity`]).
pub(crate) struct ClusterInputs<'a> {
    pub(crate) selection: SelectionInputs<'a>,
    pub(crate) query_ms: &'a ExecutionCosts,
    pub(crate) refresh_ms: &'a ExecutionCosts,
    pub(crate) cpu: &'a ExecutionCosts,
    pub(crate) memory: &'a ExecutionCosts,
    pub(crate) disk: &'a ExecutionCosts,
    pub(crate) limits: &'a ClusterLimits,
    pub(crate) cost_inputs: &'a CostInputs,
    pub(crate) data: &'a DataWorkload,
}

/// One limit of the cluster, which a plan must fit.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ClusterLimit {
    MemoryBytes,
    DiskBytes,
    CpuCores,
    /// The precompute load bound (`max_thread_utilization`).
    PrecomputeLoad,
    /// Query `query`'s `max_latency_ms`.
    QueryLatencyMs {
        query: usize,
    },
}

/// One limit that alone blocks every plan, with the least any plan meeting
/// the other limits needs.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ClusterLimitShortfall {
    pub limit: ClusterLimit,
    pub least_needed: f64,
    pub available: f64,
}

/// Why cost model 3 selected no plan.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum ClusterSelectionError {
    Milp(SelectionMilpError),
    /// No plan fits; each listed limit alone blocks every plan.
    OverLimits(Vec<ClusterLimitShortfall>),
}

/// The selection and its mean latency.
pub(crate) struct ClusterSolved {
    pub(crate) solved: SolvedSelection,
    pub(crate) mean_latency_ms: f64,
    /// The executions left out before the MILP for a latency bound.
    pub(crate) pruned_by_latency_bound: usize,
}

/// What one solve minimizes.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Objective {
    MeanLatency,
    Memory,
    Disk,
    Limit(ClusterLimit),
}

/// Bounds on earlier optima (lexicographic solves): (weighted latency sum,
/// peak memory).
#[derive(Debug, Clone, Copy, Default)]
struct Fixed {
    weighted_latency: Option<f64>,
    memory: Option<f64>,
}

/// Per query, its static facts: weight, fixed cost per request, bound, the
/// executions and edges it may read.
struct QueryShape {
    query: QueryId,
    weight: f64,
    request_ms: f64,
    max_latency_ms: Option<f64>,
    answering: Vec<usize>,
    /// Executions it may read (stopping at ingestion time), ascending.
    reach: BTreeSet<usize>,
    /// Edges between them, by position.
    edges: Vec<usize>,
}

/// The refresh groups of the ingestion-time executions: per group, its
/// window and executions; per execution, its group.
struct Groups {
    windows: Vec<RefreshWindow>,
    executions: Vec<Vec<usize>>,
    of_execution: HashMap<usize, usize>,
    /// Groups each group may read (an edge from one of its executions into
    /// one of the group's).
    reads: Vec<BTreeSet<usize>>,
}

/// The values a solve returns.
struct Solution {
    selected: Vec<usize>,
    edges: Vec<usize>,
    answering: Vec<(QueryId, usize)>,
    latency_ms: Vec<f64>,
    objective: f64,
    variable_count: usize,
    constraint_count: usize,
    solve_ms: u64,
}

/// Select under cost model 3's `lowest_mean_latency` (module doc).
pub(crate) fn solve_cluster_selection(
    inputs: &ClusterInputs<'_>,
) -> Result<ClusterSolved, ClusterSelectionError> {
    let started = Instant::now();
    let selection = &inputs.selection;
    let selectable = selection.selectable;
    let executions = selectable.executions();
    let shapes = query_shapes(inputs);
    let groups = refresh_groups(inputs);
    // Left out: executions that cannot be costed, those their
    // `OneEvaluation` twin dominates in every quantity, and those whose
    // least latency is over every reader's bound.
    let mut costed: Vec<bool> = selection
        .costs
        .per_execution
        .iter()
        .map(|cost| cost.is_ok())
        .collect();
    let quantities = [
        inputs.query_ms,
        inputs.refresh_ms,
        inputs.cpu,
        inputs.memory,
        inputs.disk,
    ];
    let mut dominated = vec![true; executions.len()];
    for costs in quantities {
        let of = dominated_by_one_evaluation_twin(&SelectionInputs {
            costs,
            ..*selection
        });
        for (all, one) in dominated.iter_mut().zip(of) {
            *all &= one;
        }
    }
    for (costed, dominated) in costed.iter_mut().zip(dominated) {
        *costed &= !dominated;
    }
    // Kept with the latency bounds of `active` only.
    let kept_for = |active: &[ClusterLimit]| -> (Vec<bool>, usize) {
        let over = over_every_readers_bound(inputs, &shapes, &costed, active);
        let pruned = over.iter().filter(|&&over| over).count();
        let kept = costed
            .iter()
            .zip(&over)
            .map(|(&costed, &over)| costed && !over)
            .collect();
        (kept, pruned)
    };
    let all_limits = || -> Vec<ClusterLimit> {
        let mut limits = vec![
            ClusterLimit::MemoryBytes,
            ClusterLimit::DiskBytes,
            ClusterLimit::PrecomputeLoad,
        ];
        if inputs.limits.cpu_cores.is_some() {
            limits.push(ClusterLimit::CpuCores);
        }
        for shape in &shapes {
            if shape.max_latency_ms.is_some() {
                limits.push(ClusterLimit::QueryLatencyMs {
                    query: shape.query.0,
                });
            }
        }
        limits
    };
    let solve = |objective: Objective, fixed: Fixed, active: &[ClusterLimit]| {
        let (kept, _) = kept_for(active);
        build_and_solve(inputs, &shapes, &groups, &kept, objective, fixed, active)
    };
    let limits = all_limits();
    let (_, pruned_by_latency_bound) = kept_for(&limits);
    let mut solve_count = 1;
    let first = match solve(Objective::MeanLatency, Fixed::default(), &limits) {
        Ok(first) => first,
        Err(SelectionMilpError::Infeasible) => {
            return Err(diagnose(&limits, |objective, active| {
                solve(objective, Fixed::default(), active).ok()
            }));
        }
        Err(error) => return Err(ClusterSelectionError::Milp(error)),
    };
    let mut solve_ms = first.solve_ms;
    let weighted = first.objective;
    let fixed = Fixed {
        weighted_latency: Some(weighted),
        memory: None,
    };
    let second = solve(Objective::Memory, fixed, &limits).map_err(ClusterSelectionError::Milp)?;
    solve_count += 1;
    solve_ms += second.solve_ms;
    let fixed = Fixed {
        memory: Some(second.objective),
        ..fixed
    };
    let last = solve(Objective::Disk, fixed, &limits).map_err(ClusterSelectionError::Milp)?;
    solve_count += 1;
    solve_ms += last.solve_ms;
    // (K) holds only to the solver's tolerance: a selection that misses a
    // target, checked exactly, is refused.
    if let Some(rules) = AccuracyRules::of(selection) {
        if let Err(query) = rules.selection_meets_targets(selection, &last.edges, &last.answering) {
            return Err(ClusterSelectionError::Milp(
                SelectionMilpError::TargetBelowSolverPrecision {
                    query,
                    failure_probability_bound: selection
                        .physical
                        .logical_candidates
                        .failure_probability_bound(query.0),
                },
            ));
        }
    }
    let weights: f64 = shapes.iter().map(|shape| shape.weight).sum();
    let mean_latency_ms = shapes
        .iter()
        .zip(&last.latency_ms)
        .map(|(shape, latency)| shape.weight * latency)
        .sum::<f64>()
        / weights.max(f64::MIN_POSITIVE);
    let (_, retention_ms) =
        selection_cost_per_second(selection, &last.selected, &last.edges, &last.answering);
    let solved = SolvedSelection {
        executions: last
            .selected
            .iter()
            .map(|&index| executions[index])
            .collect(),
        edges: last
            .edges
            .iter()
            .map(|&position| {
                let edge = selectable.edges()[position];
                SelectedEdge {
                    consumer: executions[edge.consumer],
                    edge: edge.edge,
                    producer: executions[edge.producer],
                }
            })
            .collect(),
        outputs: last
            .answering
            .iter()
            .map(|&(_, execution)| executions[execution])
            .collect(),
        retention_ms,
        cost_per_second_without_request_overhead: 0.0,
        variable_count: last.variable_count,
        constraint_count: last.constraint_count,
        construction_ms: (started.elapsed().as_millis() as u64).saturating_sub(solve_ms),
        solve_ms,
        solve_count,
    };
    Ok(ClusterSolved {
        solved,
        mean_latency_ms,
        pruned_by_latency_bound,
    })
}

/// Which limits alone block every plan: for each, a solve without it that
/// minimizes its quantity; a limit whose removal admits a plan is listed
/// with that plan's least need. None: today's `Infeasible`.
fn diagnose(
    limits: &[ClusterLimit],
    solve: impl Fn(Objective, &[ClusterLimit]) -> Option<Solution>,
) -> ClusterSelectionError {
    let mut shortfalls = Vec::new();
    for &limit in limits {
        let others: Vec<ClusterLimit> = limits.iter().copied().filter(|&l| l != limit).collect();
        if let Some(solution) = solve(Objective::Limit(limit), &others) {
            shortfalls.push((limit, solution.objective));
        }
    }
    if shortfalls.is_empty() {
        return ClusterSelectionError::Milp(SelectionMilpError::Infeasible);
    }
    ClusterSelectionError::OverLimits(
        shortfalls
            .into_iter()
            .map(|(limit, least_needed)| ClusterLimitShortfall {
                limit,
                least_needed,
                available: f64::NAN,
            })
            .collect(),
    )
}

/// Each query's weight (its evaluations per second), fixed cost per
/// request, bound, and the executions and edges it may read: from its
/// answering executions down the selectable edges, stopping at
/// ingestion-time executions.
fn query_shapes(inputs: &ClusterInputs<'_>) -> Vec<QueryShape> {
    let selection = &inputs.selection;
    let selectable = selection.selectable;
    let table = &inputs.cost_inputs.measured_operation_costs;
    let horizon = inputs.cost_inputs.non_repeating_query_horizon_seconds;
    let mut by_query: BTreeMap<QueryId, Vec<usize>> = BTreeMap::new();
    for (query, answering) in selectable.answering() {
        by_query
            .entry(*query)
            .or_default()
            .extend(answering.iter().copied());
    }
    by_query
        .into_iter()
        .map(|(query, answering)| {
            let entry = selection.entries.get(query.0);
            let weight = evaluation_rate(
                entry.map(|entry| (&entry.recurrence, entry.range_evaluation)),
                horizon,
            )
            .unwrap_or(0.0);
            let steps = entry
                .and_then(|entry| entry.range_evaluation)
                .map_or(1, |range| range.evaluations_per_request());
            let request_ms =
                (table.ns_per_evaluation + table.ns_per_evaluation_step * steps as f64) * 1e-6;
            let mut reach = BTreeSet::new();
            let mut edges = Vec::new();
            let mut stack = answering.clone();
            while let Some(execution) = stack.pop() {
                if !reach.insert(execution) || at_ingestion(inputs, execution) {
                    continue;
                }
                for input in 0..selectable.input_count(execution) {
                    for &position in selectable.edges_into(execution, input) {
                        edges.push(position);
                        stack.push(selectable.edges()[position].producer);
                    }
                }
            }
            edges.sort_unstable();
            edges.dedup();
            QueryShape {
                query,
                weight,
                request_ms,
                max_latency_ms: entry.and_then(|entry| entry.requirements.max_latency_ms),
                answering,
                reach,
                edges,
            }
        })
        .collect()
}

/// Whether execution `index` runs at ingestion time.
fn at_ingestion(inputs: &ClusterInputs<'_>, index: usize) -> bool {
    let v = inputs.selection.selectable.executions()[index].candidate;
    inputs.selection.physical.physical_candidates[v.0]
        .physical_properties
        .timing()
        == ExecutionTiming::IngestionTime
}

/// The refresh groups of the ingestion-time executions by refresh window
/// (`RefreshWindow::computed_from`, as the installed plan refreshes them).
fn refresh_groups(inputs: &ClusterInputs<'_>) -> Groups {
    let selection = &inputs.selection;
    let selectable = selection.selectable;
    let mut groups = Groups {
        windows: Vec::new(),
        executions: Vec::new(),
        of_execution: HashMap::new(),
        reads: Vec::new(),
    };
    for (index, execution) in selectable.executions().iter().enumerate() {
        if !at_ingestion(inputs, index) {
            continue;
        }
        let Some(window) = RefreshWindow::computed_from(selection.physical, execution.candidate)
        else {
            continue;
        };
        let group = match groups.windows.iter().position(|&w| w == window) {
            Some(group) => group,
            None => {
                groups.windows.push(window);
                groups.executions.push(Vec::new());
                groups.reads.push(BTreeSet::new());
                groups.windows.len() - 1
            }
        };
        groups.executions[group].push(index);
        groups.of_execution.insert(index, group);
    }
    for edge in selectable.edges() {
        if let (Some(&consumer), Some(&producer)) = (
            groups.of_execution.get(&edge.consumer),
            groups.of_execution.get(&edge.producer),
        ) {
            if consumer != producer {
                groups.reads[consumer].insert(producer);
            }
        }
    }
    groups
}

/// Whether windows `a` and `b` can close at one instant.
fn close_together(a: RefreshWindow, b: RefreshWindow) -> bool {
    let gcd = greatest_common_divisor(a.width_ms, b.width_ms).max(1);
    a.phase_ms % gcd == b.phase_ms % gcd
}

/// A cost of `costs` that could be costed, 0 otherwise.
fn of(costs: &ExecutionCosts, index: usize) -> f64 {
    costs.per_execution[index]
        .as_ref()
        .copied()
        .unwrap_or(0.0)
        .max(0.0)
}

/// Per execution, whether its least possible latency is over the
/// `max_latency_ms` of every query that may read it (S2): its least work,
/// its own and the least of each input's choices, plus its least
/// freshness, the allowed lateness and, at ingestion time, its own refresh
/// with the wait per run. Iterated from 0 (a lower bound at every step)
/// until stable, so cycles of selectable edges are bounded too.
fn over_every_readers_bound(
    inputs: &ClusterInputs<'_>,
    shapes: &[QueryShape],
    kept: &[bool],
    active: &[ClusterLimit],
) -> Vec<bool> {
    let selectable = inputs.selection.selectable;
    let count = selectable.executions().len();
    let lateness = inputs.cost_inputs.data_plane_execution.allowed_lateness_ms as f64;
    let wait_per_run = inputs
        .cost_inputs
        .measured_operation_costs
        .refresh_io_wait
        .ns_per_refresh
        * 1e-6;
    let mut work = vec![0.0_f64; count];
    let mut fresh = vec![lateness; count];
    for _ in 0..count.max(1) {
        let mut changed = false;
        for index in 0..count {
            if !kept[index] {
                continue;
            }
            let (new_work, new_fresh) = if at_ingestion(inputs, index) {
                (0.0, lateness + of(inputs.refresh_ms, index) + wait_per_run)
            } else {
                let mut least_work = of(inputs.query_ms, index);
                let mut least_fresh = lateness;
                for input in 0..selectable.input_count(index) {
                    let choices: Vec<(f64, f64)> = selectable
                        .edges_into(index, input)
                        .iter()
                        .filter(|&&position| kept[selectable.edges()[position].producer])
                        .map(|&position| {
                            let producer = selectable.edges()[position].producer;
                            let edge = inputs.query_ms.per_edge[position].unwrap_or(0.0);
                            (edge + work[producer], fresh[producer])
                        })
                        .collect();
                    if choices.is_empty() {
                        continue;
                    }
                    least_work += choices
                        .iter()
                        .map(|&(work, _)| work)
                        .fold(f64::INFINITY, f64::min)
                        .max(0.0);
                    least_fresh = least_fresh.max(
                        choices
                            .iter()
                            .map(|&(_, fresh)| fresh)
                            .fold(f64::INFINITY, f64::min),
                    );
                }
                (least_work, least_fresh)
            };
            if new_work > work[index] + 1e-12 || new_fresh > fresh[index] + 1e-12 {
                work[index] = work[index].max(new_work);
                fresh[index] = fresh[index].max(new_fresh);
                changed = true;
            }
        }
        if !changed {
            break;
        }
    }
    (0..count)
        .map(|index| {
            let readers: Vec<&QueryShape> = shapes
                .iter()
                .filter(|shape| shape.reach.contains(&index))
                .collect();
            let least = work[index] + fresh[index];
            !readers.is_empty()
                && readers.iter().all(|shape| {
                    let bounded = active.contains(&ClusterLimit::QueryLatencyMs {
                        query: shape.query.0,
                    });
                    bounded
                        && shape
                            .max_latency_ms
                            .is_some_and(|bound| least + shape.request_ms > bound * (1.0 + 1e-9))
                })
        })
        .collect()
}

/// `costs` over the selection as one linear expression: each execution's
/// and edge's share times its variable, each retention share times $h_v$,
/// and per source the most raw data a selected scan keeps (a variable at
/// least each, added to `problem` with its rows).
fn quantity_expression(
    inputs: &ClusterInputs<'_>,
    costs: &ExecutionCosts,
    variables: &SelectionVariables,
    kept: &[bool],
    problem: &mut ProblemVariables,
    rows: &mut Vec<Constraint>,
) -> Expression {
    let selectable = inputs.selection.selectable;
    let mut expression = Expression::default();
    for (index, &x) in variables.x.iter().enumerate() {
        if kept[index] {
            expression += of(costs, index) * x;
        }
    }
    for (&position, &y) in &variables.y {
        expression += costs.per_edge[position].unwrap_or(0.0) * y;
    }
    for (v, &h) in &variables.h {
        if !costs.raw_data_source.contains_key(v) {
            let share = costs
                .retention_cost_per_retained_second
                .get(v)
                .copied()
                .unwrap_or(0.0);
            expression += share * h;
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
    let mut most_of_source: BTreeMap<String, Variable> = BTreeMap::new();
    let mut scans: Vec<_> = costs.raw_data_source.iter().collect();
    scans.sort_unstable_by_key(|(v, _)| **v);
    for (v, source) in scans {
        let r = *most_of_source
            .entry(format!("{source:?}"))
            .or_insert_with(|| problem.add(variable().min(0)));
        if let Some(&h) = variables.h.get(v) {
            let share = costs
                .retention_cost_per_retained_second
                .get(v)
                .copied()
                .unwrap_or(0.0);
            rows.push(constraint!(r >= share * h));
        } else if let Some(&kept_bytes) = costs.unbounded_raw_data_on_disk_cost_per_second.get(v) {
            for &index in executions_of.get(v).into_iter().flatten() {
                rows.push(constraint!(r >= kept_bytes * variables.x[index]));
            }
        }
    }
    for r in most_of_source.into_values() {
        expression += r;
    }
    expression
}

/// The largest absolute coefficient of `expression`'s variables.
fn largest_coefficient(expression: &Expression) -> f64 {
    expression
        .linear_coefficients()
        .map(|(_, coefficient)| coefficient.abs())
        .fold(0.0, f64::max)
}

/// Build the MILP with the `active` limits and solve it for `objective`,
/// with the earlier optima `fixed`.
fn build_and_solve(
    inputs: &ClusterInputs<'_>,
    shapes: &[QueryShape],
    groups: &Groups,
    kept: &[bool],
    objective: Objective,
    fixed: Fixed,
    active: &[ClusterLimit],
) -> Result<Solution, SelectionMilpError> {
    let selection = &inputs.selection;
    let selectable = selection.selectable;
    let execution = &inputs.cost_inputs.data_plane_execution;
    let rules = AccuracyRules::of(selection);
    let components = Components::of(selection);
    let shared = copies_in_equivalent_contexts(selection, kept, rules.as_ref());
    let mut problem = ProblemVariables::new();
    let variables = selection_variables(selection, kept, &shared, &components, &mut problem);
    let accuracy_rows = rules
        .as_ref()
        .map(|rules| {
            let error_model = ErrorModel::new(selection, rules, &variables.y, &shared);
            let accuracy = AccuracyVariables::new(&error_model, &mut problem);
            accuracy_constraints(
                selection,
                &error_model,
                &accuracy,
                &variables.y,
                &variables.z,
                &shared,
                &mut problem,
            )
        })
        .unwrap_or_default();
    let mut rows: Vec<Constraint> = Vec::new();
    // The quantities' expressions.
    let memory = quantity_expression(
        inputs,
        inputs.memory,
        &variables,
        kept,
        &mut problem,
        &mut rows,
    );
    let disk = quantity_expression(
        inputs,
        inputs.disk,
        &variables,
        kept,
        &mut problem,
        &mut rows,
    );
    let cpu = quantity_expression(
        inputs,
        inputs.cpu,
        &variables,
        kept,
        &mut problem,
        &mut rows,
    );
    // Refresh groups: each one's refresh wall time per window, P_G.
    let wait_per_run = inputs
        .cost_inputs
        .measured_operation_costs
        .refresh_io_wait
        .ns_per_refresh
        * 1e-6;
    let mut refresh: Vec<Expression> = Vec::new();
    let mut refresh_bound: Vec<f64> = Vec::new();
    for members in &groups.executions {
        let mut expression = Expression::default();
        let mut bound = 0.0;
        for &index in members {
            if kept[index] {
                let p = of(inputs.refresh_ms, index);
                expression += p * variables.x[index];
                bound += p;
            }
        }
        for (&position, &y) in &variables.y {
            if members.contains(&selectable.edges()[position].consumer) {
                let p = inputs.refresh_ms.per_edge[position].unwrap_or(0.0);
                expression += p * y;
                bound += p.max(0.0);
            }
        }
        if wait_per_run > 0.0 {
            let any = problem.add(variable().binary());
            for &index in members {
                rows.push(constraint!(any >= variables.x[index]));
            }
            expression += wait_per_run * any;
            bound += wait_per_run;
        }
        refresh.push(expression);
        refresh_bound.push(bound);
    }
    let threads = f64::from(execution.precompute_threads);
    let lateness = execution.allowed_lateness_ms as f64;
    // f_G = lateness + Q_G + P_G.
    let freshness: Vec<Expression> = (0..groups.windows.len())
        .map(|group| {
            let mut expression = Expression::from(lateness) + refresh[group].clone();
            for (other, (other_refresh, &other_window)) in
                refresh.iter().zip(&groups.windows).enumerate()
            {
                if other == group {
                    continue;
                }
                if groups.reads[group].contains(&other) {
                    expression += other_refresh.clone();
                } else if close_together(groups.windows[group], other_window) {
                    expression += other_refresh.clone() * (1.0 / threads);
                }
            }
            expression
        })
        .collect();
    // Per group, the most f_G can exceed the allowed lateness (F_q's own
    // lower bound): its big-M, as tight as the bounds allow.
    let big_m: Vec<f64> = (0..groups.windows.len())
        .map(|group| {
            let mut bound = refresh_bound[group];
            for (other, (&other_bound, &other_window)) in
                refresh_bound.iter().zip(&groups.windows).enumerate()
            {
                if other == group {
                    continue;
                }
                if groups.reads[group].contains(&other) {
                    bound += other_bound;
                } else if close_together(groups.windows[group], other_window) {
                    bound += other_bound / threads;
                }
            }
            bound
        })
        .collect();
    // Per query: reads a, edge reads w, freshness F, latency L.
    let mut latencies: Vec<Variable> = Vec::new();
    for shape in shapes {
        let reads: BTreeMap<usize, Variable> = shape
            .reach
            .iter()
            .map(|&index| (index, problem.add(variable().min(0).max(1))))
            .collect();
        for &index in &shape.answering {
            if let Some(&z) = variables.z.get(&(shape.query, index)) {
                rows.push(constraint!(reads[&index] >= z));
            }
        }
        let fresh = problem.add(variable().min(lateness));
        let latency = problem.add(variable().min(0));
        let mut execution_ms = Expression::from(shape.request_ms);
        for &position in &shape.edges {
            let Some(&y) = variables.y.get(&position) else {
                continue;
            };
            let edge = selectable.edges()[position];
            let (Some(&consumer), Some(&producer)) =
                (reads.get(&edge.consumer), reads.get(&edge.producer))
            else {
                continue;
            };
            let read = problem.add(variable().min(0).max(1));
            rows.push(constraint!(read >= consumer + y - 1));
            rows.push(constraint!(producer >= read));
            let d = inputs.query_ms.per_edge[position].unwrap_or(0.0);
            if d < 0.0 {
                rows.push(constraint!(read <= y));
                rows.push(constraint!(read <= consumer));
            }
            execution_ms += d * read;
        }
        for (&index, &read) in &reads {
            if !kept[index] {
                continue;
            }
            match groups.of_execution.get(&index) {
                Some(&group) if at_ingestion(inputs, index) => {
                    rows.push(constraint!(
                        fresh >= freshness[group].clone() - big_m[group] * (1 - read)
                    ));
                }
                _ => execution_ms += of(inputs.query_ms, index) * read,
            }
        }
        rows.push(constraint!(latency == fresh + execution_ms));
        if let Some(bound) = shape.max_latency_ms {
            if active.contains(&ClusterLimit::QueryLatencyMs {
                query: shape.query.0,
            }) {
                rows.push(constraint!(latency <= bound));
            }
        }
        latencies.push(latency);
    }
    // The cluster's limits.
    let limits = inputs.limits;
    if active.contains(&ClusterLimit::MemoryBytes) {
        rows.push(constraint!(memory.clone() <= limits.memory_bytes as f64));
    }
    if active.contains(&ClusterLimit::DiskBytes) {
        rows.push(constraint!(disk.clone() <= limits.disk_bytes as f64));
    }
    // CPU: the plan's, the requests' and the decode of arriving samples.
    let fixed_cores: f64 = shapes
        .iter()
        .map(|shape| {
            let requests_per_second = selection
                .entries
                .get(shape.query.0)
                .and_then(|entry| {
                    evaluation_rate(
                        [(&entry.recurrence, None)],
                        inputs.cost_inputs.non_repeating_query_horizon_seconds,
                    )
                    .ok()
                })
                .unwrap_or(0.0);
            shape.request_ms * requests_per_second / 1_000.0
        })
        .sum::<f64>()
        + asap_cost::ingest_decode_cpu_core_seconds_per_second(inputs.data, inputs.cost_inputs);
    let cpu = cpu + fixed_cores;
    if let (Some(cores), true) = (limits.cpu_cores, active.contains(&ClusterLimit::CpuCores)) {
        rows.push(constraint!(cpu.clone() <= f64::from(cores)));
    }
    let mut load = Expression::default();
    for (group, window) in groups.windows.iter().enumerate() {
        load += refresh[group].clone() * (1.0 / (window.width_ms as f64 * threads));
    }
    if active.contains(&ClusterLimit::PrecomputeLoad) {
        rows.push(constraint!(
            load.clone() <= execution.max_thread_utilization
        ));
    }
    let weighted: Expression = shapes
        .iter()
        .zip(&latencies)
        .map(|(shape, &latency)| shape.weight * latency)
        .sum();
    if let Some(best) = fixed.weighted_latency {
        rows.push(constraint!(
            weighted.clone() <= best * (1.0 + LEXICOGRAPHIC_SLACK) + LEXICOGRAPHIC_SLACK
        ));
    }
    if let Some(best) = fixed.memory {
        rows.push(constraint!(
            memory.clone() <= best * (1.0 + LEXICOGRAPHIC_SLACK) + 1.0
        ));
    }
    let goal: Expression = match objective {
        Objective::MeanLatency => weighted,
        Objective::Memory | Objective::Limit(ClusterLimit::MemoryBytes) => memory,
        Objective::Disk | Objective::Limit(ClusterLimit::DiskBytes) => disk,
        Objective::Limit(ClusterLimit::CpuCores) => cpu,
        Objective::Limit(ClusterLimit::PrecomputeLoad) => load,
        Objective::Limit(ClusterLimit::QueryLatencyMs { query }) => shapes
            .iter()
            .position(|shape| shape.query.0 == query)
            .map_or_else(Expression::default, |at| Expression::from(latencies[at])),
    };
    let largest = largest_coefficient(&goal);
    let scale = match largest > 0.0 && largest.is_finite() {
        true => LARGEST_SCALED_COEFFICIENT / largest,
        false => 1.0,
    };
    let constraints: Vec<Constraint> = [
        input_satisfaction_constraints(selection, &variables),
        producer_selection_constraints(selection, &variables),
        query_coverage_constraints(selection, &variables),
        acyclicity_constraints(selection, &variables, &components),
        use_constraints(selection, &variables),
        retention_constraints(
            selection,
            &variables.h,
            &variables.x,
            &variables.y,
            &variables.z,
        ),
        exact_only_constraints(
            selection,
            &variables.read_by_query,
            &variables.y,
            &variables.z,
        ),
        accuracy_rows,
        rows,
    ]
    .into_iter()
    .flatten()
    .collect();
    let variable_count = problem.len();
    let column_of: HashMap<Variable, usize> = problem
        .iter_variables_with_def()
        .enumerate()
        .map(|(column, (variable, _))| (variable, column))
        .collect();
    let constraint_count = constraints.len();
    let goal_scaled = goal.clone() * scale;
    let mut model = problem
        .minimise(goal_scaled)
        .using(highs)
        .with_all(constraints)
        .try_into_inner()
        .map_err(|error| SelectionMilpError::Solver(error.to_string()))?;
    model.set_option("mip_rel_gap", 0.0);
    model.set_option("mip_abs_gap", 0.0);
    model.set_option("output_flag", false);
    model.set_option("threads", 1);
    let started = Instant::now();
    let solved = model
        .try_solve()
        .map_err(|status| SelectionMilpError::Solver(format!("{status:?}")))?;
    let solve_ms = started.elapsed().as_millis() as u64;
    use highs::{HighsModelStatus, HighsSolutionStatus};
    let columns: Vec<f64> = match solved.status() {
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
    let value = |variable: Variable| columns.get(column_of[&variable]).copied().unwrap_or(0.0);
    let chosen = |variable: Variable| value(variable) > 0.5;
    let selected: Vec<usize> = (0..variables.x.len())
        .filter(|&index| chosen(variables.x[index]))
        .collect();
    let edges: Vec<usize> = variables
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
    let objective_value = (&goal)
        .linear_coefficients()
        .map(|(variable, coefficient)| coefficient * value(variable))
        .sum::<f64>()
        + goal.constant();
    Ok(Solution {
        selected,
        edges,
        answering,
        latency_ms: latencies.iter().map(|&latency| value(latency)).collect(),
        objective: objective_value,
        variable_count,
        constraint_count,
        solve_ms,
    })
}

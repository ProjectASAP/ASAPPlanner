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
use asap_cost::{close_together, ClusterLimits, CostInputs};
use asap_types::ir::properties::ExecutionTiming;
use asap_types::plan::materialized_view::RefreshWindow;
use asap_types::stages::PhysicalCandidateId;
use asap_types::workload::DataWorkload;

use super::super::accuracy::{accuracy_constraints, AccuracyRules, AccuracyVariables, ErrorModel};
use super::super::dominance::{dominated_in_every_quantity, dominated_theta_families, Pairing};
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
    /// The executions of a feasible plan (cost model 1's), by position: its
    /// weighted latency bounds the optimum (T8).
    pub(crate) feasible_plan: Option<FeasiblePlan>,
}

/// A feasible plan (cost model 1's), by position: its executions, its
/// edges and the execution answering each query output reference.
pub(crate) struct FeasiblePlan {
    pub(crate) executions: BTreeSet<usize>,
    pub(crate) edges: BTreeSet<usize>,
    pub(crate) answering: BTreeSet<(QueryId, usize)>,
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
    /// The solver's lower bound on any plan's mean latency (equal to
    /// `mean_latency_ms` at a proved optimum).
    pub(crate) mean_latency_lower_bound_ms: f64,
    /// The LP relaxation's mean latency, when asked for.
    pub(crate) root_lp_mean_latency_ms: Option<f64>,
    /// Each query's latency in the MILP's optimum: (query, ms).
    pub(crate) latency_ms_per_query: Vec<(usize, f64)>,
    /// The executions left out before the MILP for a latency bound.
    pub(crate) pruned_by_latency_bound: usize,
}

/// What one solve minimizes.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Objective {
    MeanLatency,
    /// The mean latency's LP relaxation (a measurement).
    Relaxation,
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

/// One solve: what it minimizes, the earlier optima it keeps, the limits
/// it enforces and the solution it starts from.
struct Solve<'s> {
    objective: Objective,
    fixed: Fixed,
    active: &'s [ClusterLimit],
    start: Option<&'s [f64]>,
    /// A tie-break solve: only a plan whose objective is below this, within
    /// `tie_break_seconds`.
    improve_below: Option<f64>,
    tie_break_seconds: f64,
    /// A plan to pin: its selection variables fixed to 1, every other
    /// selection variable to 0; HiGHS completes the continuous ones. The
    /// pinned solve's columns are the next solve's MIP start.
    pinned: Option<&'s FeasiblePlan>,
    stop: Stop,
}

/// When a solve stops.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Stop {
    /// At a proved optimum.
    Exact,
    /// At a relative gap of [`STOP_RELATIVE_GAP`] or after
    /// [`STOP_SECONDS`], whichever comes first.
    GapOrTime,
    /// At a relative gap of [`STOP_RELATIVE_GAP`]: when no plan was found
    /// in [`STOP_SECONDS`].
    Gap,
}

/// The default stop rule of the mean latency solve: a relative gap of 1 %
/// or 30 s, whichever comes first (`LatencySelectionChecks::exact_solve`
/// turns it off).
const STOP_RELATIVE_GAP: f64 = 0.01;
const STOP_SECONDS: f64 = 30.0;

/// What a solve that ran out of time without a plan returns.
const NO_PLAN_IN_TIME: &str = "no plan within the time limit";

/// The longest a tie-break solve looks for a smaller plan: as long as the
/// first solve took, between `SHORTEST_TIE_BREAK_SECONDS` and this.
const TIE_BREAK_SECONDS: f64 = 2.0;
const SHORTEST_TIE_BREAK_SECONDS: f64 = 0.05;

/// Per query, its static facts: weight, fixed cost per request, bound, the
/// executions and edges it may read.
struct QueryShape {
    query: QueryId,
    weight: f64,
    /// The request's fixed latency, ms: its per-step part once per round.
    request_ms: f64,
    /// Rounds of steps per request, the steps split over the query
    /// threads (`asap_cost::step_rounds`): a query-time execution not kept
    /// across evaluations adds its work once per round to the latency.
    step_rounds: u64,
    /// The request's fixed CPU, ms: every step's, wherever it runs.
    request_cpu_ms: f64,
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
    /// The solver's lower bound on the objective, unscaled.
    lower_bound: f64,
    /// Every column's value, the next solve's start.
    columns: Vec<f64>,
    /// The plan's peak memory and disk, in bytes.
    memory_bytes: f64,
    disk_bytes: f64,
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
    let shapes = query_shapes(inputs).map_err(ClusterSelectionError::Milp)?;
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
    // What can be costed, before any dominance: a feasible plan's
    // executions are all in it.
    let costable = costed.clone();
    let dominated = dominated_in_every_quantity(selection, &quantities, Pairing::OneEvaluationTwin);
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
    if inputs.cost_inputs.latency_selection_checks.log_solver {
        log_duplicates(inputs, &costed);
    }
    let limits = all_limits();
    // The exclusions are computed once for the selection; only the
    // diagnosis of an infeasible one recomputes them without a bound.
    let (mut kept, pruned_by_latency_bound) = kept_for(&limits);
    // θ-families another family does as well as (T5).
    let checks = inputs.cost_inputs.latency_selection_checks;
    if checks.theta_dominance {
        let rules = AccuracyRules::of(selection);
        let dominated = dominated_theta_families(selection, &quantities, &kept, rules.as_ref());
        for (kept, dominated) in kept.iter_mut().zip(dominated) {
            *kept &= !dominated;
        }
    }
    // T8: the weighted latency of a feasible plan bounds the optimum;
    // executions no plan with them can beat it with are left out.
    if let (Some(plan), true) = (&inputs.feasible_plan, checks.feasible_plan_bound) {
        // The plan as cost model 1 selected it, whatever the dominance
        // left out: a feasible plan of the whole model.
        let only_plan: Vec<bool> = (0..kept.len())
            .map(|index| costable[index] && plan.executions.contains(&index))
            .collect();
        let bound = build_and_solve(
            inputs,
            &shapes,
            &groups,
            &only_plan,
            Solve {
                objective: Objective::MeanLatency,
                fixed: Fixed::default(),
                active: &limits,
                start: None,
                improve_below: None,
                tie_break_seconds: 0.0,
                pinned: None,
                stop: Stop::Exact,
            },
        );
        if let Ok(bound) = bound {
            let dearer = dearer_than_a_feasible_plan(inputs, &shapes, &kept, bound.objective);
            for (kept, dearer) in kept.iter_mut().zip(dearer) {
                *kept &= !dearer;
            }
        }
    }
    // The first solve's MIP start: the feasible plan with its selection
    // pinned, completed by HiGHS on the same model (same columns); a plan
    // the kept executions cannot form gives no start, which only costs time.
    let pinned_start: Option<(Vec<f64>, u64)> = inputs
        .feasible_plan
        .as_ref()
        .filter(|_| checks.feasible_plan_start)
        .and_then(|plan| {
            build_and_solve(
                inputs,
                &shapes,
                &groups,
                &kept,
                Solve {
                    objective: Objective::MeanLatency,
                    fixed: Fixed::default(),
                    active: &limits,
                    start: None,
                    improve_below: None,
                    tie_break_seconds: 0.0,
                    pinned: Some(plan),
                    stop: Stop::Exact,
                },
            )
            .ok()
        })
        .map(|pinned| (pinned.columns, pinned.solve_ms));
    let solve = |objective: Objective,
                 fixed: Fixed,
                 start: Option<&[f64]>,
                 tie_break: Option<(f64, f64)>,
                 stop: Stop| {
        build_and_solve(
            inputs,
            &shapes,
            &groups,
            &kept,
            Solve {
                objective,
                fixed,
                active: &limits,
                start,
                improve_below: tie_break.map(|(below, _)| below),
                tie_break_seconds: tie_break.map_or(0.0, |(_, seconds)| seconds),
                pinned: None,
                stop,
            },
        )
    };
    // The LP relaxation's mean latency, when asked for: the formulation's
    // bound, a measurement.
    let weights: f64 = shapes.iter().map(|shape| shape.weight).sum();
    let root_lp_mean_latency_ms = checks
        .report_root_lp_bound
        .then(|| {
            solve(
                Objective::Relaxation,
                Fixed::default(),
                None,
                None,
                Stop::Exact,
            )
            .ok()
        })
        .flatten()
        .map(|relaxed| relaxed.objective / weights.max(f64::MIN_POSITIVE));
    let mut solve_count = 1;
    let start = pinned_start.as_ref().map(|(columns, _)| columns.as_slice());
    let first = match checks.exact_solve {
        true => solve(
            Objective::MeanLatency,
            Fixed::default(),
            start,
            None,
            Stop::Exact,
        ),
        // No plan in the time limit: on to the gap alone.
        false => match solve(
            Objective::MeanLatency,
            Fixed::default(),
            start,
            None,
            Stop::GapOrTime,
        ) {
            Err(SelectionMilpError::Solver(reason)) if reason == NO_PLAN_IN_TIME => solve(
                Objective::MeanLatency,
                Fixed::default(),
                start,
                None,
                Stop::Gap,
            ),
            solved => solved,
        },
    };
    let first = match first {
        Ok(first) => first,
        Err(SelectionMilpError::Infeasible) => {
            return Err(diagnose(&limits, |objective, active| {
                let (kept, _) = kept_for(active);
                build_and_solve(
                    inputs,
                    &shapes,
                    &groups,
                    &kept,
                    Solve {
                        objective,
                        fixed: Fixed::default(),
                        active,
                        start: None,
                        improve_below: None,
                        tie_break_seconds: 0.0,
                        pinned: None,
                        stop: Stop::Exact,
                    },
                )
                .ok()
            }));
        }
        Err(error) => return Err(ClusterSelectionError::Milp(error)),
    };
    let mut solve_ms = first.solve_ms + pinned_start.as_ref().map_or(0, |&(_, ms)| ms);
    let tie_break_seconds =
        (first.solve_ms as f64 / 1_000.0).clamp(SHORTEST_TIE_BREAK_SECONDS, TIE_BREAK_SECONDS);
    let weighted = first.objective;
    let weighted_lower_bound = first.lower_bound;
    // Tie-breaks only when an alternative optimum may exist: each starts
    // from the plan so far and only looks for one at least a byte smaller
    // within `TIE_BREAK_SECONDS`; finding none (or none in time) keeps the
    // plan so far.
    let full_tie_breaks = !checks.lazy_tie_breaks;
    let mut last = first;
    for objective in [Objective::Memory, Objective::Disk] {
        let current = match objective {
            Objective::Memory => last.memory_bytes,
            _ => last.disk_bytes,
        };
        let fixed = Fixed {
            weighted_latency: Some(weighted),
            memory: (objective == Objective::Disk).then_some(last.memory_bytes),
        };
        let tried = solve(
            objective,
            fixed,
            Some(&last.columns),
            (!full_tie_breaks).then_some((current - 0.5, tie_break_seconds)),
            Stop::Exact,
        );
        solve_count += 1;
        if let Ok(better) = tried {
            solve_ms += better.solve_ms;
            let found = match objective {
                Objective::Memory => better.memory_bytes,
                _ => better.disk_bytes,
            };
            if found < current - 0.5 {
                last = better;
            }
        }
    }
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
    let mean_latency_lower_bound_ms =
        (weighted_lower_bound / weights.max(f64::MIN_POSITIVE)).min(mean_latency_ms);
    Ok(ClusterSolved {
        solved,
        mean_latency_ms,
        mean_latency_lower_bound_ms,
        root_lp_mean_latency_ms,
        latency_ms_per_query: shapes
            .iter()
            .map(|shape| shape.query.0)
            .zip(last.latency_ms.iter().copied())
            .collect(),
        pruned_by_latency_bound,
    })
}

/// For the solver log: how many executions are duplicates, equal to
/// another in context, memo group, operator, sketch parameters, physical
/// properties and the candidates each input may read.
fn log_duplicates(inputs: &ClusterInputs<'_>, kept: &[bool]) {
    let selection = &inputs.selection;
    let selectable = selection.selectable;
    let physical = selection.physical;
    let memo = &physical.logical_candidates.memo;
    let mut seen: HashMap<String, usize> = HashMap::new();
    for (index, execution) in selectable.executions().iter().enumerate() {
        if !kept[index] {
            continue;
        }
        let candidate = &physical.physical_candidates[execution.candidate.0];
        let logical = memo.candidate(candidate.logical_candidate_id);
        let producers: Vec<Vec<usize>> = (0..selectable.input_count(index))
            .map(|input| {
                let mut producers: Vec<usize> = selectable
                    .edges_into(index, input)
                    .iter()
                    .map(|&position| selectable.edges()[position].producer)
                    .collect();
                producers.sort_unstable();
                producers
            })
            .collect();
        let key = format!(
            "{:?}|{:?}|{:?}|{:?}|{:?}|{:?}",
            execution.context,
            candidate.logical_candidate_id.group,
            logical.expression.operator,
            logical.logical_properties.parameters(),
            candidate.physical_properties,
            producers
        );
        *seen.entry(key).or_default() += 1;
    }
    let duplicates: usize = seen.values().map(|&count| count - 1).sum();
    eprintln!(
        "cluster MILP: {} kept executions, {} duplicates of another",
        kept.iter().filter(|&&kept| kept).count(),
        duplicates
    );
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
fn query_shapes(inputs: &ClusterInputs<'_>) -> Result<Vec<QueryShape>, SelectionMilpError> {
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
            .map_err(|error| SelectionMilpError::Solver(format!("query {}: {error}", query.0)))?;
            let steps = entry
                .and_then(|entry| entry.range_evaluation)
                .map_or(1, |range| range.evaluations_per_request());
            let step_rounds = asap_cost::step_rounds(
                steps,
                inputs.cost_inputs.data_plane_execution.query_threads,
            );
            let request_ms = (table.ns_per_evaluation
                + table.ns_per_evaluation_step * step_rounds as f64)
                * 1e-6;
            let request_cpu_ms =
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
            Ok(QueryShape {
                query,
                weight,
                request_ms,
                step_rounds,
                request_cpu_ms,
                max_latency_ms: entry.and_then(|entry| entry.requirements.max_latency_ms),
                answering,
                reach,
                edges,
            })
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

/// A share of `costs` that could be costed, 0 otherwise.
fn of(costs: &ExecutionCosts, index: usize) -> f64 {
    costs.per_execution[index].as_ref().copied().unwrap_or(0.0)
}

/// A time share of `costs` at the largest retention (a latency row's
/// coefficient, which cannot depend on $h_v$), 0 when it could not be
/// costed.
fn time_of(costs: &ExecutionCosts, index: usize) -> f64 {
    costs.per_execution_at_largest_retention[index]
        .as_ref()
        .copied()
        .unwrap_or(0.0)
}

/// Whether execution `index` runs once per request, not per step: kept
/// across evaluations.
fn once_per_request(inputs: &ClusterInputs<'_>, index: usize) -> bool {
    let v = inputs.selection.selectable.executions()[index].candidate;
    inputs.selection.physical.physical_candidates[v.0]
        .physical_properties
        .is_across_evaluations()
}

/// Per execution, whether its least possible latency is over the
/// `max_latency_ms` of every query that may read it (S2): its least work,
/// its own and the least of each input's choices, plus its least
/// freshness, the allowed lateness and, at ingestion time, its own refresh
/// with the wait per run. The work is per step (at least one per request)
/// and each share is taken at its least retention (`per_execution`, which
/// takes the most retention can take off), so this is a lower bound on
/// every plan's latency through the execution. Iterated from 0 (every
/// execution's time is at least 0, so each iterate stays a lower bound)
/// until stable, so cycles of selectable edges are bounded too.
fn over_every_readers_bound(
    inputs: &ClusterInputs<'_>,
    shapes: &[QueryShape],
    kept: &[bool],
    active: &[ClusterLimit],
) -> Vec<bool> {
    let least = least_latency(inputs, kept);
    (0..least.len())
        .map(|index| {
            let readers: Vec<&QueryShape> = shapes
                .iter()
                .filter(|shape| shape.reach.contains(&index))
                .collect();
            !readers.is_empty()
                && readers.iter().all(|shape| {
                    let bounded = active.contains(&ClusterLimit::QueryLatencyMs {
                        query: shape.query.0,
                    });
                    bounded
                        && shape.max_latency_ms.is_some_and(|bound| {
                            least[index] + shape.request_ms > bound * (1.0 + 1e-9)
                        })
                })
        })
        .collect()
}

/// Per execution, the least latency of its sub-DAG (work plus freshness,
/// [`over_every_readers_bound`]), without a query's fixed cost per
/// request: a lower bound on any plan's latency through it.
fn least_latency(inputs: &ClusterInputs<'_>, kept: &[bool]) -> Vec<f64> {
    let selectable = inputs.selection.selectable;
    let count = selectable.executions().len();
    let lateness = inputs.cost_inputs.data_plane_execution.allowed_lateness_ms as f64;
    let wait_per_run = inputs
        .cost_inputs
        .measured_operation_costs
        .refresh_io_wait
        .ns_per_refresh
        * 1e-6;
    // Whether an execution's inputs may share executions below it (an
    // in-query diamond): their memo groups' descendants (over the
    // selectable edges) intersect. Without one, their least works add up.
    let group_of = |index: usize| {
        let v = selectable.executions()[index].candidate;
        inputs.selection.physical.physical_candidates[v.0]
            .logical_candidate_id
            .group
    };
    // Only query-time producers count: an ingestion-time execution's work
    // is in the freshness, which takes the largest, not a sum.
    let mut group_inputs: HashMap<_, BTreeSet<_>> = HashMap::new();
    for edge in selectable
        .edges()
        .iter()
        .filter(|edge| !at_ingestion(inputs, edge.producer))
    {
        group_inputs
            .entry(group_of(edge.consumer))
            .or_default()
            .insert(group_of(edge.producer));
    }
    let mut below: HashMap<_, BTreeSet<_>> = HashMap::new();
    let descendants = |group, below: &mut HashMap<_, BTreeSet<_>>| -> BTreeSet<_> {
        if let Some(known) = below.get(&group) {
            return known.clone();
        }
        let mut seen = BTreeSet::from([group]);
        let mut stack = vec![group];
        while let Some(next) = stack.pop() {
            for &input in group_inputs.get(&next).into_iter().flatten() {
                if seen.insert(input) {
                    stack.push(input);
                }
            }
        }
        below.insert(group, seen.clone());
        seen
    };
    // The most one execution of each memo group can add to a least work:
    // its own share and every positive edge share into it, at the largest
    // retention.
    let mut most_of_group: HashMap<_, f64> = HashMap::new();
    for index in (0..count).filter(|&index| kept[index] && !at_ingestion(inputs, index)) {
        let positive_shares: f64 = (0..selectable.input_count(index))
            .map(|input| {
                selectable
                    .edges_into(index, input)
                    .iter()
                    .filter_map(|&position| inputs.query_ms.per_edge[position])
                    .fold(0.0, f64::max)
            })
            .sum();
        let most = time_of(inputs.query_ms, index)
            .max(of(inputs.query_ms, index))
            .max(0.0)
            + positive_shares;
        let entry = most_of_group.entry(group_of(index)).or_insert(0.0);
        *entry = entry.max(most);
    }
    // Per execution, the most its inputs' least works may count twice: each
    // memo group below more than one input counts once per extra input, at
    // most what one of its executions adds. Their sub-DAGs' union has at
    // least the sum of their least works less this.
    let counted_twice: Vec<f64> = (0..count)
        .map(|index| {
            let mut inputs_reaching: HashMap<_, usize> = HashMap::new();
            for input in 0..selectable.input_count(index) {
                let mut reached = BTreeSet::new();
                for &position in selectable.edges_into(index, input) {
                    if at_ingestion(inputs, selectable.edges()[position].producer) {
                        continue;
                    }
                    reached.extend(descendants(
                        group_of(selectable.edges()[position].producer),
                        &mut below,
                    ));
                }
                for group in reached {
                    *inputs_reaching.entry(group).or_default() += 1;
                }
            }
            inputs_reaching
                .iter()
                .filter(|(_, &reaching)| reaching > 1)
                .map(|(group, &reaching)| {
                    (reaching - 1) as f64 * most_of_group.get(group).copied().unwrap_or(0.0)
                })
                .sum()
        })
        .collect();
    let mut work = vec![0.0_f64; count];
    let mut fresh = vec![lateness; count];
    for _ in 0..count.max(1) {
        let mut changed = false;
        for index in 0..count {
            if !kept[index] {
                continue;
            }
            let (new_work, new_fresh) = if at_ingestion(inputs, index) {
                // Its refresh with each producer's least share (a share
                // may take some of its baseline back).
                let shares: f64 = (0..selectable.input_count(index))
                    .map(|input| {
                        selectable
                            .edges_into(index, input)
                            .iter()
                            .filter_map(|&position| inputs.refresh_ms.per_edge[position])
                            .fold(0.0, f64::min)
                    })
                    .sum();
                let refresh = (of(inputs.refresh_ms, index) + shares).max(0.0);
                (0.0, lateness + refresh + wait_per_run)
            } else {
                // Its own work: its baseline plus, per input, the least
                // share any producer leaves it (a negative share included:
                // a consumer's baseline is costed over its dearest
                // producer, and the share takes that back). Its inputs'
                // least works add up when their sub-DAGs cannot share an
                // execution; when they can (an in-query diamond, e.g. two
                // quantiles read off one state), only the largest counts:
                // a sum would count a shared execution twice and overstate
                // the bound.
                let mut least_work = of(inputs.query_ms, index);
                let mut sum_of_inputs = 0.0_f64;
                let mut largest_input = 0.0_f64;
                let mut least_fresh = lateness;
                for input in 0..selectable.input_count(index) {
                    let choices: Vec<(f64, f64, f64)> = selectable
                        .edges_into(index, input)
                        .iter()
                        .filter(|&&position| kept[selectable.edges()[position].producer])
                        .map(|&position| {
                            let producer = selectable.edges()[position].producer;
                            let edge = inputs.query_ms.per_edge[position].unwrap_or(0.0);
                            (edge, work[producer], fresh[producer])
                        })
                        .collect();
                    if choices.is_empty() {
                        continue;
                    }
                    let least = |pick: fn(&(f64, f64, f64)) -> f64| {
                        choices.iter().map(pick).fold(f64::INFINITY, f64::min)
                    };
                    least_work += least(|choice| choice.0);
                    sum_of_inputs += least(|choice| choice.1);
                    largest_input = largest_input.max(least(|choice| choice.1));
                    least_fresh = least_fresh.max(least(|choice| choice.2));
                }
                // The union of the inputs' sub-DAGs: at least their least
                // works' sum less what may be counted twice, and at least
                // the largest.
                least_work += largest_input.max(sum_of_inputs - counted_twice[index]);
                (least_work, least_fresh)
            };
            let new_work = new_work.max(0.0);
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
    work.iter()
        .zip(&fresh)
        .map(|(work, fresh)| work + fresh)
        .collect()
}

/// Per execution, whether it is in no optimal plan given a feasible plan
/// of weighted latency `upper_bound` (T8): any plan in which a query $q$
/// reads it has $\sum_{q'} w_{q'} L_{q'} \ge w_q (\ell^{min}_e + r_q) +
/// \sum_{q' \ne q} w_{q'} L^{min}_{q'}$ ($\ell^{min}_e$ its least latency,
/// $r_q$ the fixed cost per request, $L^{min}_{q'}$ the least latency of
/// any answer of $q'$), and an execution is selected only when some query
/// reads it ((F): every selected execution is used, down from an output).
/// When that bound is over `upper_bound` for every query that may read it,
/// every plan selecting it is worse than the feasible one: it is in no
/// optimal plan, and leaving it out keeps every optimum.
fn dearer_than_a_feasible_plan(
    inputs: &ClusterInputs<'_>,
    shapes: &[QueryShape],
    kept: &[bool],
    upper_bound: f64,
) -> Vec<bool> {
    let least = least_latency(inputs, kept);
    let least_of_query: Vec<f64> = shapes
        .iter()
        .map(|shape| {
            shape
                .answering
                .iter()
                .filter(|&&index| kept[index])
                .map(|&index| least[index])
                .fold(f64::INFINITY, f64::min)
                .min(f64::MAX)
                + shape.request_ms
        })
        .collect();
    let everyone: f64 = shapes
        .iter()
        .zip(&least_of_query)
        .map(|(shape, least)| shape.weight * least)
        .sum();
    let limit = upper_bound * (1.0 + 1e-9) + 1e-9;
    (0..least.len())
        .map(|index| {
            let readers: Vec<usize> = (0..shapes.len())
                .filter(|&q| shapes[q].reach.contains(&index))
                .collect();
            let over_alone = over_a_limit_alone(inputs, index);
            kept[index]
                && (over_alone
                    || !readers.is_empty()
                        && readers.iter().all(|&q| {
                            let shape = &shapes[q];
                            everyone - shape.weight * least_of_query[q]
                                + shape.weight * (least[index] + shape.request_ms)
                                > limit
                        }))
        })
        .collect()
}

/// Whether execution `index` alone is over the cluster's memory or disk,
/// with every producer share at its most favourable: in no plan.
fn over_a_limit_alone(inputs: &ClusterInputs<'_>, index: usize) -> bool {
    let selectable = inputs.selection.selectable;
    let least = |costs: &ExecutionCosts| {
        of(costs, index)
            + (0..selectable.input_count(index))
                .map(|input| {
                    selectable
                        .edges_into(index, input)
                        .iter()
                        .filter_map(|&position| costs.per_edge[position])
                        .fold(0.0, f64::min)
                })
                .sum::<f64>()
    };
    least(inputs.memory) > inputs.limits.memory_bytes as f64 * (1.0 + 1e-9)
        || least(inputs.disk) > inputs.limits.disk_bytes as f64 * (1.0 + 1e-9)
}

/// Per refresh group, its big-M: the most $f_G$ can exceed the allowed
/// lateness ($F_q$'s own lower bound), from each group's largest refresh
/// time `refresh_bound`: its own, the groups it reads in full, and the
/// groups closing at the same instant over `threads`.
fn freshness_big_m(
    refresh_bound: &[f64],
    windows: &[RefreshWindow],
    reads: &[BTreeSet<usize>],
    threads: f64,
) -> Vec<f64> {
    (0..windows.len())
        .map(|group| {
            let mut bound = refresh_bound[group];
            for (other, (&other_bound, &other_window)) in
                refresh_bound.iter().zip(windows).enumerate()
            {
                if other == group {
                    continue;
                }
                if reads[group].contains(&other) {
                    bound += other_bound;
                } else if close_together(windows[group], other_window) {
                    bound += other_bound / threads;
                }
            }
            bound
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
    solve: Solve<'_>,
) -> Result<Solution, SelectionMilpError> {
    let Solve {
        objective,
        fixed,
        active,
        start,
        improve_below,
        tie_break_seconds,
        pinned,
        stop,
    } = solve;
    let selection = &inputs.selection;
    let selectable = selection.selectable;
    let execution = &inputs.cost_inputs.data_plane_execution;
    let rules = AccuracyRules::of(selection);
    let components = Components::of(selection);
    let shared = copies_in_equivalent_contexts(selection, kept, rules.as_ref());
    let mut problem = ProblemVariables::new();
    let variables = selection_variables(selection, kept, &shared, &components, &mut problem);
    // Variable counts by family, for the solver log (`log_solver`).
    let mut families: Vec<(&str, usize)> = vec![("selection (x, y, z, t, h, r, a)", problem.len())];
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
    families.push(("accuracy", problem.len()));
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
    families.push(("quantities (raw data per source)", problem.len()));
    let quantity_rows = rows.len();
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
                let p = time_of(inputs.refresh_ms, index);
                expression += p * variables.x[index];
                bound += p.max(0.0);
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
    families.push(("refresh groups", problem.len()));
    let big_m = freshness_big_m(&refresh_bound, &groups.windows, &groups.reads, threads);
    // Per query: reads a, edge reads w, freshness F, latency L.
    let mut latencies: Vec<Variable> = Vec::new();
    for shape in shapes {
        // Reads only of the executions kept: the others are fixed at 0.
        let reads: BTreeMap<usize, Variable> = shape
            .reach
            .iter()
            .filter(|&&index| kept[index])
            .map(|&index| (index, problem.add(variable().min(0).max(1))))
            .collect();
        for &index in &shape.answering {
            if let (Some(&z), Some(&read)) =
                (variables.z.get(&(shape.query, index)), reads.get(&index))
            {
                rows.push(constraint!(read >= z));
            }
        }
        // A query-time execution not kept across evaluations adds its work
        // once per round of the request's steps over the query threads.
        let runs = |index: usize| match once_per_request(inputs, index) {
            true => 1.0,
            false => shape.step_rounds as f64,
        };
        let fresh = problem.add(variable().min(lateness));
        let latency = problem.add(variable().min(0));
        let mut execution_ms = Expression::from(shape.request_ms);
        // Reads as a flow (T9): what the query reads of a consumer flows
        // into exactly one selected producer per input, `Σ read = a_c` over
        // the input's edges with `read ≤ y`, and a producer is read at
        // least as much as any edge into it. Unlike `read ≥ a_c + y − 1`,
        // whose LP relaxation vanishes for fractional values, the flow
        // carries a fractional read down the sub-DAG.
        let mut flows: BTreeMap<(usize, usize), Expression> = BTreeMap::new();
        // Without flow reads, the earlier rows, to compare.
        let mccormick = !inputs.cost_inputs.latency_selection_checks.flow_reads;
        for &position in &shape.edges {
            let Some(&y) = variables.y.get(&position) else {
                continue;
            };
            let edge = selectable.edges()[position];
            let (Some(_), Some(&producer)) = (reads.get(&edge.consumer), reads.get(&edge.producer))
            else {
                continue;
            };
            let read = problem.add(variable().min(0).max(1));
            rows.push(constraint!(read <= y));
            rows.push(constraint!(producer >= read));
            if mccormick {
                rows.push(constraint!(read >= reads[&edge.consumer] + y - 1));
                rows.push(constraint!(read <= reads[&edge.consumer]));
                let d = inputs.query_ms.per_edge[position].unwrap_or(0.0) * runs(edge.consumer);
                execution_ms += d * read;
                continue;
            }
            let input = inputs
                .selection
                .physical
                .edge(edge.edge)
                .input_index_of_consumer;
            *flows.entry((edge.consumer, input)).or_default() += read;
            let d = inputs.query_ms.per_edge[position].unwrap_or(0.0) * runs(edge.consumer);
            execution_ms += d * read;
        }
        for ((consumer, _), flow) in flows {
            rows.push(constraint!(flow == reads[&consumer]));
        }
        // Valid freshness cuts without the big-M (T10): a query waits at
        // least for the refresh of the executions of one group it reads.
        let mut read_refresh_of_group: BTreeMap<usize, Expression> = BTreeMap::new();
        for (&index, &read) in &reads {
            if let (Some(&group), true) =
                (groups.of_execution.get(&index), at_ingestion(inputs, index))
            {
                *read_refresh_of_group.entry(group).or_default() +=
                    time_of(inputs.refresh_ms, index).max(0.0) * read;
            }
        }
        for (_, refresh) in read_refresh_of_group {
            rows.push(constraint!(fresh >= lateness + refresh));
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
                    // Valid without the big-M: reading the execution waits
                    // at least for its own refresh ($f_G \ge a + p_e x_e$
                    // and a read execution is selected), which keeps the
                    // LP bound off zero.
                    let own = time_of(inputs.refresh_ms, index).max(0.0) + wait_per_run;
                    rows.push(constraint!(fresh >= lateness + own * read));
                }
                _ => execution_ms += time_of(inputs.query_ms, index) * runs(index) * read,
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
    families.push(("latency (reads, edge reads, F, L)", problem.len()));
    let latency_rows = rows.len();
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
            shape.request_cpu_ms * requests_per_second / 1_000.0
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
    if let Some(plan) = pinned {
        let pin = |variable: Variable, chosen: bool| {
            constraint!(variable == if chosen { 1.0 } else { 0.0 })
        };
        for (index, &x) in variables.x.iter().enumerate() {
            rows.push(pin(x, plan.executions.contains(&index)));
        }
        for (&position, &y) in &variables.y {
            rows.push(pin(y, plan.edges.contains(&position)));
        }
        for (&(query, index), &z) in &variables.z {
            rows.push(pin(z, plan.answering.contains(&(query, index))));
        }
    }
    let goal: Expression = match objective {
        Objective::MeanLatency | Objective::Relaxation => weighted,
        Objective::Memory | Objective::Limit(ClusterLimit::MemoryBytes) => memory.clone(),
        Objective::Disk | Objective::Limit(ClusterLimit::DiskBytes) => disk.clone(),
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
    let accuracy_count = accuracy_rows.len();
    let rows_total = rows.len();
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
    let builder = problem
        .minimise(goal_scaled)
        .using(highs)
        .with_all(constraints);
    let mut model = builder
        .try_into_inner()
        .map_err(|error| SelectionMilpError::Solver(error.to_string()))?;
    model.set_option(
        "mip_rel_gap",
        match stop {
            Stop::Exact => 0.0,
            Stop::GapOrTime | Stop::Gap => STOP_RELATIVE_GAP,
        },
    );
    model.set_option("mip_abs_gap", 0.0);
    if stop == Stop::GapOrTime {
        model.set_option("time_limit", STOP_SECONDS);
    }
    let log = inputs.cost_inputs.latency_selection_checks.log_solver;
    model.set_option("output_flag", log);
    model.set_option("log_to_console", log);
    if log {
        let mut previous = 0;
        let counts: Vec<String> = families
            .iter()
            .map(|&(name, total)| {
                let count = total - previous;
                previous = total;
                format!("{name} {count}")
            })
            .collect();
        eprintln!(
            "cluster MILP {objective:?}: variables: {}; rows: structure {}, accuracy {}, quantities {}, latency {}, limits and fixed {}",
            counts.join(", "),
            constraint_count - rows_total - accuracy_count,
            accuracy_count,
            quantity_rows,
            latency_rows - quantity_rows,
            rows_total - latency_rows,
        );
    }
    model.set_option("threads", 1);
    if objective == Objective::Relaxation {
        model.set_option("solve_relaxation", true);
    }
    if let Some(below) = improve_below {
        model.set_option("time_limit", tie_break_seconds);
        model.set_option("objective_bound", below * scale);
    }
    if let Some(start) = start.filter(|start| start.len() == variable_count) {
        // The variables are built in the same order every solve, so a
        // column is the same variable in each; a start HiGHS refuses is
        // dropped, which only costs time.
        let _ = model.try_set_solution(Some(start), None, None, None);
    }
    let started = Instant::now();
    let solved = model
        .try_solve()
        .map_err(|status| SelectionMilpError::Solver(format!("{status:?}")))?;
    let solve_ms = started.elapsed().as_millis() as u64;
    use highs::{HighsModelStatus, HighsSolutionStatus};
    let columns: Vec<f64> = match solved.status() {
        HighsModelStatus::ModelEmpty => vec![0.0; variable_count],
        HighsModelStatus::Optimal | HighsModelStatus::ReachedTimeLimit
            if solved.primal_solution_status() == HighsSolutionStatus::Feasible =>
        {
            solved.get_solution().columns().to_vec()
        }
        HighsModelStatus::Infeasible | HighsModelStatus::UnboundedOrInfeasible => {
            return Err(SelectionMilpError::Infeasible)
        }
        HighsModelStatus::ReachedTimeLimit if improve_below.is_none() => {
            return Err(SelectionMilpError::Solver(NO_PLAN_IN_TIME.to_string()))
        }
        other => return Err(SelectionMilpError::Solver(format!("{other:?}"))),
    };
    // The lower bound HiGHS proved, unscaled; at a proved optimum (or an
    // empty model, or the LP relaxation) the objective itself.
    let proved_optimal = stop == Stop::Exact
        || objective == Objective::Relaxation
        || solved.status() == HighsModelStatus::ModelEmpty;
    let dual_bound = (!proved_optimal)
        .then(|| solved.double_info_value(c"mip_dual_bound").ok())
        .flatten()
        .filter(|bound| bound.is_finite())
        // HiGHS's objective has no constant term.
        .map(|bound| bound / scale + goal.constant());
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
    // The LP relaxation answers fractionally: no plan to read off.
    let relaxed = objective == Objective::Relaxation;
    let answering: Vec<(QueryId, usize)> = selectable
        .answering()
        .iter()
        .filter(|_| !relaxed)
        .map(|(query, answering)| {
            let execution = answering
                .iter()
                .copied()
                .find(|&execution| chosen(variables.z[&(*query, execution)]))
                .expect("(C): one output per query output reference");
            (*query, execution)
        })
        .collect();
    let evaluate = |expression: &Expression| {
        expression
            .linear_coefficients()
            .map(|(variable, coefficient)| coefficient * value(variable))
            .sum::<f64>()
            + expression.constant()
    };
    let memory_bytes = evaluate(&memory);
    let disk_bytes = evaluate(&disk);
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
        lower_bound: dual_bound.map_or(objective_value, |bound| bound.min(objective_value)),
        memory_bytes,
        disk_bytes,
        columns,
        variable_count,
        constraint_count,
        solve_ms,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn window(width_ms: u64, phase_ms: u64) -> RefreshWindow {
        RefreshWindow { width_ms, phase_ms }
    }

    /// §6.4: windows of 60 s and 300 s at phase 0 close together (every
    /// 300 s); two 60 s windows at phases 0 and 30 s never do; 20 s at 5 s
    /// and 30 s at 15 s do (both close at 45 s).
    #[test]
    fn coinciding_refresh_groups() {
        assert!(close_together(window(60_000, 0), window(300_000, 0)));
        assert!(!close_together(window(60_000, 0), window(60_000, 30_000)));
        assert!(close_together(
            window(20_000, 5_000),
            window(30_000, 15_000)
        ));
    }

    /// §6.4: each group's big-M bounds what $f_G$ can exceed the lateness
    /// by, for every selection of the groups' members: three groups (two
    /// closing together, one read by the first), members of refresh times
    /// 1–4 ms, two precompute threads, every subset of members enumerated.
    #[test]
    fn big_m_bounds_the_freshness_of_every_selection() {
        let windows = [window(10_000, 0), window(20_000, 0), window(10_000, 5_000)];
        let members: [&[f64]; 3] = [&[1.0, 2.0], &[3.0], &[4.0, 1.5]];
        let reads = [BTreeSet::from([2]), BTreeSet::new(), BTreeSet::new()];
        let threads = 2.0;
        let bounds: Vec<f64> = members.iter().map(|m| m.iter().sum()).collect();
        let big_m = freshness_big_m(&bounds, &windows, &reads, threads);
        let all: Vec<(usize, f64)> = members
            .iter()
            .enumerate()
            .flat_map(|(group, m)| m.iter().map(move |&p| (group, p)))
            .collect();
        for selection in 0u32..(1 << all.len()) {
            let refresh: Vec<f64> = (0..3)
                .map(|group| {
                    all.iter()
                        .enumerate()
                        .filter(|&(bit, &(g, _))| g == group && selection & (1 << bit) != 0)
                        .map(|(_, &(_, p))| p)
                        .sum()
                })
                .collect();
            for group in 0..3 {
                let mut freshness = refresh[group];
                for other in 0..3 {
                    if other == group {
                        continue;
                    }
                    if reads[group].contains(&other) {
                        freshness += refresh[other];
                    } else if close_together(windows[group], windows[other]) {
                        freshness += refresh[other] / threads;
                    }
                }
                assert!(
                    freshness <= big_m[group] + 1e-12,
                    "group {group}, selection {selection:b}: {freshness} > {}",
                    big_m[group]
                );
            }
        }
    }
}

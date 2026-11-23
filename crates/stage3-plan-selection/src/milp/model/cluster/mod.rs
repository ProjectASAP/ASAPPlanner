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
use asap_cost::plan::kept_results::computed_range;
use asap_cost::{close_together, ClusterLimits, CostInputs};
use asap_types::ir::properties::ExecutionTiming;
use asap_types::plan::materialized_view::RefreshWindow;
use asap_types::stages::{MemoGroupId, PhysicalCandidateId};
use asap_types::workload::DataWorkload;

use super::super::accuracy::{accuracy_constraints, AccuracyRules, AccuracyVariables, ErrorModel};
use super::super::dominance::{dominated_in_every_quantity, dominated_theta_families, Pairing};
use super::super::equivalent_contexts::copies_in_equivalent_contexts;
use super::super::exact_only::exact_only_constraints;
use super::super::graph::Components;
use super::super::kept_results::{kept_result_terms, KeptResultVariables};
use super::super::retention::{resident_window_terms, retention_constraints};
use super::{
    acyclicity_constraints, input_satisfaction_constraints, producer_selection_constraints,
    query_coverage_constraints, selection_cost_per_second, selection_variables, use_constraints,
    SelectedEdge, SelectionInputs, SelectionMilpError, SelectionVariables, SolvedSelection,
};
use crate::cost::ExecutionCosts;
use crate::selectable::QueryId;

mod exclusions;
mod path_reads;
mod solve;

use exclusions::*;
use path_reads::*;
use solve::*;

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

/// Per query, its static facts: weight, fixed cost per request, bound, the
/// executions and edges it may read.
struct QueryShape {
    query: QueryId,
    weight: f64,
    /// The request's fixed latency, ms: its per-step part once per round.
    request_ms: f64,
    /// Rounds of steps per request, the steps split over the query
    /// threads (`asap_cost::step_rounds`): a query-time execution not kept
    /// across evaluations adds its work once per round to the latency; a
    /// mean over its requests when they compute a fractional mean of steps.
    step_rounds: f64,
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
    // Left out: executions that cannot be costed, those their lifetime
    // twin dominates in every quantity, and those whose
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
    let dominated = dominated_in_every_quantity(selection, &quantities, Pairing::LifetimeTwin);
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
    // The executions kept, with the feasible plan's own put back (the
    // exclusions may have left some out): the model of the retry, so the
    // pinned plan is a solution of it.
    let kept_with_plan: Vec<bool> = (0..kept.len())
        .map(|index| {
            kept[index]
                || (costable[index]
                    && inputs
                        .feasible_plan
                        .as_ref()
                        .is_some_and(|plan| plan.executions.contains(&index)))
        })
        .collect();
    // A start when the first solve finds no plan within its node limit:
    // the feasible plan with its selection pinned, completed by HiGHS on
    // the retry's model (same columns); or why there is none.
    let pinned_start = || -> Result<(Vec<f64>, u64), String> {
        let plan = inputs
            .feasible_plan
            .as_ref()
            .ok_or("cost model 1 selected no plan to start from")?;
        let pinned_under = |active: &[ClusterLimit]| {
            build_and_solve(
                inputs,
                &shapes,
                &groups,
                &kept_with_plan,
                Solve {
                    objective: Objective::MeanLatency,
                    fixed: Fixed::default(),
                    active,
                    start: None,
                    improve_below: None,
                    pinned: Some(plan),
                    stop: Stop::Exact,
                },
            )
        };
        match pinned_under(&limits) {
            Ok(pinned) => Ok((pinned.columns, pinned.solve_ms)),
            // The limits it breaks: each one whose removal alone lets it
            // solve (none named when it breaks several together).
            Err(SelectionMilpError::Infeasible) => {
                let broken: Vec<ClusterLimit> = limits
                    .iter()
                    .filter(|&limit| {
                        let others: Vec<ClusterLimit> = limits
                            .iter()
                            .copied()
                            .filter(|other| other != limit)
                            .collect();
                        pinned_under(&others).is_ok()
                    })
                    .copied()
                    .collect();
                let names: Vec<String> = broken.iter().map(|limit| format!("{limit:?}")).collect();
                Err(format!(
                    "cost model 1's plan breaks {}, so no start was available",
                    match names.is_empty() {
                        true => "several limits together".to_string(),
                        false => format!("a limit ({})", names.join(", ")),
                    }
                ))
            }
            Err(error) => Err(format!(
                "cost model 1's plan did not solve as a start: {error:?}"
            )),
        }
    };
    // One solve over the executions `model` keeps.
    let solve = |model: &[bool],
                 objective: Objective,
                 fixed: Fixed,
                 start: Option<&[f64]>,
                 improve_below: Option<f64>,
                 stop: Stop| {
        build_and_solve(
            inputs,
            &shapes,
            &groups,
            model,
            Solve {
                objective,
                fixed,
                active: &limits,
                start,
                improve_below,
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
                &kept,
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
    // The model the selection is solved over: the executions kept, or,
    // after a retry from the feasible plan, those with the plan's own.
    let mut model: &[bool] = &kept;
    let mut start_ms = 0;
    let first = match inputs.cost_inputs.latency_selection.exact_solve {
        true => solve(
            model,
            Objective::MeanLatency,
            Fixed::default(),
            None,
            None,
            Stop::Exact,
        ),
        // No plan within the node limit: once more from the feasible plan,
        // under the same limit, so the second phase is bounded too.
        false => match solve(
            model,
            Objective::MeanLatency,
            Fixed::default(),
            None,
            None,
            Stop::GapOrNodes,
        ) {
            Err(SelectionMilpError::Solver(reason)) if reason == NO_PLAN_IN_NODES => {
                match pinned_start() {
                    Ok((columns, ms)) => {
                        start_ms = ms;
                        solve_count += 1;
                        model = &kept_with_plan;
                        solve(
                            model,
                            Objective::MeanLatency,
                            Fixed::default(),
                            Some(&columns),
                            None,
                            Stop::GapOrNodes,
                        )
                        .map_err(|error| match error {
                            SelectionMilpError::Solver(again) if again == NO_PLAN_IN_NODES => {
                                SelectionMilpError::Solver(format!(
                                    "{NO_PLAN_IN_NODES}, nor from cost model 1's plan as a start"
                                ))
                            }
                            error => error,
                        })
                    }
                    Err(why) => Err(SelectionMilpError::Solver(format!(
                        "{NO_PLAN_IN_NODES}; {why}"
                    ))),
                }
            }
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
                        pinned: None,
                        stop: Stop::Exact,
                    },
                )
                .ok()
            }));
        }
        Err(error) => return Err(ClusterSelectionError::Milp(error)),
    };
    let mut solve_ms = first.solve_ms + start_ms;
    let weighted = first.objective;
    let weighted_lower_bound = first.lower_bound;
    // Tie-breaks only when an alternative optimum may exist: each starts
    // from the plan so far and only looks for one at least a byte smaller
    // within `TIE_BREAK_NODES`; finding none (or none in those nodes) keeps
    // the plan so far.
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
            model,
            objective,
            fixed,
            Some(&last.columns),
            (!full_tie_breaks).then_some(current - 0.5),
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
    // The reported latencies are those of the chosen selection, pinned and
    // solved exactly for the mean latency: a solve stopped at its gap and a
    // tie-break solve (whose latency variables float within the slack it
    // keeps on the mean latency) may leave a latency above the selection's
    // own. A pinned solve that fails is an error, not the looser latencies.
    {
        let chosen = FeasiblePlan {
            executions: last.selected.iter().copied().collect(),
            edges: last.edges.iter().copied().collect(),
            answering: last.answering.iter().copied().collect(),
        };
        let pinned = build_and_solve(
            inputs,
            &shapes,
            &groups,
            model,
            Solve {
                objective: Objective::MeanLatency,
                fixed: Fixed::default(),
                active: &limits,
                start: Some(&last.columns),
                improve_below: None,
                pinned: Some(&chosen),
                stop: Stop::Exact,
            },
        );
        solve_count += 1;
        let pinned = pinned.map_err(|error| {
            ClusterSelectionError::Milp(SelectionMilpError::Solver(format!(
                "the selected plan, pinned, did not solve for its latencies: {error:?}"
            )))
        })?;
        solve_ms += pinned.solve_ms;
        last.latency_ms = pinned.latency_ms;
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
        milp_optimum_per_second: None,
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
            // Its range as the data plane computes it (with an answer cache,
            // only its new steps, as `plan_cost` counts them).
            let answer_cache = inputs.cost_inputs.data_plane_execution.answer_cache;
            let range = entry.and_then(|entry| computed_range(entry, answer_cache));
            let weight = evaluation_rate(entry.map(|entry| (&entry.recurrence, range)), horizon)
                .map_err(|error| {
                    SelectionMilpError::Solver(format!("query {}: {error}", query.0))
                })?;
            let steps = range.map_or(1.0, |range| range.mean_evaluations_per_request());
            let step_rounds = asap_cost::step_rounds(
                steps,
                inputs.cost_inputs.data_plane_execution.query_threads,
            );
            let request_ms =
                (table.ns_per_evaluation + table.ns_per_evaluation_step * step_rounds) * 1e-6;
            let request_cpu_ms =
                (table.ns_per_evaluation + table.ns_per_evaluation_step * steps) * 1e-6;
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

/// The memo group of execution `index`.
fn memo_group_of(inputs: &ClusterInputs<'_>, index: usize) -> MemoGroupId {
    let v = inputs.selection.selectable.executions()[index].candidate;
    inputs.selection.physical.physical_candidates[v.0]
        .logical_candidate_id
        .group
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

/// Execution `index`'s share of `costs`, in the unit of `costs`' quantity
/// (ms per run, ms per window, cores, bytes), 0 when it could not be
/// costed.
fn share_or_zero(costs: &ExecutionCosts, index: usize) -> f64 {
    costs.per_execution[index].as_ref().copied().unwrap_or(0.0)
}

/// Execution `index`'s share of `costs` at the largest retention its
/// readers may need, in the unit of `costs`' quantity (a latency row's
/// coefficient, which cannot depend on $h_v$), 0 when it could not be
/// costed.
fn share_at_largest_retention_or_zero(costs: &ExecutionCosts, index: usize) -> f64 {
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

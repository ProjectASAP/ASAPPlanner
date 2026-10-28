//! MILP encoding and solution decoding for the finite candidate graph.
//!
//! Local fixed-context cost bounds and evaluated assignment costs constrain the
//! objective. Workload demand, physical realization, and evaluator refinement
//! are supplied by the surrounding planning stages.

#[cfg(test)]
use std::collections::HashSet;

#[path = "warm_start.rs"]
mod warm_start;

fn initialized(
    definition: good_lp::VariableDefinition,
    value: Option<f64>,
) -> good_lp::VariableDefinition {
    match value {
        Some(value) => definition.initial(value),
        None => definition,
    }
}

use crate::search::candidates::graph::*;
#[cfg(test)]
use asap_logical_asap_optimization::memo::MemoGroupId;
use good_lp::solvers::highs::highs;
use good_lp::solvers::{ResolutionError, SolutionStatus};
use good_lp::{
    constraint, variable, Expression, ProblemVariables, Solution, SolverModel, Variable,
};

/// Only outputs and dependency paths inspected by reader constraints need flow
/// variables. Activation symmetry is independent of readers. A corridor starts
/// at a constrained producer and follows possible consumers back toward roots.
fn required_reader_reach(
    graph: &CandidateDependencyGraph,
    all_outputs: bool,
    check_deadline: impl Fn() -> Result<(), CandidateSelectionError>,
) -> Result<Vec<Vec<bool>>, CandidateSelectionError> {
    let count = graph.candidates.len();
    let mut potential = vec![vec![false; count]; graph.query_roots.len()];
    let mut consumers = vec![Vec::new(); count];
    for (consumer, candidate) in graph.candidates.iter().enumerate() {
        check_deadline()?;
        for input in &candidate.producer_requirements {
            for producer in &input.compatible_producers {
                consumers[producer.0].push(consumer);
            }
        }
    }
    for (output, root) in graph.query_roots.iter().enumerate() {
        let mut pending = root.compatible_candidates.clone();
        while let Some(candidate) = pending.pop() {
            check_deadline()?;
            if !std::mem::replace(&mut potential[output][candidate.0], true) {
                pending.extend(
                    graph.candidates[candidate.0]
                        .producer_requirements
                        .iter()
                        .flat_map(|input| input.compatible_producers.iter().copied()),
                );
            }
        }
    }
    if all_outputs {
        return Ok(potential);
    }
    let mut needed = vec![vec![false; count]; graph.query_roots.len()];
    let mut pending = Vec::new();
    for constraint in &graph.reader_constraints {
        check_deadline()?;
        match constraint {
            ReaderConstraint::ActivationRequires { .. } => {}
            ReaderConstraint::HighestSharedQueryMaterialization { candidate, .. } => {
                for (output, reachable) in potential.iter().enumerate() {
                    if reachable[candidate.0] {
                        pending.push((output, candidate.0));
                    }
                }
            }
            ReaderConstraint::AllowedOutputs { candidate, outputs } => {
                for (output, reachable) in potential.iter().enumerate() {
                    if reachable[candidate.0] && !outputs.contains(&output) {
                        pending.push((output, candidate.0));
                    }
                }
            }
            ReaderConstraint::IncompatibleOutputs {
                candidate,
                left,
                right,
            } => {
                if potential[*left][candidate.0] && potential[*right][candidate.0] {
                    pending.extend([(*left, candidate.0), (*right, candidate.0)]);
                }
            }
            ReaderConstraint::MinimumQueryReaders {
                candidate,
                query_outputs,
                minimum,
            } => {
                // Every active candidate has a supported path to a root. Thus
                // one reader is already guaranteed without a separate flow.
                if *minimum > 1 && *minimum <= query_outputs.len() {
                    for (output, reachable) in potential.iter().enumerate() {
                        if reachable[candidate.0] {
                            pending.push((output, candidate.0));
                        }
                    }
                }
            }
        }
    }
    while let Some((output, candidate)) = pending.pop() {
        check_deadline()?;
        if !std::mem::replace(&mut needed[output][candidate], true) {
            pending.extend(
                consumers[candidate]
                    .iter()
                    .filter(|&&consumer| potential[output][consumer])
                    .map(|&consumer| (output, consumer)),
            );
        }
    }
    Ok(needed)
}

/// Return the cheapest covering DAG, or `None` when no such graph exists.
/// Numerical optimality is subject to HiGHS tolerances. No production timeout or
/// incumbent fallback is implied by this fixed-context core.
#[cfg(test)]
pub(crate) fn select_candidates(
    graph: &CandidateDependencyGraph,
) -> Result<Option<SelectedCandidateGraph>, CandidateSelectionError> {
    let proposal = solve_candidate_proposal(graph, &[], None)?;
    proposal
        .map(|proposal| {
            if proposal.model_lower_bound.is_none() {
                return Err(CandidateSelectionError::Solver(
                    "covering graph is feasible but optimality is unproven".into(),
                ));
            }
            Ok(proposal.selected)
        })
        .transpose()
}

/// One validated observation over every candidate, connection and output decision.
/// `None` excludes an infeasible assignment; a cost supplies its exact objective.
#[derive(Debug, Clone)]
pub(crate) struct ObservedAssignment {
    pub selected: SelectedCandidateGraph,
    pub exact_cost: Option<f64>,
    pub rejected_queries: Vec<RejectedQuerySubgraph>,
}

#[derive(Debug)]
pub(crate) struct CandidateProposal {
    pub selected: SelectedCandidateGraph,
    /// Complete modeled cost of this selected DAG, only for a certified direct profile.
    pub direct_modeled_cost: Option<f64>,
    /// Certified optimal relaxation objective, distinct from the local sum.
    /// A feasible gap/time-limited incumbent objective is NOT a lower bound.
    pub model_lower_bound: Option<f64>,
}

/// Accumulated across refinement rounds, including infeasible and stopped solves.
#[derive(Debug, Default)]
pub(crate) struct CandidateSolveMetrics {
    pub model_construction_seconds: f64,
    pub solver_seconds: f64,
    pub solve_calls: usize,
    pub direct_cost_solve_calls: usize,
    /// Direct solves jointly selecting supplier-dependent pricing options.
    pub joint_price_solve_calls: usize,
    pub direct_cost_validated_plans: usize,
    pub direct_cost_best_validated_cost: Option<f64>,
    pub direct_cost_fallback_reasons: Vec<String>,
    /// Largest model submitted across refinement rounds, before solver presolve.
    pub peak_variables: usize,
    pub peak_constraints: usize,
}

/// Refine exact costs at complete assignments and certified query failures at
/// their complete upstream dependency closures.
/// Feasible stopped solves may return proposals without a certified bound.
#[cfg(test)]
pub(crate) fn solve_candidate_proposal(
    graph: &CandidateDependencyGraph,
    observations: &[ObservedAssignment],
    remaining_seconds: Option<f64>,
) -> Result<Option<CandidateProposal>, CandidateSelectionError> {
    solve_candidate_proposal_with_metrics(
        graph,
        observations,
        remaining_seconds,
        &mut CandidateSolveMetrics::default(),
    )
}

pub(crate) fn solve_candidate_proposal_with_metrics(
    graph: &CandidateDependencyGraph,
    observations: &[ObservedAssignment],
    remaining_seconds: Option<f64>,
    metrics: &mut CandidateSolveMetrics,
) -> Result<Option<CandidateProposal>, CandidateSelectionError> {
    solve_with_objective(graph, observations, remaining_seconds, metrics, None, None)
}

pub(crate) fn solve_direct_cost_proposal_with_metrics(
    graph: &CandidateDependencyGraph,
    observations: &[ObservedAssignment],
    remaining_seconds: Option<f64>,
    metrics: &mut CandidateSolveMetrics,
    profile: &super::direct_cost::DirectCostProfile,
) -> Result<Option<CandidateProposal>, CandidateSelectionError> {
    solve_with_objective(
        graph,
        observations,
        remaining_seconds,
        metrics,
        Some(profile),
        None,
    )
}

pub(super) fn validate_observations(
    graph: &CandidateDependencyGraph,
    observations: &[ObservedAssignment],
) -> Result<(), CandidateSelectionError> {
    for observation in observations {
        validate_selected_graph(graph, &observation.selected)
            .map_err(CandidateSelectionError::InvalidGraph)?;
        for witness in &observation.rejected_queries {
            witness
                .validate(&observation.selected)
                .map_err(CandidateSelectionError::InvalidGraph)?;
        }
        if observation.exact_cost.is_some() && !observation.rejected_queries.is_empty() {
            return Err(CandidateSelectionError::InvalidGraph(
                "feasible observation cannot reject a query".into(),
            ));
        }
        if observation
            .exact_cost
            .is_some_and(|cost| !cost.is_finite() || cost < observation.selected.modeled_cost)
        {
            return Err(CandidateSelectionError::InvalidGraph(
                "observed cost must be finite and at least its certified local bound".into(),
            ));
        }
    }
    Ok(())
}

pub(super) fn solve_with_objective(
    graph: &CandidateDependencyGraph,
    observations: &[ObservedAssignment],
    remaining_seconds: Option<f64>,
    metrics: &mut CandidateSolveMetrics,
    direct_profile: Option<&super::direct_cost::DirectCostProfile>,
    projection: Option<&super::direct_price_options::PriceOptionProjection<'_>>,
) -> Result<Option<CandidateProposal>, CandidateSelectionError> {
    let started = std::time::Instant::now();
    graph.validate()?;
    if let Some(profile) = direct_profile {
        profile.validate(graph)?;
    }
    if remaining_seconds.is_some_and(|seconds| !seconds.is_finite() || seconds <= 0.0) {
        return Err(CandidateSelectionError::Solver(
            "no positive solver time remains".into(),
        ));
    }
    let check_deadline = || {
        if remaining_seconds.is_some_and(|seconds| started.elapsed().as_secs_f64() >= seconds) {
            Err(CandidateSelectionError::Solver(
                "time budget exhausted constructing model".into(),
            ))
        } else {
            Ok(())
        }
    };
    validate_observations(graph, observations)?;
    if graph.query_roots.is_empty() {
        if observations
            .iter()
            .any(|observation| observation.exact_cost.is_none())
        {
            return Ok(None);
        }
        return Ok(Some(CandidateProposal {
            direct_modeled_cost: direct_profile.map(|profile| profile.request_cost),
            selected: SelectedCandidateGraph {
                selected_candidates: Vec::new(),
                connections: Vec::new(),
                query_roots: Vec::new(),
                modeled_cost: 0.0,
            },
            model_lower_bound: Some(
                observations
                    .iter()
                    .filter_map(|observation| observation.exact_cost)
                    .fold(
                        direct_profile.map_or(0.0, |profile| profile.request_cost),
                        f64::max,
                    ),
            ),
        }));
    }
    if graph
        .query_roots
        .iter()
        .any(|root| root.compatible_candidates.is_empty())
    {
        return Ok(None);
    }

    // Direct objective auxiliaries have no complete feasible start yet.
    let warm_start = direct_profile
        .is_none()
        .then(|| warm_start::cheapest_start(graph, observations))
        .flatten();
    let count = graph.candidates.len();
    let mut variables = ProblemVariables::new();
    let candidate_selected: Vec<Variable> = (0..count)
        .map(|index| {
            variables.add(initialized(
                variable().binary(),
                warm_start
                    .as_ref()
                    .map(|start| f64::from(start.active.contains(&PhysicalCandidateId(index)))),
            ))
        })
        .collect();
    let topological_rank: Vec<Variable> = (0..count)
        .map(|index| {
            variables.add(initialized(
                variable().min(0).max((count - 1) as f64),
                warm_start.as_ref().map(|start| start.ranks[index] as f64),
            ))
        })
        .collect();
    let mut connections = Vec::new();
    let mut root_assignments = Vec::new();
    let mut constraints = Vec::new();
    let mut support = vec![Expression::from(0.0); count];
    if let Some(projection) = projection {
        projection.constrain_activation(&candidate_selected, &mut constraints);
    }

    for (consumer, candidate) in graph.candidates.iter().enumerate() {
        check_deadline()?;
        for (requirement, input) in candidate.producer_requirements.iter().enumerate() {
            let mut producers_selected = Expression::from(0.0);
            for &producer in &input.compatible_producers {
                check_deadline()?;
                let connection_selected = variables.add(initialized(
                    variable().binary(),
                    warm_start.as_ref().map(|start| {
                        f64::from(start.selected.connections.contains(
                            &ProducerConsumerConnection {
                                producer,
                                consumer: PhysicalCandidateId(consumer),
                                requirement,
                            },
                        ))
                    }),
                ));
                producers_selected += connection_selected;
                support[producer.0] += connection_selected;
                constraints.push(constraint!(
                    connection_selected <= candidate_selected[producer.0]
                ));
                constraints.push(constraint!(
                    topological_rank[producer.0] + 1.0
                        <= topological_rank[consumer] + count as f64 * (1.0 - connection_selected)
                ));
                connections.push((
                    ProducerConsumerConnection {
                        producer,
                        consumer: PhysicalCandidateId(consumer),
                        requirement,
                    },
                    connection_selected,
                ));
            }
            constraints.push(constraint!(
                producers_selected == candidate_selected[consumer]
            ));
        }
    }
    for (query_root, root) in graph.query_roots.iter().enumerate() {
        let mut root_selected = Expression::from(0.0);
        for &candidate in &root.compatible_candidates {
            let assignment_selected = variables.add(initialized(
                variable().binary(),
                warm_start.as_ref().map(|start| {
                    f64::from(start.selected.query_roots.contains(&QueryRootAssignment {
                        query_root,
                        candidate,
                    }))
                }),
            ));
            root_selected += assignment_selected;
            support[candidate.0] += assignment_selected;
            constraints.push(constraint!(
                assignment_selected <= candidate_selected[candidate.0]
            ));
            root_assignments.push((
                QueryRootAssignment {
                    query_root,
                    candidate,
                },
                assignment_selected,
            ));
        }
        constraints.push(constraint!(root_selected == 1));
    }
    let mut objective_reach = Vec::new();
    if !graph.reader_constraints.is_empty() || direct_profile.is_some() {
        // Reachability is implied integral: roots and selected edges are binary,
        // and selected edges form a DAG. Induction from query roots makes each
        // AND/OR below exactly zero or one, so these auxiliaries need no integer
        // branching. Unselected edges force their conjunction to zero.
        // Allocate only potentially reachable output/candidate pairs. Selected
        // edges determine actual reach; incompatible potential readers do not
        // rule out a candidate used by a compatible subset.
        let mut reach = Vec::with_capacity(graph.query_roots.len());
        for (output, potential) in
            required_reader_reach(graph, direct_profile.is_some(), check_deadline)?
                .into_iter()
                .enumerate()
        {
            reach.push(
                potential
                    .into_iter()
                    .enumerate()
                    .map(|(candidate, possible)| {
                        possible.then(|| {
                            variables.add(initialized(
                                variable().min(0).max(1),
                                warm_start.as_ref().map(|start| {
                                    f64::from(
                                        start.reached[output]
                                            .contains(&PhysicalCandidateId(candidate)),
                                    )
                                }),
                            ))
                        })
                    })
                    .collect::<Vec<_>>(),
            );
        }
        for (output, reachable) in reach.iter().enumerate() {
            check_deadline()?;
            let mut demanded = vec![Expression::from(0.0); count];
            for (root, selected) in &root_assignments {
                if root.query_root == output {
                    if let Some(target) = reachable[root.candidate.0] {
                        constraints.push(constraint!(*selected <= target));
                        demanded[root.candidate.0] += *selected;
                    }
                }
            }
            for (edge, connected) in &connections {
                check_deadline()?;
                if let (Some(consumer_reached), Some(producer_reached)) =
                    (reachable[edge.consumer.0], reachable[edge.producer.0])
                {
                    let used = variables.add(initialized(
                        variable().min(0).max(1),
                        warm_start.as_ref().map(|start| {
                            f64::from(
                                start.reached[output].contains(&edge.consumer)
                                    && start.selected.connections.contains(edge),
                            )
                        }),
                    ));
                    constraints.push(constraint!(used <= consumer_reached));
                    constraints.push(constraint!(used <= *connected));
                    constraints.push(constraint!(used >= consumer_reached + *connected - 1));
                    constraints.push(constraint!(used <= producer_reached));
                    demanded[edge.producer.0] += used;
                }
            }
            for (candidate, variable) in reachable.iter().enumerate() {
                if let Some(reached) = variable {
                    constraints.push(constraint!(*reached <= demanded[candidate].clone()));
                    constraints.push(constraint!(*reached <= candidate_selected[candidate]));
                }
            }
        }
        if direct_profile.is_some() {
            objective_reach = reach.clone();
        }
        for requirement in &graph.reader_constraints {
            match requirement {
                ReaderConstraint::ActivationRequires {
                    candidate,
                    required_candidate,
                } => {
                    constraints.push(constraint!(
                        candidate_selected[candidate.0] <= candidate_selected[required_candidate.0]
                    ));
                }
                ReaderConstraint::MinimumQueryReaders {
                    candidate,
                    query_outputs,
                    minimum,
                } => {
                    if *minimum <= 1 {
                        continue;
                    }
                    if *minimum > query_outputs.len() {
                        constraints.push(constraint!(candidate_selected[candidate.0] == 0));
                        continue;
                    }
                    let mut readers = Expression::from(0.0);
                    for outputs in query_outputs {
                        let reached: Vec<_> = outputs
                            .iter()
                            .filter_map(|&output| reach[output][candidate.0])
                            .collect();
                        if reached.is_empty() {
                            continue;
                        }
                        let query_reached = variables.add(initialized(
                            variable().min(0).max(1),
                            warm_start.as_ref().map(|start| {
                                f64::from(
                                    outputs
                                        .iter()
                                        .any(|output| start.reached[*output].contains(candidate)),
                                )
                            }),
                        ));
                        let mut any_output = Expression::from(0.0);
                        for output_reached in reached {
                            constraints.push(constraint!(query_reached >= output_reached));
                            any_output += output_reached;
                        }
                        constraints.push(constraint!(query_reached <= any_output));
                        readers += query_reached;
                    }
                    constraints.push(constraint!(
                        readers >= *minimum as f64 * candidate_selected[candidate.0]
                    ));
                }
                ReaderConstraint::HighestSharedQueryMaterialization {
                    candidate,
                    query_outputs,
                } => {
                    let mut witnesses: Expression = root_assignments
                        .iter()
                        .filter(|(root, _)| root.candidate == *candidate)
                        .map(|(_, value)| *value)
                        .sum();
                    for (edge, connected) in &connections {
                        if edge.producer != *candidate {
                            continue;
                        }
                        let witness = variables.add(initialized(
                            variable().binary(),
                            warm_start.as_ref().map(|start| {
                                f64::from(
                                    start.selected.connections.contains(edge)
                                        && query_outputs
                                            .iter()
                                            .filter(|outputs| {
                                                outputs.iter().any(|output| {
                                                    start.reached[*output].contains(&edge.consumer)
                                                })
                                            })
                                            .count()
                                            <= 1,
                                )
                            }),
                        ));
                        constraints.push(constraint!(witness <= *connected));
                        let mut readers = Expression::from(0.0);
                        for outputs in query_outputs {
                            let reached: Vec<_> = outputs
                                .iter()
                                .filter_map(|output| reach[*output][edge.consumer.0])
                                .collect();
                            if reached.is_empty() {
                                continue;
                            }
                            let query = variables.add(initialized(
                                variable().min(0).max(1),
                                warm_start.as_ref().map(|start| {
                                    f64::from(outputs.iter().any(|output| {
                                        start.reached[*output].contains(&edge.consumer)
                                    }))
                                }),
                            ));
                            for value in &reached {
                                constraints.push(constraint!(query >= *value));
                            }
                            constraints.push(constraint!(
                                query <= reached.into_iter().sum::<Expression>()
                            ));
                            readers += query;
                        }
                        constraints.push(constraint!(
                            readers <= 1.0 + query_outputs.len() as f64 * (1.0 - witness)
                        ));
                        witnesses += witness;
                    }
                    constraints.push(constraint!(candidate_selected[candidate.0] <= witnesses));
                }
                ReaderConstraint::AllowedOutputs { candidate, outputs } => {
                    for (output, reachable) in reach.iter().enumerate() {
                        if !outputs.contains(&output) {
                            if let Some(reached) = reachable[candidate.0] {
                                constraints.push(constraint!(reached == 0));
                            }
                        }
                    }
                }
                ReaderConstraint::IncompatibleOutputs {
                    candidate,
                    left,
                    right,
                } => {
                    if let (Some(left), Some(right)) =
                        (reach[*left][candidate.0], reach[*right][candidate.0])
                    {
                        constraints.push(constraint!(left + right <= 1));
                    }
                }
            }
        }
    }
    for candidate in 0..count {
        constraints.push(constraint!(
            candidate_selected[candidate] <= support[candidate].clone()
        ));
    }
    for budget in &graph.resource_budgets {
        // Normalize each resource independently of currency and other resources.
        let scale = budget
            .candidate_usage
            .iter()
            .copied()
            .fold(budget.capacity, f64::max)
            .max(f64::MIN_POSITIVE);
        let usage: Expression = budget
            .candidate_usage
            .iter()
            .zip(&candidate_selected)
            .map(|(usage, selected)| (*usage / scale) * *selected)
            .sum();
        constraints.push(constraint!(usage <= budget.capacity / scale));
    }
    // Validation rejects positive coefficients outside the supported relative
    // range; uniform currency-unit changes do not alter the selected graph.
    let maximum_cost = graph
        .candidates
        .iter()
        .map(|c| c.local_cost)
        .fold(0.0, f64::max);
    let maximum_cost = direct_profile
        .into_iter()
        .flat_map(|profile| profile.coefficients(graph))
        .fold(maximum_cost, f64::max);
    let maximum_cost = observations
        .iter()
        .filter_map(|observation| observation.exact_cost)
        .fold(maximum_cost, f64::max);
    // Keep the smallest meaningful monetary coefficient at unit scale.
    // Scaling by the largest (possibly unused, very expensive) candidate can
    // push every feasible objective below the solver's absolute tolerance and
    // turn an incumbent into a spurious optimum despite zero MIP gap settings.
    let divisor = graph
        .candidates
        .iter()
        .map(|candidate| candidate.local_cost)
        .chain(
            observations
                .iter()
                .filter_map(|observation| observation.exact_cost),
        )
        .chain(
            direct_profile
                .into_iter()
                .flat_map(|profile| profile.coefficients(graph)),
        )
        .filter(|cost| *cost > 0.0)
        .reduce(f64::min)
        .unwrap_or(1.0);
    // Direct held-memory profiles can span tiny reader buffers and large raw
    // windows. Their lifetimes are dimensionless and the smallest currency
    // coefficient is scaled to one; permit up to 1e12 here without rounding
    // away fees. The fixed-node graph keeps its original stricter range.
    let relative_coefficient_limit = if direct_profile.is_some() {
        super::direct_cost::MINIMUM_DIRECT_RELATIVE_COEFFICIENT
    } else {
        MINIMUM_RELATIVE_COEFFICIENT
    };
    if graph
        .candidates
        .iter()
        .map(|candidate| candidate.local_cost)
        .chain(
            observations
                .iter()
                .filter_map(|observation| observation.exact_cost),
        )
        .chain(
            direct_profile
                .into_iter()
                .flat_map(|profile| profile.coefficients(graph)),
        )
        .any(|cost| cost > 0.0 && cost / maximum_cost < relative_coefficient_limit)
    {
        let local_minimum = graph
            .candidates
            .iter()
            .map(|candidate| candidate.local_cost)
            .filter(|cost| *cost > 0.0)
            .reduce(f64::min);
        let direct_minimum = direct_profile
            .into_iter()
            .flat_map(|profile| profile.coefficients(graph))
            .filter(|cost| *cost > 0.0)
            .reduce(f64::min);
        let reason = format!("observed costs and objective coefficients exceed supported numerical range: min={divisor:e}, max={maximum_cost:e}, local_min={local_minimum:?}, direct_min={direct_minimum:?}");
        return Err(if direct_profile.is_some() {
            CandidateSelectionError::UnsupportedDirectCost(reason)
        } else {
            CandidateSelectionError::InvalidGraph(reason)
        });
    }
    let local_bound: Expression = graph
        .candidates
        .iter()
        .enumerate()
        .map(|(index, candidate)| (candidate.local_cost / divisor) * candidate_selected[index])
        .sum();
    let total_cost = variables.add(initialized(
        variable().min(0.0),
        warm_start.as_ref().map(|start| start.cost / divisor),
    ));
    constraints.push(constraint!(total_cost >= local_bound));
    let objective = if let Some(profile) = direct_profile {
        super::direct_cost::encode(
            graph,
            profile,
            &mut variables,
            &mut constraints,
            &candidate_selected,
            &connections,
            &root_assignments,
            &objective_reach,
            divisor,
            check_deadline,
        )?
    } else {
        Expression::from(total_cost)
    };
    // A certified direct expression needs no epigraph row. Keeping its large
    // currency sum in the objective avoids postsolve cancellation in that row.
    for observation in observations {
        check_deadline()?;
        for witness in &observation.rejected_queries {
            let mut changed = Expression::from(0.0);
            for (edge, variable) in &connections {
                if witness.connections.contains(edge) {
                    changed += 1.0 - *variable;
                }
            }
            for (root, variable) in &root_assignments {
                if witness.query_roots.contains(root) {
                    changed += 1.0 - *variable;
                }
            }
            constraints.push(constraint!(changed >= 1.0));
        }
        let mut mismatch = Expression::from(0.0);
        for (id, &variable) in candidate_selected.iter().enumerate() {
            if observation
                .selected
                .selected_candidates
                .contains(&PhysicalCandidateId(id))
            {
                mismatch += 1.0 - variable;
            } else {
                mismatch += variable;
            }
        }
        for (connection, variable) in &connections {
            if observation.selected.connections.contains(connection) {
                mismatch += 1.0 - *variable;
            } else {
                mismatch += *variable;
            }
        }
        for (assignment, variable) in &root_assignments {
            if observation.selected.query_roots.contains(assignment) {
                mismatch += 1.0 - *variable;
            } else {
                mismatch += *variable;
            }
        }
        match observation.exact_cost {
            Some(_) if direct_profile.is_some() => {} // Already priced exactly.
            Some(cost) => constraints.push(constraint!(
                total_cost >= (cost / divisor) * (1.0 - mismatch)
            )),
            None => constraints.push(constraint!(mismatch >= 1.0)),
        }
    }
    if let Some(projection) = projection {
        projection.constrain_input_tuples(
            &mut variables,
            &candidate_selected,
            &connections,
            &mut constraints,
            check_deadline,
        )?;
        projection.constrain_reader_contexts(
            &candidate_selected,
            &objective_reach,
            &mut constraints,
            check_deadline,
        )?;
        projection.constrain_observations(
            &candidate_selected,
            &connections,
            &root_assignments,
            &mut constraints,
        );
    }
    if warm_start.is_some() && variables.initial_solution_len() != variables.len() {
        return Err(CandidateSelectionError::Solver(
            "incomplete candidate warm start".into(),
        ));
    }
    #[cfg(test)]
    if warm_start.is_some() {
        let values: std::collections::HashMap<_, _> = variables
            .iter_variables_with_def()
            .map(|(variable, definition)| (variable, definition.get_initial().unwrap()))
            .collect();
        for constraint in &constraints {
            let value = constraint.expression().eval_with(&values);
            assert!(
                if constraint.is_equality() {
                    value.abs() <= 1e-7
                } else {
                    value <= 1e-7
                },
                "infeasible warm-start constraint: {constraint:?} = {value}"
            );
        }
    }
    metrics.peak_variables = metrics.peak_variables.max(variables.len());
    metrics.peak_constraints = metrics.peak_constraints.max(constraints.len());
    let mut model = variables
        .minimise(objective.clone())
        .using(highs)
        .set_option("mip_rel_gap", 0.0)
        .set_option("mip_abs_gap", 0.0)
        .with_all(constraints);
    metrics.model_construction_seconds += started.elapsed().as_secs_f64();
    if let Some(seconds) = remaining_seconds {
        let remaining = seconds - started.elapsed().as_secs_f64();
        if remaining <= 0.0 {
            return Err(CandidateSelectionError::Solver(
                "time budget exhausted constructing model".into(),
            ));
        }
        model = model.set_option("time_limit", remaining);
    }
    let solve_started = std::time::Instant::now();
    metrics.solve_calls += 1;
    metrics.direct_cost_solve_calls += usize::from(direct_profile.is_some());
    let result = model.solve();
    metrics.solver_seconds += solve_started.elapsed().as_secs_f64();
    let solution = match result {
        Ok(solution) => solution,
        Err(ResolutionError::Infeasible) => return Ok(None),
        Err(error) => return Err(CandidateSelectionError::Solver(error.to_string())),
    };
    // A stopped solve may still have a useful integral incumbent. Never round
    // fractional or nonfinite decision values into a different feasible graph.
    for variable in candidate_selected
        .iter()
        .chain(connections.iter().map(|(_, variable)| variable))
        .chain(root_assignments.iter().map(|(_, variable)| variable))
    {
        validate_binary_value(solution.value(*variable))?;
    }
    let selected_candidates: Vec<_> = candidate_selected
        .iter()
        .enumerate()
        .filter(|(_, variable)| solution.value(**variable) > 0.5)
        .map(|(index, _)| PhysicalCandidateId(index))
        .collect();
    let modeled_cost = selected_candidates
        .iter()
        .map(|id| graph.candidates[id.0].local_cost)
        .sum();
    let selected = SelectedCandidateGraph {
        selected_candidates,
        connections: connections
            .into_iter()
            .filter(|(_, selected)| solution.value(*selected) > 0.5)
            .map(|(connection, _)| connection)
            .collect(),
        query_roots: root_assignments
            .into_iter()
            .filter(|(_, selected)| solution.value(*selected) > 0.5)
            .map(|(assignment, _)| assignment)
            .collect(),
        modeled_cost,
    };
    let direct_modeled_cost = direct_profile.map(|profile| profile.selected_cost(graph, &selected));
    let solved_objective = solution.eval(&objective) * divisor;
    if matches!(solution.status(), SolutionStatus::Optimal) {
        if let Some(decoded) = direct_modeled_cost {
            // Decoding checks the selected graph independently of LP lifetime
            // auxiliaries. A lost small term must not become a false certificate.
            let tolerance = 1e-7 * decoded.abs().max(solved_objective.abs());
            if !decoded.is_finite()
                || !solved_objective.is_finite()
                || (decoded - solved_objective).abs() > tolerance
            {
                return Err(CandidateSelectionError::UnsupportedDirectCost(format!(
                    "direct solver objective disagrees with decoded cost: solver={solved_objective:e}, decoded={decoded:e}"
                )));
            }
        }
    }
    decoded_proposal(graph, selected, solution.status(), solved_objective).map(|mut proposal| {
        proposal.direct_modeled_cost = direct_modeled_cost;
        Some(proposal)
    })
}

fn validate_binary_value(value: f64) -> Result<(), CandidateSelectionError> {
    if !value.is_finite() || (value.abs() > 1e-6 && (value - 1.0).abs() > 1e-6) {
        return Err(CandidateSelectionError::Solver(
            "nonintegral candidate decision in solver incumbent".into(),
        ));
    }
    Ok(())
}

fn decoded_proposal(
    graph: &CandidateDependencyGraph,
    selected: SelectedCandidateGraph,
    status: SolutionStatus,
    objective: f64,
) -> Result<CandidateProposal, CandidateSelectionError> {
    validate_selected_graph(graph, &selected).map_err(CandidateSelectionError::Solver)?;
    if !objective.is_finite() || objective < 0.0 {
        return Err(CandidateSelectionError::Solver(
            "invalid solver incumbent objective".into(),
        ));
    }
    Ok(CandidateProposal {
        direct_modeled_cost: None,
        selected,
        model_lower_bound: matches!(status, SolutionStatus::Optimal).then_some(objective),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    // A feasible stopped incumbent is usable, but its objective is not a dual bound.
    #[test]
    fn stopped_proposal_keeps_valid_graph_without_certifying_objective() {
        let graph = dependency_graph(vec![candidate(0, 2.0, &[])], &[&[0]]);
        let selected = select_candidates(&graph).unwrap().unwrap();
        for status in [SolutionStatus::GapLimit, SolutionStatus::TimeLimit] {
            let proposal = decoded_proposal(&graph, selected.clone(), status, 50.0).unwrap();
            assert_eq!(proposal.selected, selected);
            assert_eq!(proposal.model_lower_bound, None);
        }
        assert_eq!(
            decoded_proposal(&graph, selected.clone(), SolutionStatus::Optimal, 2.0)
                .unwrap()
                .model_lower_bound,
            Some(2.0)
        );
        let mut invalid = selected;
        invalid.query_roots.clear();
        assert!(decoded_proposal(&graph, invalid, SolutionStatus::GapLimit, 50.0).is_err());
        for value in [f64::NAN, f64::INFINITY, -1.0, 0.4, 0.999] {
            assert!(validate_binary_value(value).is_err());
        }
        for value in [0.0, 1.0, 1.0 - 1e-8] {
            assert!(validate_binary_value(value).is_ok());
        }
    }

    // A shared producer owns resident state once, regardless of its reader count.
    #[test]
    fn resource_limits_price_shared_state_once_and_change_the_selected_plan() {
        let mut graph = dependency_graph(
            vec![
                candidate(0, 8.0, &[]),
                candidate(1, 1.0, &[&[0]]),
                candidate(2, 1.0, &[&[0]]),
                candidate(1, 12.0, &[]),
                candidate(2, 12.0, &[]),
            ],
            &[&[1, 3], &[2, 4]],
        );
        graph.resource_budgets.push(ResourceBudget {
            name: "resident memory".into(),
            capacity: 6.0,
            candidate_usage: vec![6.0, 0.0, 0.0, 1.0, 1.0],
        });
        assert_eq!(compare_with_oracle(&graph).unwrap().modeled_cost, 10.0);
        let shared = compare_with_oracle(&graph).unwrap();
        graph.resource_budgets[0].capacity = 5.0;
        assert!(validate_selected_graph(&graph, &shared).is_err());
        assert_eq!(compare_with_oracle(&graph).unwrap().modeled_cost, 24.0);
        graph.resource_budgets[0].capacity = 1.0;
        assert!(compare_with_oracle(&graph).is_none());
        graph.resource_budgets[0].candidate_usage.pop();
        assert!(matches!(
            select_candidates(&graph),
            Err(CandidateSelectionError::InvalidGraph(_))
        ));
    }

    // Unreachable large states must not silently erase a live resource constraint.
    #[test]
    fn unsupported_resource_dynamic_range_is_explicitly_rejected() {
        let mut graph = dependency_graph(
            vec![
                candidate(0, 2.0, &[]),
                candidate(0, 1.0, &[]),
                candidate(1, 1.0, &[]),
            ],
            &[&[0, 1]],
        );
        graph.resource_budgets.push(ResourceBudget {
            name: "memory".into(),
            capacity: 1.0,
            candidate_usage: vec![1.0, 2.0, 1e20],
        });
        assert!(matches!(
            select_candidates(&graph),
            Err(CandidateSelectionError::InvalidGraph(_))
        ));
    }

    // Activation ordering does not inspect readers and must not allocate
    // per-output reachability flows through the entire candidate graph.
    #[test]
    fn activation_prefix_needs_no_reader_flow_variables() {
        let mut graph = dependency_graph(
            vec![candidate(0, 1.0, &[]), candidate(0, 1.0, &[])],
            &[&[0], &[1]],
        );
        let mut plain = CandidateSolveMetrics::default();
        solve_candidate_proposal_with_metrics(&graph, &[], None, &mut plain)
            .unwrap()
            .unwrap();
        graph
            .reader_constraints
            .push(ReaderConstraint::ActivationRequires {
                candidate: PhysicalCandidateId(1),
                required_candidate: PhysicalCandidateId(0),
            });
        let mut ordered = CandidateSolveMetrics::default();
        solve_candidate_proposal_with_metrics(&graph, &[], None, &mut ordered)
            .unwrap()
            .unwrap();
        assert_eq!(ordered.peak_variables, plain.peak_variables);
        assert_eq!(ordered.peak_constraints, plain.peak_constraints + 1);
        compare_with_oracle(&graph).unwrap();
    }

    // An incompatible potential reader must choose another state, not disable it for everyone.
    #[test]
    fn reader_legality_depends_on_selected_transitive_connections() {
        let mut graph = dependency_graph(
            vec![
                candidate(0, 2.0, &[]),
                candidate(0, 5.0, &[]),
                candidate(1, 1.0, &[&[0, 1]]),
                candidate(2, 1.0, &[&[0, 1]]),
                candidate(3, 0.0, &[&[2]]),
            ],
            &[&[4], &[3]],
        );
        assert_eq!(compare_with_oracle(&graph).unwrap().modeled_cost, 4.0);
        graph
            .reader_constraints
            .push(ReaderConstraint::AllowedOutputs {
                candidate: PhysicalCandidateId(0),
                outputs: vec![0],
            });
        assert_eq!(compare_with_oracle(&graph).unwrap().modeled_cost, 7.0);
        graph.reader_constraints.clear();
        graph
            .reader_constraints
            .push(ReaderConstraint::IncompatibleOutputs {
                candidate: PhysicalCandidateId(0),
                left: 0,
                right: 1,
            });
        assert_eq!(compare_with_oracle(&graph).unwrap().modeled_cost, 7.0);
        graph
            .reader_constraints
            .push(ReaderConstraint::IncompatibleOutputs {
                candidate: PhysicalCandidateId(1),
                left: 0,
                right: 1,
            });
        assert_eq!(compare_with_oracle(&graph).unwrap().modeled_cost, 9.0);
    }

    // Scalar output references count as one query; separate queries can enable sharing.
    #[test]
    fn minimum_readers_count_queries_not_output_references() {
        let mut graph = dependency_graph(
            vec![candidate(0, 1.0, &[]), candidate(0, 5.0, &[])],
            &[&[0, 1], &[0, 1]],
        );
        graph
            .reader_constraints
            .push(ReaderConstraint::MinimumQueryReaders {
                candidate: PhysicalCandidateId(0),
                query_outputs: vec![vec![0, 1], vec![]],
                minimum: 2,
            });
        let one_query = compare_with_oracle(&graph).unwrap();
        assert_eq!(one_query.selected_candidates, vec![PhysicalCandidateId(1)]);
        // Decoding must reject the same selected cheap state under the one-query mapping.
        graph.reader_constraints.clear();
        let invalid_shared = select_candidates(&graph).unwrap().unwrap();
        graph
            .reader_constraints
            .push(ReaderConstraint::MinimumQueryReaders {
                candidate: PhysicalCandidateId(0),
                query_outputs: vec![vec![0, 1]],
                minimum: 2,
            });
        assert!(validate_selected_graph(&graph, &invalid_shared).is_err());
        graph.reader_constraints[0] = ReaderConstraint::MinimumQueryReaders {
            candidate: PhysicalCandidateId(0),
            query_outputs: vec![vec![0], vec![1], vec![]],
            minimum: 2,
        };
        assert_eq!(
            compare_with_oracle(&graph).unwrap().selected_candidates,
            vec![PhysicalCandidateId(0)]
        );
    }

    // Missing, repeated, and invalid references cannot define a query partition.
    #[test]
    fn minimum_reader_buckets_must_partition_outputs() {
        for query_outputs in [
            vec![vec![0]],
            vec![vec![0, 0]],
            vec![vec![0, 2]],
            vec![vec![0], vec![1, 1]],
        ] {
            let mut graph = dependency_graph(vec![candidate(0, 1.0, &[])], &[&[0], &[0]]);
            graph
                .reader_constraints
                .push(ReaderConstraint::MinimumQueryReaders {
                    candidate: PhysicalCandidateId(0),
                    query_outputs,
                    minimum: 2,
                });
            assert!(matches!(
                select_candidates(&graph),
                Err(CandidateSelectionError::InvalidGraph(_))
            ));
        }
    }

    #[test]
    fn certified_query_cut_ignores_unrelated_queries_but_allows_repaired_input() {
        let graph = dependency_graph(
            vec![
                candidate(0, 1.0, &[]),
                candidate(0, 4.0, &[]),
                candidate(1, 1.0, &[&[0, 1]]),
                candidate(2, 1.0, &[]),
                candidate(2, 2.0, &[]),
            ],
            &[&[2], &[3, 4]],
        );
        let selected = select_candidates(&graph).unwrap().unwrap();
        let witness = RejectedQuerySubgraph::from_query_outputs(&selected, &[0]).unwrap();
        assert_eq!(witness.connections.len(), 1);
        let observations = [ObservedAssignment {
            selected,
            exact_cost: None,
            rejected_queries: vec![witness],
        }];
        let repaired = solve_candidate_proposal(&graph, &observations, None)
            .unwrap()
            .unwrap();
        assert!(repaired
            .selected
            .connections
            .iter()
            .any(|edge| edge.producer == PhysicalCandidateId(1)));
        // Even forcing another unrelated output cannot make the bad input legal.
        let mut changed = graph.clone();
        changed.query_roots[1].compatible_candidates = vec![PhysicalCandidateId(4)];
        // Keep observations structurally valid by changing that unrelated binding.
        let mut observation = observations[0].clone();
        observation
            .selected
            .selected_candidates
            .retain(|id| *id != PhysicalCandidateId(3));
        observation
            .selected
            .selected_candidates
            .push(PhysicalCandidateId(4));
        observation.selected.query_roots[1].candidate = PhysicalCandidateId(4);
        observation.selected.modeled_cost += 1.0;
        let repaired = solve_candidate_proposal(&changed, &[observation], None)
            .unwrap()
            .unwrap();
        assert!(repaired
            .selected
            .connections
            .iter()
            .any(|edge| edge.producer == PhysicalCandidateId(1)));
    }

    #[test]
    fn certified_query_cut_rejects_incomplete_or_foreign_closure() {
        let graph = dependency_graph(
            vec![candidate(0, 1.0, &[]), candidate(1, 1.0, &[&[0]])],
            &[&[1]],
        );
        let selected = select_candidates(&graph).unwrap().unwrap();
        let mut witness = RejectedQuerySubgraph::from_query_outputs(&selected, &[0]).unwrap();
        witness.connections.clear();
        let observation = ObservedAssignment {
            selected,
            exact_cost: None,
            rejected_queries: vec![witness],
        };
        assert!(matches!(
            solve_candidate_proposal(&graph, &[observation], None),
            Err(CandidateSelectionError::InvalidGraph(_))
        ));
    }

    #[test]
    fn query_cut_contains_every_requested_scalar_output_closure() {
        let graph = dependency_graph(
            vec![
                candidate(0, 1.0, &[]),
                candidate(1, 1.0, &[&[0]]),
                candidate(2, 1.0, &[]),
            ],
            &[&[1], &[2]],
        );
        let selected = select_candidates(&graph).unwrap().unwrap();
        let witness = RejectedQuerySubgraph::from_query_outputs(&selected, &[0, 1]).unwrap();
        assert_eq!(witness.query_roots.len(), 2);
        assert_eq!(witness.connections.len(), 1);
        witness.validate(&selected).unwrap();
    }

    // A cut may stop at named producers, but must then hold exactly the
    // closure above them: a stop at a producer the closure never reaches, or
    // a missing edge into one it does not name, is not a valid witness.
    #[test]
    fn query_cut_stops_only_at_named_reached_producers() {
        let graph = dependency_graph(
            vec![
                candidate(0, 1.0, &[]),
                candidate(1, 1.0, &[&[0]]),
                candidate(2, 1.0, &[&[1]]),
            ],
            &[&[2]],
        );
        let selected = select_candidates(&graph).unwrap().unwrap();
        let above = RejectedQuerySubgraph::from_query_outputs_above(&selected, &[0], |id| {
            id == PhysicalCandidateId(1)
        })
        .unwrap();
        assert!(above.connections.is_empty());
        assert_eq!(above.stopped_at, vec![PhysicalCandidateId(1)]);
        above.validate(&selected).unwrap();
        let mut unreached = above.clone();
        unreached.stopped_at = vec![PhysicalCandidateId(0)];
        assert!(unreached.validate(&selected).is_err());
        let mut unnamed = above;
        unnamed.stopped_at.clear();
        assert!(unnamed.validate(&selected).is_err());
    }

    /// An unused ingestion configuration must not scale away improvements in
    /// cheap query-time sharing. The unobserved materialized reader still has
    /// zero certified cost and must be explored before declaring the known
    /// unmaterialized plan optimal, independently of currency units.
    #[test]
    fn expensive_unused_candidate_cannot_hide_cheap_sharing_improvement() {
        for currency_scale in [1e-6, 1.0, 1e6] {
            let graph = dependency_graph(
                vec![
                    candidate(0, 0.0, &[]),
                    candidate(1, 0.0, &[&[0]]),
                    candidate(1, 0.0, &[&[0]]),
                    candidate(1, 16.800072 * currency_scale, &[]),
                ],
                &[&[1, 2, 3], &[1, 2, 3]],
            );
            let observed = SelectedCandidateGraph {
                selected_candidates: vec![PhysicalCandidateId(0), PhysicalCandidateId(1)],
                connections: vec![
                    crate::search::candidates::graph::ProducerConsumerConnection {
                        consumer: PhysicalCandidateId(1),
                        requirement: 0,
                        producer: PhysicalCandidateId(0),
                    },
                ],
                query_roots: (0..2)
                    .map(
                        |query_root| crate::search::candidates::graph::QueryRootAssignment {
                            query_root,
                            candidate: PhysicalCandidateId(1),
                        },
                    )
                    .collect(),
                modeled_cost: 0.0,
            };
            let proposal = solve_candidate_proposal(
                &graph,
                &[ObservedAssignment {
                    selected: observed,
                    exact_cost: Some(8.533333870933332e-6 * currency_scale),
                    rejected_queries: vec![],
                }],
                None,
            )
            .unwrap()
            .unwrap();
            assert_eq!(proposal.model_lower_bound, Some(0.0));
            assert!(proposal
                .selected
                .selected_candidates
                .contains(&PhysicalCandidateId(2)));
        }
    }

    // The start includes ranks and all per-output AND/OR auxiliaries, including
    // two references belonging to one query. It does not change certified cost.
    #[test]
    fn complete_warm_start_preserves_oracle_optimum_and_reader_constraints() {
        let mut graph = dependency_graph(
            vec![
                candidate(0, 3.0, &[]),
                candidate(1, 1.0, &[&[0]]),
                candidate(2, 1.0, &[&[0]]),
            ],
            &[&[1], &[1], &[2]],
        );
        graph
            .reader_constraints
            .push(ReaderConstraint::MinimumQueryReaders {
                candidate: PhysicalCandidateId(0),
                query_outputs: vec![vec![0, 1], vec![2]],
                minimum: 2,
            });
        let selected = compare_with_oracle(&graph).unwrap();
        let observations = vec![ObservedAssignment {
            exact_cost: Some(selected.modeled_cost),
            selected: selected.clone(),
            rejected_queries: vec![],
        }];
        let start = warm_start::cheapest_start(&graph, &observations).unwrap();
        assert_eq!(start.ranks, vec![0, 1, 1]);
        assert_eq!(
            start.reached[0],
            HashSet::from([PhysicalCandidateId(0), PhysicalCandidateId(1)])
        );
        let proposal = solve_candidate_proposal(&graph, &observations, None)
            .unwrap()
            .unwrap();
        assert_eq!(proposal.model_lower_bound, Some(selected.modeled_cost));
        assert_eq!(proposal.selected.modeled_cost, selected.modeled_cost);
    }

    // A certified query rejection invalidates a seed even if another output
    // differs; conflicting full-assignment observations also cannot seed HiGHS.
    #[test]
    fn warm_start_filters_rejections_and_uses_strongest_matching_cost() {
        let graph = dependency_graph(
            vec![candidate(0, 1.0, &[]), candidate(0, 2.0, &[])],
            &[&[0, 1]],
        );
        let selected = compare_with_oracle(&graph).unwrap();
        let mut observations = vec![
            ObservedAssignment {
                selected: selected.clone(),
                exact_cost: Some(3.0),
                rejected_queries: vec![],
            },
            ObservedAssignment {
                selected: selected.clone(),
                exact_cost: Some(5.0),
                rejected_queries: vec![],
            },
        ];
        assert_eq!(
            warm_start::cheapest_start(&graph, &observations)
                .unwrap()
                .cost,
            5.0
        );
        let proposal = solve_candidate_proposal(&graph, &observations, None)
            .unwrap()
            .unwrap();
        assert_eq!(
            proposal.selected.selected_candidates,
            vec![PhysicalCandidateId(1)]
        );
        assert_eq!(proposal.model_lower_bound, Some(2.0));
        observations.push(ObservedAssignment {
            selected: selected.clone(),
            exact_cost: None,
            rejected_queries: vec![
                RejectedQuerySubgraph::from_query_outputs(&selected, &[0]).unwrap()
            ],
        });
        assert!(warm_start::cheapest_start(&graph, &observations).is_none());
        assert_eq!(
            solve_candidate_proposal(&graph, &observations, None)
                .unwrap()
                .unwrap()
                .selected
                .selected_candidates,
            vec![PhysicalCandidateId(1)]
        );
    }

    // A query-scoped cut excludes a warm start even when the rejected complete
    // assignment used a different implementation for another query.
    #[test]
    fn warm_start_checks_partial_rejection_closures() {
        let graph = dependency_graph(
            vec![
                candidate(0, 1.0, &[]),
                candidate(1, 1.0, &[]),
                candidate(1, 2.0, &[]),
            ],
            &[&[0], &[1, 2]],
        );
        let selected = compare_with_oracle(&graph).unwrap();
        let mut rejected = selected.clone();
        rejected.selected_candidates = vec![PhysicalCandidateId(0), PhysicalCandidateId(2)];
        rejected.query_roots[1].candidate = PhysicalCandidateId(2);
        rejected.modeled_cost = 3.0;
        validate_selected_graph(&graph, &rejected).unwrap();
        let witness = RejectedQuerySubgraph::from_query_outputs(&rejected, &[0]).unwrap();
        let observations = vec![
            ObservedAssignment {
                selected,
                exact_cost: Some(2.0),
                rejected_queries: vec![],
            },
            ObservedAssignment {
                selected: rejected,
                exact_cost: None,
                rejected_queries: vec![witness],
            },
        ];
        assert!(warm_start::cheapest_start(&graph, &observations).is_none());
        assert!(solve_candidate_proposal(&graph, &observations, None)
            .unwrap()
            .is_none());
    }

    // Stage 2 permits non-summary materialization only at the highest shared
    // node: a shared parent blocks it, while distinct private parents allow it.
    #[test]
    fn highest_shared_materialization_matches_selected_connections() {
        let mut graph = dependency_graph(
            vec![
                candidate(0, 1.0, &[]),
                candidate(0, 5.0, &[]),
                candidate(1, 1.0, &[&[0, 1]]),
                candidate(2, 1.0, &[&[0, 1]]),
            ],
            &[&[2], &[2]],
        );
        graph
            .reader_constraints
            .push(ReaderConstraint::HighestSharedQueryMaterialization {
                candidate: PhysicalCandidateId(0),
                query_outputs: vec![vec![0], vec![1]],
            });
        assert_eq!(compare_with_oracle(&graph).unwrap().modeled_cost, 6.0);
        graph.query_roots[1].compatible_candidates = vec![PhysicalCandidateId(3)];
        assert_eq!(compare_with_oracle(&graph).unwrap().modeled_cost, 3.0);
        graph.query_roots[0].compatible_candidates = vec![PhysicalCandidateId(0)];
        let selected = compare_with_oracle(&graph).unwrap();
        assert_eq!(selected.modeled_cost, 2.0);
        let observation = ObservedAssignment {
            selected,
            exact_cost: Some(2.0),
            rejected_queries: vec![],
        };
        assert!(solve_candidate_proposal(&graph, &[observation], None)
            .unwrap()
            .is_some());
    }

    fn candidate(group: u32, cost: f64, inputs: &[&[usize]]) -> PhysicalCandidate {
        PhysicalCandidate {
            group: MemoGroupId(group),
            local_cost: cost,
            producer_requirements: inputs
                .iter()
                .map(|sources| ProducerRequirement {
                    compatible_producers: sources
                        .iter()
                        .copied()
                        .map(PhysicalCandidateId)
                        .collect(),
                })
                .collect(),
        }
    }

    fn dependency_graph(
        candidates: Vec<PhysicalCandidate>,
        roots: &[&[usize]],
    ) -> CandidateDependencyGraph {
        CandidateDependencyGraph {
            candidates,
            resource_budgets: Vec::new(),
            reader_constraints: Vec::new(),
            query_roots: roots
                .iter()
                .map(|choices| QueryRootCandidates {
                    compatible_candidates: choices
                        .iter()
                        .copied()
                        .map(PhysicalCandidateId)
                        .collect(),
                })
                .collect(),
        }
    }

    /// Independent small-input oracle: enumerate roots and edges, then traverse
    /// their reachable graph. It uses DFS cycle detection, not rank constraints.
    fn exhaustive_cost(graph: &CandidateDependencyGraph) -> Option<f64> {
        let edges: Vec<_> = graph
            .candidates
            .iter()
            .enumerate()
            .flat_map(|(consumer, c)| {
                c.producer_requirements
                    .iter()
                    .enumerate()
                    .flat_map(move |(requirement, input)| {
                        input
                            .compatible_producers
                            .iter()
                            .map(move |producer| (consumer, requirement, producer.0))
                    })
            })
            .collect();
        let roots: Vec<_> = graph
            .query_roots
            .iter()
            .enumerate()
            .flat_map(|(q, root)| root.compatible_candidates.iter().map(move |c| (q, c.0)))
            .collect();
        let bits = edges.len() + roots.len();
        assert!(
            bits <= 20,
            "oracle is deliberately restricted to tiny graphs"
        );
        let mut best: Option<f64> = None;
        for assignment in 0..(1usize << bits) {
            let chosen = |bit: usize| assignment & (1usize << bit) != 0;
            let selected_roots: Vec<_> = roots
                .iter()
                .enumerate()
                .filter(|(i, _)| chosen(edges.len() + i))
                .map(|(_, root)| *root)
                .collect();
            if (0..graph.query_roots.len())
                .any(|q| selected_roots.iter().filter(|(root, _)| *root == q).count() != 1)
            {
                continue;
            }
            let mut dependencies = vec![Vec::new(); graph.candidates.len()];
            for (index, &(consumer, requirement, producer)) in edges.iter().enumerate() {
                if chosen(index) {
                    dependencies[consumer].push((requirement, producer));
                }
            }
            fn visit(
                candidate: usize,
                graph: &CandidateDependencyGraph,
                dependencies: &[Vec<(usize, usize)>],
                marks: &mut [u8],
            ) -> bool {
                if marks[candidate] == 1 {
                    return false;
                }
                if marks[candidate] == 2 {
                    return true;
                }
                marks[candidate] = 1;
                for requirement in 0..graph.candidates[candidate].producer_requirements.len() {
                    let producers: Vec<_> = dependencies[candidate]
                        .iter()
                        .filter(|(r, _)| *r == requirement)
                        .collect();
                    if producers.len() != 1 || !visit(producers[0].1, graph, dependencies, marks) {
                        return false;
                    }
                }
                marks[candidate] = 2;
                true
            }
            let mut marks = vec![0; graph.candidates.len()];
            if !selected_roots
                .iter()
                .all(|(_, root)| visit(*root, graph, &dependencies, &mut marks))
            {
                continue;
            }
            if dependencies
                .iter()
                .enumerate()
                .any(|(c, edges)| marks[c] == 0 && !edges.is_empty())
            {
                continue;
            }
            if graph.resource_budgets.iter().any(|budget| {
                budget
                    .candidate_usage
                    .iter()
                    .zip(&marks)
                    .filter(|(_, mark)| **mark == 2)
                    .map(|(usage, _)| usage)
                    .sum::<f64>()
                    > budget.capacity
            }) {
                continue;
            }
            let reaches = |output: usize, target: PhysicalCandidateId| {
                let mut individual = vec![0; graph.candidates.len()];
                for (_, root) in selected_roots.iter().filter(|(q, _)| *q == output) {
                    if !visit(*root, graph, &dependencies, &mut individual) {
                        return false;
                    }
                }
                individual[target.0] == 2
            };
            if graph
                .reader_constraints
                .iter()
                .any(|requirement| match requirement {
                    ReaderConstraint::ActivationRequires {
                        candidate,
                        required_candidate,
                    } => marks[candidate.0] == 2 && marks[required_candidate.0] != 2,
                    ReaderConstraint::HighestSharedQueryMaterialization {
                        candidate,
                        query_outputs,
                    } => {
                        marks[candidate.0] == 2
                            && !selected_roots.iter().any(|(_, root)| *root == candidate.0)
                            && !dependencies
                                .iter()
                                .enumerate()
                                .any(|(consumer, producers)| {
                                    marks[consumer] == 2
                                        && producers
                                            .iter()
                                            .any(|(_, producer)| *producer == candidate.0)
                                        && query_outputs
                                            .iter()
                                            .filter(|outputs| {
                                                outputs.iter().any(|q| {
                                                    reaches(*q, PhysicalCandidateId(consumer))
                                                })
                                            })
                                            .count()
                                            <= 1
                                })
                    }
                    ReaderConstraint::MinimumQueryReaders {
                        candidate,
                        query_outputs,
                        minimum,
                    } => {
                        marks[candidate.0] == 2
                            && query_outputs
                                .iter()
                                .filter(|outputs| {
                                    outputs.iter().any(|&output| reaches(output, *candidate))
                                })
                                .count()
                                < *minimum
                    }
                    ReaderConstraint::AllowedOutputs { candidate, outputs } => {
                        (0..graph.query_roots.len())
                            .any(|output| !outputs.contains(&output) && reaches(output, *candidate))
                    }
                    ReaderConstraint::IncompatibleOutputs {
                        candidate,
                        left,
                        right,
                    } => reaches(*left, *candidate) && reaches(*right, *candidate),
                })
            {
                continue;
            }
            let cost = graph
                .candidates
                .iter()
                .zip(&marks)
                .filter(|(_, mark)| **mark == 2)
                .map(|(c, _)| c.local_cost)
                .sum::<f64>();
            best = Some(best.map_or(cost, |previous| previous.min(cost)));
        }
        best
    }

    fn compare_with_oracle(graph: &CandidateDependencyGraph) -> Option<SelectedCandidateGraph> {
        let expected = exhaustive_cost(graph);
        let actual = select_candidates(graph).unwrap();
        match (&actual, expected) {
            (Some(plan), Some(expected)) => {
                assert!((plan.modeled_cost - expected).abs() <= 1e-8 * expected.abs().max(1.0))
            }
            (None, None) => {}
            _ => panic!("MILP {actual:?}, exhaustive {expected:?}"),
        }
        actual
    }

    /// One activated build serves two query roots without paying per connection.
    #[test]
    fn shared_build_cost_is_owned_once() {
        let graph = dependency_graph(
            vec![
                candidate(0, 8.0, &[]),
                candidate(1, 1.0, &[&[0]]),
                candidate(2, 1.0, &[&[0]]),
                candidate(1, 9.0, &[]),
                candidate(2, 9.0, &[]),
            ],
            &[&[1, 3], &[2, 4]],
        );
        let selected = compare_with_oracle(&graph).unwrap();
        assert_eq!(selected.modeled_cost, 10.0);
        assert_eq!(selected.connections.len(), 2);
        assert!(selected
            .connections
            .iter()
            .all(|c| c.producer == PhysicalCandidateId(0)));
    }

    /// Distinct consumer contracts can require two physical choices of one group.
    #[test]
    fn one_group_can_have_multiple_selected_candidates() {
        let graph = dependency_graph(
            vec![
                candidate(0, 3.0, &[]),
                candidate(0, 5.0, &[]),
                candidate(1, 1.0, &[&[0]]),
                candidate(2, 1.0, &[&[1]]),
            ],
            &[&[2], &[3]],
        );
        let selected = compare_with_oracle(&graph).unwrap();
        assert_eq!(selected.modeled_cost, 10.0);
        assert_eq!(
            selected
                .selected_candidates
                .iter()
                .filter(|c| graph.candidates[c.0].group == MemoGroupId(0))
                .count(),
            2
        );
    }

    /// A join needs both requirements, even when one producer supplies both.
    #[test]
    fn every_requirement_has_a_producer() {
        let graph = dependency_graph(
            vec![candidate(0, 2.0, &[]), candidate(1, 1.0, &[&[0], &[0]])],
            &[&[1]],
        );
        let selected = compare_with_oracle(&graph).unwrap();
        assert_eq!(selected.modeled_cost, 3.0);
        assert_eq!(
            selected
                .connections
                .iter()
                .map(|c| c.requirement)
                .collect::<Vec<_>>(),
            vec![0, 1]
        );
        let impossible = dependency_graph(vec![candidate(0, 1.0, &[&[]])], &[&[0]]);
        assert!(compare_with_oracle(&impossible).is_none());
    }

    /// Root-reachable cycles are infeasible; unused zero-cost cycles cannot activate.
    #[test]
    fn cycles_and_unused_nodes_are_excluded() {
        let graph = dependency_graph(
            vec![
                candidate(0, 0.0, &[]),
                candidate(1, 0.0, &[&[2]]),
                candidate(2, 0.0, &[&[1]]),
                candidate(3, 0.0, &[]),
            ],
            &[&[0]],
        );
        let selected = compare_with_oracle(&graph).unwrap();
        assert_eq!(selected.selected_candidates, vec![PhysicalCandidateId(0)]);
        let cyclic = dependency_graph(graph.candidates, &[&[1]]);
        assert!(compare_with_oracle(&cyclic).is_none());
        let self_cycle = dependency_graph(vec![candidate(0, 0.0, &[&[0]])], &[&[0]]);
        assert!(compare_with_oracle(&self_cycle).is_none());
    }

    /// Dense compatibility choices agree with exhaustive graph selection across costs.
    #[test]
    fn competing_producers_match_exhaustive_enumeration() {
        for build_cost in [0.0, 1.0, 4.0, 12.0] {
            for local_cost in [0.5, 3.0, 8.0] {
                let graph = dependency_graph(
                    vec![
                        candidate(0, build_cost, &[]),
                        candidate(0, 2.0, &[]),
                        candidate(1, 0.25, &[&[0, 1]]),
                        candidate(1, local_cost, &[]),
                        candidate(2, 0.25, &[&[0, 1]]),
                        candidate(2, local_cost, &[]),
                    ],
                    &[&[2, 3], &[4, 5]],
                );
                compare_with_oracle(&graph);
            }
        }
    }

    /// Currency-unit scaling must not erase the cheaper candidate's advantage.
    #[test]
    fn small_uniform_cost_units_preserve_selection() {
        let graph = dependency_graph(
            vec![candidate(0, 3e-15, &[]), candidate(0, 1e-15, &[])],
            &[&[0, 1]],
        );
        let selected = select_candidates(&graph).unwrap().unwrap();
        assert_eq!(selected.selected_candidates, vec![PhysicalCandidateId(1)]);
        assert_eq!(selected.modeled_cost, 1e-15);
    }

    /// An unrelated large candidate must not erase relevant root-choice costs.
    #[test]
    fn unsupported_cost_dynamic_range_is_rejected() {
        let graph = dependency_graph(
            vec![
                candidate(0, 1e300, &[]),
                candidate(1, 1e-300, &[]),
                candidate(1, 2e-300, &[]),
            ],
            &[&[1, 2]],
        );
        assert!(matches!(
            select_candidates(&graph),
            Err(CandidateSelectionError::InvalidGraph(_))
        ));
    }

    /// Decoder validation catches missing requirements and unused selected nodes.
    #[test]
    fn decoded_graph_is_checked_independently_of_solver_values() {
        let graph = dependency_graph(
            vec![
                candidate(0, 2.0, &[]),
                candidate(1, 1.0, &[&[0]]),
                candidate(2, 0.0, &[]),
            ],
            &[&[1]],
        );
        let selected = select_candidates(&graph).unwrap().unwrap();
        assert!(validate_selected_graph(&graph, &selected).is_ok());
        let mut missing_input = selected.clone();
        missing_input.connections.clear();
        assert!(validate_selected_graph(&graph, &missing_input).is_err());
        let mut unused = selected.clone();
        unused.selected_candidates.push(PhysicalCandidateId(2));
        assert!(validate_selected_graph(&graph, &unused).is_err());
        let mut bad_root = selected;
        bad_root.query_roots[0].query_root = 9;
        assert!(validate_selected_graph(&graph, &bad_root).is_err());
    }

    /// Invalid indices and numeric costs fail before entering the native solver.
    #[test]
    fn malformed_graphs_return_errors() {
        for cost in [f64::NAN, f64::INFINITY, -1.0] {
            assert!(matches!(
                select_candidates(&dependency_graph(vec![candidate(0, cost, &[])], &[&[0]])),
                Err(CandidateSelectionError::InvalidGraph(_))
            ));
        }
        for invalid in [
            dependency_graph(vec![candidate(0, 1.0, &[&[1]])], &[&[0]]),
            dependency_graph(vec![candidate(0, 1.0, &[])], &[&[1]]),
            dependency_graph(vec![candidate(0, 1.0, &[&[0, 0]])], &[&[0]]),
            dependency_graph(vec![candidate(0, 1.0, &[])], &[&[0, 0]]),
        ] {
            assert!(matches!(
                select_candidates(&invalid),
                Err(CandidateSelectionError::InvalidGraph(_))
            ));
        }
        assert_eq!(
            select_candidates(&CandidateDependencyGraph::default())
                .unwrap()
                .unwrap()
                .modeled_cost,
            0.0
        );
        assert!(select_candidates(&dependency_graph(Vec::new(), &[&[]]))
            .unwrap()
            .is_none());
    }
}

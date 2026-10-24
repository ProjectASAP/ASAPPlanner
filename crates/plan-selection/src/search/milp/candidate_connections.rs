//! Fixed-context selection over explicit physical candidates and connections.
//!
//! This is the selection core for the joint-planning migration, not a new
//! production search entry point. Candidate generation must supply executable
//! candidates and compatible producers. Costs cover a fixed accounting context;
//! demand-dependent costs, accuracy refinement, materialization policy,
//! nonadditive resource limits, and physical IR decoding still belong to the surrounding pipeline.

use std::collections::HashSet;

use asap_logical_asap_optimization::memo::MemoGroupId;
use good_lp::solvers::highs::highs;
use good_lp::solvers::{ResolutionError, SolutionStatus};
use good_lp::{
    constraint, variable, Expression, ProblemVariables, Solution, SolverModel, Variable,
};

/// Index into [`CandidateDependencyGraph::candidates`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub(crate) struct PhysicalCandidateId(pub usize);

/// One required input, distinct even when another input accepts the same producers.
#[derive(Debug, Clone)]
pub(crate) struct ProducerRequirement {
    pub compatible_producers: Vec<PhysicalCandidateId>,
}

/// One executable choice with costs already expressed over the same fixed horizon.
#[derive(Debug, Clone)]
pub(crate) struct PhysicalCandidate {
    /// Logical ownership does not impose one selected candidate per group.
    pub group: MemoGroupId,
    /// Local work and state only; producer costs must not appear here again.
    pub local_cost: f64,
    pub producer_requirements: Vec<ProducerRequirement>,
}

/// Candidates allowed to provide one required query output.
#[derive(Debug, Clone)]
pub(crate) struct QueryRootCandidates {
    pub compatible_candidates: Vec<PhysicalCandidateId>,
}

/// Additive usage over a fixed accounting context, owned once per candidate.
/// Peak temporaries and connection-dependent demand require a different model.
#[derive(Debug, Clone)]
pub(crate) struct ResourceBudget {
    pub name: String,
    pub capacity: f64,
    /// Same index order as the graph's candidates.
    pub candidate_usage: Vec<f64>,
}

#[derive(Debug, Clone, Default)]
pub(crate) struct CandidateDependencyGraph {
    pub candidates: Vec<PhysicalCandidate>,
    pub query_roots: Vec<QueryRootCandidates>,
    pub resource_budgets: Vec<ResourceBudget>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ProducerConsumerConnection {
    pub producer: PhysicalCandidateId,
    pub consumer: PhysicalCandidateId,
    /// Index in the consumer's `producer_requirements`.
    pub requirement: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct QueryRootAssignment {
    /// Index in the graph's `query_roots`.
    pub query_root: usize,
    pub candidate: PhysicalCandidateId,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct SelectedCandidateGraph {
    pub selected_candidates: Vec<PhysicalCandidateId>,
    pub connections: Vec<ProducerConsumerConnection>,
    pub query_roots: Vec<QueryRootAssignment>,
    pub modeled_cost: f64,
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum CandidateSelectionError {
    #[error("invalid candidate dependency graph: {0}")]
    InvalidGraph(String),
    #[error("candidate selection solver: {0}")]
    Solver(String),
}

// The core normalizes its largest local cost to one. Smaller positive
// coefficients must stay representable by HiGHS; rejecting a wider range is an
// explicit model restriction, not proof of infeasibility or a completeness claim.
// Resource rows use the same conservative positive coefficient range.
const MINIMUM_RELATIVE_COST: f64 = 1e-9;

impl CandidateDependencyGraph {
    pub(super) fn validate(&self) -> Result<(), CandidateSelectionError> {
        let validate_ids = |ids: &[PhysicalCandidateId], context: &str| {
            let mut seen = HashSet::new();
            for id in ids {
                if id.0 >= self.candidates.len() {
                    return Err(CandidateSelectionError::InvalidGraph(format!(
                        "{context} references missing candidate {}",
                        id.0
                    )));
                }
                if !seen.insert(*id) {
                    return Err(CandidateSelectionError::InvalidGraph(format!(
                        "{context} repeats candidate {}",
                        id.0
                    )));
                }
            }
            Ok(())
        };
        for budget in &self.resource_budgets {
            if budget.name.is_empty()
                || !budget.capacity.is_finite()
                || budget.capacity < 0.0
                || budget.candidate_usage.len() != self.candidates.len()
                || budget
                    .candidate_usage
                    .iter()
                    .any(|usage| !usage.is_finite() || *usage < 0.0)
                || !budget.candidate_usage.iter().sum::<f64>().is_finite()
            {
                return Err(CandidateSelectionError::InvalidGraph(format!(
                    "resource budget {:?} needs finite nonnegative capacity and one finite nonnegative usage per candidate", budget.name
                )));
            }
        }
        for budget in &self.resource_budgets {
            let scale = budget
                .candidate_usage
                .iter()
                .copied()
                .fold(budget.capacity, f64::max);
            if scale > 0.0
                && budget
                    .candidate_usage
                    .iter()
                    .chain(std::iter::once(&budget.capacity))
                    .any(|value| *value > 0.0 && *value / scale < MINIMUM_RELATIVE_COST)
            {
                return Err(CandidateSelectionError::InvalidGraph(format!(
                    "resource budget {:?} exceeds the supported positive coefficient range ({MINIMUM_RELATIVE_COST}); rescale or reformulate the resource model", budget.name
                )));
            }
        }
        let mut total_cost = 0.0;
        for (index, candidate) in self.candidates.iter().enumerate() {
            if !candidate.local_cost.is_finite() || candidate.local_cost < 0.0 {
                return Err(CandidateSelectionError::InvalidGraph(format!(
                    "candidate {index} needs a finite nonnegative local cost"
                )));
            }
            total_cost += candidate.local_cost;
            for (requirement, input) in candidate.producer_requirements.iter().enumerate() {
                validate_ids(
                    &input.compatible_producers,
                    &format!("candidate {index}, requirement {requirement}"),
                )?;
            }
        }
        if !total_cost.is_finite() {
            return Err(CandidateSelectionError::InvalidGraph(
                "total candidate cost overflows".into(),
            ));
        }
        let maximum_cost = self
            .candidates
            .iter()
            .map(|c| c.local_cost)
            .fold(0.0, f64::max);
        if maximum_cost > 0.0
            && self.candidates.iter().any(|candidate| {
                candidate.local_cost > 0.0
                    && candidate.local_cost / maximum_cost < MINIMUM_RELATIVE_COST
            })
        {
            return Err(CandidateSelectionError::InvalidGraph(format!(
                "positive local costs must be at least {MINIMUM_RELATIVE_COST} of the largest cost; wider ranges require a different numerical policy"
            )));
        }
        for (index, root) in self.query_roots.iter().enumerate() {
            validate_ids(&root.compatible_candidates, &format!("query root {index}"))?;
        }
        Ok(())
    }
}

/// Return the cheapest covering DAG, or `None` when no such graph exists.
/// Numerical optimality is subject to HiGHS tolerances. No production timeout or
/// incumbent fallback is implied by this fixed-context core.
pub(crate) fn select_candidates(
    graph: &CandidateDependencyGraph,
) -> Result<Option<SelectedCandidateGraph>, CandidateSelectionError> {
    graph.validate()?;
    if graph.query_roots.is_empty() {
        return Ok(Some(SelectedCandidateGraph {
            selected_candidates: Vec::new(),
            connections: Vec::new(),
            query_roots: Vec::new(),
            modeled_cost: 0.0,
        }));
    }
    if graph
        .query_roots
        .iter()
        .any(|root| root.compatible_candidates.is_empty())
    {
        return Ok(None);
    }

    let count = graph.candidates.len();
    let mut variables = ProblemVariables::new();
    let candidate_selected: Vec<Variable> = (0..count)
        .map(|_| variables.add(variable().binary()))
        .collect();
    let topological_rank: Vec<Variable> = (0..count)
        .map(|_| variables.add(variable().min(0).max((count - 1) as f64)))
        .collect();
    let mut connections = Vec::new();
    let mut root_assignments = Vec::new();
    let mut constraints = Vec::new();
    let mut support = vec![Expression::from(0.0); count];

    for (consumer, candidate) in graph.candidates.iter().enumerate() {
        for (requirement, input) in candidate.producer_requirements.iter().enumerate() {
            let mut producers_selected = Expression::from(0.0);
            for &producer in &input.compatible_producers {
                let connection_selected = variables.add(variable().binary());
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
            let assignment_selected = variables.add(variable().binary());
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
    let divisor = if maximum_cost > 0.0 {
        maximum_cost
    } else {
        1.0
    };
    let objective: Expression = graph
        .candidates
        .iter()
        .enumerate()
        .map(|(index, candidate)| (candidate.local_cost / divisor) * candidate_selected[index])
        .sum();
    let model = variables
        .minimise(objective)
        .using(highs)
        .set_option("mip_rel_gap", 0.0)
        .set_option("mip_abs_gap", 0.0)
        .with_all(constraints);
    let solution = match model.solve() {
        Ok(solution) => solution,
        Err(ResolutionError::Infeasible) => return Ok(None),
        Err(error) => return Err(CandidateSelectionError::Solver(error.to_string())),
    };
    if !matches!(solution.status(), SolutionStatus::Optimal) {
        return Err(CandidateSelectionError::Solver(format!(
            "search stopped with {:?}",
            solution.status()
        )));
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
    validate_selected_graph(graph, &selected).map_err(CandidateSelectionError::Solver)?;
    Ok(Some(selected))
}

/// Check the decoded integral graph instead of trusting rounded solver values.
pub(super) fn validate_selected_graph(
    graph: &CandidateDependencyGraph,
    selected: &SelectedCandidateGraph,
) -> Result<(), String> {
    let invalid =
        || "decoded candidate selection violates dependency or root constraints".to_string();
    let active: HashSet<_> = selected.selected_candidates.iter().copied().collect();
    if active.len() != selected.selected_candidates.len()
        || active.iter().any(|c| c.0 >= graph.candidates.len())
        || selected.query_roots.len() != graph.query_roots.len()
    {
        return Err(invalid());
    }
    for budget in &graph.resource_budgets {
        let usage: f64 = active.iter().map(|c| budget.candidate_usage[c.0]).sum();
        if usage > budget.capacity {
            return Err(format!(
                "decoded selection exceeds resource budget {:?}",
                budget.name
            ));
        }
    }
    let mut root_seen = vec![false; graph.query_roots.len()];
    for root in &selected.query_roots {
        let Some(allowed) = graph.query_roots.get(root.query_root) else {
            return Err(invalid());
        };
        if root_seen[root.query_root]
            || !active.contains(&root.candidate)
            || !allowed.compatible_candidates.contains(&root.candidate)
        {
            return Err(invalid());
        }
        root_seen[root.query_root] = true;
    }
    let mut producers: Vec<Vec<Option<PhysicalCandidateId>>> = graph
        .candidates
        .iter()
        .map(|c| vec![None; c.producer_requirements.len()])
        .collect();
    for connection in &selected.connections {
        if !active.contains(&connection.producer) || !active.contains(&connection.consumer) {
            return Err(invalid());
        }
        let Some(requirement) = graph.candidates[connection.consumer.0]
            .producer_requirements
            .get(connection.requirement)
        else {
            return Err(invalid());
        };
        let target = &mut producers[connection.consumer.0][connection.requirement];
        if target.is_some()
            || !requirement
                .compatible_producers
                .contains(&connection.producer)
        {
            return Err(invalid());
        }
        *target = Some(connection.producer);
    }
    if active
        .iter()
        .any(|c| producers[c.0].iter().any(Option::is_none))
    {
        return Err(invalid());
    }
    // Kahn's algorithm checks cycles without recursion on a potentially deep DAG.
    let mut outstanding: Vec<_> = producers.iter().map(Vec::len).collect();
    let mut consumers = vec![Vec::new(); graph.candidates.len()];
    for connection in &selected.connections {
        consumers[connection.producer.0].push(connection.consumer.0);
    }
    let mut ready: Vec<_> = active
        .iter()
        .filter(|c| outstanding[c.0] == 0)
        .map(|c| c.0)
        .collect();
    let mut visited = 0;
    while let Some(producer) = ready.pop() {
        visited += 1;
        for &consumer in &consumers[producer] {
            outstanding[consumer] -= 1;
            if outstanding[consumer] == 0 {
                ready.push(consumer);
            }
        }
    }
    if visited != active.len() {
        return Err(invalid());
    }
    let mut reachable = HashSet::new();
    let mut pending: Vec<_> = selected
        .query_roots
        .iter()
        .map(|root| root.candidate)
        .collect();
    while let Some(candidate) = pending.pop() {
        if reachable.insert(candidate) {
            pending.extend(producers[candidate.0].iter().flatten().copied());
        }
    }
    if reachable != active {
        return Err(invalid());
    }
    let expected_cost: f64 = active
        .iter()
        .map(|c| graph.candidates[c.0].local_cost)
        .sum();
    if !selected.modeled_cost.is_finite()
        || (selected.modeled_cost - expected_cost).abs() > 1e-9 * expected_cost.abs()
    {
        return Err("decoded candidate cost disagrees with selected candidates".into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

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

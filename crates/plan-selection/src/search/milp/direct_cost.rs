//! Exact objective encoding for certified equal-schedule query-time profiles.
//! Profiles are supplied by physical costing; this module never infers prices
//! from a logical template or treats accuracy as a monetary penalty.
use good_lp::{constraint, variable, Constraint, Expression, ProblemVariables, Variable};
use std::collections::BTreeMap;

#[path = "direct_schedule.rs"]
mod schedules;

use crate::search::candidates::graph::*;

/// Smallest/largest monetary coefficient ratio after lifetime normalization.
/// The minimum is scaled to one; no positive cost is rounded away.
pub(super) const MINIMUM_DIRECT_RELATIVE_COEFFICIENT: f64 = 1e-12;

// Weighted-lifetime equalities use dimensionless rates. Their small entries
// need a stronger guard than monetary objective coefficients, since absolute
// LP feasibility tolerances can erase a rare reader's otherwise large cost.
const MINIMUM_QUERY_RATE_WEIGHT: f64 = 1e-6;

#[derive(Debug, Clone, Default)]
pub(crate) struct NodeCostProfile {
    pub execution_cost: f64,
    pub ingestion: bool,
    pub standing_cost_per_second: f64,
    pub work_seconds: f64,
    pub held_memory_cost_per_second: f64,
    pub materialized: bool,
    pub raw_retention: Option<(String, f64)>,
}

impl NodeCostProfile {
    fn query_work_seconds(&self) -> f64 {
        if self.ingestion {
            0.0
        } else {
            self.work_seconds
        }
    }
}

/// Exact evaluator demand for each nonempty selected query subset. Only small
/// heterogeneous domains are admitted; this is not a workload-plan product.
#[derive(Debug, Clone)]
pub(crate) struct QueryScheduleRates {
    pub per_query_rates: Vec<f64>,
    pub materialized_subset_rates: Vec<f64>,
}

#[derive(Debug, Clone)]
pub(crate) struct DirectCostProfile {
    pub nodes: Vec<NodeCostProfile>,
    pub schedules: Option<QueryScheduleRates>,
    pub query_rate: f64,
    pub request_latency_seconds: f64,
    /// Total request fee per second across all queries.
    pub request_cost: f64,
}

impl DirectCostProfile {
    pub(crate) fn validate(
        &self,
        graph: &CandidateDependencyGraph,
    ) -> Result<(), CandidateSelectionError> {
        let valid = |v: f64| v.is_finite() && v >= 0.0;
        if self.nodes.len() != graph.candidates.len()
            || !valid(self.query_rate)
            || self.query_rate == 0.0
            || !valid(self.request_latency_seconds)
            || !valid(self.request_cost)
            || self.nodes.iter().any(|n| {
                !valid(n.execution_cost)
                    || !valid(n.standing_cost_per_second)
                    || (n.ingestion && n.held_memory_cost_per_second != 0.0)
                    || !valid(n.work_seconds)
                    || !valid(n.held_memory_cost_per_second)
                    || n.raw_retention
                        .as_ref()
                        .is_some_and(|(_, cost)| !valid(*cost))
            })
        {
            return Err(CandidateSelectionError::InvalidGraph(
                "invalid direct-cost profile".into(),
            ));
        }
        for (consumer, candidate) in graph.candidates.iter().enumerate() {
            for producer in candidate
                .producer_requirements
                .iter()
                .flat_map(|input| input.compatible_producers.iter())
            {
                let source = &self.nodes[producer.0];
                if (self.nodes[consumer].ingestion && !source.ingestion)
                    || (!self.nodes[consumer].ingestion && source.ingestion && !source.materialized)
                {
                    return Err(CandidateSelectionError::InvalidGraph(
                        "direct-cost edge crosses an unsupported ingestion boundary".into(),
                    ));
                }
            }
        }
        if graph
            .query_roots
            .iter()
            .flat_map(|root| &root.compatible_candidates)
            .any(|id| self.nodes[id.0].ingestion && !self.nodes[id.0].materialized)
        {
            return Err(CandidateSelectionError::InvalidGraph(
                "query output cannot directly read unmaterialized ingestion work".into(),
            ));
        }
        if let Some(schedule) = &self.schedules {
            let queries = graph.query_roots.len();
            if queries > 3 {
                return Err(CandidateSelectionError::UnsupportedDirectCost(
                    "heterogeneous query schedule table supports at most three output queries"
                        .into(),
                ));
            }
            if !schedule.per_query_rates.iter().sum::<f64>().is_finite()
                || schedule.per_query_rates.len() != queries
                || schedule.materialized_subset_rates.len() != (1usize << queries)
                || schedule.materialized_subset_rates[0] != 0.0
                || schedule
                    .per_query_rates
                    .iter()
                    .any(|rate| !rate.is_finite() || *rate <= 0.0)
                || schedule
                    .materialized_subset_rates
                    .iter()
                    .skip(1)
                    .any(|rate| !rate.is_finite() || *rate <= 0.0)
                || schedule
                    .per_query_rates
                    .iter()
                    .enumerate()
                    .any(|(q, rate)| *rate != schedule.materialized_subset_rates[1 << q])
            {
                return Err(CandidateSelectionError::InvalidGraph(
                    "invalid direct query-schedule rates".into(),
                ));
            }
        }
        if let Some(schedule) = &self.schedules {
            let total_rate: f64 = schedule.per_query_rates.iter().sum();
            if schedule
                .per_query_rates
                .iter()
                .any(|rate| rate / total_rate < MINIMUM_QUERY_RATE_WEIGHT)
            {
                return Err(CandidateSelectionError::UnsupportedDirectCost(
                    "query-rate weights are below the supported weighted-lifetime numerical range"
                        .into(),
                ));
            }
        }
        let maximum_latency = self.maximum_latency();
        if self.coefficients().any(|cost| !cost.is_finite())
            || !maximum_latency.is_finite()
            || !(maximum_latency * graph.query_roots.len() as f64).is_finite()
        {
            return Err(CandidateSelectionError::InvalidGraph(
                "direct-cost latency bound overflow".into(),
            ));
        }
        Ok(())
    }

    /// Recompute the complete selected objective independently of LP auxiliaries.
    /// A time-limited incumbent may leave nonminimal epigraph variables.
    pub(crate) fn selected_cost(
        &self,
        graph: &CandidateDependencyGraph,
        selected: &SelectedCandidateGraph,
    ) -> f64 {
        let mut inputs = vec![Vec::new(); self.nodes.len()];
        for edge in &selected.connections {
            inputs[edge.consumer.0].push(edge.producer.0);
        }
        let mut reached = vec![vec![false; self.nodes.len()]; graph.query_roots.len()];
        for root in &selected.query_roots {
            let mut pending = vec![root.candidate.0];
            while let Some(id) = pending.pop() {
                if !std::mem::replace(&mut reached[root.query_root][id], true) {
                    pending.extend(&inputs[id]);
                }
            }
        }
        let latency: Vec<f64> = reached
            .iter()
            .map(|row| {
                self.request_latency_seconds
                    + row
                        .iter()
                        .enumerate()
                        .filter(|(_, yes)| **yes)
                        .map(|(id, _)| self.nodes[id].query_work_seconds())
                        .sum::<f64>()
            })
            .collect();
        let rates = self.schedules.as_ref().map_or_else(
            || vec![self.query_rate; graph.query_roots.len()],
            |schedule| schedule.per_query_rates.clone(),
        );
        let union_rate = |id: usize| {
            if self.nodes[id].ingestion {
                return 1.0;
            }
            let Some(schedule) = &self.schedules else {
                return self.query_rate;
            };
            let mask = reached
                .iter()
                .enumerate()
                .filter(|(_, row)| row[id])
                .fold(0usize, |mask, (q, _)| mask | (1 << q));
            schedule.materialized_subset_rates[mask]
        };
        let mut execution_rates = vec![0.0; self.nodes.len()];
        let boundaries = selected
            .query_roots
            .iter()
            .map(|root| (vec![root.candidate.0], rates[root.query_root]))
            .chain(
                selected
                    .selected_candidates
                    .iter()
                    .filter(|id| self.nodes[id.0].materialized)
                    .map(|id| (inputs[id.0].clone(), union_rate(id.0))),
            );
        for (mut pending, rate) in boundaries {
            let mut seen = vec![false; self.nodes.len()];
            while let Some(id) = pending.pop() {
                if self.nodes[id].materialized || std::mem::replace(&mut seen[id], true) {
                    continue;
                }
                execution_rates[id] += rate;
                pending.extend(&inputs[id]);
            }
        }
        let mut total = self.request_cost;
        let mut raw = BTreeMap::<&str, f64>::new();
        for id in &selected.selected_candidates {
            let node = &self.nodes[id.0];
            let readers: Vec<_> = reached
                .iter()
                .enumerate()
                .filter(|(_, row)| row[id.0])
                .map(|(q, _)| latency[q])
                .collect();
            let execution_rate = if node.materialized {
                union_rate(id.0)
            } else {
                execution_rates[id.0]
            };
            let lifetime = if node.materialized {
                readers.iter().copied().fold(0.0, f64::max)
            } else {
                let reader_rates = reached.iter().enumerate().filter(|(_, row)| row[id.0]);
                let denominator: f64 = reader_rates.clone().map(|(q, _)| rates[q]).sum();
                if denominator > 0.0 {
                    reader_rates
                        .map(|(q, _)| rates[q] * latency[q])
                        .sum::<f64>()
                        / denominator
                } else {
                    0.0
                }
            };
            total += node.standing_cost_per_second
                + execution_rate
                    * (node.execution_cost + node.held_memory_cost_per_second * lifetime);
            if let Some((source, cost)) = &node.raw_retention {
                let value = raw.entry(source).or_default();
                *value = value.max(*cost);
            }
        }
        total + raw.values().sum::<f64>()
    }

    fn maximum_latency(&self) -> f64 {
        self.request_latency_seconds
            + self
                .nodes
                .iter()
                .map(NodeCostProfile::query_work_seconds)
                .sum::<f64>()
    }

    pub(crate) fn coefficients(&self) -> impl Iterator<Item = f64> + '_ {
        // Constant request fees cannot influence selection and need not limit
        // the coefficient range. Lifetimes use a dimensionless [0,1] scale.
        let latency_scale = self.maximum_latency().max(f64::MIN_POSITIVE);
        let rates = self.schedules.as_ref().map_or_else(
            || vec![self.query_rate],
            |schedule| {
                schedule
                    .materialized_subset_rates
                    .iter()
                    .copied()
                    .filter(|rate| *rate > 0.0)
                    .collect()
            },
        );
        self.nodes.iter().flat_map(move |node| {
            let mut coefficients = Vec::with_capacity(rates.len() * 2 + 1);
            let node_rates = if node.ingestion {
                &[1.0][..]
            } else {
                &rates[..]
            };
            for rate in node_rates {
                coefficients.extend([
                    node.execution_cost * rate,
                    node.held_memory_cost_per_second * rate * latency_scale,
                ]);
            }
            coefficients.push(node.standing_cost_per_second);
            coefficients.push(node.raw_retention.as_ref().map_or(0.0, |(_, cost)| *cost));
            coefficients
        })
    }
}

fn gated(
    variables: &mut ProblemVariables,
    constraints: &mut Vec<Constraint>,
    value: Expression,
    enabled: Variable,
    maximum: f64,
) -> Variable {
    let product = variables.add(variable().min(0).max(maximum));
    constraints.push(constraint!(product <= value.clone()));
    constraints.push(constraint!(product <= maximum * enabled));
    constraints.push(constraint!(product >= value - maximum * (1.0 - enabled)));
    product
}

struct BoundaryReach {
    boundary: usize,
    reached: Vec<Option<Variable>>,
}

#[allow(clippy::too_many_arguments)]
fn boundary_flows(
    graph: &CandidateDependencyGraph,
    profile: &DirectCostProfile,
    variables: &mut ProblemVariables,
    constraints: &mut Vec<Constraint>,
    active: &[Variable],
    edges: &[(ProducerConsumerConnection, Variable)],
    roots: &[(QueryRootAssignment, Variable)],
    check_deadline: &impl Fn() -> Result<(), CandidateSelectionError>,
) -> Result<Vec<BoundaryReach>, CandidateSelectionError> {
    let count = graph.candidates.len();
    let queries = graph.query_roots.len();
    let mut flows = Vec::new();
    // Each root query and each selected materialized computation is one
    // independent execution boundary. Materialized nodes stop propagation.
    for boundary in 0..queries + count {
        check_deadline()?;
        if boundary >= queries && !profile.nodes[boundary - queries].materialized {
            continue;
        }
        let mut possible = vec![false; count];
        let mut pending = if boundary < queries {
            graph.query_roots[boundary].compatible_candidates.clone()
        } else {
            graph.candidates[boundary - queries]
                .producer_requirements
                .iter()
                .flat_map(|input| input.compatible_producers.iter().copied())
                .collect()
        };
        while let Some(id) = pending.pop() {
            check_deadline()?;
            if profile.nodes[id.0].materialized || std::mem::replace(&mut possible[id.0], true) {
                continue;
            }
            pending.extend(
                graph.candidates[id.0]
                    .producer_requirements
                    .iter()
                    .flat_map(|input| input.compatible_producers.iter().copied()),
            );
        }
        let used: Vec<_> = possible
            .iter()
            .map(|possible| possible.then(|| variables.add(variable().min(0).max(1))))
            .collect();
        let mut support = vec![Expression::from(0.0); count];
        if boundary < queries {
            for (root, selected) in roots.iter().filter(|(root, _)| root.query_root == boundary) {
                if let Some(reached) = used[root.candidate.0] {
                    constraints.push(constraint!(*selected <= reached));
                    support[root.candidate.0] += *selected;
                }
            }
        }
        for (edge, connected) in edges {
            check_deadline()?;
            let Some(producer) = used[edge.producer.0] else {
                continue;
            };
            if boundary >= queries && edge.consumer.0 == boundary - queries {
                constraints.push(constraint!(*connected <= producer));
                support[edge.producer.0] += *connected;
            } else if let Some(consumer) = used[edge.consumer.0] {
                let traversed = variables.add(variable().min(0).max(1));
                constraints.push(constraint!(traversed <= consumer));
                constraints.push(constraint!(traversed <= *connected));
                constraints.push(constraint!(traversed >= consumer + *connected - 1));
                constraints.push(constraint!(traversed <= producer));
                support[edge.producer.0] += traversed;
            }
        }
        for (id, reached) in used.iter().enumerate() {
            if let Some(reached) = reached {
                constraints.push(constraint!(*reached <= support[id].clone()));
                constraints.push(constraint!(*reached <= active[id]));
            }
        }
        flows.push(BoundaryReach {
            boundary,
            reached: used,
        });
    }
    Ok(flows)
}

/// Exact on integer activation/connection decisions. Reach variables are
/// implied integral by the selected acyclic graph, as in the main model.
#[allow(clippy::too_many_arguments)]
pub(super) fn encode(
    graph: &CandidateDependencyGraph,
    profile: &DirectCostProfile,
    variables: &mut ProblemVariables,
    constraints: &mut Vec<Constraint>,
    active: &[Variable],
    edges: &[(ProducerConsumerConnection, Variable)],
    roots: &[(QueryRootAssignment, Variable)],
    reach: &[Vec<Option<Variable>>],
    divisor: f64,
    check_deadline: impl Fn() -> Result<(), CandidateSelectionError>,
) -> Result<Expression, CandidateSelectionError> {
    if let Some(schedule) = &profile.schedules {
        return schedules::encode(
            graph,
            profile,
            schedule,
            variables,
            constraints,
            active,
            edges,
            roots,
            reach,
            divisor,
            check_deadline,
        );
    }
    let count = graph.candidates.len();
    let queries = graph.query_roots.len();
    let latency_scale = profile.maximum_latency().max(f64::MIN_POSITIVE);
    let maximum_latency = 1.0;
    let latency: Vec<Expression> = reach
        .iter()
        .map(|row| {
            Expression::from(profile.request_latency_seconds / latency_scale)
                + row
                    .iter()
                    .enumerate()
                    .filter_map(|(id, reached)| {
                        reached.map(|r| profile.nodes[id].query_work_seconds() / latency_scale * r)
                    })
                    .sum::<Expression>()
        })
        .collect();
    let mut boundary_count = vec![Expression::from(0.0); count];
    let mut boundary_limit = vec![0usize; count];
    for flow in boundary_flows(
        graph,
        profile,
        variables,
        constraints,
        active,
        edges,
        roots,
        &check_deadline,
    )? {
        for (id, reached) in flow.reached.into_iter().enumerate() {
            if let Some(reached) = reached {
                boundary_count[id] += reached;
                boundary_limit[id] += 1;
            }
        }
    }
    let mut objective = Expression::from(profile.request_cost / divisor);
    let mut retained: BTreeMap<&str, Vec<(usize, f64)>> = BTreeMap::new();
    for (id, node) in profile.nodes.iter().enumerate() {
        check_deadline()?;
        let executions = if node.materialized {
            Expression::from(active[id])
        } else {
            boundary_count[id].clone()
        };
        let execution_rate = if node.ingestion {
            1.0
        } else {
            profile.query_rate
        };
        objective += node.standing_cost_per_second / divisor * active[id];
        objective += node.execution_cost * execution_rate / divisor * executions.clone();
        if let Some((source, cost)) = &node.raw_retention {
            retained.entry(source).or_default().push((id, *cost));
        }
        if node.held_memory_cost_per_second == 0.0 {
            continue;
        }
        let held_latency = if node.materialized {
            let longest = variables.add(variable().min(0).max(maximum_latency));
            for (query, row) in reach.iter().enumerate() {
                if let Some(reached) = row[id] {
                    constraints.push(constraint!(
                        longest >= latency[query].clone() - maximum_latency * (1.0 - reached)
                    ));
                }
            }
            Expression::from(longest)
        } else {
            let sum: Expression = reach
                .iter()
                .enumerate()
                .filter_map(|(query, row)| {
                    row[id].map(|reached| {
                        gated(
                            variables,
                            constraints,
                            latency[query].clone(),
                            reached,
                            maximum_latency,
                        )
                    })
                })
                .sum();
            let reader_count: Expression = reach.iter().filter_map(|row| row[id]).sum();
            let sizes: Vec<_> = (0..=queries)
                .map(|_| variables.add(variable().binary()))
                .collect();
            constraints.push(constraint!(sizes.iter().copied().sum::<Expression>() == 1));
            constraints.push(constraint!(
                sizes
                    .iter()
                    .enumerate()
                    .map(|(k, v)| k as f64 * *v)
                    .sum::<Expression>()
                    == reader_count
            ));
            let mut weighted = Expression::from(0.0);
            // The boundary count is an integer in [0, roots + states]. Gate
            // its possible values, then multiply only by a bounded continuous
            // lifetime. This reproduces B/Q without dividing by a variable.
            let boundaries: Vec<_> = (0..=boundary_limit[id])
                .map(|_| variables.add(variable().binary()))
                .collect();
            constraints.push(constraint!(
                boundaries.iter().copied().sum::<Expression>() == 1
            ));
            constraints.push(constraint!(
                boundaries
                    .iter()
                    .enumerate()
                    .map(|(b, v)| b as f64 * *v)
                    .sum::<Expression>()
                    == executions
            ));
            for (k, size) in sizes.iter().enumerate().skip(1) {
                check_deadline()?;
                let selected_sum = gated(
                    variables,
                    constraints,
                    sum.clone(),
                    *size,
                    maximum_latency * queries as f64,
                );
                for (b, boundary) in boundaries.iter().enumerate().skip(1) {
                    check_deadline()?;
                    let value = gated(
                        variables,
                        constraints,
                        selected_sum.into(),
                        *boundary,
                        maximum_latency * queries as f64,
                    );
                    weighted += b as f64 / k as f64 * value;
                }
            }
            weighted
        };
        objective += node.held_memory_cost_per_second * profile.query_rate * latency_scale
            / divisor
            * held_latency;
    }
    for entries in retained.into_values() {
        let cost = variables.add(variable().min(0));
        for (id, bytes_cost) in entries {
            constraints.push(constraint!(cost >= bytes_cost / divisor * active[id]));
        }
        objective += cost;
    }
    Ok(objective)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::search::milp::model::{
        solve_direct_cost_proposal_with_metrics, CandidateSolveMetrics,
    };
    use asap_logical_asap_optimization::memo::MemoGroupId;

    fn node(input: Option<usize>) -> PhysicalCandidate {
        PhysicalCandidate {
            group: MemoGroupId(0),
            local_cost: 0.0,
            producer_requirements: input
                .into_iter()
                .map(|id| ProducerRequirement {
                    compatible_producers: vec![PhysicalCandidateId(id)].into(),
                })
                .collect(),
        }
    }
    fn root(id: usize) -> QueryRootCandidates {
        QueryRootCandidates {
            compatible_candidates: vec![PhysicalCandidateId(id)],
        }
    }
    fn solve(graph: &CandidateDependencyGraph, profile: &DirectCostProfile) -> f64 {
        solve_direct_cost_proposal_with_metrics(
            graph,
            &[],
            None,
            &mut CandidateSolveMetrics::default(),
            profile,
        )
        .unwrap()
        .unwrap()
        .model_lower_bound
        .unwrap()
    }

    // Shared build demand is one; unmaterialized demand is two. Raw data is
    // retained once at the maximum horizon, while fees remain per workload.
    #[test]
    fn complete_cost_counts_shared_demand_held_memory_retention_and_fees() {
        let graph = CandidateDependencyGraph {
            candidates: vec![node(None), node(Some(0)), node(Some(0))],
            query_roots: vec![root(1), root(2)],
            ..Default::default()
        };
        let mut profile = DirectCostProfile {
            schedules: None,
            query_rate: 2.0,
            request_latency_seconds: 0.5,
            request_cost: 7.0,
            nodes: vec![
                NodeCostProfile {
                    ingestion: false,
                    standing_cost_per_second: 0.0,
                    execution_cost: 2.0,
                    work_seconds: 1.0,
                    held_memory_cost_per_second: 3.0,
                    materialized: true,
                    raw_retention: None,
                },
                NodeCostProfile {
                    ingestion: false,
                    standing_cost_per_second: 0.0,
                    execution_cost: 1.0,
                    work_seconds: 2.0,
                    raw_retention: Some(("source".into(), 4.0)),
                    ..Default::default()
                },
                NodeCostProfile {
                    ingestion: false,
                    standing_cost_per_second: 0.0,
                    execution_cost: 1.0,
                    work_seconds: 3.0,
                    raw_retention: Some(("source".into(), 6.0)),
                    ..Default::default()
                },
            ],
        };
        assert!((solve(&graph, &profile) - 48.0).abs() < 1e-7);
        profile.nodes[0].materialized = false;
        assert!((solve(&graph, &profile) - 73.0).abs() < 1e-7);
    }

    // Two queries behind one materialized computation produce B=1,Q=2 for
    // its unmaterialized input. Charging it once per query would double memory.
    #[test]
    fn upstream_lifetime_uses_boundary_count_divided_by_query_count() {
        let graph = CandidateDependencyGraph {
            candidates: vec![node(None), node(Some(0)), node(Some(1)), node(Some(1))],
            query_roots: vec![root(2), root(3)],
            ..Default::default()
        };
        let profile = DirectCostProfile {
            schedules: None,
            query_rate: 2.0,
            request_latency_seconds: 0.5,
            request_cost: 7.0,
            nodes: vec![
                NodeCostProfile {
                    ingestion: false,
                    standing_cost_per_second: 0.0,
                    execution_cost: 2.0,
                    work_seconds: 1.0,
                    held_memory_cost_per_second: 3.0,
                    ..Default::default()
                },
                NodeCostProfile {
                    ingestion: false,
                    standing_cost_per_second: 0.0,
                    execution_cost: 1.0,
                    work_seconds: 2.0,
                    held_memory_cost_per_second: 4.0,
                    materialized: true,
                    raw_retention: None,
                },
                NodeCostProfile {
                    ingestion: false,
                    standing_cost_per_second: 0.0,
                    work_seconds: 3.0,
                    ..Default::default()
                },
                NodeCostProfile {
                    ingestion: false,
                    standing_cost_per_second: 0.0,
                    work_seconds: 4.0,
                    ..Default::default()
                },
            ],
        };
        assert!((solve(&graph, &profile) - 115.0).abs() < 1e-7);
    }
    // Independently enumerate both readers' choices. Sharing changes execution
    // counts and held lifetime; the least-node graph alone cannot price these.
    #[test]
    fn direct_selection_matches_four_assignment_cost_oracle() {
        let mut graph = CandidateDependencyGraph {
            candidates: vec![node(None), node(None), node(Some(0)), node(Some(0))],
            query_roots: vec![root(2), root(3)],
            ..Default::default()
        };
        for reader in [2, 3] {
            graph.candidates[reader].producer_requirements[0].compatible_producers =
                vec![PhysicalCandidateId(0), PhysicalCandidateId(1)].into();
        }
        for materialized_build_cost in [1.0, 10.0, 100.0] {
            let profile = DirectCostProfile {
                schedules: None,
                query_rate: 2.0,
                request_latency_seconds: 0.5,
                request_cost: 7.0,
                nodes: vec![
                    NodeCostProfile {
                        ingestion: false,
                        standing_cost_per_second: 0.0,
                        execution_cost: materialized_build_cost,
                        work_seconds: 1.0,
                        held_memory_cost_per_second: 3.0,
                        materialized: true,
                        raw_retention: None,
                    },
                    NodeCostProfile {
                        ingestion: false,
                        standing_cost_per_second: 0.0,
                        execution_cost: 2.0,
                        work_seconds: 0.5,
                        held_memory_cost_per_second: 1.0,
                        ..Default::default()
                    },
                    NodeCostProfile {
                        ingestion: false,
                        standing_cost_per_second: 0.0,
                        execution_cost: 1.0,
                        work_seconds: 2.0,
                        ..Default::default()
                    },
                    NodeCostProfile {
                        ingestion: false,
                        standing_cost_per_second: 0.0,
                        execution_cost: 1.0,
                        work_seconds: 3.0,
                        ..Default::default()
                    },
                ],
            };
            let mut minimum = f64::INFINITY;
            for assignment in [[0, 0], [0, 1], [1, 0], [1, 1]] {
                let durations = [
                    2.5 + [1.0, 0.5][assignment[0]],
                    3.5 + [1.0, 0.5][assignment[1]],
                ];
                let mut cost = 7.0 + 4.0;
                for producer in [0, 1] {
                    let times: Vec<f64> = assignment
                        .iter()
                        .enumerate()
                        .filter(|(_, supplier)| **supplier == producer)
                        .map(|(q, _)| durations[q])
                        .collect();
                    if times.is_empty() {
                        continue;
                    }
                    cost += if producer == 0 {
                        2.0 * (materialized_build_cost
                            + 3.0 * times.iter().copied().fold(0.0, f64::max))
                    } else {
                        2.0 * (2.0 * times.len() as f64 + times.iter().sum::<f64>())
                    };
                }
                minimum = minimum.min(cost);
            }
            assert!((solve(&graph, &profile) - minimum).abs() < 1e-7);
        }
    }
    // Small reader fees remain meaningful alongside a 2.2-billion-times larger
    // possible raw-buffer cost; currency normalization must not drop the fee.
    #[test]
    fn direct_currency_range_preserves_tiny_meaningful_costs() {
        let graph = CandidateDependencyGraph {
            candidates: vec![node(None), node(None)],
            query_roots: vec![QueryRootCandidates {
                compatible_candidates: vec![PhysicalCandidateId(0), PhysicalCandidateId(1)],
            }],
            ..Default::default()
        };
        let profile = DirectCostProfile {
            schedules: None,
            nodes: vec![
                NodeCostProfile {
                    ingestion: false,
                    standing_cost_per_second: 0.0,
                    execution_cost: 7.1e-13,
                    ..Default::default()
                },
                NodeCostProfile {
                    ingestion: false,
                    standing_cost_per_second: 0.0,
                    execution_cost: 7.1e-13 * 2.2e9,
                    ..Default::default()
                },
            ],
            query_rate: 1.0,
            request_latency_seconds: 0.0,
            request_cost: 1.2e-15,
        };
        let proposal = solve_direct_cost_proposal_with_metrics(
            &graph,
            &[],
            None,
            &mut CandidateSolveMetrics::default(),
            &profile,
        )
        .unwrap()
        .unwrap();
        assert_eq!(
            proposal.selected.selected_candidates,
            vec![PhysicalCandidateId(0)]
        );
        let expected = 7.1e-13 + 1.2e-15;
        assert!((proposal.direct_modeled_cost.unwrap() / expected - 1.0).abs() < 1e-9);
        assert!((proposal.model_lower_bound.unwrap() / expected - 1.0).abs() < 1e-9);
    }
    fn ingestion_profile(
        two_states: bool,
        query_rate: f64,
    ) -> (CandidateDependencyGraph, DirectCostProfile) {
        let mut candidates = vec![node(None), node(Some(0))];
        let mut nodes = vec![
            NodeCostProfile {
                ingestion: true,
                execution_cost: 2.0,
                work_seconds: 0.2,
                ..Default::default()
            },
            NodeCostProfile {
                ingestion: true,
                materialized: true,
                execution_cost: 3.0,
                standing_cost_per_second: 7.0,
                work_seconds: 0.4,
                ..Default::default()
            },
        ];
        if two_states {
            candidates.push(node(Some(0)));
            nodes.push(nodes[1].clone());
        }
        let first_reader = candidates.len();
        candidates.extend([node(Some(1)), node(Some(if two_states { 2 } else { 1 }))]);
        nodes.extend((0..2).map(|_| NodeCostProfile {
            execution_cost: 1.0,
            work_seconds: 0.5,
            held_memory_cost_per_second: 2.0,
            ..Default::default()
        }));
        (
            CandidateDependencyGraph {
                candidates,
                query_roots: vec![root(first_reader), root(first_reader + 1)],
                ..Default::default()
            },
            DirectCostProfile {
                nodes,
                schedules: None,
                query_rate,
                request_latency_seconds: 0.1,
                request_cost: 1.0,
            },
        )
    }

    // Update work and standing state are charged once regardless of query
    // frequency; ingestion work contributes no query-held evaluation latency.
    #[test]
    fn maintained_state_cost_is_independent_of_reader_frequency() {
        for rate in [1.0, 5.0] {
            let (graph, profile) = ingestion_profile(false, rate);
            let expected = 13.0 + 4.4 * rate;
            let proposal = solve_direct_cost_proposal_with_metrics(
                &graph,
                &[],
                None,
                &mut CandidateSolveMetrics::default(),
                &profile,
            )
            .unwrap()
            .unwrap();
            assert!((proposal.direct_modeled_cost.unwrap() - expected).abs() < 1e-8);
            assert!((proposal.model_lower_bound.unwrap() - expected).abs() < 1e-6);
        }
    }

    // Two maintained states independently update a shared unmaterialized scan.
    // Both equal and heterogeneous query schedules leave that ingestion rate2.
    #[test]
    fn two_ingestion_boundaries_recompute_their_shared_input() {
        let (graph, mut profile) = ingestion_profile(true, 1.0);
        assert!((solve(&graph, &profile) - 29.4).abs() < 1e-6);
        profile.schedules = Some(QueryScheduleRates {
            per_query_rates: vec![1.0, 2.0],
            materialized_subset_rates: vec![0.0, 1.0, 2.0, 3.0],
        });
        let proposal = solve_direct_cost_proposal_with_metrics(
            &graph,
            &[],
            None,
            &mut CandidateSolveMetrics::default(),
            &profile,
        )
        .unwrap()
        .unwrap();
        assert!((proposal.direct_modeled_cost.unwrap() - 31.6).abs() < 1e-8);
        assert!((proposal.model_lower_bound.unwrap() - 31.6).abs() < 1e-6);
    }

    // Enumerate shared or separate state choices without charging a shared
    // producer twice; standing memory can change which choice wins.
    #[test]
    fn ingestion_selection_matches_shared_state_cost_oracle() {
        let graph = CandidateDependencyGraph {
            candidates: vec![
                node(None),
                node(Some(0)),
                node(Some(0)),
                node(Some(0)),
                PhysicalCandidate {
                    group: MemoGroupId(1),
                    local_cost: 0.0,
                    producer_requirements: vec![ProducerRequirement {
                        compatible_producers: vec![PhysicalCandidateId(1), PhysicalCandidateId(2)]
                            .into(),
                    }],
                },
                PhysicalCandidate {
                    group: MemoGroupId(1),
                    local_cost: 0.0,
                    producer_requirements: vec![ProducerRequirement {
                        compatible_producers: vec![PhysicalCandidateId(1), PhysicalCandidateId(3)]
                            .into(),
                    }],
                },
            ],
            query_roots: vec![root(4), root(5)],
            ..Default::default()
        };
        for shared_standing in [1.0, 20.0] {
            let profile = DirectCostProfile {
                query_rate: 5.0,
                schedules: None,
                request_latency_seconds: 0.0,
                request_cost: 0.0,
                nodes: vec![
                    NodeCostProfile {
                        ingestion: true,
                        execution_cost: 2.0,
                        ..Default::default()
                    },
                    NodeCostProfile {
                        ingestion: true,
                        materialized: true,
                        execution_cost: 3.0,
                        standing_cost_per_second: shared_standing,
                        ..Default::default()
                    },
                    NodeCostProfile {
                        ingestion: true,
                        materialized: true,
                        execution_cost: 3.0,
                        standing_cost_per_second: 7.0,
                        ..Default::default()
                    },
                    NodeCostProfile {
                        ingestion: true,
                        materialized: true,
                        execution_cost: 3.0,
                        standing_cost_per_second: 7.0,
                        ..Default::default()
                    },
                    NodeCostProfile {
                        execution_cost: 1.0,
                        ..Default::default()
                    },
                    NodeCostProfile {
                        execution_cost: 1.0,
                        ..Default::default()
                    },
                ],
            };
            let expected = [[1, 1], [1, 3], [2, 1], [2, 3]]
                .into_iter()
                .map(|assignment| {
                    let states: std::collections::HashSet<_> = assignment.into_iter().collect();
                    10.0 + states
                        .iter()
                        .map(|state| 2.0 + 3.0 + if *state == 1 { shared_standing } else { 7.0 })
                        .sum::<f64>()
                })
                .fold(f64::INFINITY, f64::min);
            let proposal = solve_direct_cost_proposal_with_metrics(
                &graph,
                &[],
                None,
                &mut CandidateSolveMetrics::default(),
                &profile,
            )
            .unwrap()
            .unwrap();
            assert!((proposal.direct_modeled_cost.unwrap() - expected).abs() < 1e-8);
            assert!((proposal.model_lower_bound.unwrap() - expected).abs() < 1e-6);
        }
    }
}

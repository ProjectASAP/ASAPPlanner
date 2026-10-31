//! Exact objective encoding for certified query-time and ingestion profiles.
//! Equal schedules have a compact fast path; mixed recurrence/range demand
//! uses exact evaluator atoms. Retained state carries separate standing charges.
//! Profiles are supplied by physical costing; this module never infers prices
//! from a logical template or treats accuracy as a monetary penalty.
use crate::search::candidates::graph::{QueryRootAssignment, SelectedCandidateGraph};
use good_lp::{constraint, variable, Constraint, Expression, ProblemVariables, Variable};
use std::collections::BTreeMap;

#[path = "general_schedule.rs"]
mod general_schedule;
#[path = "general_schedule_rates.rs"]
mod general_schedule_rates;
#[cfg(test)]
#[path = "schedule_selection_tests.rs"]
mod schedule_selection_tests;
pub(crate) use general_schedule_rates::QueryScheduleRates;

use crate::search::candidates::dag::*;
use crate::search::candidates::graph::ProducerConsumerConnection;

/// Smallest/largest monetary coefficient ratio after lifetime normalization.
/// The minimum is scaled to one; no positive cost is rounded away.
pub(super) const MINIMUM_DIRECT_RELATIVE_COEFFICIENT: f64 = 1e-12;

// Held-time terms weight lifetimes by dimensionless rates. Their small entries
// need a stronger guard than monetary objective coefficients, since absolute
// LP feasibility tolerances can erase a rare reader's otherwise large cost.
const MINIMUM_QUERY_RATE_WEIGHT: f64 = 1e-6;
// Lifetime gates use unit-scale bounds. Smaller positive terms can disappear
// within LP feasibility tolerances and then be amplified by a memory price.
const MINIMUM_NORMALIZED_LATENCY_TERM: f64 = 1e-6;

#[derive(Debug, Clone, Default)]
pub(crate) struct NodeCostProfile {
    pub per_request: bool,
    pub execution_cost: f64,
    pub ingestion: bool,
    pub standing_cost_per_second: f64,
    pub work_seconds: f64,
    /// Certified fixed-worker merge delay; the query pays the maximum reached delay.
    pub compaction_seconds: f64,
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
        graph: &DagOfPhysicalCandidatesWithCosts,
    ) -> Result<(), CandidateSelectionError> {
        let valid = |v: f64| v.is_finite() && v >= 0.0;
        if self.nodes.len() != graph.candidates.len()
            || !valid(self.query_rate)
            || self.query_rate == 0.0
            || !valid(self.request_latency_seconds)
            || !valid(self.request_cost)
            || self.nodes.iter().any(|n| {
                !valid(n.execution_cost)
                    || (n.per_request && (!n.materialized || n.ingestion))
                    || !valid(n.standing_cost_per_second)
                    || (n.ingestion && n.held_memory_cost_per_second != 0.0)
                    || !valid(n.compaction_seconds)
                    || (n.compaction_seconds > 0.0 && !(n.ingestion && n.materialized))
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
                .producer_candidates_per_input
                .iter()
                .flat_map(|input| input.candidates.iter())
            {
                let source = &self.nodes[producer.0];
                // Query latency stops at the first maintained ingestion state.
                // This certified slice admits only raw upstream closures, not
                // nested maintained states whose deeper compaction is hidden.
                if (self.nodes[consumer].ingestion && (!source.ingestion || source.materialized))
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
            .flat_map(|root| &root.candidates)
            .any(|id| self.nodes[id.0].ingestion && !self.nodes[id.0].materialized)
        {
            return Err(CandidateSelectionError::InvalidGraph(
                "query output cannot directly read unmaterialized ingestion work".into(),
            ));
        }
        if let Some(schedule) = &self.schedules {
            schedule.validate(graph.query_roots.len())?;
        }
        if let Some(schedule) = &self.schedules {
            let total_rate: f64 = schedule.per_query_rates().iter().sum();
            if schedule
                .per_query_rates()
                .iter()
                .any(|rate| *rate > 0.0 && rate / total_rate < MINIMUM_QUERY_RATE_WEIGHT)
            {
                return Err(CandidateSelectionError::UnsupportedDirectCost(
                    "query-rate weights are below the supported held-time numerical range".into(),
                ));
            }
        }
        let maximum_latency = self.maximum_latency(graph);
        if self.coefficients(graph).any(|cost| !cost.is_finite())
            || !maximum_latency.is_finite()
            || !(maximum_latency * graph.query_roots.len() as f64).is_finite()
        {
            return Err(CandidateSelectionError::InvalidGraph(
                "direct-cost latency bound overflow".into(),
            ));
        }
        if self
            .nodes
            .iter()
            .any(|node| node.held_memory_cost_per_second > 0.0)
        {
            let mut compaction_levels: Vec<_> = self
                .nodes
                .iter()
                .map(|node| node.compaction_seconds)
                .filter(|delay| *delay > 0.0)
                .collect();
            compaction_levels.sort_by(f64::total_cmp);
            compaction_levels.dedup();
            let mut previous = 0.0;
            let increments = compaction_levels.into_iter().map(|delay| {
                let increment = delay - previous;
                previous = delay;
                increment
            });
            let minimum = std::iter::once(self.request_latency_seconds)
                .chain(self.nodes.iter().map(NodeCostProfile::query_work_seconds))
                .chain(increments)
                .filter(|term| *term > 0.0)
                .reduce(f64::min);
            let minimum_rate_weight = self.schedules.as_ref().map_or(1.0, |schedule| {
                let total: f64 = schedule.per_query_rates().iter().sum();
                schedule
                    .per_query_rates()
                    .iter()
                    .filter(|rate| **rate > 0.0)
                    .map(|rate| rate / total)
                    .fold(1.0, f64::min)
            });
            // Per-consumer held-time terms multiply these coefficients;
            // checking each factor separately would miss their small product.
            if minimum.is_some_and(|term| {
                term / maximum_latency * minimum_rate_weight < MINIMUM_NORMALIZED_LATENCY_TERM
            }) {
                return Err(CandidateSelectionError::UnsupportedDirectCost(
                    format!("normalized latency terms are below the supported held-memory numerical range: minimum={minimum:?}, maximum={maximum_latency:e}, rate_weight={minimum_rate_weight:e}, request={:e}", self.request_latency_seconds),
                ));
            }
        }
        Ok(())
    }

    /// Recompute the complete selected objective independently of LP auxiliaries.
    /// A time-limited incumbent may leave nonminimal epigraph variables.
    pub(crate) fn selected_cost(
        &self,
        graph: &DagOfPhysicalCandidatesWithCosts,
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
                    + row
                        .iter()
                        .enumerate()
                        .filter(|(_, yes)| **yes)
                        .map(|(id, _)| self.nodes[id].compaction_seconds)
                        .fold(0.0, f64::max)
            })
            .collect();
        let rates = self.schedules.as_ref().map_or_else(
            || vec![self.query_rate; graph.query_roots.len()],
            |schedule| schedule.per_query_rates().to_vec(),
        );
        let union_rate = |id: usize| {
            if self.nodes[id].ingestion {
                return 1.0;
            }
            let Some(schedule) = &self.schedules else {
                return self.query_rate;
            };
            if self.nodes[id].per_request {
                schedule.request_union_rate(reached.iter().map(|row| row[id]))
            } else {
                schedule.union_rate(reached.iter().map(|row| row[id]))
            }
        };
        // The longest latency of the queries reaching each node.
        let longest = |id: usize| {
            reached
                .iter()
                .zip(&latency)
                .filter(|(row, _)| row[id])
                .map(|(_, latency)| *latency)
                .fold(0.0, f64::max)
        };
        // Per unmaterialized node, its executions per second and its held
        // time per second: each consumer (a query, or a materialized node)
        // computes its own copy at its rate and holds it for its query's
        // latency, or for the materialized node's longest reader's.
        let mut execution_rates = vec![0.0; self.nodes.len()];
        let mut held_seconds = vec![0.0; self.nodes.len()];
        let boundaries = selected
            .query_roots
            .iter()
            .map(|root| {
                let query = root.query_root;
                (vec![root.candidate.0], rates[query], latency[query])
            })
            .chain(
                selected
                    .selected_candidates
                    .iter()
                    .filter(|id| self.nodes[id.0].materialized)
                    .map(|id| (inputs[id.0].clone(), union_rate(id.0), longest(id.0))),
            );
        for (mut pending, rate, lifetime) in boundaries {
            let mut seen = vec![false; self.nodes.len()];
            while let Some(id) = pending.pop() {
                if self.nodes[id].materialized || std::mem::replace(&mut seen[id], true) {
                    continue;
                }
                execution_rates[id] += rate;
                held_seconds[id] += rate * lifetime;
                pending.extend(&inputs[id]);
            }
        }
        let mut total = self.request_cost;
        let mut raw = BTreeMap::<&str, f64>::new();
        for id in &selected.selected_candidates {
            let node = &self.nodes[id.0];
            let (execution_rate, held_seconds) = if node.materialized {
                let rate = union_rate(id.0);
                (rate, rate * longest(id.0))
            } else {
                (execution_rates[id.0], held_seconds[id.0])
            };
            total += node.standing_cost_per_second
                + execution_rate * node.execution_cost
                + node.held_memory_cost_per_second * held_seconds;
            if let Some((source, cost)) = &node.raw_retention {
                let value = raw.entry(source).or_default();
                *value = value.max(*cost);
            }
        }
        total + raw.values().sum::<f64>()
    }

    /// Sum all required input roles, but take the maximum compatible producer
    /// for each role. This bounds every selected DAG (shared work may be counted
    /// twice) without summing mutually exclusive catalog choices. Potential
    /// cycles conservatively use the old all-node sum, never a cyclic DP value.
    fn maximum_latency(&self, graph: &DagOfPhysicalCandidatesWithCosts) -> f64 {
        let count = self.nodes.len();
        let mut pending = vec![0usize; count];
        let mut parents = vec![Vec::new(); count];
        for (id, candidate) in graph.candidates.iter().enumerate() {
            if self.nodes[id].ingestion {
                continue;
            }
            for supplier in candidate
                .producer_candidates_per_input
                .iter()
                .flat_map(|input| input.candidates.iter())
            {
                pending[id] += 1;
                parents[supplier.0].push(id);
            }
        }
        let mut ready: Vec<_> = (0..count).filter(|id| pending[*id] == 0).collect();
        let mut bounds = vec![0.0_f64; count];
        let mut processed = 0;
        while let Some(id) = ready.pop() {
            processed += 1;
            if !self.nodes[id].ingestion {
                bounds[id] = self.nodes[id].query_work_seconds()
                    + graph.candidates[id]
                        .producer_candidates_per_input
                        .iter()
                        .map(|input| {
                            input
                                .candidates
                                .iter()
                                .map(|supplier| bounds[supplier.0])
                                .fold(0.0, f64::max)
                        })
                        .sum::<f64>();
            }
            for parent in &parents[id] {
                pending[*parent] -= 1;
                if pending[*parent] == 0 {
                    ready.push(*parent);
                }
            }
        }
        let work = if processed == count {
            graph
                .query_roots
                .iter()
                .flat_map(|root| &root.candidates)
                .map(|id| bounds[id.0])
                .fold(0.0, f64::max)
        } else {
            self.nodes
                .iter()
                .map(NodeCostProfile::query_work_seconds)
                .sum()
        };
        self.request_latency_seconds
            + work
            + self
                .nodes
                .iter()
                .map(|node| node.compaction_seconds)
                .fold(0.0, f64::max)
    }

    pub(crate) fn coefficients<'a>(
        &'a self,
        graph: &DagOfPhysicalCandidatesWithCosts,
    ) -> impl Iterator<Item = f64> + 'a {
        // Constant request fees cannot influence selection and need not limit
        // the coefficient range. Lifetimes use a dimensionless [0,1] scale.
        let latency_scale = self.maximum_latency(graph).max(f64::MIN_POSITIVE);
        let rates = self.schedules.as_ref().map_or_else(
            || vec![self.query_rate],
            |schedule| schedule.coefficient_rates(),
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
    graph: &DagOfPhysicalCandidatesWithCosts,
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
            graph.query_roots[boundary].candidates.clone()
        } else {
            graph.candidates[boundary - queries]
                .producer_candidates_per_input
                .iter()
                .flat_map(|input| input.candidates.iter().copied())
                .collect()
        };
        while let Some(id) = pending.pop() {
            check_deadline()?;
            if profile.nodes[id.0].materialized || std::mem::replace(&mut possible[id.0], true) {
                continue;
            }
            pending.extend(
                graph.candidates[id.0]
                    .producer_candidates_per_input
                    .iter()
                    .flat_map(|input| input.candidates.iter().copied()),
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

/// Encode max compaction exactly, including when no memory cost prices latency.
/// Descending threshold ORs are implied integral by binary query reach. Their
/// telescoping weighted sum equals the largest reached delay, with O(N) rows.
fn query_latency_expressions(
    graph: &DagOfPhysicalCandidatesWithCosts,
    profile: &DirectCostProfile,
    variables: &mut ProblemVariables,
    constraints: &mut Vec<Constraint>,
    reach: &[Vec<Option<Variable>>],
    check_deadline: &impl Fn() -> Result<(), CandidateSelectionError>,
) -> Result<Vec<Expression>, CandidateSelectionError> {
    let scale = profile.maximum_latency(graph).max(f64::MIN_POSITIVE);
    let mut result = Vec::with_capacity(reach.len());
    for row in reach {
        check_deadline()?;
        let mut latency = Expression::from(profile.request_latency_seconds / scale);
        let mut delays = Vec::new();
        for (id, reached) in row.iter().enumerate() {
            if let Some(reached) = reached {
                latency += profile.nodes[id].query_work_seconds() / scale * *reached;
                let delay = profile.nodes[id].compaction_seconds;
                if delay > 0.0 {
                    delays.push((delay, *reached));
                }
            }
        }
        delays.sort_by(|left, right| right.0.total_cmp(&left.0));
        let mut previous = None;
        let mut index = 0;
        while index < delays.len() {
            check_deadline()?;
            let delay = delays[index].0;
            let threshold_reached = variables.add(variable().min(0.0).max(1.0));
            let mut support = Expression::from(0.0);
            if let Some(previous) = previous {
                constraints.push(constraint!(threshold_reached >= previous));
                support += previous;
            }
            while index < delays.len() && delays[index].0 == delay {
                let reached = delays[index].1;
                constraints.push(constraint!(threshold_reached >= reached));
                support += reached;
                index += 1;
            }
            constraints.push(constraint!(threshold_reached <= support));
            let next_delay = delays.get(index).map_or(0.0, |entry| entry.0);
            latency += (delay - next_delay) / scale * threshold_reached;
            previous = Some(threshold_reached);
        }
        result.push(latency);
    }
    Ok(result)
}

/// Exact on integer activation/connection decisions. Reach variables are
/// implied integral by the selected acyclic graph, as in the main model.
#[allow(clippy::too_many_arguments)]
pub(super) fn encode(
    graph: &DagOfPhysicalCandidatesWithCosts,
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
        return general_schedule::encode(
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
    let latency_scale = profile.maximum_latency(graph).max(f64::MIN_POSITIVE);
    let maximum_latency = 1.0;
    let latency = query_latency_expressions(
        graph,
        profile,
        variables,
        constraints,
        reach,
        &check_deadline,
    )?;
    let flows = boundary_flows(
        graph,
        profile,
        variables,
        constraints,
        active,
        edges,
        roots,
        &check_deadline,
    )?;
    let mut boundary_count = vec![Expression::from(0.0); count];
    for flow in &flows {
        for (id, reached) in flow.reached.iter().enumerate() {
            if let Some(reached) = reached {
                boundary_count[id] += *reached;
            }
        }
    }
    // Each materialized node's longest reader's latency, built on first use.
    let mut longest_of: BTreeMap<usize, Variable> = BTreeMap::new();
    let mut longest =
        |id: usize, variables: &mut ProblemVariables, constraints: &mut Vec<Constraint>| {
            *longest_of.entry(id).or_insert_with(|| {
                let longest = variables.add(variable().min(0).max(maximum_latency));
                for (query, row) in reach.iter().enumerate() {
                    if let Some(reached) = row[id] {
                        constraints.push(constraint!(
                            longest >= latency[query].clone() - maximum_latency * (1.0 - reached)
                        ));
                    }
                }
                longest
            })
        };
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
        // Every consumer runs at the one query rate: a materialized node
        // holds its result for its longest reader's latency; an
        // unmaterialized node is computed once per consumer (a query, or a
        // materialized node), each copy held for that query's latency or
        // that node's longest reader's.
        let held_latency = if node.materialized {
            Expression::from(longest(id, variables, constraints))
        } else {
            let mut held = Expression::from(0.0);
            for flow in &flows {
                check_deadline()?;
                let Some(reached) = flow.reached[id] else {
                    continue;
                };
                let lifetime = if flow.boundary < queries {
                    latency[flow.boundary].clone()
                } else {
                    longest(flow.boundary - queries, variables, constraints).into()
                };
                held += gated(variables, constraints, lifetime, reached, maximum_latency);
            }
            held
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
    use asap_types::stages::*;

    fn node(input: Option<usize>) -> PhysicalCandidateWithCost {
        PhysicalCandidateWithCost {
            local_cost: 0.0,
            producer_candidates_per_input: input
                .into_iter()
                .map(|id| InputProducerCandidates {
                    candidates: vec![PhysicalCandidateId(id)].into(),
                })
                .collect(),
        }
    }
    fn root(id: usize) -> QueryOutputCandidates {
        QueryOutputCandidates {
            candidates: vec![PhysicalCandidateId(id)],
        }
    }
    fn solve(graph: &DagOfPhysicalCandidatesWithCosts, profile: &DirectCostProfile) -> f64 {
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
        let graph = DagOfPhysicalCandidatesWithCosts {
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
                    per_request: false,
                    ingestion: false,
                    standing_cost_per_second: 0.0,
                    execution_cost: 2.0,
                    work_seconds: 1.0,
                    compaction_seconds: 0.0,
                    held_memory_cost_per_second: 3.0,
                    materialized: true,
                    raw_retention: None,
                },
                NodeCostProfile {
                    ingestion: false,
                    standing_cost_per_second: 0.0,
                    execution_cost: 1.0,
                    work_seconds: 2.0,
                    compaction_seconds: 0.0,
                    raw_retention: Some(("source".into(), 4.0)),
                    ..Default::default()
                },
                NodeCostProfile {
                    ingestion: false,
                    standing_cost_per_second: 0.0,
                    execution_cost: 1.0,
                    work_seconds: 3.0,
                    compaction_seconds: 0.0,
                    raw_retention: Some(("source".into(), 6.0)),
                    ..Default::default()
                },
            ],
        };
        assert!((solve(&graph, &profile) - 48.0).abs() < 1e-7);
        profile.nodes[0].materialized = false;
        assert!((solve(&graph, &profile) - 73.0).abs() < 1e-7);
    }

    // Two queries behind one materialized computation: its unmaterialized
    // input is computed once, for that computation, and held, as its result
    // is, for the longer reader's latency (7.5 s, not the 7 s mean).
    // Charging it once per query would double memory.
    #[test]
    fn upstream_lifetime_is_the_materialized_consumers_longest_reader() {
        let graph = DagOfPhysicalCandidatesWithCosts {
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
                    compaction_seconds: 0.0,
                    held_memory_cost_per_second: 3.0,
                    ..Default::default()
                },
                NodeCostProfile {
                    per_request: false,
                    ingestion: false,
                    standing_cost_per_second: 0.0,
                    execution_cost: 1.0,
                    work_seconds: 2.0,
                    compaction_seconds: 0.0,
                    held_memory_cost_per_second: 4.0,
                    materialized: true,
                    raw_retention: None,
                },
                NodeCostProfile {
                    ingestion: false,
                    standing_cost_per_second: 0.0,
                    work_seconds: 3.0,
                    compaction_seconds: 0.0,
                    ..Default::default()
                },
                NodeCostProfile {
                    ingestion: false,
                    standing_cost_per_second: 0.0,
                    work_seconds: 4.0,
                    compaction_seconds: 0.0,
                    ..Default::default()
                },
            ],
        };
        assert!((solve(&graph, &profile) - 118.0).abs() < 1e-7);
    }
    // Independently enumerate both readers' choices. Sharing changes execution
    // counts and held lifetime; the least-node graph alone cannot price these.
    #[test]
    fn direct_selection_matches_four_assignment_cost_oracle() {
        let mut graph = DagOfPhysicalCandidatesWithCosts {
            candidates: vec![node(None), node(None), node(Some(0)), node(Some(0))],
            query_roots: vec![root(2), root(3)],
            ..Default::default()
        };
        for reader in [2, 3] {
            graph.candidates[reader].producer_candidates_per_input[0].candidates =
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
                        per_request: false,
                        ingestion: false,
                        standing_cost_per_second: 0.0,
                        execution_cost: materialized_build_cost,
                        work_seconds: 1.0,
                        compaction_seconds: 0.0,
                        held_memory_cost_per_second: 3.0,
                        materialized: true,
                        raw_retention: None,
                    },
                    NodeCostProfile {
                        ingestion: false,
                        standing_cost_per_second: 0.0,
                        execution_cost: 2.0,
                        work_seconds: 0.5,
                        compaction_seconds: 0.0,
                        held_memory_cost_per_second: 1.0,
                        ..Default::default()
                    },
                    NodeCostProfile {
                        ingestion: false,
                        standing_cost_per_second: 0.0,
                        execution_cost: 1.0,
                        work_seconds: 2.0,
                        compaction_seconds: 0.0,
                        ..Default::default()
                    },
                    NodeCostProfile {
                        ingestion: false,
                        standing_cost_per_second: 0.0,
                        execution_cost: 1.0,
                        work_seconds: 3.0,
                        compaction_seconds: 0.0,
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
        let graph = DagOfPhysicalCandidatesWithCosts {
            candidates: vec![node(None), node(None)],
            query_roots: vec![QueryOutputCandidates {
                candidates: vec![PhysicalCandidateId(0), PhysicalCandidateId(1)],
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
    ) -> (DagOfPhysicalCandidatesWithCosts, DirectCostProfile) {
        let mut candidates = vec![node(None), node(Some(0))];
        let mut nodes = vec![
            NodeCostProfile {
                ingestion: true,
                execution_cost: 2.0,
                work_seconds: 0.2,
                compaction_seconds: 0.0,
                ..Default::default()
            },
            NodeCostProfile {
                ingestion: true,
                materialized: true,
                execution_cost: 3.0,
                standing_cost_per_second: 7.0,
                work_seconds: 0.4,
                compaction_seconds: 0.0,
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
            compaction_seconds: 0.0,
            held_memory_cost_per_second: 2.0,
            ..Default::default()
        }));
        (
            DagOfPhysicalCandidatesWithCosts {
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
        profile.schedules = Some(schedule_selection_tests::rate_schedule(&[1.0, 2.0]));
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
        let graph = DagOfPhysicalCandidatesWithCosts {
            candidates: vec![
                node(None),
                node(Some(0)),
                node(Some(0)),
                node(Some(0)),
                PhysicalCandidateWithCost {
                    local_cost: 0.0,
                    producer_candidates_per_input: vec![InputProducerCandidates {
                        candidates: vec![PhysicalCandidateId(1), PhysicalCandidateId(2)].into(),
                    }],
                },
                PhysicalCandidateWithCost {
                    local_cost: 0.0,
                    producer_candidates_per_input: vec![InputProducerCandidates {
                        candidates: vec![PhysicalCandidateId(1), PhysicalCandidateId(3)].into(),
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
    // Query-kept state refreshes once at its readers' union rate. Residency is
    // charged once, while its unmaterialized input, computed once per
    // refresh, is held for the longest reader's latency, as the state is.
    // This differs from charging the kept state per reader.
    #[test]
    fn retained_query_state_prices_residency_refresh_and_input_lifetime() {
        let graph = DagOfPhysicalCandidatesWithCosts {
            candidates: vec![node(None), node(Some(0)), node(Some(1)), node(Some(1))],
            query_roots: vec![root(2), root(3)],
            ..Default::default()
        };
        let mut profile = DirectCostProfile {
            query_rate: 2.0,
            schedules: None,
            request_latency_seconds: 0.1,
            request_cost: 0.0,
            nodes: vec![
                NodeCostProfile {
                    execution_cost: 2.0,
                    work_seconds: 1.0,
                    compaction_seconds: 0.0,
                    held_memory_cost_per_second: 3.0,
                    ..Default::default()
                },
                NodeCostProfile {
                    execution_cost: 1.0,
                    work_seconds: 2.0,
                    compaction_seconds: 0.0,
                    materialized: true,
                    standing_cost_per_second: 7.0,
                    ..Default::default()
                },
                NodeCostProfile {
                    execution_cost: 1.0,
                    work_seconds: 0.5,
                    compaction_seconds: 0.0,
                    held_memory_cost_per_second: 2.0,
                    ..Default::default()
                },
                NodeCostProfile {
                    execution_cost: 1.0,
                    work_seconds: 1.5,
                    compaction_seconds: 0.0,
                    held_memory_cost_per_second: 2.0,
                    ..Default::default()
                },
            ],
        };
        // Latencies are 3.6 and 4.6 seconds. Equal-rate readers share one
        // refresh; distinct fixed intervals add under the evaluator's policy.
        for (rates, union, expected) in [(vec![2.0, 2.0], 2.0, 77.4), (vec![1.0, 2.0], 3.0, 86.0)] {
            if rates[0] != rates[1] {
                profile.schedules = Some(schedule_selection_tests::rate_schedule(&rates));
            }
            let weighted_latency = rates[0] * 3.6 + rates[1] * 4.6;
            let rate_sum: f64 = rates.iter().sum();
            let independent = 7.0 + union * (3.0 + 3.0 * 4.6) + rate_sum + 2.0 * weighted_latency;
            assert!((independent - expected).abs() < 1e-10);
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
            assert!((proposal.model_lower_bound.unwrap() - expected).abs() < 1e-7 * expected);
        }
    }
    // Max is fixed by reach even without held-memory pricing: minimizing or
    // maximizing the latency expression gives the same value for every mask.
    #[test]
    fn compaction_max_is_exact_without_a_held_memory_objective() {
        let graph = DagOfPhysicalCandidatesWithCosts {
            candidates: vec![node(None); 3],
            query_roots: vec![root(0), root(1), root(2)],
            ..Default::default()
        };
        use good_lp::{highs, Solution, SolverModel};
        let profile = DirectCostProfile {
            nodes: [2.0, 5.0, 5.0]
                .into_iter()
                .map(|delay| NodeCostProfile {
                    ingestion: true,
                    materialized: true,
                    compaction_seconds: delay,
                    ..Default::default()
                })
                .collect(),
            query_rate: 1.0,
            schedules: None,
            request_latency_seconds: 1.0,
            request_cost: 0.0,
        };
        for mask in 0..8 {
            let expected = 1.0
                + [2.0_f64, 5.0, 5.0]
                    .iter()
                    .enumerate()
                    .filter(|(id, _)| mask & (1 << id) != 0)
                    .map(|(_, delay)| *delay)
                    .fold(0.0, f64::max);
            for sign in [-1.0, 1.0] {
                let mut variables = ProblemVariables::new();
                let mut constraints = Vec::new();
                let row = (0..3)
                    .map(|id| {
                        let reached = variables.add(variable().binary());
                        constraints.push(constraint!(reached == f64::from(mask & (1 << id) != 0)));
                        Some(reached)
                    })
                    .collect();
                let latency = query_latency_expressions(
                    &graph,
                    &profile,
                    &mut variables,
                    &mut constraints,
                    &[row],
                    &|| Ok(()),
                )
                .unwrap()
                .remove(0);
                let solution = variables
                    .minimise(sign * latency.clone())
                    .using(highs)
                    .with_all(constraints)
                    .solve()
                    .unwrap();
                assert!(
                    (solution.eval(&latency) * profile.maximum_latency(&graph) - expected).abs()
                        < 1e-9
                );
            }
        }
    }

    // Exhaustive reader bindings distinguish max compaction from summing
    // delays, and preserve once-only standing/update costs for shared states.
    #[test]
    fn compaction_selection_matches_exhaustive_reader_bindings() {
        let mut reader = node(Some(0));
        reader.producer_candidates_per_input[0].candidates =
            vec![PhysicalCandidateId(0), PhysicalCandidateId(1)].into();
        reader
            .producer_candidates_per_input
            .push(node(Some(2)).producer_candidates_per_input.remove(0));
        let graph = DagOfPhysicalCandidatesWithCosts {
            candidates: vec![node(None), node(None), node(None), reader.clone(), reader],
            query_roots: vec![root(3), root(4)],
            ..Default::default()
        };
        let mut profile = DirectCostProfile {
            nodes: [(2.0, 4.0), (5.0, 1.0), (3.0, 2.0)]
                .into_iter()
                .map(|(delay, cost)| NodeCostProfile {
                    ingestion: true,
                    materialized: true,
                    execution_cost: cost,
                    compaction_seconds: delay,
                    ..Default::default()
                })
                .chain((0..2).map(|_| NodeCostProfile {
                    work_seconds: 1.0,
                    held_memory_cost_per_second: 2.0,
                    ..Default::default()
                }))
                .collect(),
            query_rate: 1.0,
            schedules: None,
            request_latency_seconds: 0.0,
            request_cost: 0.0,
        };
        for rates in [[1.0, 1.0], [1.0, 2.0]] {
            if rates[0] != rates[1] {
                profile.schedules = Some(schedule_selection_tests::rate_schedule(&rates));
            }
            let expected = (0..4)
                .map(|mask| {
                    let chosen = [mask & 1, (mask >> 1) & 1];
                    let update = 2.0
                        + if chosen.contains(&0) { 4.0 } else { 0.0 }
                        + if chosen.contains(&1) { 1.0 } else { 0.0 };
                    update
                        + (0..2)
                            .map(|q| {
                                rates[q] * 2.0 * (1.0 + if chosen[q] == 0 { 3.0 } else { 5.0 })
                            })
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
            assert!((proposal.model_lower_bound.unwrap() - expected).abs() < 1e-7);
        }
    }

    // Hidden compaction below an already maintained state is not query-visible;
    // reject that unsupported profile instead of charging all ancestor maxima.
    #[test]
    fn nested_maintained_ingestion_states_are_not_admitted() {
        let graph = DagOfPhysicalCandidatesWithCosts {
            candidates: vec![node(None), node(Some(0))],
            query_roots: vec![root(1)],
            ..Default::default()
        };
        let profile = DirectCostProfile {
            nodes: (0..2)
                .map(|_| NodeCostProfile {
                    ingestion: true,
                    materialized: true,
                    compaction_seconds: 1.0,
                    ..Default::default()
                })
                .collect(),
            query_rate: 1.0,
            schedules: None,
            request_latency_seconds: 0.0,
            request_cost: 0.0,
        };
        assert!(matches!(
            profile.validate(&graph),
            Err(CandidateSelectionError::InvalidGraph(_))
        ));
    }
    // An inactive huge-delay candidate must not erase a selected tiny delay:
    // its held-memory charge remains significant in actual currency units.
    #[test]
    fn extreme_unselected_latency_does_not_erase_selected_memory_cost() {
        let mut graph = DagOfPhysicalCandidatesWithCosts {
            candidates: vec![node(None), node(None), node(Some(0))],
            query_roots: vec![root(2)],
            ..Default::default()
        };
        graph.candidates[2].producer_candidates_per_input[0].candidates =
            vec![PhysicalCandidateId(0), PhysicalCandidateId(1)].into();
        for tiny_compaction in [false, true] {
            let profile = DirectCostProfile {
                nodes: vec![
                    NodeCostProfile {
                        ingestion: true,
                        materialized: true,
                        compaction_seconds: if tiny_compaction { 1e-6 } else { 0.0 },
                        ..Default::default()
                    },
                    NodeCostProfile {
                        ingestion: true,
                        materialized: true,
                        compaction_seconds: 1e9,
                        ..Default::default()
                    },
                    NodeCostProfile {
                        work_seconds: if tiny_compaction { 0.0 } else { 1e-6 },
                        held_memory_cost_per_second: 1e6,
                        ..Default::default()
                    },
                ],
                query_rate: 1.0,
                schedules: None,
                request_latency_seconds: 0.0,
                request_cost: 0.0,
            };
            assert!(matches!(
                solve_direct_cost_proposal_with_metrics(
                    &graph,
                    &[],
                    None,
                    &mut CandidateSolveMetrics::default(),
                    &profile
                ),
                Err(CandidateSelectionError::UnsupportedDirectCost(_))
            ));
            // Ordinary microsecond-scale candidates remain admitted and priced;
            // only the extreme dynamic range requires evaluator-based fallback.
            let mut supported = profile.clone();
            supported.nodes[1].compaction_seconds = 2e-6;
            let proposal = solve_direct_cost_proposal_with_metrics(
                &graph,
                &[],
                None,
                &mut CandidateSolveMetrics::default(),
                &supported,
            )
            .unwrap()
            .unwrap();
            assert!((proposal.direct_modeled_cost.unwrap() - 1.0).abs() < 1e-8);
            assert!((proposal.model_lower_bound.unwrap() - 1.0).abs() < 1e-8);
        }
    }
    // Producer choices use max, required positions use sum, and potential
    // cycles conservatively fall back instead of inventing a smaller bound.
    #[test]
    fn latency_bound_respects_choices_required_inputs_and_potential_cycles() {
        let mut graph = DagOfPhysicalCandidatesWithCosts {
            candidates: vec![node(None), node(None), node(Some(0))],
            query_roots: vec![root(2)],
            ..Default::default()
        };
        graph.candidates[2].producer_candidates_per_input[0].candidates =
            vec![PhysicalCandidateId(0), PhysicalCandidateId(1)].into();
        let profile = DirectCostProfile {
            nodes: [1.0, 3.0, 2.0]
                .into_iter()
                .map(|work_seconds| NodeCostProfile {
                    work_seconds,
                    ..Default::default()
                })
                .collect(),
            query_rate: 1.0,
            schedules: None,
            request_latency_seconds: 0.0,
            request_cost: 0.0,
        };
        assert_eq!(profile.maximum_latency(&graph), 5.0);
        graph.candidates[2]
            .producer_candidates_per_input
            .push(node(Some(0)).producer_candidates_per_input.remove(0));
        assert_eq!(profile.maximum_latency(&graph), 6.0);
        graph.candidates[0].producer_candidates_per_input =
            node(Some(2)).producer_candidates_per_input;
        assert_eq!(profile.maximum_latency(&graph), 6.0);
    }
}

//! Exact mixed-schedule demand and per-consumer lifetimes without subset tables.
use super::*;

fn both(
    variables: &mut ProblemVariables,
    constraints: &mut Vec<Constraint>,
    left: Variable,
    right: Variable,
) -> Variable {
    let value = variables.add(variable().min(0).max(1));
    constraints.push(constraint!(value <= left));
    constraints.push(constraint!(value <= right));
    constraints.push(constraint!(value >= left + right - 1));
    value
}

/// Whose lifetime a demand's held memory lasts: query `.0`'s latency, or
/// node `.0`'s longest reader's latency.
#[derive(Debug, Clone, Copy)]
enum Holder {
    Query(usize),
    Node(usize),
}

#[allow(clippy::too_many_arguments)]
pub(super) fn encode(
    graph: &DagOfPhysicalCandidatesWithCosts,
    profile: &DirectCostProfile,
    schedule: &QueryScheduleRates,
    variables: &mut ProblemVariables,
    constraints: &mut Vec<Constraint>,
    active: &[Variable],
    edges: &[(ProducerConsumerConnection, Variable)],
    roots: &[(QueryRootAssignment, Variable)],
    reach: &[Vec<Option<Variable>>],
    divisor: f64,
    check_deadline: impl Fn() -> Result<(), CandidateSelectionError>,
) -> Result<Expression, CandidateSelectionError> {
    let count = graph.candidates.len();
    let queries = graph.query_roots.len();
    let latency_scale = profile.maximum_latency(graph).max(f64::MIN_POSITIVE);
    let latency = query_latency_expressions(
        graph,
        profile,
        variables,
        constraints,
        reach,
        &check_deadline,
    )?;
    let mut materialized = vec![Vec::new(); count];
    for (id, node) in profile.nodes.iter().enumerate() {
        check_deadline()?;
        if node.materialized {
            materialized[id] = if node.ingestion {
                vec![(active[id], 1.0)]
            } else {
                schedule.encode_terms(
                    id,
                    reach,
                    variables,
                    constraints,
                    &check_deadline,
                    node.per_request,
                )?
            };
        }
    }
    // Per node, its demands `(used, rate, holder)`: a materialized node's own
    // executions, and an unmaterialized node's per consumer, each held for
    // its holder's lifetime; the consumers maintained at ingestion of one
    // refresh window share one ingestion run, one demand used when any of
    // them uses the node.
    let mut demands: Vec<Vec<(Variable, f64, Holder)>> = materialized
        .iter()
        .enumerate()
        .map(|(id, terms)| {
            terms
                .iter()
                .map(|&(used, rate)| (used, rate, Holder::Node(id)))
                .collect()
        })
        .collect();
    // Per (node, refresh window), the uses of the node by that window's
    // consumers, and one of them, whose lifetime holds the run's result.
    let mut runs: BTreeMap<(usize, RefreshWindow), (Vec<Variable>, usize)> = BTreeMap::new();
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
            check_deadline()?;
            let Some(reached) = reached else { continue };
            if flow.boundary < queries {
                if schedule.per_query_rates()[flow.boundary] > 0.0 {
                    demands[id].push((
                        reached,
                        schedule.per_query_rates()[flow.boundary],
                        Holder::Query(flow.boundary),
                    ));
                }
            } else {
                let consumer = flow.boundary - queries;
                for (used, rate) in &materialized[consumer] {
                    let use_of_node = both(variables, constraints, reached, *used);
                    match profile.nodes[consumer].refresh_window {
                        Some(window) => runs
                            .entry((id, window))
                            .or_insert_with(|| (Vec::new(), consumer))
                            .0
                            .push(use_of_node),
                        None => demands[id].push((use_of_node, *rate, Holder::Node(consumer))),
                    }
                }
            }
        }
    }
    // One ingestion run per (node, refresh window), at the ingestion rate:
    // used when any consumer of the window uses the node (minimization keeps
    // it at the largest use).
    for ((id, _), (uses, consumer)) in runs {
        let run = variables.add(variable().min(0).max(1));
        for use_of_node in uses {
            constraints.push(constraint!(run >= use_of_node));
        }
        demands[id].push((run, 1.0, Holder::Node(consumer)));
    }
    let mut objective = Expression::from(profile.request_cost / divisor);
    let mut retained = BTreeMap::<&str, Vec<(usize, f64)>>::new();
    // Each materialized node's longest reader's latency, built on first use.
    let mut longest_of: BTreeMap<usize, Variable> = BTreeMap::new();
    for (id, node) in profile.nodes.iter().enumerate() {
        check_deadline()?;
        objective += node.standing_cost_per_second / divisor * active[id];
        for (used, rate, _) in &demands[id] {
            objective += node.execution_cost * *rate / divisor * *used;
        }
        if let Some((source, cost)) = &node.raw_retention {
            retained.entry(source).or_default().push((id, *cost));
        }
        if node.held_memory_cost_per_second == 0.0 || demands[id].is_empty() {
            continue;
        }
        // Held for each demand's holder's lifetime: a query's latency, or a
        // materialized node's longest reader's.
        for &(used, rate, holder) in &demands[id] {
            let lifetime = match holder {
                Holder::Query(query) => latency[query].clone(),
                Holder::Node(holder) => {
                    let longest = *longest_of.entry(holder).or_insert_with(|| {
                        let longest = variables.add(variable().min(0).max(1));
                        for (q, row) in reach.iter().enumerate() {
                            if let Some(reached) = row[holder] {
                                constraints.push(constraint!(
                                    longest >= latency[q].clone() - (1.0 - reached)
                                ));
                            }
                        }
                        longest
                    });
                    Expression::from(longest)
                }
            };
            objective += node.held_memory_cost_per_second * latency_scale * rate / divisor
                * gated(variables, constraints, lifetime, used, 1.0);
        }
    }
    for costs in retained.values() {
        check_deadline()?;
        let maximum = costs
            .iter()
            .map(|(_, cost)| *cost / divisor)
            .fold(0.0, f64::max);
        let cost = variables.add(variable().min(0).max(maximum));
        for (id, charge) in costs {
            constraints.push(constraint!(cost >= *charge / divisor * active[*id]));
        }
        objective += cost;
    }
    Ok(objective)
}

#[cfg(test)]
#[path = "general_schedule_tests.rs"]
mod tests;

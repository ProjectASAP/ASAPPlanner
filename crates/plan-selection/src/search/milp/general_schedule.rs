//! Exact mixed-schedule demand and weighted lifetimes without subset tables.
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

#[allow(clippy::too_many_arguments)]
pub(super) fn encode(
    graph: &CandidateDependencyGraph,
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
    let mut demands = materialized.clone();
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
                    demands[id].push((reached, schedule.per_query_rates()[flow.boundary]));
                }
            } else {
                for (used, rate) in &materialized[flow.boundary - queries] {
                    demands[id].push((both(variables, constraints, reached, *used), *rate));
                }
            }
        }
    }
    let mut objective = Expression::from(profile.request_cost / divisor);
    let mut retained = BTreeMap::<&str, Vec<(usize, f64)>>::new();
    for (id, node) in profile.nodes.iter().enumerate() {
        check_deadline()?;
        objective += node.standing_cost_per_second / divisor * active[id];
        for (used, rate) in &demands[id] {
            objective += node.execution_cost * *rate / divisor * *used;
        }
        if let Some((source, cost)) = &node.raw_retention {
            retained.entry(source).or_default().push((id, *cost));
        }
        if node.held_memory_cost_per_second == 0.0 || demands[id].is_empty() {
            continue;
        }
        let lifetime = variables.add(variable().min(0).max(1));
        if node.materialized {
            for (q, row) in reach.iter().enumerate() {
                if let Some(reached) = row[id] {
                    constraints.push(constraint!(
                        lifetime >= latency[q].clone() - (1.0 - reached)
                    ));
                }
            }
        } else {
            // D*A=N is linear after gating A and each query latency by binary
            // reach: sum(w_q * reached_q * A) = sum(w_q * reached_q * L_q).
            // No reciprocal, LCM, or enumeration of selected reader subsets.
            let total: f64 = schedule.per_query_rates().iter().sum();
            let mut left = Expression::from(0.0);
            let mut right = Expression::from(0.0);
            if total > 0.0 {
                for (q, row) in reach.iter().enumerate() {
                    check_deadline()?;
                    if let Some(reached) = row[id] {
                        let weight = schedule.per_query_rates()[q] / total;
                        if weight == 0.0 {
                            continue;
                        }
                        left +=
                            weight * gated(variables, constraints, lifetime.into(), reached, 1.0);
                        right += weight
                            * gated(variables, constraints, latency[q].clone(), reached, 1.0);
                    }
                }
            }
            constraints.push(constraint!(left == right));
            if schedule.per_query_rates().contains(&0.0) {
                let positive: Vec<_> = reach
                    .iter()
                    .enumerate()
                    .filter(|(q, _)| schedule.per_query_rates()[*q] > 0.0)
                    .filter_map(|(_, row)| row[id])
                    .collect();
                let has_positive = variables.add(variable().min(0).max(1));
                for reached in &positive {
                    constraints.push(constraint!(has_positive >= *reached));
                }
                constraints.push(constraint!(
                    has_positive <= positive.iter().copied().sum::<Expression>()
                ));
                // Match the evaluator's longest-reader fallback when every reached
                // reader has zero expected demand; no division by zero is needed.
                for (q, row) in reach.iter().enumerate() {
                    if let Some(reached) = row[id] {
                        constraints.push(constraint!(
                            lifetime >= latency[q].clone() - (1.0 - reached) - has_positive
                        ));
                    }
                }
            }
        }
        for (used, rate) in &demands[id] {
            objective += node.held_memory_cost_per_second * latency_scale * *rate / divisor
                * gated(variables, constraints, lifetime.into(), *used, 1.0);
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

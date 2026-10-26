//! Heterogeneous execution demand for a small query-subset domain.
//! Subset rates come from the evaluator, not a second scheduling policy.
use super::*;

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
    if let Some(general) = &schedule.general {
        return general_schedule::encode(
            graph,
            profile,
            schedule,
            general,
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
    if let Some(lattice) = &schedule.compact_fixed {
        return fixed_schedules::encode(
            graph,
            profile,
            schedule,
            lattice,
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
    let subsets = 1usize << queries;
    let latency_scale = profile.maximum_latency(graph).max(f64::MIN_POSITIVE);
    let latency = query_latency_expressions(
        graph,
        profile,
        variables,
        constraints,
        reach,
        &check_deadline,
    )?;
    let mut masks = Vec::with_capacity(count);
    for id in 0..count {
        check_deadline()?;
        let allowed = reach
            .iter()
            .enumerate()
            .filter(|(_, row)| row[id].is_some())
            .fold(0usize, |mask, (q, _)| mask | (1 << q));
        let row: Vec<_> = (0..subsets)
            .map(|mask| (mask & !allowed == 0).then(|| variables.add(variable().min(0).max(1))))
            .collect();
        // Integral query reach fixes exactly one subset, so these selector
        // auxiliaries need no additional integer branching.
        constraints.push(constraint!(
            row.iter().flatten().copied().sum::<Expression>() == 1
        ));
        constraints.push(constraint!(
            row.iter().skip(1).flatten().copied().sum::<Expression>() == active[id]
        ));
        for (query, reachable) in reach.iter().enumerate() {
            let included: Expression = row
                .iter()
                .enumerate()
                .filter(|(mask, _)| mask & (1 << query) != 0)
                .filter_map(|(_, variable)| *variable)
                .sum();
            constraints.push(constraint!(
                included == reachable[id].map_or_else(|| Expression::from(0.0), Expression::from)
            ));
        }
        masks.push(row);
    }
    // Each pair is (binary demand indicator, executions per second). A query
    // boundary has its own rate; a materialized boundary uses its selected
    // downstream query subset's union rate and stops upstream propagation.
    let mut demands = vec![Vec::<(Variable, f64)>::new(); count];
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
        let boundary = flow.boundary;
        for (id, reaches) in flow.reached.into_iter().enumerate() {
            check_deadline()?;
            let Some(reaches) = reaches else { continue };
            if boundary < queries {
                demands[id].push((reaches, schedule.per_query_rates[boundary]));
            } else if profile.nodes[boundary - queries].ingestion {
                demands[id].push((reaches, 1.0));
            } else {
                for (mask, selector) in masks[boundary - queries].iter().enumerate().skip(1) {
                    let Some(selector) = selector else { continue };
                    let used = variables.add(variable().min(0).max(1));
                    constraints.push(constraint!(used <= reaches));
                    constraints.push(constraint!(used <= *selector));
                    constraints.push(constraint!(used >= reaches + *selector - 1));
                    demands[id].push((used, schedule.materialized_subset_rates[mask]));
                }
            }
        }
    }
    let mut objective = Expression::from(profile.request_cost / divisor);
    let mut retained = BTreeMap::<&str, Vec<(usize, f64)>>::new();
    for (id, node) in profile.nodes.iter().enumerate() {
        check_deadline()?;
        let executions: Expression = if node.materialized && node.ingestion {
            active[id].into()
        } else if node.materialized {
            masks[id]
                .iter()
                .enumerate()
                .skip(1)
                .filter_map(|(mask, selector)| {
                    selector.map(|s| schedule.materialized_subset_rates[mask] * s)
                })
                .sum()
        } else {
            demands[id].iter().map(|(used, rate)| *rate * *used).sum()
        };
        objective += node.standing_cost_per_second / divisor * active[id];
        objective += node.execution_cost / divisor * executions;
        if let Some((source, cost)) = &node.raw_retention {
            retained.entry(source).or_default().push((id, *cost));
        }
        if node.held_memory_cost_per_second == 0.0 {
            continue;
        }
        let mut lifetime_demand = Expression::from(0.0);
        if node.materialized {
            let longest = variables.add(variable().min(0).max(1));
            for (query, row) in reach.iter().enumerate() {
                if let Some(reached) = row[id] {
                    constraints.push(constraint!(
                        longest >= latency[query].clone() - (1.0 - reached)
                    ));
                }
            }
            for (mask, selector) in masks[id].iter().enumerate().skip(1) {
                if let Some(selector) = selector {
                    lifetime_demand += schedule.materialized_subset_rates[mask]
                        * gated(variables, constraints, longest.into(), *selector, 1.0);
                }
            }
        } else {
            for (mask, selector) in masks[id].iter().enumerate().skip(1) {
                check_deadline()?;
                let Some(selector) = selector else { continue };
                let total_rate: f64 = schedule
                    .per_query_rates
                    .iter()
                    .enumerate()
                    .filter(|(q, _)| mask & (1 << q) != 0)
                    .map(|(_, rate)| *rate)
                    .sum();
                let mean: Expression = latency
                    .iter()
                    .enumerate()
                    .filter(|(q, _)| mask & (1 << q) != 0)
                    .map(|(q, latency)| schedule.per_query_rates[q] / total_rate * latency.clone())
                    .sum();
                let selected_mean = gated(variables, constraints, mean, *selector, 1.0);
                for (demand, rate) in &demands[id] {
                    check_deadline()?;
                    lifetime_demand +=
                        *rate * gated(variables, constraints, selected_mean.into(), *demand, 1.0);
                }
            }
        }
        objective += node.held_memory_cost_per_second * latency_scale / divisor * lifetime_demand;
    }
    for entries in retained.into_values() {
        let retained_cost = variables.add(variable().min(0));
        for (id, cost) in entries {
            constraints.push(constraint!(retained_cost >= cost / divisor * active[id]));
        }
        objective += retained_cost;
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
    use asap_types::workload::{QueryRecurrence, RepeatedDemand, RepetitionInterval};

    fn schedules(queries: &[QueryRecurrence]) -> QueryScheduleRates {
        let union = |mask: usize| {
            crate::cost::execution_demand::evaluation_rate(
                queries
                    .iter()
                    .enumerate()
                    .filter(|(q, _)| mask & (1 << q) != 0)
                    .map(|(_, query)| (query, None)),
                3600.0,
            )
            .unwrap()
        };
        QueryScheduleRates {
            general: None,
            compact_fixed: None,
            per_query_rates: (0..queries.len()).map(|q| union(1 << q)).collect(),
            materialized_subset_rates: (0..1 << queries.len()).map(union).collect(),
        }
    }
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

    // Boundary demand is the evaluator's union rate; upstream held memory uses
    // the rate-weighted mean, not the arithmetic mean or the sum of root rates.
    #[test]
    fn heterogeneous_intervals_price_boundary_rates_and_weighted_lifetimes() {
        let queries = [500, 1000].map(|ms| {
            QueryRecurrence::Repeated(RepeatedDemand::FixedInterval(RepetitionInterval(ms)))
        });
        let graph = CandidateDependencyGraph {
            candidates: vec![node(None), node(Some(0)), node(Some(1)), node(Some(1))],
            query_roots: vec![root(2), root(3)],
            ..Default::default()
        };
        let profile = DirectCostProfile {
            schedules: Some(schedules(&queries)),
            query_rate: 1.0,
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
        assert_eq!(
            profile
                .schedules
                .as_ref()
                .unwrap()
                .materialized_subset_rates[3],
            3.0
        );
        let result = solve_direct_cost_proposal_with_metrics(
            &graph,
            &[],
            None,
            &mut CandidateSolveMetrics::default(),
            &profile,
        )
        .unwrap()
        .unwrap();
        assert!((result.direct_modeled_cost.unwrap() - 167.5).abs() < 1e-8);
        assert!((result.model_lower_bound.unwrap() - 167.5).abs() < 1e-8);
    }

    // Same-interval readers can share one evaluation only at compatible phases.
    // Use the actual evaluator's table rather than assuming all equal rates merge.
    #[test]
    fn aligned_and_offset_phases_use_evaluator_union_policy() {
        use asap_types::workload::TimestampMs;
        let graph = CandidateDependencyGraph {
            candidates: vec![node(None)],
            query_roots: vec![root(0), root(0)],
            ..Default::default()
        };
        for (second_phase, expected) in [(0, 7.0), (500, 14.0)] {
            let queries = [0, second_phase].map(|phase| {
                QueryRecurrence::Repeated(RepeatedDemand::FixedIntervalAt {
                    interval: RepetitionInterval(1000),
                    evaluation_phase: TimestampMs(phase),
                })
            });
            let profile = DirectCostProfile {
                schedules: Some(schedules(&queries)),
                query_rate: 1.0,
                request_latency_seconds: 0.0,
                request_cost: 0.0,
                nodes: vec![NodeCostProfile {
                    per_request: false,
                    ingestion: false,
                    standing_cost_per_second: 0.0,
                    execution_cost: 3.0,
                    work_seconds: 2.0,
                    compaction_seconds: 0.0,
                    held_memory_cost_per_second: 2.0,
                    materialized: true,
                    raw_retention: None,
                }],
            };
            let result = solve_direct_cost_proposal_with_metrics(
                &graph,
                &[],
                None,
                &mut CandidateSolveMetrics::default(),
                &profile,
            )
            .unwrap()
            .unwrap();
            assert!((result.direct_modeled_cost.unwrap() - expected).abs() < 1e-8);
            // HiGHS epigraph feasibility is tolerance-based; the decoded selected
            // cost above is recomputed independently and remains exact here.
            assert!(
                (result.model_lower_bound.unwrap() - expected).abs() <= 1e-7 * expected,
                "bound={:?}, expected={expected}",
                result.model_lower_bound
            );
        }
    }

    // Every reader can independently choose either materialized producer.
    // Compare the joint optimum with four independently priced assignments.
    #[test]
    fn heterogeneous_selection_matches_exhaustive_reader_assignments() {
        let queries = [500, 1000].map(|ms| {
            QueryRecurrence::Repeated(RepeatedDemand::FixedInterval(RepetitionInterval(ms)))
        });
        let schedule = schedules(&queries);
        let mut graph = CandidateDependencyGraph {
            candidates: vec![node(None), node(None), node(Some(0)), node(Some(0))],
            query_roots: vec![root(2), root(3)],
            ..Default::default()
        };
        for id in [2, 3] {
            graph.candidates[id].producer_requirements[0].compatible_producers =
                vec![PhysicalCandidateId(0), PhysicalCandidateId(1)].into();
        }
        let profile = DirectCostProfile {
            schedules: Some(schedule.clone()),
            query_rate: 1.0,
            request_latency_seconds: 0.5,
            request_cost: 7.0,
            nodes: vec![
                NodeCostProfile {
                    per_request: false,
                    ingestion: false,
                    standing_cost_per_second: 0.0,
                    execution_cost: 10.0,
                    work_seconds: 1.0,
                    compaction_seconds: 0.0,
                    held_memory_cost_per_second: 3.0,
                    materialized: true,
                    raw_retention: None,
                },
                NodeCostProfile {
                    per_request: false,
                    ingestion: false,
                    standing_cost_per_second: 0.0,
                    execution_cost: 2.0,
                    work_seconds: 0.5,
                    compaction_seconds: 0.0,
                    held_memory_cost_per_second: 1.0,
                    materialized: true,
                    raw_retention: None,
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
        let expected = [[0, 0], [0, 1], [1, 0], [1, 1]]
            .into_iter()
            .map(|assignment| {
                let latency = [
                    2.5 + [1.0, 0.5][assignment[0]],
                    3.5 + [1.0, 0.5][assignment[1]],
                ];
                let mut cost = 7.0 + 3.0;
                for supplier in [0, 1] {
                    let mask = assignment
                        .iter()
                        .enumerate()
                        .filter(|(_, chosen)| **chosen == supplier)
                        .fold(0, |mask, (q, _)| mask | (1 << q));
                    let longest = latency
                        .iter()
                        .enumerate()
                        .filter(|(q, _)| mask & (1 << q) != 0)
                        .map(|(_, time)| *time)
                        .fold(0.0, f64::max);
                    cost += schedule.materialized_subset_rates[mask]
                        * ([10.0, 2.0][supplier] + [3.0, 1.0][supplier] * longest);
                }
                cost
            })
            .fold(f64::INFINITY, f64::min);
        let result = solve_direct_cost_proposal_with_metrics(
            &graph,
            &[],
            None,
            &mut CandidateSolveMetrics::default(),
            &profile,
        )
        .unwrap()
        .unwrap();
        assert!((result.direct_modeled_cost.unwrap() - expected).abs() < 1e-8);
        // HiGHS epigraph feasibility is tolerance-based; the decoded selected
        // cost above is recomputed independently and remains exact here.
        assert!(
            (result.model_lower_bound.unwrap() - expected).abs() <= 1e-7 * expected,
            "bound={:?}, expected={expected}",
            result.model_lower_bound
        );
    }
    // A rare slow reader's weighted held lifetime can dominate cost despite a
    // tiny rate. Reject that numerically unsupported mean before optimization,
    // rather than erase its contribution in a relaxed equality.
    #[test]
    fn extreme_reader_rates_are_explicitly_unsupported() {
        let queries = [1, u32::MAX].map(|ms| {
            QueryRecurrence::Repeated(RepeatedDemand::FixedInterval(RepetitionInterval(ms)))
        });
        let graph = CandidateDependencyGraph {
            candidates: vec![node(None), node(None), node(Some(0)), node(Some(0))],
            query_roots: vec![root(2), root(3)],
            ..Default::default()
        };
        let profile = DirectCostProfile {
            schedules: Some(schedules(&queries)),
            query_rate: 1.0,
            request_latency_seconds: 0.0,
            request_cost: 0.0,
            nodes: vec![
                NodeCostProfile {
                    ingestion: false,
                    standing_cost_per_second: 0.0,
                    execution_cost: 1.0,
                    held_memory_cost_per_second: 1e6,
                    ..Default::default()
                },
                NodeCostProfile {
                    ingestion: false,
                    standing_cost_per_second: 0.0,
                    execution_cost: 2.0,
                    held_memory_cost_per_second: 1e6,
                    ..Default::default()
                },
                NodeCostProfile {
                    ingestion: false,
                    standing_cost_per_second: 0.0,
                    work_seconds: 1.0,
                    compaction_seconds: 0.0,
                    ..Default::default()
                },
                NodeCostProfile {
                    ingestion: false,
                    standing_cost_per_second: 0.0,
                    work_seconds: 1e9,
                    compaction_seconds: 0.0,
                    ..Default::default()
                },
            ],
        };
        let rates = &profile.schedules.as_ref().unwrap().per_query_rates;
        assert!(
            rates[1] * 1e9 * 1e6 > 1e8,
            "the rare reader has meaningful memory cost"
        );
        assert!(matches!(
            profile.validate(&graph),
            Err(CandidateSelectionError::UnsupportedDirectCost(_))
        ));
    }
}

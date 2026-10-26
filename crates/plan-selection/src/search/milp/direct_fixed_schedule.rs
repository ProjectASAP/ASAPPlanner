//! Compact fixed-interval accounting follows the evaluator's policy: distinct
//! intervals add, distinct known phases add, unknown phase joins a known phase.
//! Weighted mean lifetimes use reachable integer rate totals, not reader subsets.
use super::*;

const MAXIMUM_RATE_TICKS: u64 = 4096;

#[derive(Debug, Clone, Copy)]
pub(crate) struct FixedQuerySchedule {
    pub interval_ms: u32,
    pub phase_ms: Option<u64>,
}

#[derive(Debug, Clone)]
pub(crate) struct FixedScheduleLattice {
    queries: Vec<FixedQuerySchedule>,
    weights: Vec<usize>,
}

fn gcd(mut left: u64, mut right: u64) -> u64 {
    while right != 0 {
        (left, right) = (right, left % right);
    }
    left
}

impl QueryScheduleRates {
    pub(crate) fn from_fixed_schedules(
        queries: Vec<FixedQuerySchedule>,
    ) -> Result<Self, CandidateSelectionError> {
        if queries.is_empty() || queries.iter().any(|q| q.interval_ms == 0) {
            return Err(CandidateSelectionError::InvalidGraph(
                "empty or zero-interval fixed schedule".into(),
            ));
        }
        let mut common = 1u64;
        for query in &queries {
            let interval = u64::from(query.interval_ms);
            common = (common / gcd(common, interval))
                .checked_mul(interval)
                .ok_or_else(|| {
                    CandidateSelectionError::UnsupportedDirectCost(
                        "fixed schedule lattice arithmetic overflow".into(),
                    )
                })?;
        }
        let mut weights: Vec<_> = queries
            .iter()
            .map(|q| common / u64::from(q.interval_ms))
            .collect();
        let divisor = weights.iter().copied().reduce(gcd).unwrap();
        for weight in &mut weights {
            *weight /= divisor;
        }
        let total = weights
            .iter()
            .try_fold(0u64, |sum, weight| sum.checked_add(*weight));
        if total.is_none_or(|total| total > MAXIMUM_RATE_TICKS) {
            return Err(CandidateSelectionError::UnsupportedDirectCost(format!(
                "fixed schedule rate lattice exceeds {MAXIMUM_RATE_TICKS} total ticks"
            )));
        }
        Ok(Self {
            per_query_rates: queries
                .iter()
                .map(|q| 1000.0 / f64::from(q.interval_ms))
                .collect(),
            materialized_subset_rates: Vec::new(),
            general: None,
            compact_fixed: Some(FixedScheduleLattice {
                queries,
                weights: weights.into_iter().map(|w| w as usize).collect(),
            }),
        })
    }
}

impl FixedScheduleLattice {
    pub(super) fn maximum_denominator_scale(&self) -> f64 {
        self.weights.iter().sum::<usize>() as f64
    }

    pub(super) fn validate(
        &self,
        schedule: &QueryScheduleRates,
        queries: usize,
    ) -> Result<(), CandidateSelectionError> {
        let expected = QueryScheduleRates::from_fixed_schedules(self.queries.clone())?;
        if queries != self.queries.len()
            || expected.per_query_rates != schedule.per_query_rates
            || !schedule.materialized_subset_rates.is_empty()
            || expected.compact_fixed.as_ref().unwrap().weights != self.weights
        {
            return Err(CandidateSelectionError::InvalidGraph(
                "inconsistent compact fixed schedule".into(),
            ));
        }
        Ok(())
    }

    pub(super) fn union_rate(&self, selected: impl Iterator<Item = bool>) -> f64 {
        let mut intervals = BTreeMap::<u32, (bool, std::collections::BTreeSet<u64>)>::new();
        for (query, selected) in self.queries.iter().zip(selected) {
            if !selected {
                continue;
            }
            let (unknown, phases) = intervals.entry(query.interval_ms).or_default();
            if let Some(phase) = query.phase_ms {
                phases.insert(phase % u64::from(query.interval_ms));
            } else {
                *unknown = true;
            }
        }
        intervals
            .into_iter()
            .map(|(interval, (unknown, phases))| {
                phases.len().max(usize::from(unknown)) as f64 * 1000.0 / f64::from(interval)
            })
            .sum()
    }
}

fn logical_or(
    variables: &mut ProblemVariables,
    constraints: &mut Vec<Constraint>,
    inputs: &[Variable],
) -> Option<Variable> {
    if inputs.is_empty() {
        return None;
    }
    if inputs.len() == 1 {
        return Some(inputs[0]);
    }
    let value = variables.add(variable().min(0).max(1));
    for input in inputs {
        constraints.push(constraint!(value >= *input));
    }
    constraints.push(constraint!(
        value <= inputs.iter().copied().sum::<Expression>()
    ));
    Some(value)
}

fn logical_and(
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

fn union_terms(
    lattice: &FixedScheduleLattice,
    node: usize,
    reach: &[Vec<Option<Variable>>],
    variables: &mut ProblemVariables,
    constraints: &mut Vec<Constraint>,
) -> Vec<(Variable, f64)> {
    let mut intervals = BTreeMap::<u32, BTreeMap<Option<u64>, Vec<Variable>>>::new();
    for (q, schedule) in lattice.queries.iter().enumerate() {
        if let Some(reached) = reach[q][node] {
            intervals
                .entry(schedule.interval_ms)
                .or_default()
                .entry(
                    schedule
                        .phase_ms
                        .map(|p| p % u64::from(schedule.interval_ms)),
                )
                .or_default()
                .push(reached);
        }
    }
    let mut terms = Vec::new();
    for (interval, mut phases) in intervals {
        let rate = 1000.0 / f64::from(interval);
        let unknown = logical_or(
            variables,
            constraints,
            &phases.remove(&None).unwrap_or_default(),
        );
        let known: Vec<_> = phases
            .values()
            .filter_map(|readers| logical_or(variables, constraints, readers))
            .collect();
        terms.extend(known.iter().map(|phase| (*phase, rate)));
        if let Some(unknown) = unknown {
            if let Some(any_known) = logical_or(variables, constraints, &known) {
                let extra = variables.add(variable().min(0).max(1));
                constraints.push(constraint!(extra <= unknown));
                constraints.push(constraint!(extra <= 1.0 - any_known));
                constraints.push(constraint!(extra >= unknown - any_known));
                terms.push((extra, rate));
            } else {
                terms.push((unknown, rate));
            }
        }
    }
    terms
}

#[allow(clippy::too_many_arguments)]
pub(super) fn encode(
    graph: &CandidateDependencyGraph,
    profile: &DirectCostProfile,
    schedule: &QueryScheduleRates,
    lattice: &FixedScheduleLattice,
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
    let scale = profile.maximum_latency(graph).max(f64::MIN_POSITIVE);
    let latency = query_latency_expressions(
        graph,
        profile,
        variables,
        constraints,
        reach,
        &check_deadline,
    )?;
    let mut unions = Vec::with_capacity(count);
    for id in 0..count {
        check_deadline()?;
        unions.push(union_terms(lattice, id, reach, variables, constraints));
    }
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
        for (id, used) in flow.reached.into_iter().enumerate() {
            check_deadline()?;
            let Some(used) = used else { continue };
            if flow.boundary < queries {
                demands[id].push((used, schedule.per_query_rates[flow.boundary]));
            } else if profile.nodes[flow.boundary - queries].ingestion {
                demands[id].push((used, 1.0));
            } else {
                for (phase, rate) in &unions[flow.boundary - queries] {
                    demands[id].push((logical_and(variables, constraints, used, *phase), *rate));
                }
            }
        }
    }
    let mut objective = Expression::from(profile.request_cost / divisor);
    let mut raw = BTreeMap::<&str, Vec<(usize, f64)>>::new();
    for (id, node) in profile.nodes.iter().enumerate() {
        check_deadline()?;
        let terms = if node.materialized {
            if node.ingestion {
                vec![(active[id], 1.0)]
            } else {
                unions[id].clone()
            }
        } else {
            demands[id].clone()
        };
        objective += node.standing_cost_per_second / divisor * active[id];
        objective +=
            node.execution_cost / divisor * terms.iter().map(|(v, r)| *r * *v).sum::<Expression>();
        if let Some((source, cost)) = &node.raw_retention {
            raw.entry(source).or_default().push((id, *cost));
        }
        if node.held_memory_cost_per_second == 0.0 {
            continue;
        }
        let mut lifetime = Expression::from(0.0);
        if node.materialized {
            let longest = variables.add(variable().min(0).max(1));
            for (q, row) in reach.iter().enumerate() {
                if let Some(reached) = row[id] {
                    constraints.push(constraint!(longest >= latency[q].clone() - (1.0 - reached)));
                }
            }
            for (used, rate) in terms {
                lifetime += rate * gated(variables, constraints, longest.into(), used, 1.0);
            }
        } else {
            let possible: Vec<_> = reach
                .iter()
                .enumerate()
                .filter_map(|(q, row)| row[id].map(|r| (q, r)))
                .collect();
            let maximum: usize = possible.iter().map(|(q, _)| lattice.weights[*q]).sum();
            if maximum == 0 {
                continue;
            }
            let mut totals = vec![false; maximum + 1];
            totals[0] = true;
            let mut accumulated = 0;
            for (q, _) in &possible {
                let weight = lattice.weights[*q];
                for sum in (0..=accumulated).rev() {
                    if totals[sum] {
                        totals[sum + weight] = true;
                    }
                }
                accumulated += weight;
            }
            // Denominator totals need binary selectors: unlike full reader
            // subsets, their weighted-sum equality alone permits mixtures.
            let selectors: Vec<_> = totals
                .iter()
                .enumerate()
                .filter(|(_, yes)| **yes)
                .map(|(total, _)| (total, variables.add(variable().binary())))
                .collect();
            constraints.push(constraint!(
                selectors.iter().map(|(_, v)| *v).sum::<Expression>() == 1
            ));
            constraints.push(constraint!(
                selectors
                    .iter()
                    .map(|(d, v)| *d as f64 * *v)
                    .sum::<Expression>()
                    == possible
                        .iter()
                        .map(|(q, r)| lattice.weights[*q] as f64 * *r)
                        .sum::<Expression>()
            ));
            let numerator: Expression = possible
                .iter()
                .map(|(q, r)| {
                    lattice.weights[*q] as f64 / maximum as f64
                        * gated(variables, constraints, latency[*q].clone(), *r, 1.0)
                })
                .sum();
            for (denominator, selector) in selectors.into_iter().filter(|(d, _)| *d > 0) {
                check_deadline()?;
                let chosen = gated(variables, constraints, numerator.clone(), selector, 1.0);
                for (used, rate) in &terms {
                    lifetime += *rate * maximum as f64 / denominator as f64
                        * gated(variables, constraints, chosen.into(), *used, 1.0);
                }
            }
        }
        objective += node.held_memory_cost_per_second * scale / divisor * lifetime;
    }
    for entries in raw.into_values() {
        let held = variables.add(variable().min(0));
        for (id, cost) in entries {
            constraints.push(constraint!(held >= cost / divisor * active[id]));
        }
        objective += held;
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

    // All reader subsets agree with the existing interval accounting policy;
    // the compact representation never constructs a 2^Q rate table.
    #[test]
    fn compact_fixed_rates_match_evaluator_subsets() {
        let intervals = [1000, 2000, 1000, 4000, 2000, 1000];
        let fixed: Vec<_> = intervals
            .iter()
            .map(|ms| FixedQuerySchedule {
                interval_ms: *ms,
                phase_ms: None,
            })
            .collect();
        let schedule = QueryScheduleRates::from_fixed_schedules(fixed).unwrap();
        let recurrences: Vec<_> = intervals
            .iter()
            .map(|ms| {
                QueryRecurrence::Repeated(RepeatedDemand::FixedInterval(RepetitionInterval(*ms)))
            })
            .collect();
        for mask in 0..1usize << intervals.len() {
            let expected = crate::cost::execution_demand::evaluation_rate(
                recurrences
                    .iter()
                    .enumerate()
                    .filter(|(q, _)| mask & (1 << q) != 0)
                    .map(|(_, r)| (r, None)),
                3600.0,
            )
            .unwrap();
            let actual = schedule
                .compact_fixed
                .as_ref()
                .unwrap()
                .union_rate((0..intervals.len()).map(|q| mask & (1 << q) != 0));
            assert_eq!(actual, expected);
        }
        assert!(schedule.materialized_subset_rates.is_empty());
        let large = QueryScheduleRates::from_fixed_schedules(vec![
            FixedQuerySchedule {
                interval_ms: 1000,
                phase_ms: None
            };
            128
        ])
        .unwrap();
        assert_eq!(large.compact_fixed.unwrap().weights.len(), 128);
        assert!(matches!(
            QueryScheduleRates::from_fixed_schedules(vec![
                FixedQuerySchedule {
                    interval_ms: 1,
                    phase_ms: None
                },
                FixedQuerySchedule {
                    interval_ms: 100000,
                    phase_ms: None
                },
            ]),
            Err(CandidateSelectionError::UnsupportedDirectCost(_))
        ));
    }

    // Known phases add; unknown phase adds only when no known phase is active.
    #[test]
    fn compact_phase_activation_is_exact_for_every_subset() {
        let schedule = QueryScheduleRates::from_fixed_schedules(vec![
            FixedQuerySchedule {
                interval_ms: 1000,
                phase_ms: None,
            },
            FixedQuerySchedule {
                interval_ms: 1000,
                phase_ms: Some(0),
            },
            FixedQuerySchedule {
                interval_ms: 1000,
                phase_ms: Some(500),
            },
            FixedQuerySchedule {
                interval_ms: 1000,
                phase_ms: Some(1500),
            },
        ])
        .unwrap();
        for mask in 0..16 {
            let mut variables = ProblemVariables::new();
            let mut constraints = Vec::new();
            let reach: Vec<_> = (0..4)
                .map(|q| {
                    let value = variables.add(variable().binary());
                    constraints.push(constraint!(value == f64::from(mask & (1 << q) != 0)));
                    vec![Some(value)]
                })
                .collect();
            let lattice = schedule.compact_fixed.as_ref().unwrap();
            let terms = union_terms(lattice, 0, &reach, &mut variables, &mut constraints);
            let expression: Expression = terms.iter().map(|(v, r)| *r * *v).sum();
            use good_lp::{highs, Solution, SolverModel};
            let solution = variables
                .minimise(-expression.clone())
                .using(highs)
                .with_all(constraints)
                .solve()
                .unwrap();
            let expected = lattice.union_rate((0..4).map(|q| mask & (1 << q) != 0));
            assert_eq!(solution.eval(&expression), expected);
        }
    }

    // Five readers share a query-materialized build. Upstream lifetime uses
    // their weighted mean; the build uses their maximum and union refresh rate.
    #[test]
    fn compact_schedule_prices_shared_refresh_and_weighted_memory() {
        let intervals = [1000, 2000, 1000, 4000, 2000];
        let schedule = QueryScheduleRates::from_fixed_schedules(
            intervals
                .iter()
                .map(|ms| FixedQuerySchedule {
                    interval_ms: *ms,
                    phase_ms: None,
                })
                .collect(),
        )
        .unwrap();
        let mut candidates = vec![node(None), node(Some(0))];
        candidates.extend((0..5).map(|_| node(Some(1))));
        let graph = CandidateDependencyGraph {
            candidates,
            query_roots: (2..7)
                .map(|id| QueryRootCandidates {
                    compatible_candidates: vec![PhysicalCandidateId(id)],
                })
                .collect(),
            ..Default::default()
        };
        let mut nodes = vec![
            NodeCostProfile {
                execution_cost: 2.0,
                work_seconds: 1.0,
                held_memory_cost_per_second: 3.0,
                ..Default::default()
            },
            NodeCostProfile {
                execution_cost: 4.0,
                work_seconds: 2.0,
                held_memory_cost_per_second: 2.0,
                materialized: true,
                ..Default::default()
            },
        ];
        nodes.extend((0..5).map(|q| NodeCostProfile {
            execution_cost: 1.0,
            work_seconds: q as f64 + 1.0,
            ..Default::default()
        }));
        let rates = schedule.per_query_rates.clone();
        let union = 1.75;
        let weighted: f64 = rates
            .iter()
            .enumerate()
            .map(|(q, r)| r * (q as f64 + 4.0))
            .sum();
        let sum: f64 = rates.iter().sum();
        let expected = union * (6.0 + 3.0 * weighted / sum + 2.0 * 8.0) + sum;
        let profile = DirectCostProfile {
            nodes,
            schedules: Some(schedule),
            query_rate: 1.0,
            request_latency_seconds: 0.0,
            request_cost: 0.0,
        };
        let mut metrics = CandidateSolveMetrics::default();
        let proposal =
            solve_direct_cost_proposal_with_metrics(&graph, &[], None, &mut metrics, &profile)
                .unwrap()
                .unwrap();
        assert!((proposal.direct_modeled_cost.unwrap() - expected).abs() < 1e-8);
        assert!((proposal.model_lower_bound.unwrap() - expected).abs() < 1e-6);
        assert!(metrics.peak_variables < 5000);
    }
}

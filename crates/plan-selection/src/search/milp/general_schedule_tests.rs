use super::*;
use crate::search::milp::model::{solve_direct_cost_proposal_with_metrics, CandidateSolveMetrics};
use asap_types::ir::properties::AccuracyTarget;
use asap_types::workload::{
    DemandEstimate, Predictability, Query, QueryRecurrence, QueryRequirements, QueryWorkloadEntry,
    RangeEvaluation, Rate, RepeatedDemand, RepetitionInterval,
};

fn entry(recurrence: QueryRecurrence, evaluations: u64) -> QueryWorkloadEntry {
    QueryWorkloadEntry {
        query: Query("sum(m)".into()),
        requirements: QueryRequirements {
            accuracy: AccuracyTarget::exact(),
            max_latency_ms: None,
        },
        predictability: Predictability::Predictable { known_at: None },
        recurrence,
        range_evaluation: (evaluations > 1).then_some(RangeEvaluation {
            span_ms: (evaluations - 1) * 1000,
            step_ms: 1000,
        }),
    }
}
fn fixed(interval: u32, evaluations: u64) -> QueryWorkloadEntry {
    entry(
        QueryRecurrence::Repeated(RepeatedDemand::FixedInterval(RepetitionInterval(interval))),
        evaluations,
    )
}
fn node(id: usize, input: Option<usize>) -> PhysicalCandidate {
    PhysicalCandidate {
        group: asap_logical_asap_optimization::pass2::memo::MemoGroupId(id as u32),
        local_cost: 0.0,
        producer_requirements: input
            .into_iter()
            .map(|input| ProducerRequirement {
                compatible_producers: vec![PhysicalCandidateId(input)].into(),
            })
            .collect(),
    }
}
fn check_cost(
    entries: &[QueryWorkloadEntry],
    expected_rates: &[f64],
    expected_union: f64,
    per_request: bool,
) {
    let general = QueryScheduleRates::new(entries, 60.0, None).unwrap();
    for (actual, expected) in general.per_query_rates().iter().zip(expected_rates) {
        assert!((actual - expected).abs() < 1e-12);
    }
    let schedule = general;
    let mut candidates = vec![node(0, None), node(1, Some(0))];
    candidates.extend((0..entries.len()).map(|q| node(q + 2, Some(1))));
    let graph = CandidateDependencyGraph {
        candidates,
        query_roots: (0..entries.len())
            .map(|q| QueryRootCandidates {
                compatible_candidates: vec![PhysicalCandidateId(q + 2)],
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
            held_memory_cost_per_second: if per_request { 0.0 } else { 2.0 },
            standing_cost_per_second: if per_request { 13.0 } else { 0.0 },
            materialized: true,
            per_request,
            ..Default::default()
        },
    ];
    nodes.extend((0..entries.len()).map(|q| NodeCostProfile {
        execution_cost: 1.0,
        work_seconds: q as f64 + 1.0,
        ..Default::default()
    }));
    let sum: f64 = expected_rates.iter().sum();
    let weighted: f64 = expected_rates
        .iter()
        .enumerate()
        .map(|(q, rate)| rate * (q as f64 + 4.0))
        .sum();
    let longest = entries.len() as f64 + 3.0;
    let expected = expected_union
        * (6.0 + 3.0 * weighted / sum + if per_request { 0.0 } else { 2.0 * longest })
        + sum
        + if per_request { 13.0 } else { 0.0 };
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
    let direct = proposal.direct_modeled_cost.unwrap();
    assert!(
        (direct - expected).abs() < 1e-8 * expected.max(1.0),
        "direct {direct}, independent expected {expected}"
    );
    assert!((proposal.model_lower_bound.unwrap() - expected).abs() < 1e-6 * expected.max(1.0));
    assert!(
        metrics.peak_variables < 5000,
        "a rate lattice or reader subset product is unnecessary"
    );
}

#[test]
fn noncommensurate_fixed_intervals_price_weighted_memory_without_integer_lattice() {
    let intervals = [59_000, 61_000, 67_000, 71_000];
    let entries: Vec<_> = intervals
        .iter()
        .map(|interval| fixed(*interval, 1))
        .collect();
    let rates: Vec<_> = intervals
        .iter()
        .map(|interval| 1000.0 / f64::from(*interval))
        .collect();
    check_cost(&entries, &rates, rates.iter().sum(), false);
}

#[test]
fn mixed_schedules_price_materialized_union_and_unmaterialized_mean() {
    let estimate = QueryRecurrence::Repeated(RepeatedDemand::EstimatedRate(DemandEstimate {
        expected_rate: Rate(0.137),
        observed_at: None,
        valid_for: None,
    }));
    let entries = vec![
        fixed(1000, 2),
        fixed(2000, 3),
        entry(estimate, 4),
        entry(
            QueryRecurrence::OneTime {
                invocations: 3,
                execute_at: None,
            },
            5,
        ),
        entry(QueryRecurrence::Unknown, 6),
    ];
    let rates = [2.0, 1.5, 0.548, 0.25, 0.1];
    // Fixed intervals and estimates add; the two batch demands use max(15,6)/60.
    check_cost(&entries, &rates, 2.0 + 1.5 + 0.548 + 0.25, false);
    // Retained refreshes use request rates; readers and weighted lifetime still
    // use evaluation rates including each query's different range length.
    check_cost(&entries, &rates, 1.0 + 0.5 + 0.137 + 3.0 / 60.0, true);
}

#[test]
fn zero_rate_readers_preserve_materialized_and_unmaterialized_lifetimes() {
    // With all rates zero the execution cost is zero. With one positive reader,
    // unmaterialized lifetime ignores the zero-rate reader, while a shared
    // query-time materialization still uses its longest reached latency.
    for rates in [[0.0, 0.0], [0.0, 0.5], [0.5, 0.0]] {
        let entries: Vec<_> = rates
            .iter()
            .map(|rate| {
                entry(
                    QueryRecurrence::Repeated(RepeatedDemand::EstimatedRate(DemandEstimate {
                        expected_rate: Rate(*rate),
                        observed_at: None,
                        valid_for: None,
                    })),
                    1,
                )
            })
            .collect();
        let general = QueryScheduleRates::new(&entries, 60.0, None).unwrap();
        let graph = CandidateDependencyGraph {
            candidates: vec![
                node(0, None),
                node(1, Some(0)),
                node(2, Some(1)),
                node(3, Some(1)),
            ],
            query_roots: vec![
                QueryRootCandidates {
                    compatible_candidates: vec![PhysicalCandidateId(2)],
                },
                QueryRootCandidates {
                    compatible_candidates: vec![PhysicalCandidateId(3)],
                },
            ],
            ..Default::default()
        };
        let profile = DirectCostProfile {
            nodes: vec![
                NodeCostProfile {
                    execution_cost: 2.0,
                    work_seconds: 1.0,
                    held_memory_cost_per_second: 3.0,
                    ..Default::default()
                },
                NodeCostProfile {
                    materialized: true,
                    execution_cost: 4.0,
                    work_seconds: 2.0,
                    held_memory_cost_per_second: 2.0,
                    ..Default::default()
                },
                NodeCostProfile {
                    execution_cost: 1.0,
                    work_seconds: 1.0,
                    ..Default::default()
                },
                NodeCostProfile {
                    execution_cost: 1.0,
                    work_seconds: 2.0,
                    ..Default::default()
                },
            ],
            schedules: Some(general),
            query_rate: 1.0,
            request_latency_seconds: 0.0,
            request_cost: 0.0,
        };
        let sum: f64 = rates.iter().sum();
        let lifetime = if sum > 0.0 {
            (rates[0] * 4.0 + rates[1] * 5.0) / sum
        } else {
            5.0
        };
        let expected = sum * (2.0 + 4.0 + 3.0 * lifetime + 2.0 * 5.0) + sum;
        let proposal = solve_direct_cost_proposal_with_metrics(
            &graph,
            &[],
            None,
            &mut Default::default(),
            &profile,
        )
        .unwrap()
        .unwrap();
        assert!(
            (proposal.direct_modeled_cost.unwrap() - expected).abs() < 1e-8,
            "rates {rates:?}: expected {expected}, got {:?}",
            proposal.direct_modeled_cost
        );
        assert!((proposal.model_lower_bound.unwrap() - expected).abs() < 1e-6);
    }
}

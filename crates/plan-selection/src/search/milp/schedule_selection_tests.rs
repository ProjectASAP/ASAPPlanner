use super::*;
use crate::search::milp::model::{solve_direct_cost_proposal_with_metrics, CandidateSolveMetrics};
use asap_logical_asap_optimization::pass2::memo::MemoGroupId;
use asap_types::workload::{QueryRecurrence, RepeatedDemand, RepetitionInterval};

fn union(queries: &[QueryRecurrence], mask: usize) -> f64 {
    asap_cost::execution_demand::evaluation_rate(
        queries
            .iter()
            .enumerate()
            .filter(|(q, _)| mask & (1 << q) != 0)
            .map(|(_, query)| (query, None)),
        3600.0,
    )
    .unwrap()
}
fn schedules(queries: &[QueryRecurrence]) -> QueryScheduleRates {
    use asap_types::workload::{Predictability, Query, QueryRequirements, QueryWorkloadEntry};
    let entries: Vec<_> = queries
        .iter()
        .map(|recurrence| QueryWorkloadEntry {
            query: Query("m".into()),
            requirements: QueryRequirements {
                accuracy: asap_types::ir::properties::AccuracyTarget::exact(),
                max_latency_ms: None,
            },
            predictability: Predictability::Predictable { known_at: None },
            recurrence: recurrence.clone(),
            range_evaluation: None,
        })
        .collect();
    QueryScheduleRates::new(&entries, 3600.0, None).unwrap()
}
pub(super) fn rate_schedule(rates: &[f64]) -> QueryScheduleRates {
    schedules(
        &rates
            .iter()
            .map(|rate| {
                QueryRecurrence::Repeated(RepeatedDemand::FixedInterval(RepetitionInterval(
                    (1000.0 / rate) as u32,
                )))
            })
            .collect::<Vec<_>>(),
    )
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
    let queries = [500, 1000]
        .map(|ms| QueryRecurrence::Repeated(RepeatedDemand::FixedInterval(RepetitionInterval(ms))));
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
            .union_rate([true, true].into_iter()),
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
// Compare with the evaluator's policy rather than assuming equal rates merge.
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
    let queries = [500, 1000]
        .map(|ms| QueryRecurrence::Repeated(RepeatedDemand::FixedInterval(RepetitionInterval(ms))));
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
                cost += union(&queries, mask)
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
    let queries = [1, u32::MAX]
        .map(|ms| QueryRecurrence::Repeated(RepeatedDemand::FixedInterval(RepetitionInterval(ms))));
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
    let rates = &profile.schedules.as_ref().unwrap().per_query_rates();
    assert!(
        rates[1] * 1e9 * 1e6 > 1e8,
        "the rare reader has meaningful memory cost"
    );
    assert!(matches!(
        profile.validate(&graph),
        Err(CandidateSelectionError::UnsupportedDirectCost(_))
    ));
}

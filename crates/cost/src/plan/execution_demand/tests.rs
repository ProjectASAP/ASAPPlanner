use super::*;
use asap_types::workload::{DemandEstimate, Rate, RepetitionInterval, TimestampMs};

fn fixed(interval: u32, phase: u64) -> QueryRecurrence {
    QueryRecurrence::Repeated(RepeatedDemand::FixedIntervalAt {
        interval: RepetitionInterval(interval),
        evaluation_phase: TimestampMs(phase),
    })
}

fn close(actual: f64, expected: f64) {
    assert!((actual - expected).abs() < 1e-12, "{actual} != {expected}");
}

// Equal phases share evaluations; separate phases add.
#[test]
fn chosen_readers_preserve_phase_accounting() {
    let queries = [
        fixed(20_000, 5_000),
        fixed(20_000, 25_000),
        fixed(20_000, 0),
    ];
    close(
        evaluation_rate(queries[..2].iter().map(|r| (r, None)), 3600.0).unwrap(),
        1.0 / 20.0,
    );
    close(
        evaluation_rate(queries.iter().map(|r| (r, None)), 3600.0).unwrap(),
        2.0 / 20.0,
    );
    // No global subset cache: each caller's selected queries determine the rate.
    close(evaluation_rate(std::iter::empty(), 3600.0).unwrap(), 0.0);
}

// Range offsets union within one cadence; retained state instead uses requests.
#[test]
fn range_offsets_and_request_rates_stay_distinct() {
    let recurrence = fixed(60_000, 0);
    let ranges = [
        Some(RangeEvaluation {
            step_ms: 60_000,
            span_ms: 3_600_000,
        }),
        Some(RangeEvaluation {
            step_ms: 30_000,
            span_ms: 600_000,
        }),
        None,
    ];
    close(
        evaluation_rate(ranges.iter().map(|r| (&recurrence, *r)), 3600.0).unwrap(),
        71.0 / 60.0,
    );
    close(
        evaluation_rate(ranges.iter().map(|_| (&recurrence, None)), 3600.0).unwrap(),
        1.0 / 60.0,
    );
}

// Different fixed intervals remain additive even if timestamps can coincide.
#[test]
fn different_intervals_preserve_existing_conservative_rate() {
    let queries = [fixed(20_000, 0), fixed(30_000, 0)];
    close(
        evaluation_rate(queries.iter().map(|r| (r, None)), 3600.0).unwrap(),
        1.0 / 20.0 + 1.0 / 30.0,
    );
}

// Estimates add with range multiplicity; scheduled/one-off/unknown use a maximum.
#[test]
fn estimated_and_batched_nonrepeating_demand_keep_their_policy() {
    let estimate = QueryRecurrence::Repeated(RepeatedDemand::EstimatedRate(DemandEstimate {
        expected_rate: Rate(2.0),
        observed_at: None,
        valid_for: None,
    }));
    let scheduled = QueryRecurrence::Repeated(RepeatedDemand::Scheduled(vec![
        TimestampMs(0),
        TimestampMs(10),
    ]));
    let once = QueryRecurrence::OneTime {
        invocations: 4,
        execute_at: None,
    };
    let unknown = QueryRecurrence::Unknown;
    let range = Some(RangeEvaluation {
        step_ms: 1000,
        span_ms: 2000,
    });
    close(
        evaluation_rate(
            [
                (&estimate, range),
                (&estimate, None),
                (&scheduled, range),
                (&once, None),
                (&unknown, None),
            ],
            100.0,
        )
        .unwrap(),
        8.0 + 6.0 / 100.0,
    );
}

#[test]
fn billion_offset_mixed_ranges_are_counted_arithmetically() {
    let recurrence = fixed(1000, 0);
    let rate = evaluation_rate(
        [
            (
                &recurrence,
                Some(RangeEvaluation {
                    span_ms: 1_000_000_000,
                    step_ms: 2,
                }),
            ),
            (
                &recurrence,
                Some(RangeEvaluation {
                    span_ms: 1_000_000_000,
                    step_ms: 3,
                }),
            ),
        ],
        60.0,
    )
    .unwrap();
    assert_eq!(rate, 666_666_668.0);
}

#[test]
fn arithmetic_union_matches_small_explicit_offset_sets() {
    for left_step in 1..=7 {
        for right_step in 1..=7 {
            for left_span in [0, 5, 17, 30] {
                for right_span in [0, 9, 23, 30] {
                    let mut ranges = BTreeMap::new();
                    for (step, span) in [(left_step, left_span), (right_step, right_span), (11, 29)]
                    {
                        let longest = ranges.entry(step).or_insert(0u64);
                        *longest = (*longest).max(span);
                    }
                    let mut oracle = BTreeSet::new();
                    for (&step, &span) in &ranges {
                        oracle.extend((0..=span / step).map(|k| k * step));
                    }
                    assert_eq!(
                        distinct_instants_per_request(&ranges),
                        oracle.len() as u64,
                        "{ranges:?}"
                    );
                }
            }
        }
    }
    let large = 1u64 << 63;
    assert_eq!(
        offset_intersection_count(&[(large - 1, large), (large - 3, large)]),
        1
    );
}

/// One workload entry per recurrence, each with `range_evaluation`.
fn entries(
    recurrence: &[QueryRecurrence],
    range_evaluation: Option<RangeEvaluation>,
) -> Vec<QueryWorkloadEntry> {
    recurrence
        .iter()
        .map(|recurrence| QueryWorkloadEntry {
            query: asap_types::workload::Query("q".into()),
            requirements: Default::default(),
            predictability: Default::default(),
            recurrence: recurrence.clone(),
            range_evaluation,
        })
        .collect()
}

/// A plan node `id` running `payload` with `timing` and `lifetime`.
fn node(
    id: PhysicalASAPNodeId,
    payload: Operator<PhysicalASAPNodeId>,
    timing: ExecutionTiming,
    lifetime: Lifetime,
) -> asap_types::plan::physical_asap_dag::PhysicalASAPDAGNode {
    use asap_types::ir::properties::{DataPrimitive, ExecutionDataState};
    use asap_types::ir::schema::{DataType, Field, FieldDataType, Schema};
    asap_types::plan::physical_asap_dag::PhysicalASAPDAGNode {
        id,
        output_state: ExecutionDataState {
            timing,
            primitive: DataPrimitive::of(&payload, asap_types::ir::OperatorResultKind::RangeVector),
        },
        parameters: matches!(payload, Operator::ASAP(_))
            .then_some(asap_types::summary_type::SketchParams::Kll { k: 200 }),
        payload,
        lifetime,
        output_schema: Schema::with_time_index(
            vec![
                Field::plain("ts", DataType::Timestamp, false),
                Field::new("value", FieldDataType::Plain(DataType::Float64), false),
            ],
            0,
            vec![],
        ),
        retention_ms: None,
        selected_physical_candidate: None,
        view_coverage: None,
    }
}

fn scan() -> Operator<PhysicalASAPNodeId> {
    use asap_types::ir::operator::Source;
    use asap_types::ir::schema::{DataType, Field, Schema};
    Operator::NonASAP(asap_types::ir::NonASAPOp::Scan {
        source: Source::TimeSeries { metric: "m".into() },
        predicates: vec![],
        schema: Schema::with_time_index(
            vec![
                Field::plain("ts", DataType::Timestamp, false),
                Field::plain("value", DataType::Float64, false),
            ],
            0,
            vec![],
        ),
    })
}

fn last(seconds: u64, child: PhysicalASAPNodeId) -> Operator<PhysicalASAPNodeId> {
    Operator::NonASAP(asap_types::ir::NonASAPOp::TimeRange {
        range: std::time::Duration::from_secs(seconds),
        kind: asap_types::ir::TimeRangeKind::Range,
        child,
    })
}

/// `m[5m]` read by three queries: two every 10 s at phase 0, one every
/// 30 s. The two on the 10 s grid are one execution context, which
/// computes the not-materialized range and scan once for both: two
/// contexts, 1/10 + 1/30 per second.
#[test]
fn a_not_materialized_node_is_computed_once_per_evaluation_grid() {
    let query_time = ExecutionTiming::QueryTime;
    let dag = PhysicalASAPDAG::new(
        vec![
            node(0, scan(), query_time, Lifetime::None),
            node(1, last(300, 0), query_time, Lifetime::None),
        ],
        vec![1, 1, 1],
        vec![],
    )
    .unwrap();
    let recurrence = [fixed(10_000, 0), fixed(10_000, 0), fixed(30_000, 0)];
    let rates = computations(&dag, &entries(&recurrence, None), 3600.0).unwrap();
    for id in [0, 1] {
        assert_eq!(rates[&id].0, 2);
        close(rates[&id].1, 1.0 / 10.0 + 1.0 / 30.0);
    }
}

/// `max_over_time(m[5m] @ end())` requested every 60 s over the last
/// hour at a 10 s step (361 steps per request): the shift runs at
/// every step, its input (the range and the scan) once per request,
/// at the request end: 1/60 per second, not 361/60.
#[test]
fn a_range_query_computes_its_at_end_subtree_once_per_request() {
    use asap_types::ir::operator::TimeShift;
    let query_time = ExecutionTiming::QueryTime;
    let at_end = Operator::NonASAP(NonASAPOp::TimeShift {
        shift: TimeShift {
            offset_ms: 0,
            at: Some(AtModifier::End),
        },
        child: 1,
    });
    let dag = PhysicalASAPDAG::new(
        vec![
            node(0, scan(), query_time, Lifetime::None),
            node(1, last(300, 0), query_time, Lifetime::None),
            node(2, at_end, query_time, Lifetime::None),
        ],
        vec![2],
        vec![],
    )
    .unwrap();
    let hour = RangeEvaluation {
        step_ms: 10_000,
        span_ms: 3_600_000,
    };
    let rates = computations(&dag, &entries(&[fixed(60_000, 0)], Some(hour)), 3600.0).unwrap();
    close(rates[&2].1, 361.0 / 60.0);
    for id in [0, 1] {
        assert_eq!(rates[&id].0, 1);
        close(rates[&id].1, 1.0 / 60.0);
    }
}

// Invalid fixed intervals and estimated rates still produce the evaluator's error.
#[test]
fn invalid_recurrence_is_rejected() {
    assert!(matches!(
        evaluation_rate([(&fixed(0, 0), None)], 3600.0),
        Err(AnalyticalCostError::InvalidRecurrence)
    ));
    let invalid = QueryRecurrence::Repeated(RepeatedDemand::EstimatedRate(DemandEstimate {
        expected_rate: Rate(-1.0),
        observed_at: None,
        valid_for: None,
    }));
    assert!(matches!(
        evaluation_rate([(&invalid, None)], 3600.0),
        Err(AnalyticalCostError::InvalidRecurrence)
    ));
}

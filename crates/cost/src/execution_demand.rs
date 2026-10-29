//! Execution demand shared by DAG evaluation and candidate-connection selection.
//!
//! This preserves the configured accounting policy, not an exact timestamp
//! union: different fixed intervals add, unknown phases join a known phase,
//! estimated rates add, and one-off/scheduled demand uses a batch maximum.
//! Call `evaluation_rate` on the chosen reader subset only; it does not enumerate
//! subsets. For retained query-time state pass `None` for each range evaluation
//! to price once per request, as `computations` does.

use super::analytical::AnalyticalCostError;
use asap_types::ir::physical_export::{PhysicalASAPDAG, PhysicalASAPNodeId};
use asap_types::physical::Materialization;
use asap_types::workload::{QueryRecurrence, RangeEvaluation, RepeatedDemand};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

/// Evaluations per second of a node computed once for `queries` together,
/// each a recurrence with its range evaluation: a request with a range
/// evaluation is [`RangeEvaluation::evaluations_per_request`] evaluations,
/// one without it one. Repeating queries with equal intervals and equal
/// phases on them are evaluated together, every request ending at the
/// same instant: a node they share is computed once per distinct instant
/// their steps fall on, `end − k · step_ms` for `k · step_ms ≤ span_ms`. A
/// query of unknown phase runs with the first known phase on its interval,
/// if any. A query evaluated once per request (no range, or one step)
/// evaluates only at the end. Queries on different intervals or phases add
/// (an over-estimate for intervals whose instants coincide, e.g. every 20 s
/// and every 30 s meet every 60 s); estimated rates add. One-off, scheduled and unknown queries run as one
/// batch: the most evaluations among them, spread over
/// `non_repeating_query_horizon_seconds`.
pub fn evaluation_rate<'r>(
    queries: impl IntoIterator<Item = (&'r QueryRecurrence, Option<RangeEvaluation>)>,
    non_repeating_query_horizon_seconds: f64,
) -> Result<f64, AnalyticalCostError> {
    // Per interval in ms and phase in ms on it (`None`: unknown), each step
    // in ms its readers use and the longest span in ms at that step; once
    // per request is the interval with span 0.
    let mut longest_span_ms_by_step_ms_by_schedule: BTreeMap<
        (u32, Option<u64>),
        BTreeMap<u64, u64>,
    > = BTreeMap::new();
    let mut estimated = 0.0;
    let mut invocations = 0u64;
    for (recurrence, range_evaluation) in queries {
        let steps = range_evaluation.map_or(1, |range| range.evaluations_per_request());
        match recurrence {
            QueryRecurrence::Repeated(
                RepeatedDemand::FixedInterval(interval)
                | RepeatedDemand::FixedIntervalAt { interval, .. },
            ) => {
                if interval.0 == 0 {
                    return Err(AnalyticalCostError::InvalidRecurrence);
                }
                let phase_ms = match recurrence {
                    QueryRecurrence::Repeated(RepeatedDemand::FixedIntervalAt {
                        evaluation_phase,
                        ..
                    }) => Some(evaluation_phase.0 % u64::from(interval.0)),
                    _ => None,
                };
                let (step_ms, span_ms) = range_evaluation
                    .filter(|_| steps > 1)
                    .map_or((u64::from(interval.0), 0), |range| {
                        (range.step_ms, range.span_ms)
                    });
                let longest_span_ms = longest_span_ms_by_step_ms_by_schedule
                    .entry((interval.0, phase_ms))
                    .or_default()
                    .entry(step_ms)
                    .or_default();
                *longest_span_ms = (*longest_span_ms).max(span_ms);
            }
            QueryRecurrence::Repeated(RepeatedDemand::EstimatedRate(estimate)) => {
                let rate = estimate.expected_rate.0;
                if !rate.is_finite() || rate < 0.0 {
                    return Err(AnalyticalCostError::InvalidRecurrence);
                }
                estimated += rate * steps as f64;
            }
            QueryRecurrence::Repeated(RepeatedDemand::Scheduled(times)) => {
                invocations = invocations.max(times.len() as u64 * steps);
            }
            QueryRecurrence::OneTime {
                invocations: count, ..
            } => invocations = invocations.max(*count * steps),
            QueryRecurrence::Unknown => invocations = invocations.max(steps),
        }
    }
    // Queries of unknown phase run with the first known phase on their
    // interval.
    let unknown_phase_intervals_ms: Vec<u32> = longest_span_ms_by_step_ms_by_schedule
        .keys()
        .filter(|(_, phase_ms)| phase_ms.is_none())
        .map(|&(interval_ms, _)| interval_ms)
        .collect();
    for interval_ms in unknown_phase_intervals_ms {
        let known = longest_span_ms_by_step_ms_by_schedule
            .range((interval_ms, Some(0))..=(interval_ms, Some(u64::MAX)))
            .next()
            .map(|(&schedule, _)| schedule);
        if let Some(known) = known {
            let unknown = longest_span_ms_by_step_ms_by_schedule
                .remove(&(interval_ms, None))
                .unwrap_or_default();
            let with = longest_span_ms_by_step_ms_by_schedule
                .get_mut(&known)
                .expect("found above");
            for (step_ms, span_ms) in unknown {
                let longest_span_ms = with.entry(step_ms).or_default();
                *longest_span_ms = (*longest_span_ms).max(span_ms);
            }
        }
    }
    let repeated: f64 = longest_span_ms_by_step_ms_by_schedule
        .into_iter()
        .map(|((interval_ms, _), longest_span_ms_by_step_ms)| {
            1_000.0 / f64::from(interval_ms)
                * distinct_instants_per_request(&longest_span_ms_by_step_ms) as f64
        })
        .sum();
    Ok(repeated + estimated + invocations as f64 / non_repeating_query_horizon_seconds)
}

/// Number of offsets common to nonempty arithmetic progressions
/// `{k * step | k * step <= span}`. A checked LCM beyond the shortest span
/// means only offset zero is shared. Kept in costing so both evaluator and
/// MILP demand atoms use the same integer arithmetic without layer inversion.
pub fn offset_intersection_count(geometries: &[(u64, u64)]) -> i128 {
    fn gcd(mut a: u64, mut b: u64) -> u64 {
        while b != 0 {
            let remainder = a % b;
            a = b;
            b = remainder;
        }
        a
    }
    let Some(span) = geometries.iter().map(|(_, span)| *span).min() else {
        return 0;
    };
    let mut lcm = 1u64;
    for &(step, _) in geometries {
        let step = step.max(1);
        match (lcm / gcd(lcm, step)).checked_mul(step) {
            Some(next) if next <= span => lcm = next,
            _ => return 1,
        }
    }
    i128::from(span / lcm) + 1
}

/// The distinct instants one request evaluates at, for steps in ms each with
/// the longest span in ms at that step: the union of the offsets
/// `k · step_ms ≤ span_ms` back from the request's end. One step needs no
/// union: `span_ms / step_ms + 1`.
fn distinct_instants_per_request(longest_span_ms_by_step_ms: &BTreeMap<u64, u64>) -> u64 {
    if longest_span_ms_by_step_ms.len() == 1 {
        let (&step_ms, &span_ms) = longest_span_ms_by_step_ms.iter().next().expect("one step");
        return span_ms / step_ms.max(1) + 1;
    }
    // Inclusion-exclusion is bounded by 4095 intersections for up to twelve
    // distinct steps, regardless of the number of evaluations in each range.
    if longest_span_ms_by_step_ms.len() <= 12 {
        let geometries: Vec<_> = longest_span_ms_by_step_ms
            .iter()
            .map(|(&step, &span)| (step, span))
            .collect();
        let mut total = 0i128;
        for mask in 1usize..(1usize << geometries.len()) {
            let intersection: Vec<_> = geometries
                .iter()
                .enumerate()
                .filter(|(bit, _)| mask & (1 << bit) != 0)
                .map(|(_, geometry)| *geometry)
                .collect();
            let count = offset_intersection_count(&intersection);
            if mask.count_ones() % 2 == 1 {
                total += count;
            } else {
                total -= count;
            }
        }
        return u64::try_from(total).expect("distinct evaluation offset count fits u64");
    }
    let mut offsets_ms = BTreeSet::new();
    for (&step_ms, &span_ms) in longest_span_ms_by_step_ms {
        offsets_ms.extend((0..=span_ms / step_ms.max(1)).map(|k| k * step_ms));
    }
    offsets_ms.len() as u64
}

/// The nodes each query reads first, in query order: query `i` is the scalar
/// root keyed `i` when there is one, else the next operator root.
pub fn query_starts(dag: &PhysicalASAPDAG) -> Vec<Vec<PhysicalASAPNodeId>> {
    let mut roots = dag.roots.iter();
    (0..dag.roots.len() + dag.scalar_roots.len())
        .map(|query| {
            match dag
                .scalar_roots
                .iter()
                .find(|(position, _)| *position == query)
            {
                Some((_, expr)) => expr.operator_refs().into_iter().copied().collect(),
                None => roots.next().into_iter().copied().collect(),
            }
        })
        .collect()
}

/// How often each node of `dag` is computed: `(multiplier, per_second)`
/// (module doc, "Cost model"). A materialized node is computed once for all
/// its readers: once per second at ingestion time (`MatAtIngestion`), at
/// the combined rate of the queries reaching it (`MatAtQuery`,
/// [`evaluation_rate`]), or once per request of those queries for a chain
/// kept across evaluations, whose request builds only its newest pane
/// however many range steps it evaluates. `range_evaluations[q]` is query
/// `q`'s range evaluation (none when missing). A `NotMaterialized` node is computed once per
/// consumer that reaches it, at that consumer's rate. A consumer is a query
/// (operator or scalar root), at its own rate, or a materialized node, at
/// that node's rate; a walk stops at the first materialized node it meets,
/// and a node read several times within one consumer's walk counts once for
/// it. Its `multiplier` is that number of consumers.
pub fn computations(
    dag: &PhysicalASAPDAG,
    recurrence: &[QueryRecurrence],
    range_evaluations: &[Option<RangeEvaluation>],
    non_repeating_query_horizon_seconds: f64,
    root_rates: Option<&[f64]>,
) -> Result<HashMap<PhysicalASAPNodeId, (u64, f64)>, AnalyticalCostError> {
    let range_of = |query: usize| range_evaluations.get(query).copied().flatten();
    let materialized: HashMap<_, _> = dag
        .nodes
        .iter()
        .filter(|n| n.materialization != Materialization::NotMaterialized)
        .map(|n| (n.id, n.materialization))
        .collect();
    let inputs = |id: PhysicalASAPNodeId| {
        dag.edges
            .iter()
            .filter(move |e| e.consumer == id)
            .map(|e| e.producer)
    };
    let starts = query_starts(dag);
    // The queries reaching each node, through any node.
    let mut reached: HashMap<PhysicalASAPNodeId, Vec<usize>> = HashMap::new();
    for (query, start) in starts.iter().enumerate() {
        let mut stack = start.clone();
        while let Some(id) = stack.pop() {
            let queries = reached.entry(id).or_default();
            if queries.last() == Some(&query) {
                continue;
            }
            queries.push(query);
            stack.extend(inputs(id));
        }
    }
    let mut rate_of_materialized = HashMap::new();
    for (&id, materialization) in &materialized {
        // A chain kept across evaluations is built once per request.
        let per_request = matches!(
            materialization,
            Materialization::MatAtQueryKeptAcrossEvaluations(_)
        );
        let rate = match materialization {
            Materialization::MatAtIngestion(_) => 1.0,
            _ => evaluation_rate(
                reached.get(&id).into_iter().flatten().filter_map(|&q| {
                    let range = if per_request { None } else { range_of(q) };
                    recurrence.get(q).map(|r| (r, range))
                }),
                non_repeating_query_horizon_seconds,
            )?,
        };
        rate_of_materialized.insert(id, rate);
    }
    // Each consumer's starting nodes and its rate.
    let mut consumers = Vec::new();
    for (query, start) in starts.into_iter().enumerate() {
        let rate = match root_rates.and_then(|rates| rates.get(query)) {
            Some(&rate) => rate,
            None => evaluation_rate(
                recurrence.get(query).map(|r| (r, range_of(query))),
                non_repeating_query_horizon_seconds,
            )?,
        };
        consumers.push((start, rate));
    }
    for (&id, &rate) in &rate_of_materialized {
        consumers.push((inputs(id).collect(), rate));
    }
    let mut count: HashMap<_, (u64, f64)> = HashMap::new();
    for (start, rate) in consumers {
        let mut seen = HashSet::new();
        let mut stack: Vec<PhysicalASAPNodeId> = start;
        while let Some(id) = stack.pop() {
            if materialized.contains_key(&id) || !seen.insert(id) {
                continue;
            }
            let entry = count.entry(id).or_default();
            entry.0 += 1;
            entry.1 += rate;
            stack.extend(inputs(id));
        }
    }
    Ok(dag
        .nodes
        .iter()
        .map(|n| {
            let times = match rate_of_materialized.get(&n.id) {
                Some(&rate) => (1, rate),
                None => count.get(&n.id).copied().unwrap_or((0, 0.0)),
            };
            (n.id, times)
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use asap_types::workload::{DemandEstimate, Rate, RepetitionInterval, TimestampMs};

    fn fixed(interval: u32, phase: Option<u64>) -> QueryRecurrence {
        QueryRecurrence::Repeated(match phase {
            Some(phase) => RepeatedDemand::FixedIntervalAt {
                interval: RepetitionInterval(interval),
                evaluation_phase: TimestampMs(phase),
            },
            None => RepeatedDemand::FixedInterval(RepetitionInterval(interval)),
        })
    }

    fn close(actual: f64, expected: f64) {
        assert!((actual - expected).abs() < 1e-12, "{actual} != {expected}");
    }

    // Unknown phases join the first known phase; separate known phases add.
    #[test]
    fn chosen_readers_preserve_phase_accounting() {
        let readers = [
            fixed(20_000, None),
            fixed(20_000, Some(5_000)),
            fixed(20_000, Some(0)),
        ];
        close(
            evaluation_rate(readers[..2].iter().map(|r| (r, None)), 3600.0).unwrap(),
            1.0 / 20.0,
        );
        close(
            evaluation_rate(readers.iter().map(|r| (r, None)), 3600.0).unwrap(),
            2.0 / 20.0,
        );
        // No global subset cache: each caller's selected readers determine the rate.
        close(evaluation_rate(std::iter::empty(), 3600.0).unwrap(), 0.0);
    }

    // Range offsets union within one cadence; retained state instead uses requests.
    #[test]
    fn range_offsets_and_request_rates_stay_distinct() {
        let recurrence = fixed(60_000, Some(0));
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
        let readers = [fixed(20_000, Some(0)), fixed(30_000, Some(0))];
        close(
            evaluation_rate(readers.iter().map(|r| (r, None)), 3600.0).unwrap(),
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
        let recurrence = fixed(1000, None);
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
                        for (step, span) in
                            [(left_step, left_span), (right_step, right_span), (11, 29)]
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

    // Invalid fixed intervals and estimated rates still produce the evaluator's error.
    #[test]
    fn invalid_recurrence_is_rejected() {
        assert!(matches!(
            evaluation_rate([(&fixed(0, None), None)], 3600.0),
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
}

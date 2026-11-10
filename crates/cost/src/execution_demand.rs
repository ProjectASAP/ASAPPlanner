//! Execution demand shared by DAG evaluation and candidate-connection selection.
//!
//! This preserves the configured accounting policy, not an exact timestamp
//! union: different fixed intervals add, estimated rates add, and one-off/scheduled demand uses a batch maximum.
//! Call `schedule_groups` or `evaluation_rate` on the chosen query subset
//! only; neither enumerates subsets. For retained query-time state pass `None` for each range evaluation
//! to cost once per request, as `computations` does.

use super::analytical::AnalyticalCostError;
use asap_types::ir::operator::AtModifier;
use asap_types::ir::physical_export::{PhysicalASAPDAG, PhysicalASAPNodeId};
use asap_types::ir::properties::ExecutionTiming;
use asap_types::ir::{ASAPOp, NonASAPOp, Operator};
use asap_types::physical::Lifetime;
use asap_types::workload::{
    greatest_common_divisor, QueryRecurrence, RangeEvaluation, RepeatedDemand,
};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

/// How a [`ScheduleGroup`]'s evaluations add to the others'.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Schedule {
    /// Repeating queries on one interval and phase: evaluated together,
    /// once per distinct instant their steps fall on.
    FixedInterval,
    /// One query with an estimated rate: its rate times its steps.
    EstimatedRate,
    /// The one-off, scheduled and unknown queries, run as one batch: the
    /// most evaluations among them, spread over the horizon.
    NonRepeating,
}

/// Queries of one node that are evaluated together, and how often the node
/// is computed for them. Groups add ([`evaluation_rate`]).
#[derive(Debug, Clone, PartialEq)]
pub struct ScheduleGroup {
    /// The queries in the group, as positions in the query list given to
    /// [`schedule_groups`], ascending.
    pub queries: Vec<usize>,
    /// How often the node is computed per second for these queries.
    pub evaluations_per_second: f64,
    /// Which kind of group it is.
    pub schedule: Schedule,
}

/// One fixed (interval, phase) schedule's queries: each step in ms they
/// use with the longest span in ms at that step, and the queries.
type FixedSchedule = (BTreeMap<u64, u64>, Vec<usize>);

/// The groups of `queries` evaluated together (each a recurrence with its
/// range evaluation), in the order [`evaluation_rate`] adds them: fixed
/// intervals by (interval, phase), then each estimated rate, then the
/// non-repeating batch. A request with a range evaluation is
/// [`RangeEvaluation::evaluations_per_request`] evaluations, one without it
/// one. Repeating queries with equal intervals and equal phases form one
/// group, every request ending at the same instant: a node they share is
/// computed once per distinct instant their steps fall on,
/// `end − k · step_ms` for `k · step_ms ≤ span_ms`. A query evaluated once per request (no range, or one step) evaluates
/// only at the end. Different intervals or phases are different groups (an
/// over-estimate for intervals whose instants coincide, e.g. every 20 s and
/// every 30 s meet every 60 s); each estimated rate is its own group.
/// One-off, scheduled and unknown queries form one batch group: the most
/// evaluations among them, spread over `non_repeating_query_horizon_seconds`.
pub fn schedule_groups<'r>(
    queries: impl IntoIterator<Item = (&'r QueryRecurrence, Option<RangeEvaluation>)>,
    non_repeating_query_horizon_seconds: f64,
) -> Result<Vec<ScheduleGroup>, AnalyticalCostError> {
    // Per interval in ms and phase in ms on it, each step
    // in ms its queries use and the longest span in ms at that step, with
    // the queries; once per request is the interval with span 0.
    let mut by_schedule: BTreeMap<(u32, u64), FixedSchedule> = BTreeMap::new();
    let mut estimated = Vec::new();
    let mut invocations = 0u64;
    let mut non_repeating = Vec::new();
    for (query, (recurrence, range_evaluation)) in queries.into_iter().enumerate() {
        let steps = range_evaluation.map_or(1, |range| range.evaluations_per_request());
        match recurrence {
            QueryRecurrence::Repeated(RepeatedDemand::FixedIntervalAt {
                interval,
                evaluation_phase,
            }) => {
                if interval.0 == 0 {
                    return Err(AnalyticalCostError::InvalidRecurrence);
                }
                let phase_ms = evaluation_phase.0 % u64::from(interval.0);
                let (step_ms, span_ms) = range_evaluation
                    .filter(|_| steps > 1)
                    .map_or((u64::from(interval.0), 0), |range| {
                        (range.step_ms, range.span_ms)
                    });
                let (longest_span_ms_by_step_ms, members) =
                    by_schedule.entry((interval.0, phase_ms)).or_default();
                let longest_span_ms = longest_span_ms_by_step_ms.entry(step_ms).or_default();
                *longest_span_ms = (*longest_span_ms).max(span_ms);
                members.push(query);
            }
            QueryRecurrence::Repeated(RepeatedDemand::EstimatedRate(estimate)) => {
                let rate = estimate.expected_rate.0;
                if !rate.is_finite() || rate < 0.0 {
                    return Err(AnalyticalCostError::InvalidRecurrence);
                }
                estimated.push(ScheduleGroup {
                    queries: vec![query],
                    evaluations_per_second: rate * steps as f64,
                    schedule: Schedule::EstimatedRate,
                });
            }
            QueryRecurrence::Repeated(RepeatedDemand::Scheduled(times)) => {
                invocations = invocations.max(times.len() as u64 * steps);
                non_repeating.push(query);
            }
            QueryRecurrence::OneTime {
                invocations: count, ..
            } => {
                invocations = invocations.max(*count * steps);
                non_repeating.push(query);
            }
            QueryRecurrence::Unknown => {
                invocations = invocations.max(steps);
                non_repeating.push(query);
            }
        }
    }
    let mut groups: Vec<ScheduleGroup> = by_schedule
        .into_iter()
        .map(
            |((interval_ms, _), (longest_span_ms_by_step_ms, queries))| ScheduleGroup {
                queries,
                evaluations_per_second: 1_000.0 / f64::from(interval_ms)
                    * distinct_instants_per_request(&longest_span_ms_by_step_ms) as f64,
                schedule: Schedule::FixedInterval,
            },
        )
        .collect();
    groups.extend(estimated);
    if !non_repeating.is_empty() {
        groups.push(ScheduleGroup {
            queries: non_repeating,
            evaluations_per_second: invocations as f64 / non_repeating_query_horizon_seconds,
            schedule: Schedule::NonRepeating,
        });
    }
    Ok(groups)
}

/// Evaluations per second of a node computed once for `queries` together:
/// the sum of their [`schedule_groups`].
pub fn evaluation_rate<'r>(
    queries: impl IntoIterator<Item = (&'r QueryRecurrence, Option<RangeEvaluation>)>,
    non_repeating_query_horizon_seconds: f64,
) -> Result<f64, AnalyticalCostError> {
    let groups = schedule_groups(queries, non_repeating_query_horizon_seconds)?;
    let rates = |schedule| {
        groups
            .iter()
            .filter(move |group| group.schedule == schedule)
            .map(|group| group.evaluations_per_second)
    };
    // Added as fixed intervals, estimated rates and the batch, each kind
    // summed on its own first.
    let repeated: f64 = rates(Schedule::FixedInterval).sum();
    let mut estimated = 0.0;
    for rate in rates(Schedule::EstimatedRate) {
        estimated += rate;
    }
    let batch = rates(Schedule::NonRepeating).next().unwrap_or(0.0);
    Ok(repeated + estimated + batch)
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

/// What computes a not-materialized node once per its own execution
/// ([`consumers`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Consumer {
    /// Query `.0` (an operator or scalar root), at its own rate.
    Query(usize),
    /// Query `query` below a `TimeShift` anchored with `@ end()` or (when
    /// `at_start`) `@ start()`: once per request, on the grid of its
    /// request end (start) times ([`request_time_grid_ms`]).
    QueryRequest { query: usize, at_start: bool },
    /// A materialized node, at that node's rate.
    Materialized(PhysicalASAPNodeId),
}

/// The consumers of each not-materialized node of `dag` that any consumer
/// reaches ([`computations`]): the queries and the materialized nodes whose
/// walk down their inputs reaches it before meeting a materialized node,
/// each once; the queries first, in order, then the materialized nodes in
/// `dag`'s node order.
pub fn consumers(dag: &PhysicalASAPDAG) -> HashMap<PhysicalASAPNodeId, Vec<Consumer>> {
    let materialized_nodes: Vec<PhysicalASAPNodeId> = dag
        .nodes
        .iter()
        .filter(|n| n.lifetime != Lifetime::None)
        .map(|n| n.id)
        .collect();
    let is_materialized: HashSet<PhysicalASAPNodeId> = materialized_nodes.iter().copied().collect();
    let inputs = |id: PhysicalASAPNodeId| {
        dag.edges
            .iter()
            .filter(move |e| e.consumer == id)
            .map(|e| e.producer)
    };
    // `Some(at_start)` for a `TimeShift` anchored with `@ end()` or
    // `@ start()`.
    let request_anchor = |id: PhysicalASAPNodeId| {
        dag.nodes
            .iter()
            .find(|node| node.id == id)
            .and_then(|node| match &node.payload {
                Operator::NonASAP(NonASAPOp::TimeShift { shift, .. }) => match shift.at {
                    Some(AtModifier::End) => Some(false),
                    Some(AtModifier::Start) => Some(true),
                    _ => None,
                },
                _ => None,
            })
    };
    let walks = query_starts(dag)
        .into_iter()
        .enumerate()
        .map(|(query, start)| (Consumer::Query(query), start))
        .chain(
            materialized_nodes
                .iter()
                .map(|&id| (Consumer::Materialized(id), inputs(id).collect())),
        );
    let mut consumers: HashMap<_, Vec<Consumer>> = HashMap::new();
    for (consumer, start) in walks {
        let mut seen = HashSet::new();
        let mut stack: Vec<(PhysicalASAPNodeId, Consumer)> =
            start.into_iter().map(|id| (id, consumer)).collect();
        while let Some((id, consumer)) = stack.pop() {
            if is_materialized.contains(&id) || !seen.insert((id, consumer)) {
                continue;
            }
            consumers.entry(id).or_default().push(consumer);
            // A query's input of an `@ end()` / `@ start()` shift runs once
            // per request.
            let below = match (consumer, request_anchor(id)) {
                (Consumer::Query(query), Some(at_start)) => {
                    Consumer::QueryRequest { query, at_start }
                }
                (consumer, _) => consumer,
            };
            stack.extend(inputs(id).map(|input| (input, below)));
        }
    }
    consumers
}

/// The grid, in ms, of the times the requests of a query with `recurrence`
/// and `range_evaluation` anchor `@ end()` or `@ start()` (`at`) at: its
/// request end times, shifted back by its range-evaluation span for
/// `@ start()`; 1 ms when not known.
pub fn request_time_grid_ms(
    recurrence: &QueryRecurrence,
    range_evaluation: Option<RangeEvaluation>,
    at: AtModifier,
) -> u64 {
    let end_ms = recurrence.evaluation_grid_ms(None).unwrap_or(1);
    match (at, range_evaluation) {
        (AtModifier::Start, Some(range)) => greatest_common_divisor(end_ms, range.span_ms),
        _ => end_ms,
    }
}

/// How often each node of `dag` is computed: `(multiplier, per_second)`
/// (module doc, "Cost model"; the execution contexts of the concepts doc,
/// section 5). A materialized node is computed once for all its queries:
/// once per second at ingestion time (materialized at ingestion time), at
/// the combined rate of the queries reaching it (`OneEvaluation`,
/// [`evaluation_rate`]), once per tumbling window for a query-time chain
/// materialized across evaluations, which builds each window when it ends,
/// or once per request of those queries for another query-time result
/// materialized across evaluations. `range_evaluations[q]` is query `q`'s
/// range evaluation (none when missing). A not-materialized node is
/// computed once per execution context of the consumers that reach it
/// ([`consumers`]). A consumer is a query (operator or scalar root) or a
/// materialized node; a walk stops at the first materialized node it meets,
/// and a node read several times within one consumer's walk counts once for
/// it. The queries on one evaluation grid
/// (`QueryRecurrence::evaluation_grid_ms` of their range evaluation; 1 ms
/// when not known) are one context, which computes the node at their
/// combined rate ([`evaluation_rate`]); a materialized node is a context of
/// its own, at its rate. The consumers materialized at ingestion time of one refresh window
/// ([`PhysicalASAPDAG::refresh_window`]) are refreshed in one ingestion run,
/// which computes the node once for all of them: they are one context (a
/// consumer with an unbounded window is one alone). Its `multiplier` is
/// that number of contexts.
pub fn computations(
    dag: &PhysicalASAPDAG,
    recurrence: &[QueryRecurrence],
    range_evaluations: &[Option<RangeEvaluation>],
    non_repeating_query_horizon_seconds: f64,
    root_rates: Option<&[f64]>,
) -> Result<HashMap<PhysicalASAPNodeId, (u64, f64)>, AnalyticalCostError> {
    let range_of = |query: usize| range_evaluations.get(query).copied().flatten();
    // Each materialized node's timing and lifetime.
    let materialized_nodes: HashMap<PhysicalASAPNodeId, (ExecutionTiming, Lifetime)> = dag
        .nodes
        .iter()
        .filter(|node| node.lifetime != Lifetime::None)
        .map(|node| (node.id, (node.output_state.timing, node.lifetime)))
        .collect();
    // Each tumbling-window chain's width.
    let chain_width_ms: HashMap<PhysicalASAPNodeId, u64> = dag
        .nodes
        .iter()
        .filter_map(|node| match node.payload {
            Operator::ASAP(ASAPOp::SummaryTumblingWindowChain {
                tumbling_window_width_ms,
                ..
            }) => Some((node.id, tumbling_window_width_ms)),
            _ => None,
        })
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
    for (&id, &(timing, lifetime)) in &materialized_nodes {
        let at_ingestion = timing == ExecutionTiming::IngestionTime;
        // A query-time result materialized across evaluations is computed
        // once per request; a query-time chain builds one window when it ends.
        let per_request = !at_ingestion && matches!(lifetime, Lifetime::AcrossEvaluations(_));
        let rate = match (at_ingestion, chain_width_ms.get(&id)) {
            (true, _) => 1.0,
            (false, Some(&width_ms)) if per_request => 1_000.0 / width_ms.max(1) as f64,
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
    // Each query consumer's rate.
    let mut query_rates = Vec::new();
    for query in 0..starts.len() {
        query_rates.push(match root_rates.and_then(|rates| rates.get(query)) {
            Some(&rate) => rate,
            None => evaluation_rate(
                recurrence.get(query).map(|r| (r, range_of(query))),
                non_repeating_query_horizon_seconds,
            )?,
        });
    }
    // The refresh window of each node refreshed at ingestion.
    let refresh_windows = dag.refresh_windows(
        materialized_nodes
            .iter()
            .filter(|(_, (timing, _))| *timing == ExecutionTiming::IngestionTime)
            .map(|(&id, _)| id),
    );
    // Each query's evaluation grid: the queries on one grid are one
    // execution context, evaluated together.
    let grid_ms = |query: usize| {
        recurrence
            .get(query)
            .and_then(|r| r.evaluation_grid_ms(range_of(query)))
            .unwrap_or(1)
    };
    let mut count: HashMap<_, (u64, f64)> = HashMap::new();
    for (id, of_node) in consumers(dag) {
        let entry = count.entry(id).or_default();
        // The refresh windows whose ingestion run already computes the node.
        let mut runs = HashSet::new();
        // The queries reaching it, by evaluation grid: each evaluated at
        // its steps, or once per request (below an `@ end()` / `@ start()`
        // shift) when it reaches the node only so on that grid.
        let mut queries_by_grid: BTreeMap<u64, BTreeMap<usize, bool>> = BTreeMap::new();
        for consumer in of_node {
            match consumer {
                Consumer::Query(query) => {
                    queries_by_grid
                        .entry(grid_ms(query))
                        .or_default()
                        .insert(query, false);
                }
                Consumer::QueryRequest { query, at_start } => {
                    let at = match at_start {
                        true => AtModifier::Start,
                        false => AtModifier::End,
                    };
                    let grid = recurrence
                        .get(query)
                        .map_or(1, |r| request_time_grid_ms(r, range_of(query), at));
                    queries_by_grid
                        .entry(grid)
                        .or_default()
                        .entry(query)
                        .or_insert(true);
                }
                Consumer::Materialized(node) => {
                    if let Some(Some(window)) = refresh_windows.get(&node) {
                        if !runs.insert(*window) {
                            continue;
                        }
                    }
                    entry.0 += 1;
                    entry.1 += rate_of_materialized[&node];
                }
            }
        }
        for queries in queries_by_grid.into_values() {
            entry.0 += 1;
            entry.1 += match root_rates {
                Some(_) if queries.values().all(|per_request| !per_request) => {
                    queries.keys().map(|&query| query_rates[query]).sum()
                }
                _ => evaluation_rate(
                    queries.iter().filter_map(|(&query, &per_request)| {
                        let range = if per_request { None } else { range_of(query) };
                        recurrence.get(query).map(|r| (r, range))
                    }),
                    non_repeating_query_horizon_seconds,
                )?,
            };
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

    /// A plan node `id` running `payload` with `timing` and `lifetime`.
    fn node(
        id: PhysicalASAPNodeId,
        payload: Operator<PhysicalASAPNodeId>,
        timing: ExecutionTiming,
        lifetime: Lifetime,
    ) -> asap_types::ir::physical_export::PhysicalASAPDAGNode {
        use asap_types::ir::properties::{DataPrimitive, ExecutionDataState};
        use asap_types::ir::schema::{DataType, Field, FieldDataType, Schema};
        asap_types::ir::physical_export::PhysicalASAPDAGNode {
            id,
            output_state: ExecutionDataState {
                timing,
                primitive: DataPrimitive::of(
                    &payload,
                    asap_types::ir::OperatorResultKind::RangeVector,
                ),
            },
            parameters: matches!(payload, Operator::ASAP(_))
                .then_some(asap_types::ir::schema::SketchParams::Kll { k: 200 }),
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
            guarantee: None,
            retention_ms: None,
            summary_coverage: None,
            selected_physical_candidate: None,
        }
    }

    fn scan() -> Operator<PhysicalASAPNodeId> {
        use asap_types::ir::operator::operator_properties::Source;
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
        let rates = computations(&dag, &recurrence, &[], 3600.0, None).unwrap();
        for id in [0, 1] {
            assert_eq!(rates[&id].0, 2);
            close(rates[&id].1, 1.0 / 10.0 + 1.0 / 30.0);
        }
    }

    /// A query-time chain of 10 s windows materialized across evaluations,
    /// read through its last minute by a query every 60 s: it builds one
    /// window when each ends, 1/10 per second, and so does the scan it
    /// builds from; the range over it runs at the query's rate.
    #[test]
    fn a_query_time_chain_builds_one_window_per_width() {
        use asap_types::ir::operator::operator_properties::Reduction;
        use asap_types::ir::schema::{FieldDataType, SketchAlgorithm, SummaryUpdate};
        let query_time = ExecutionTiming::QueryTime;
        let chain = Operator::ASAP(ASAPOp::SummaryTumblingWindowChain {
            child: 0,
            summary_type: FieldDataType::Sketch(SketchAlgorithm::Kll),
            input: SummaryUpdate::column(asap_types::ir::scalar::expr_ir::ColumnRef::SampleValue),
            reduction: Reduction::PerEntity,
            filter: None,
            tumbling_window_width_ms: 10_000,
        });
        let dag = PhysicalASAPDAG::new(
            vec![
                node(0, scan(), query_time, Lifetime::None),
                node(
                    1,
                    chain,
                    query_time,
                    Lifetime::AcrossEvaluations(asap_types::physical::StorageMedium::Memory),
                ),
                node(2, last(60, 1), query_time, Lifetime::None),
            ],
            vec![2],
            vec![],
        )
        .unwrap();
        let rates = computations(&dag, &[fixed(60_000, 0)], &[], 3600.0, None).unwrap();
        close(rates[&0].1, 1.0 / 10.0);
        close(rates[&1].1, 1.0 / 10.0);
        close(rates[&2].1, 1.0 / 60.0);
    }

    /// `max_over_time(m[5m] @ end())` requested every 60 s over the last
    /// hour at a 10 s step (361 steps per request): the shift runs at
    /// every step, its input (the range and the scan) once per request,
    /// at the request end: 1/60 per second, not 361/60.
    #[test]
    fn a_range_query_computes_its_at_end_subtree_once_per_request() {
        use asap_types::ir::operator::operator_properties::TimeShift;
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
        let rates = computations(&dag, &[fixed(60_000, 0)], &[Some(hour)], 3600.0, None).unwrap();
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
}

//! Execution demand shared by DAG evaluation and candidate-connection selection.
//!
//! This preserves the configured accounting policy, not an exact timestamp
//! union: different fixed intervals add, estimated rates add, and
//! one-off/scheduled demand uses a batch maximum.
//! Call `schedule_groups` or `evaluation_rate` on the chosen query subset
//! only; neither enumerates subsets. For retained query-time state pass `None` for each range evaluation
//! to cost once per request, as `computations` does.

use crate::node::analytical::AnalyticalCostError;
use asap_types::ir::operator::AtModifier;
use asap_types::ir::properties::ExecutionTiming;
use asap_types::ir::{NonASAPOp, Operator};
use asap_types::plan::physical_asap_dag::{PhysicalASAPDAG, PhysicalASAPNodeId};
use asap_types::stages::Lifetime;
use asap_types::workload::{
    greatest_common_divisor, QueryRecurrence, QueryWorkloadEntry, RangeEvaluation, RepeatedDemand,
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
/// `end − k · step_ms` for `k · step_ms ≤ span_ms`. A query evaluated once
/// per request (no range, or one step) evaluates only at the end. Different
/// intervals or phases are different groups (an over-estimate for intervals
/// whose instants coincide, e.g. every 20 s and every 30 s meet every 60 s);
/// each estimated rate is its own group.
/// One-off, scheduled and unknown queries form one batch group: the most
/// evaluations among them, spread over `non_repeating_query_horizon_seconds`.
pub fn schedule_groups<'r>(
    queries: impl IntoIterator<Item = (&'r QueryRecurrence, Option<RangeEvaluation>)>,
    non_repeating_query_horizon_seconds: f64,
) -> Result<Vec<ScheduleGroup>, AnalyticalCostError> {
    // Per interval in ms and phase in ms on it, each step in ms its queries
    // use and the longest span in ms at that step, with the queries; once
    // per request is the interval with span 0.
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

/// How many times per second a node computed once for the queries
/// `reaching` it runs, at `timing` with `lifetime`: at ingestion time once
/// per second (each arriving row once); at query time at their combined
/// rate ([`evaluation_rate`]), a range query's steps each an evaluation,
/// except a node materialized across evaluations, which is computed once
/// per request (its newest window). Shared by [`computations`] and
/// Stage 3, which costs each candidate at the queries reaching its group.
///
/// Example: two queries every 60 s at phase 0 reaching a query-time
/// `OneEvaluation` node: 1/60 per second.
pub fn executions_per_second<'e>(
    timing: ExecutionTiming,
    lifetime: Lifetime,
    reaching: impl IntoIterator<Item = &'e QueryWorkloadEntry>,
    non_repeating_query_horizon_seconds: f64,
) -> Result<f64, AnalyticalCostError> {
    if timing == ExecutionTiming::IngestionTime {
        return Ok(1.0);
    }
    let per_request = matches!(lifetime, Lifetime::AcrossEvaluations(_));
    evaluation_rate(
        reaching.into_iter().map(|entry| {
            (
                &entry.recurrence,
                (!per_request).then_some(entry.range_evaluation).flatten(),
            )
        }),
        non_repeating_query_horizon_seconds,
    )
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
///
/// Example: a query every 60 s over the last hour: `@ end()` 60,000 ms,
/// `@ start()` gcd(60,000, 3,600,000) = 60,000 ms.
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
/// (module doc, "Cost model"). A materialized node is computed once for all
/// its queries: once per second at ingestion time (materialized at ingestion time), at
/// the combined rate of the queries reaching it (`OneEvaluation`,
/// [`evaluation_rate`]), or once per request of those queries for a chain
/// materialized across evaluations, whose request builds only its newest tumbling window
/// however many range steps it evaluates ([`executions_per_second`]).
/// `entries[q]` is query `q`'s workload entry (its recurrence and range
/// evaluation). A not-materialized node is computed once per execution
/// context of the consumers that reach it ([`consumers`]; the execution
/// contexts of the concepts doc, section 5). A consumer is a query
/// (operator or scalar root) or a materialized node; a walk stops at the
/// first materialized node it meets, and a node read several times within
/// one consumer's walk counts once for it. The queries on one evaluation
/// grid (`QueryRecurrence::evaluation_grid_ms` of their range evaluation;
/// 1 ms when not known) are one context, which computes the node at their
/// combined rate ([`evaluation_rate`]); below a `TimeShift` anchored with
/// `@ end()` or `@ start()` a query computes the node once per request, on
/// the grid of its request end (start) times ([`request_time_grid_ms`]);
/// a materialized node is a context of its own, at its rate. Its
/// `multiplier` is that number of contexts. A node computed at
/// ingestion time, materialized or not, computes each arriving row once:
/// once per second, `(1, 1.0)`, however many ingestion-time views of
/// however many refresh windows read it.
pub fn computations(
    dag: &PhysicalASAPDAG,
    entries: &[QueryWorkloadEntry],
    non_repeating_query_horizon_seconds: f64,
) -> Result<HashMap<PhysicalASAPNodeId, (u64, f64)>, AnalyticalCostError> {
    // Each materialized node's timing and lifetime.
    let materialized_nodes: HashMap<PhysicalASAPNodeId, (ExecutionTiming, Lifetime)> = dag
        .nodes
        .iter()
        .filter(|node| node.lifetime != Lifetime::None)
        .map(|node| (node.id, (node.output_state.timing, node.lifetime)))
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
        let reaching = reached
            .get(&id)
            .into_iter()
            .flatten()
            .filter_map(|&q| entries.get(q));
        let rate = executions_per_second(
            timing,
            lifetime,
            reaching,
            non_repeating_query_horizon_seconds,
        )?;
        rate_of_materialized.insert(id, rate);
    }
    // Each query's evaluation grid: the queries on one grid are one
    // execution context, evaluated together.
    let grid_ms = |query: usize| {
        entries
            .get(query)
            .and_then(|entry| entry.recurrence.evaluation_grid_ms(entry.range_evaluation))
            .unwrap_or(1)
    };
    let mut count: HashMap<_, (u64, f64)> = HashMap::new();
    for (id, of_node) in consumers(dag) {
        let entry = count.entry(id).or_default();
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
                    let grid = entries.get(query).map_or(1, |entry| {
                        request_time_grid_ms(&entry.recurrence, entry.range_evaluation, at)
                    });
                    queries_by_grid
                        .entry(grid)
                        .or_default()
                        .entry(query)
                        .or_insert(true);
                }
                Consumer::Materialized(node) => {
                    entry.0 += 1;
                    entry.1 += rate_of_materialized[&node];
                }
            }
        }
        for queries in queries_by_grid.into_values() {
            entry.0 += 1;
            entry.1 += evaluation_rate(
                queries.iter().filter_map(|(&query, &per_request)| {
                    let entry = entries.get(query)?;
                    let range = (!per_request).then_some(entry.range_evaluation).flatten();
                    Some((&entry.recurrence, range))
                }),
                non_repeating_query_horizon_seconds,
            )?;
        }
    }
    Ok(dag
        .nodes
        .iter()
        .map(|n| {
            let times = match rate_of_materialized.get(&n.id) {
                Some(&rate) => (1, rate),
                // Each arriving row is computed once at ingestion, whatever
                // its consumers.
                None if n.output_state.timing == ExecutionTiming::IngestionTime => (1, 1.0),
                None => count.get(&n.id).copied().unwrap_or((0, 0.0)),
            };
            (n.id, times)
        })
        .collect())
}

#[cfg(test)]
mod tests;

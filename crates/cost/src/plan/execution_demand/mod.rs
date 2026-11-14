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

/// What computes a node that is not materialized across evaluations once
/// per its own execution ([`consumers`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Consumer {
    /// Query `.0` (an operator or scalar root), at its own rate.
    Query(usize),
    /// Query `query` below a `TimeShift` anchored with `at` (`@ end()` or
    /// `@ start()`): once per request ([`context_executions_per_second`]).
    QueryRequest { query: usize, at: AtModifier },
    /// A node materialized across evaluations, at that node's rate.
    Materialized(PhysicalASAPNodeId),
}

/// How each query reads the nodes below it, in a plan or a memo: per node,
/// each (query, anchor) whose walk from the query's `starts` down
/// `inputs_of` reaches it, once, skipping (and stopping at) a node
/// `stops_at` keeps. Below a `TimeShift` anchored with `@ end()` or
/// `@ start()` (`anchor_of`) a query reads per request, with that anchor,
/// unless the anchor is its evaluation time ([`anchor_is_evaluation_time`]
/// over the queries on its grid that reach the shift through any node);
/// elsewhere with none. Stage 3 walks memo groups with it and
/// [`consumers`] plan nodes, so both rate a context by the queries whose
/// walk reaches it. Stage 3 walks every candidate of a group, so a query
/// reading a group through one candidate counts for the group's other
/// candidates too.
///
/// Example: an instant `sum_over_time(m[5m] @ end())` and a range
/// `sum_over_time(m[5m] @ end() offset 10m)`, both every 60 s: `m[5m]` is
/// read by the instant query with no anchor and by the range query with
/// `@ end()` (its requests' ends are not its evaluations).
pub fn query_reads<N: Copy + Eq + std::hash::Hash>(
    starts: &[Vec<N>],
    inputs_of: impl Fn(N) -> Vec<N>,
    anchor_of: impl Fn(N) -> Option<AtModifier>,
    stops_at: impl Fn(N) -> bool,
    entries: &[QueryWorkloadEntry],
) -> HashMap<N, Vec<(usize, Option<AtModifier>)>> {
    // The queries reaching each anchored shift, through any node.
    let mut reaching_shift: HashMap<N, BTreeSet<usize>> = HashMap::new();
    for (query, start) in starts.iter().enumerate() {
        let mut seen = HashSet::new();
        let mut stack = start.clone();
        while let Some(id) = stack.pop() {
            if seen.insert(id) {
                if anchor_of(id).is_some() {
                    reaching_shift.entry(id).or_default().insert(query);
                }
                stack.extend(inputs_of(id));
            }
        }
    }
    let in_evaluation_context = |shift: N, query: usize, at: AtModifier| {
        let Some(entry) = entries.get(query) else {
            return false;
        };
        let on_grid = queries_of_context(
            evaluation_grid_ms(entry),
            reaching_shift.get(&shift).into_iter().flatten().copied(),
            entries,
        );
        anchor_is_evaluation_time(at, on_grid.iter().map(|&q| &entries[q]))
    };
    let mut reads: HashMap<N, Vec<(usize, Option<AtModifier>)>> = HashMap::new();
    for (query, start) in starts.iter().enumerate() {
        let mut seen = HashSet::new();
        let mut stack: Vec<(N, Option<AtModifier>)> = start.iter().map(|&id| (id, None)).collect();
        while let Some((id, anchor)) = stack.pop() {
            if stops_at(id) || !seen.insert((id, anchor)) {
                continue;
            }
            reads.entry(id).or_default().push((query, anchor));
            let below = match (anchor, anchor_of(id)) {
                (None, Some(at)) if !in_evaluation_context(id, query, at) => Some(at),
                (anchor, _) => anchor,
            };
            stack.extend(inputs_of(id).into_iter().map(|input| (input, below)));
        }
    }
    reads
}

/// The anchor of `operator` when it is a `TimeShift` anchored with
/// `@ end()` or `@ start()` ([`query_reads`]).
pub fn request_anchor<C>(operator: &Operator<C>) -> Option<AtModifier> {
    match operator {
        Operator::NonASAP(NonASAPOp::TimeShift { shift, .. }) => match shift.at {
            Some(at @ (AtModifier::End | AtModifier::Start)) => Some(at),
            _ => None,
        },
        _ => None,
    }
}

/// The consumers of each node of `dag` not materialized across evaluations
/// that any consumer reaches ([`computations`]): the queries
/// ([`query_reads`]) and the nodes materialized across evaluations whose
/// walk down their inputs reaches it before meeting such a node, each
/// once, sorted. A `OneEvaluation` node is computed within its consumers'
/// context, so a walk goes through it.
pub fn consumers(
    dag: &PhysicalASAPDAG,
    entries: &[QueryWorkloadEntry],
) -> HashMap<PhysicalASAPNodeId, Vec<Consumer>> {
    let materialized_nodes: Vec<PhysicalASAPNodeId> = dag
        .nodes
        .iter()
        .filter(|n| matches!(n.lifetime, Lifetime::AcrossEvaluations(_)))
        .map(|n| n.id)
        .collect();
    let is_materialized: HashSet<PhysicalASAPNodeId> = materialized_nodes.iter().copied().collect();
    let inputs = |id: PhysicalASAPNodeId| -> Vec<PhysicalASAPNodeId> {
        dag.edges
            .iter()
            .filter(|e| e.consumer == id)
            .map(|e| e.producer)
            .collect()
    };
    let payload: HashMap<PhysicalASAPNodeId, &Operator<PhysicalASAPNodeId>> = dag
        .nodes
        .iter()
        .map(|node| (node.id, &node.payload))
        .collect();
    let mut consumers: HashMap<_, Vec<Consumer>> = HashMap::new();
    let queries = query_reads(
        &query_starts(dag),
        inputs,
        |id| {
            payload
                .get(&id)
                .and_then(|operator| request_anchor(operator))
        },
        |id| is_materialized.contains(&id),
        entries,
    );
    for (id, reads) in queries {
        consumers
            .entry(id)
            .or_default()
            .extend(reads.into_iter().map(|read| match read {
                (query, None) => Consumer::Query(query),
                (query, Some(at)) => Consumer::QueryRequest { query, at },
            }));
    }
    // Each node materialized across evaluations, over the nodes it reads
    // down to the next such node.
    for &materialized in &materialized_nodes {
        let mut seen = HashSet::new();
        let mut stack = inputs(materialized);
        while let Some(id) = stack.pop() {
            if is_materialized.contains(&id) || !seen.insert(id) {
                continue;
            }
            consumers
                .entry(id)
                .or_default()
                .push(Consumer::Materialized(materialized));
            stack.extend(inputs(id));
        }
    }
    // Queries first, in order, then the materialized nodes.
    for of_node in consumers.values_mut() {
        of_node.sort();
    }
    consumers
}

/// Whether a `TimeShift` anchored with `at`, read by `queries` (those on
/// one evaluation grid that reach it), is anchored at their evaluation
/// times, so its input runs in their evaluation context, shared with
/// their other reads: `@ end()` with an instant query among them (an
/// instant query's request ends at its evaluation; the input computed at
/// every evaluation of the grid serves the range queries' request ends
/// too). Otherwise it runs once per request, in a context of its own.
///
/// `@ start()` of an instant query is its evaluation time too; it keeps
/// its request context here, as the user decided for `@ end()` only.
///
/// Example: `sum_over_time(m[5m] @ end())` and `sum_over_time(m[5m])`,
/// both instant every 60 s: true, so `m[5m]` is computed once per minute
/// for both; the same `@ end()` in a range query alone: false.
pub fn anchor_is_evaluation_time<'e>(
    at: AtModifier,
    queries: impl IntoIterator<Item = &'e QueryWorkloadEntry>,
) -> bool {
    at == AtModifier::End
        && queries.into_iter().any(|entry| {
            entry
                .range_evaluation
                .is_none_or(|range| range.evaluations_per_request() <= 1)
        })
}

/// The queries of the execution context on evaluation grid `grid_ms`
/// among `reaching` (the queries reaching a node, by index in `entries`):
/// those evaluated on that grid ([`evaluation_grid_ms`]; 1 ms for a query
/// without an entry). Stage 3 (over the queries reaching a memo group or
/// an anchored shift) and [`computations`] (over the queries whose walk
/// reaches a plan node) pick a context's queries with it.
pub fn queries_of_context(
    grid_ms: u64,
    reaching: impl IntoIterator<Item = usize>,
    entries: &[QueryWorkloadEntry],
) -> Vec<usize> {
    reaching
        .into_iter()
        .filter(|&query| entries.get(query).map_or(1, evaluation_grid_ms) == grid_ms)
        .collect()
}

/// The grid, in ms, every evaluation of `entry`'s query falls on
/// (`QueryRecurrence::evaluation_grid_ms` of its range evaluation), the
/// grid of its evaluation context; 1 ms when not known, a grid on which
/// only producers stored at every time deliver.
pub fn evaluation_grid_ms(entry: &QueryWorkloadEntry) -> u64 {
    entry
        .recurrence
        .evaluation_grid_ms(entry.range_evaluation)
        .unwrap_or(1)
}

/// The grid, in ms, of the times `entry`'s requests anchor `@ end()` or
/// `@ start()` (`at`) at: its request end times, shifted back by its
/// range-evaluation span for `@ start()`; 1 ms when not known.
///
/// Example: a query every 60 s over the last hour: `@ end()` 60,000 ms,
/// `@ start()` gcd(60,000, 3,600,000) = 60,000 ms.
pub fn request_time_grid_ms(entry: &QueryWorkloadEntry, at: AtModifier) -> u64 {
    let end_ms = entry.recurrence.evaluation_grid_ms(None).unwrap_or(1);
    match (at, entry.range_evaluation) {
        (AtModifier::Start, Some(range)) => greatest_common_divisor(end_ms, range.span_ms),
        _ => end_ms,
    }
}

/// How many times per second a node runs in one execution context of the
/// queries `queries` (a node not materialized across evaluations, run once
/// per execution of the context; Stage 3 and [`computations`] both cost it
/// so): with no `anchor`, at every evaluation of theirs, a range query's
/// steps each one ([`evaluation_rate`]); below a `TimeShift` anchored with
/// `anchor`, once per request: below `@ end()` once per distinct request
/// end ([`evaluation_rate`] of their request schedules); below
/// `@ start()` that per span, summed over the distinct spans, as two
/// queries requested together with different spans need the input at two
/// start times, and a result not kept across evaluations serves only the
/// request it is computed for.
///
/// Example: two range queries every 60 s at phase 0 with steps of 30 s,
/// spans 1 h and 2 m: below `@ start()` 2/60 per second (one run per
/// request of each); below `@ end()` both end at `k · 60 s`, 1/60 per
/// second.
pub fn context_executions_per_second<'e>(
    anchor: Option<AtModifier>,
    queries: impl IntoIterator<Item = &'e QueryWorkloadEntry>,
    non_repeating_query_horizon_seconds: f64,
) -> Result<f64, AnalyticalCostError> {
    let queries = queries.into_iter();
    match anchor {
        // Requests of one span need the input at one start time each;
        // different spans need it at different times of one request.
        Some(AtModifier::Start) => {
            let mut by_span: BTreeMap<Option<u64>, Vec<&QueryRecurrence>> = BTreeMap::new();
            for entry in queries {
                by_span
                    .entry(entry.range_evaluation.map(|range| range.span_ms))
                    .or_default()
                    .push(&entry.recurrence);
            }
            by_span
                .values()
                .map(|recurrences| {
                    evaluation_rate(
                        recurrences.iter().map(|&recurrence| (recurrence, None)),
                        non_repeating_query_horizon_seconds,
                    )
                })
                .sum()
        }
        // Once per request end, however many steps the request evaluates.
        Some(_) => evaluation_rate(
            queries.map(|entry| (&entry.recurrence, None)),
            non_repeating_query_horizon_seconds,
        ),
        None => evaluation_rate(
            queries.map(|entry| (&entry.recurrence, entry.range_evaluation)),
            non_repeating_query_horizon_seconds,
        ),
    }
}

/// How often each node of `dag` is computed: `(multiplier, per_second)`
/// (module doc, "Cost model"). A node materialized across evaluations is
/// computed once for all its queries: once per second at ingestion time,
/// or once per request of the queries reaching it at query time, a chain
/// building only its newest tumbling window however many range steps it
/// evaluates ([`executions_per_second`]). `entries[q]` is query `q`'s
/// workload entry (its recurrence and range evaluation). Any other node
/// (ℓ None or `OneEvaluation`) is computed once per execution context of
/// the consumers that reach it ([`consumers`]; the execution contexts of
/// the concepts doc, section 5). A consumer is a query (operator or scalar
/// root) or a node materialized across evaluations; a walk stops at the
/// first such node it meets, and a node read several times within one
/// consumer's walk counts once for it. The queries on one evaluation grid
/// ([`evaluation_grid_ms`]) are one context, and the requests of the
/// queries on one grid below a `TimeShift` anchored with `@ end()` (or
/// `@ start()`) another, each at [`context_executions_per_second`]; a
/// materialized node is a context of its own, at its rate. Its
/// `multiplier` is that number of contexts. A node computed at ingestion
/// time, materialized or not, computes each arriving row once: once per
/// second, `(1, 1.0)`, however many ingestion-time views of however many
/// refresh windows read it.
pub fn computations(
    dag: &PhysicalASAPDAG,
    entries: &[QueryWorkloadEntry],
    non_repeating_query_horizon_seconds: f64,
) -> Result<HashMap<PhysicalASAPNodeId, (u64, f64)>, AnalyticalCostError> {
    // Each node materialized across evaluations: its timing and lifetime.
    let materialized_nodes: HashMap<PhysicalASAPNodeId, (ExecutionTiming, Lifetime)> = dag
        .nodes
        .iter()
        .filter(|node| matches!(node.lifetime, Lifetime::AcrossEvaluations(_)))
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
    let mut count: HashMap<_, (u64, f64)> = HashMap::new();
    for (id, of_node) in consumers(dag, entries) {
        let entry = count.entry(id).or_default();
        // The queries reaching it, per anchor.
        let mut queries_by_anchor: BTreeMap<Option<AtModifier>, BTreeSet<usize>> = BTreeMap::new();
        for consumer in of_node {
            let (query, anchor) = match consumer {
                Consumer::Query(query) => (query, None),
                Consumer::QueryRequest { query, at } => (query, Some(at)),
                Consumer::Materialized(node) => {
                    entry.0 += 1;
                    entry.1 += rate_of_materialized[&node];
                    continue;
                }
            };
            queries_by_anchor.entry(anchor).or_default().insert(query);
        }
        // One context per (evaluation grid, anchor).
        for (anchor, reaching) in queries_by_anchor {
            let grids: BTreeSet<u64> = reaching
                .iter()
                .map(|&query| entries.get(query).map_or(1, evaluation_grid_ms))
                .collect();
            for grid_ms in grids {
                let queries = queries_of_context(grid_ms, reaching.iter().copied(), entries);
                entry.0 += 1;
                entry.1 += context_executions_per_second(
                    anchor,
                    queries.iter().filter_map(|&query| entries.get(query)),
                    non_repeating_query_horizon_seconds,
                )?;
            }
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

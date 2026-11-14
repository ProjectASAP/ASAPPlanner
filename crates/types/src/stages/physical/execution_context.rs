//! $\kappa$: a class of executions that happen at the same times (concepts
//! doc, section 5). Stage 3 indexes $x_{\kappa,v}$ and $y_{\kappa,u,c,i}$ by
//! it; [`super::PhysicalCandidate::delivers_on`] reads it, so it lives with
//! the physical candidates.

use super::candidate::PhysicalCandidateId;
use crate::ir::operator::AtModifier;
use crate::plan::materialized_view::RefreshWindow;
use crate::stages::EvaluationGrid;
use crate::workload::QueryWorkloadEntry;
use std::collections::{BTreeSet, HashMap, HashSet};

/// Index of an [`ExecutionContext`] in the workload's list of distinct
/// contexts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ExecutionContextId(pub usize);

/// $\kappa$: within one context a computation runs once, and every consumer
/// in it shares that run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ExecutionContext {
    /// At every time of an evaluation grid, answering queries: every query
    /// evaluated on the grid, a range query's steps included. An alert
    /// every 10 s at phase 0 is the grid 10 s @ 0.
    Evaluation { grid: EvaluationGrid },
    /// As data arrives, in batches of one refresh window.
    IngestionPipeline(RefreshWindow),
    /// The own computation of a candidate materialized across evaluations,
    /// on its own schedule, whoever reads it.
    Materialized(PhysicalCandidateId),
}

impl ExecutionContext {
    /// The one context of every ingestion-time execution that is not
    /// materialized: each second of arriving rows, which it computes once
    /// for every ingestion-time view of every refresh window that reads it.
    pub const EVERY_ARRIVING_ROW: Self = Self::IngestionPipeline(RefreshWindow {
        width_ms: 1_000,
        phase_ms: 0,
    });
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
/// those evaluated on that grid
/// ([`QueryWorkloadEntry::evaluation_grid_ms`]; 1 ms for a query without
/// an entry). Stage 3 (over memo groups) and `asap_cost`'s `computations`
/// (over plan nodes) pick a context's queries with it.
pub fn queries_of_context(
    grid_ms: u64,
    reaching: impl IntoIterator<Item = usize>,
    entries: &[QueryWorkloadEntry],
) -> Vec<usize> {
    reaching
        .into_iter()
        .filter(|&query| {
            entries
                .get(query)
                .map_or(1, QueryWorkloadEntry::evaluation_grid_ms)
                == grid_ms
        })
        .collect()
}

/// How each query reads the nodes below it, in a plan or a memo: per node,
/// each (query, anchor) whose walk from the query's `starts` down
/// `inputs_of` reaches it, once, skipping (and stopping at) a node
/// `stops_at` keeps. Below a `TimeShift` anchored with `@ end()` or
/// `@ start()` (`anchor_of`, `Operator::request_anchor`) a query reads per request, with that anchor,
/// unless the anchor is its evaluation time ([`anchor_is_evaluation_time`]
/// over the queries on its grid that reach the shift through any node);
/// elsewhere with none. Stage 3 walks memo groups with it and
/// `asap_cost`'s `consumers` plan nodes, so both rate a context by the queries whose
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
            entry.evaluation_grid_ms(),
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

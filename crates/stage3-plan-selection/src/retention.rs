//! How far back from the evaluation time a candidate that retains rows (a
//! materialized one, or a query-time `Scan`) must keep them for a reader
//! over one edge, $\operatorname{need}_{\kappa,u,c,i}$ of constraint (R),
//! or for a query reading it directly, and how far back one execution of a
//! query-time `Scan` reads for that reader: [`asap_cost::read_back_ms`]
//! over candidates
//! (`docs/design_docs/concepts/memo-groups-properties-requirements.md`,
//! section 5, "Cost and retention").

use asap_types::ir::{NonASAPOp, Operator};
use asap_types::plan::time_window::TimeIndexedRead;
use asap_types::stages::{PhysicalASAPMemoCandidates, PhysicalCandidateId, PhysicalEdgeId};
use asap_types::workload::QueryWorkloadEntry;

/// $\operatorname{need}_{\kappa,u,c,i}$: how far back from the evaluation
/// time `edge`'s producer $u$ must keep its rows for its consumer $c$
/// ([`retention_need_ms`] with $c$ $u$'s only reader, and every candidate
/// that may read $c$ above it); `None` when read unbounded.
///
/// Example: `TimeRange(5m)` over a chain of 10 s windows, read by a range
/// query with a 1 h span: 3,900,000 ms.
pub(crate) fn edge_retention_need_ms(
    physical: &PhysicalASAPMemoCandidates,
    entries: &[QueryWorkloadEntry],
    edge: PhysicalEdgeId,
) -> Option<u64> {
    let producer = physical.edge(edge).producer_candidate_id;
    retention_need_ms(physical, entries, producer, readers_through(physical, edge))
}

/// How far back one execution of the query-time `Scan` that is `edge`'s
/// producer reads for its consumer ([`scan_read_ms`] with that consumer
/// its only reader); `None` when read whole.
pub(crate) fn edge_scan_read_ms(
    physical: &PhysicalASAPMemoCandidates,
    edge: PhysicalEdgeId,
) -> Option<u64> {
    let producer = physical.edge(edge).producer_candidate_id;
    scan_read_ms(physical, producer, readers_through(physical, edge))
}

/// How far back a query reads the output it reads directly: its
/// range-evaluation span (0 without one).
pub(crate) fn output_retention_need_ms(entry: &QueryWorkloadEntry) -> u64 {
    entry.range_evaluation.map_or(0, |range| range.span_ms)
}

/// The readers of each candidate when `edge`'s producer is read over
/// `edge` only: its consumer, and above it every candidate that may read
/// one (its admissible edges out).
fn readers_through(
    physical: &PhysicalASAPMemoCandidates,
    edge: PhysicalEdgeId,
) -> impl Fn(PhysicalCandidateId) -> Vec<PhysicalCandidateId> + '_ {
    let edge = physical.edge(edge);
    move |v| match v == edge.producer_candidate_id {
        true => vec![edge.consumer_candidate_id],
        false => physical
            .admissible_edges_out_of(v)
            .iter()
            .map(|&out| physical.edge(out).consumer_candidate_id)
            .collect(),
    }
}

/// How far back from the evaluation time `producer`'s rows must be kept
/// for its readers (`readers_of`, the candidates that may read it):
/// [`asap_cost::read_back_ms`] with, at each reader, the longest
/// range-evaluation span of the queries reaching it.
fn retention_need_ms<R: IntoIterator<Item = PhysicalCandidateId>>(
    physical: &PhysicalASAPMemoCandidates,
    entries: &[QueryWorkloadEntry],
    producer: PhysicalCandidateId,
    readers_of: impl Fn(PhysicalCandidateId) -> R,
) -> Option<u64> {
    let memo = &physical.logical_candidates.memo;
    let span_ms = |v: PhysicalCandidateId| {
        let group = physical.physical_candidates[v.0].logical_candidate_id.group;
        memo.groups()[group.0 as usize]
            .reached_by_queries()
            .iter()
            .filter_map(|&query| entries.get(query)?.range_evaluation)
            .map(|range| range.span_ms)
            .max()
            .unwrap_or(0)
    };
    read_back(physical, producer, readers_of, span_ms)
}

/// How far back from the evaluation time one execution of the query-time
/// `Scan` `producer` reads, for its readers (`readers_of`):
/// [`asap_cost::read_back_ms`] without a range-evaluation span.
fn scan_read_ms<R: IntoIterator<Item = PhysicalCandidateId>>(
    physical: &PhysicalASAPMemoCandidates,
    producer: PhysicalCandidateId,
    readers_of: impl Fn(PhysicalCandidateId) -> R,
) -> Option<u64> {
    read_back(physical, producer, readers_of, |_| 0)
}

/// [`asap_cost::read_back_ms`] over candidates, each read as its operator
/// at its timing reads ([`TimeIndexedRead::of`]).
fn read_back<R: IntoIterator<Item = PhysicalCandidateId>>(
    physical: &PhysicalASAPMemoCandidates,
    producer: PhysicalCandidateId,
    readers_of: impl Fn(PhysicalCandidateId) -> R,
    span_ms: impl Fn(PhysicalCandidateId) -> u64,
) -> Option<u64> {
    let memo = &physical.logical_candidates.memo;
    let operator_of = |v: PhysicalCandidateId| {
        &memo
            .candidate(physical.physical_candidates[v.0].logical_candidate_id)
            .expression
            .operator
    };
    asap_cost::read_back_ms(
        producer,
        readers_of,
        |v| {
            TimeIndexedRead::of(
                operator_of(v),
                physical.physical_candidates[v.0]
                    .physical_properties
                    .timing(),
            )
        },
        span_ms,
        matches!(
            operator_of(producer),
            Operator::NonASAP(NonASAPOp::Scan { .. })
        ),
    )
}

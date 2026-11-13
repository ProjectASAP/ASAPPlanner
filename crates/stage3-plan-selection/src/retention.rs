//! How far back from the evaluation time a candidate that retains rows (a
//! materialized one, or a query-time `Scan`) must keep them, and how far
//! back one execution of a query-time `Scan` reads, for the candidates
//! that may read it: [`asap_cost::read_back_ms`] over candidates
//! (`docs/design_docs/concepts/memo-groups-properties-requirements.md`,
//! section 5, "Cost and retention").

use asap_types::ir::{NonASAPOp, Operator};
use asap_types::plan::time_window::TimeIndexedRead;
use asap_types::stages::{PhysicalASAPMemoCandidates, PhysicalCandidateId};
use asap_types::workload::QueryWorkloadEntry;

/// How far back from the evaluation time `producer`'s rows must be kept
/// for its readers (`readers_of`, the candidates that may read it):
/// [`asap_cost::read_back_ms`] with, at each reader, the longest
/// range-evaluation span of the queries reaching it.
pub(crate) fn retention_need_ms<R: IntoIterator<Item = PhysicalCandidateId>>(
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
pub(crate) fn scan_read_ms<R: IntoIterator<Item = PhysicalCandidateId>>(
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
            let properties = physical.physical_candidates[v.0].physical_properties;
            TimeIndexedRead::of(operator_of(v), properties.timing(), properties.lifetime())
        },
        span_ms,
        matches!(
            operator_of(producer),
            Operator::NonASAP(NonASAPOp::Scan { .. })
        ),
    )
}

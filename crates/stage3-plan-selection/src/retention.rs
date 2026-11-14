//! How far back from the evaluation time a candidate that retains rows (a
//! materialized one, or a query-time `Scan`) must keep them, and how far
//! back one execution of a query-time `Scan` reads, for the candidates
//! that may read it: [`asap_cost::read_back_ms`] over candidates
//! (`docs/design_docs/concepts/memo-groups-properties-requirements.md`,
//! section 5, "Cost and retention").

use asap_types::ir::{NonASAPOp, Operator};
use asap_types::plan::time_window::{InputTimeAccess, TimeIndexedPlan};
use asap_types::stages::{MemoGroupId, PhysicalASAPMemoCandidates, PhysicalCandidateId};
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

/// [`asap_cost::read_back_ms`] over candidates ([`Candidates`]).
fn read_back<R: IntoIterator<Item = PhysicalCandidateId>>(
    physical: &PhysicalASAPMemoCandidates,
    producer: PhysicalCandidateId,
    readers_of: impl Fn(PhysicalCandidateId) -> R,
    span_ms: impl Fn(PhysicalCandidateId) -> u64,
) -> Option<u64> {
    let readers_of = |v| readers_of(v).into_iter().collect();
    let candidates = Candidates {
        physical,
        readers_of: &readers_of,
    };
    asap_cost::read_back_ms(&candidates, producer, span_ms)
}

/// Stage 3's candidates as the time walks see them: the readers a caller
/// gives (those that may read a candidate, or the selected ones), each
/// candidate's admissible producers as its inputs, its operator at its
/// timing and lifetime ([`InputTimeAccess::of`]), and a current-time read
/// of a `Scan` (raw data) needing every row.
struct Candidates<'a> {
    physical: &'a PhysicalASAPMemoCandidates,
    readers_of: &'a dyn Fn(PhysicalCandidateId) -> Vec<PhysicalCandidateId>,
}

impl TimeIndexedPlan for Candidates<'_> {
    type Node = PhysicalCandidateId;
    type Child = MemoGroupId;

    fn readers_of(&self, v: PhysicalCandidateId) -> Vec<PhysicalCandidateId> {
        (self.readers_of)(v)
    }

    fn inputs_of(&self, v: PhysicalCandidateId) -> Vec<PhysicalCandidateId> {
        (0..self.physical.input_count(v))
            .flat_map(|input| self.physical.admissible_edges_into(v, input))
            .map(|&edge| self.physical.edge(edge).producer_candidate_id)
            .collect()
    }

    fn input_time_access(&self, v: PhysicalCandidateId) -> InputTimeAccess {
        let properties = self.physical.physical_candidates[v.0].physical_properties;
        InputTimeAccess::of(
            self.operator_of(v),
            properties.timing(),
            properties.lifetime(),
        )
    }

    fn current_time_read_needs_every_row(&self, v: PhysicalCandidateId) -> bool {
        matches!(
            self.operator_of(v),
            Operator::NonASAP(NonASAPOp::Scan { .. })
        )
    }

    fn operator_of(&self, v: PhysicalCandidateId) -> &Operator<MemoGroupId> {
        &self
            .physical
            .logical_candidates
            .memo
            .candidate(self.physical.physical_candidates[v.0].logical_candidate_id)
            .expression
            .operator
    }
}

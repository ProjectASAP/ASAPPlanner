//! How far back from the evaluation time a candidate that retains rows (a
//! materialized one, or a query-time `Scan`) must keep them for a reader
//! over one edge, $\operatorname{need}_{\kappa,u,c,i}$ of constraint (R),
//! or for a query reading it directly, and how far back one execution of a
//! query-time `Scan` reads for that reader: [`asap_cost::read_back_ms`]
//! over candidates; and every read of an execution's rows
//! ([`reads_back`])
//! (`docs/design_docs/concepts/memo-groups-properties-requirements.md`,
//! section 5, "Cost and retention").

use asap_types::ir::{NonASAPOp, Operator};
use asap_types::plan::time_window::{InputTimeAccess, TimeIndexedPlan};
use asap_types::stages::{
    MemoGroupId, PhysicalASAPMemoCandidates, PhysicalCandidateId, PhysicalEdgeId,
};
use asap_types::workload::QueryWorkloadEntry;

use crate::selectable::{QueryId, SelectableExecutions};

/// $\operatorname{need}_{\kappa,u,c,i}$: how far back from the evaluation
/// time `edge`'s producer $u$ must keep its rows for its consumer $c$, with
/// $c$ $u$'s only reader and every candidate that may read $c$ above it
/// ([`read_back`]), at each reader the longest range-evaluation span of
/// the queries reaching it; `None` when read unbounded.
///
/// Example: `TimeRange(5m)` over a chain of 10 s windows, read by a range
/// query with a 1 h span: 3,900,000 ms.
pub(crate) fn edge_retention_need_ms(
    physical: &PhysicalASAPMemoCandidates,
    entries: &[QueryWorkloadEntry],
    edge: PhysicalEdgeId,
) -> Option<u64> {
    let memo = &physical.logical_candidates.memo;
    read_back(physical, edge, |v| {
        let group = physical.physical_candidates[v.0].logical_candidate_id.group;
        memo.groups()[group.0 as usize]
            .reached_by_queries()
            .iter()
            .filter_map(|&query| entries.get(query)?.range_evaluation)
            .map(|range| range.span_ms)
            .max()
            .unwrap_or(0)
    })
}

/// How far back one execution of the query-time `Scan` that is `edge`'s
/// producer reads for its consumer ([`read_back`] without a
/// range-evaluation span); `None` when read whole.
pub(crate) fn edge_scan_read_ms(
    physical: &PhysicalASAPMemoCandidates,
    edge: PhysicalEdgeId,
) -> Option<u64> {
    read_back(physical, edge, |_| 0)
}

/// How far back a query reads the output it reads directly: its
/// range-evaluation span (0 without one).
fn output_retention_need_ms(entry: &QueryWorkloadEntry) -> u64 {
    entry.range_evaluation.map_or(0, |range| range.span_ms)
}

/// What reads an execution's rows back ([`reads_back`]): a selectable edge
/// out of it, by position in [`SelectableExecutions::edges`], or a query
/// output it may answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Reader {
    Edge(usize),
    Output(QueryId),
}

/// Every read of an execution's rows, as (the execution read, by position
/// in [`SelectableExecutions::executions`]; its reader; how far back it
/// reads, in ms, `None` when unbounded): each selectable edge into its
/// producer, `need_ms_per_edge` by edge position
/// ([`edge_retention_need_ms`]), and each query output into each execution
/// that may answer it, the query's range-evaluation span. Costing ($h_v$
/// at its largest), (R), the least $h_v$ of a selection and the greedy
/// bound each fold it.
pub(crate) fn reads_back<'a>(
    selectable: &'a SelectableExecutions,
    need_ms_per_edge: &'a [Option<u64>],
    entries: &'a [QueryWorkloadEntry],
) -> impl Iterator<Item = (usize, Reader, Option<u64>)> + 'a {
    let edges = selectable
        .edges()
        .iter()
        .zip(need_ms_per_edge)
        .enumerate()
        .map(|(position, (edge, &need_ms))| (edge.producer, Reader::Edge(position), need_ms));
    let outputs = selectable
        .answering()
        .iter()
        .flat_map(move |(query, answering)| {
            let need_ms = output_retention_need_ms(&entries[query.0]);
            answering
                .iter()
                .map(move |&index| (index, Reader::Output(*query), Some(need_ms)))
        });
    edges.chain(outputs)
}

/// `ms` in seconds, the unit of $h_v$ in the MILP and of the costs per
/// second retained.
pub(crate) fn ms_to_seconds(ms: u64) -> f64 {
    ms as f64 / 1_000.0
}

/// [`asap_cost::read_back_ms`] of `edge`'s producer over candidates, read
/// by `edge`'s consumer only and above it by every candidate that may read
/// one (its admissible edges out), each read as its operator at its timing
/// reads ([`Candidates`]), with `span_ms` at each reader.
fn read_back(
    physical: &PhysicalASAPMemoCandidates,
    edge: PhysicalEdgeId,
    span_ms: impl Fn(PhysicalCandidateId) -> u64,
) -> Option<u64> {
    let edge = physical.edge(edge);
    let producer = edge.producer_candidate_id;
    let readers_of = |v: PhysicalCandidateId| match v == producer {
        true => vec![edge.consumer_candidate_id],
        false => physical
            .admissible_edges_out_of(v)
            .iter()
            .map(|&out| physical.edge(out).consumer_candidate_id)
            .collect(),
    };
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

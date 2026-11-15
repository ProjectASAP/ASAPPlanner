//! How far back from the evaluation time a candidate that retains rows (a
//! materialized one, or a query-time `Scan`) must keep them for a reader
//! over one edge, $\operatorname{need}_{\kappa,u,c,i}$ of constraint (R),
//! or for a query reading it directly, and how far back one execution of a
//! query-time `Scan` reads for that reader: [`asap_cost::read_back_ms`]
//! over candidates; and every read of an execution's rows
//! (`SelectableExecutions::reads_back`)
//! (`docs/design_docs/concepts/memo-groups-properties-requirements.md`,
//! section 5, "Cost and retention").

use asap_types::ir::Operator;
use asap_types::plan::time_window::{InputTimeAccess, TimeIndexedPlan};
use asap_types::stages::{
    MemoGroupId, PhysicalASAPMemoCandidates, PhysicalCandidateId, PhysicalEdge, PhysicalEdgeId,
};
use asap_types::workload::QueryWorkloadEntry;

use crate::cost::ExecutionCosts;
use crate::selectable::{Reader, SelectableExecutions};

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
            .filter_map(|&query| entries.get(query))
            .map(QueryWorkloadEntry::range_span_ms)
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

/// The least retention, in ms, over each execution's costed reads
/// (`SelectableExecutions::reads_back`: an edge out of it `costs.per_edge`
/// costs, or a query output it may answer), each read's need at least its
/// candidate's newest output (`ExecutionCosts::retention_ms_for_need`), by
/// position in [`SelectableExecutions::executions`]; `None` without a
/// bounded one. (F) gives a selected execution a use and (R) then retains
/// at least that: the greedy bound charges it, and the MILP has
/// $h_v \ge$ it $\cdot x$.
pub(crate) fn least_retention_ms_per_execution(
    selectable: &SelectableExecutions,
    costs: &ExecutionCosts,
    entries: &[QueryWorkloadEntry],
) -> Vec<Option<u64>> {
    let mut least_retention_ms = vec![None; selectable.executions().len()];
    for (index, reader, need_ms) in
        selectable.reads_back(&costs.retention_need_ms_per_edge, entries)
    {
        let costed = match reader {
            Reader::Edge(position) => costs.per_edge[position].is_some(),
            Reader::Output(_) => true,
        };
        if let (true, Some(need_ms)) = (costed, need_ms) {
            let retention_ms =
                costs.retention_ms_for_need(selectable.executions()[index].candidate, need_ms);
            let least: &mut Option<u64> = &mut least_retention_ms[index];
            *least = Some(least.map_or(retention_ms, |least| least.min(retention_ms)));
        }
    }
    least_retention_ms
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
    let candidates = Candidates {
        physical,
        edge: physical.edge(edge),
    };
    asap_cost::read_back_ms(&candidates, candidates.edge.producer_candidate_id, span_ms)
}

/// Stage 3's candidates as the time walks see them above `edge`: its
/// producer read by its consumer only, any other candidate as
/// `PhysicalASAPMemoCandidates` sees it (`TimeIndexedPlan`: by every
/// candidate that may read it, over its admissible edges out).
struct Candidates<'a> {
    physical: &'a PhysicalASAPMemoCandidates,
    edge: &'a PhysicalEdge,
}

impl TimeIndexedPlan for Candidates<'_> {
    type Node = PhysicalCandidateId;
    type Child = MemoGroupId;

    fn readers_of(&self, v: PhysicalCandidateId) -> Vec<PhysicalCandidateId> {
        match v == self.edge.producer_candidate_id {
            true => vec![self.edge.consumer_candidate_id],
            false => self.physical.readers_of(v),
        }
    }

    fn inputs_of(&self, v: PhysicalCandidateId) -> Vec<PhysicalCandidateId> {
        self.physical.inputs_of(v)
    }

    fn input_time_access(&self, v: PhysicalCandidateId) -> InputTimeAccess {
        self.physical.input_time_access(v)
    }

    fn current_time_read_needs_every_row(&self, v: PhysicalCandidateId) -> bool {
        self.physical.current_time_read_needs_every_row(v)
    }

    fn operator_of(&self, v: PhysicalCandidateId) -> &Operator<MemoGroupId> {
        self.physical.operator(v)
    }
}

//! What the baseline selection may select: the selectable edges (each
//! admissible edge whose producer delivers on every evaluation its
//! consumer serves) and, per query output, the candidates that may answer
//! it.

use std::collections::BTreeSet;

use asap_types::ir::properties::ExecutionTiming;
use asap_types::ir::QueryRoot;
use asap_types::stages::{
    output_references, producer_satisfies_physical_requirements, EvaluationGrid, ExecutionContext,
    MemoGroupId, PhysicalASAPMemoCandidates, PhysicalCandidate, PhysicalCandidateId, PhysicalEdge,
    PhysicalEdgeId,
};
use asap_types::workload::QueryWorkloadEntry;

use crate::cost::SelectionCosts;
use crate::milp::model::QueryId;

/// The edges a plan may select: admissible, with a producer that delivers
/// on every evaluation its query-time consumer serves
/// ([`delivers_to_consumer`]).
pub(crate) fn selectable_edges(
    physical: &PhysicalASAPMemoCandidates,
    entries: &[QueryWorkloadEntry],
) -> Vec<PhysicalEdgeId> {
    physical
        .edges()
        .filter(|(_, edge)| {
            edge.is_logically_and_physically_admissible()
                && delivers_to_consumer(physical, entries, edge)
        })
        .map(|(id, _)| id)
        .collect()
}

/// Each query output reference (a query, and the group its operator root,
/// or one operator of its scalar root, reads), in order.
pub(crate) fn output_groups(physical: &PhysicalASAPMemoCandidates) -> Vec<(QueryId, MemoGroupId)> {
    let roots: Vec<QueryRoot<MemoGroupId>> = physical
        .logical_candidates
        .memo
        .queries()
        .iter()
        .map(|query| query.root.clone())
        .collect();
    output_references(&roots)
        .into_iter()
        .map(|(query, group)| (QueryId(query), group))
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

/// Per output of `outputs`, the candidates of its group that may answer
/// it: costed, readable at query time and delivering on its query's
/// evaluations.
pub(crate) fn query_outputs(
    physical: &PhysicalASAPMemoCandidates,
    entries: &[QueryWorkloadEntry],
    costs: &SelectionCosts,
    outputs: &[(QueryId, MemoGroupId)],
) -> Vec<(QueryId, Vec<PhysicalCandidateId>)> {
    outputs
        .iter()
        .map(|&(query, group)| {
            let candidates = physical
                .physical_candidates
                .iter()
                .enumerate()
                .filter(|(v, candidate)| {
                    candidate.logical_candidate_id.group == group
                        && costs.candidate_cost_per_second[*v].is_ok()
                        && producer_satisfies_physical_requirements(
                            candidate,
                            ExecutionTiming::QueryTime,
                        )
                        && candidate.delivers_on(physical, &evaluation_context(&entries[query.0]))
                })
                .map(|(v, _)| PhysicalCandidateId(v))
                .collect();
            (query, candidates)
        })
        .collect()
}

/// The evaluation context of `entry`'s query: the grid its evaluations
/// fall on (`QueryRecurrence::evaluation_grid_ms`), anchored at the epoch.
/// Times that are not known may fall on any millisecond: a 1 ms grid, on
/// which only producers stored at every time deliver.
fn evaluation_context(entry: &QueryWorkloadEntry) -> ExecutionContext {
    ExecutionContext::Evaluation {
        grid: EvaluationGrid {
            period_ms: entry
                .recurrence
                .evaluation_grid_ms(entry.range_evaluation)
                .unwrap_or(1),
            phase_ms: 0,
        },
    }
}

/// Whether `edge`'s producer delivers on every evaluation its query-time
/// consumer serves: on the evaluation context of each query reaching the
/// consumer's group ([`PhysicalCandidate::delivers_on`]). The baseline has
/// no other context: an ingestion-time consumer's (its pipeline) is checked
/// with execution contexts.
fn delivers_to_consumer(
    physical: &PhysicalASAPMemoCandidates,
    entries: &[QueryWorkloadEntry],
    edge: &PhysicalEdge,
) -> bool {
    let producer: &PhysicalCandidate = &physical.physical_candidates[edge.producer_candidate_id.0];
    let consumer = &physical.physical_candidates[edge.consumer_candidate_id.0];
    if consumer.physical_properties.timing() != ExecutionTiming::QueryTime {
        return true;
    }
    physical.logical_candidates.memo.groups()[consumer.logical_candidate_id.group.0 as usize]
        .reached_by_queries()
        .iter()
        .filter_map(|&query| entries.get(query))
        .all(|entry| producer.delivers_on(physical, &evaluation_context(entry)))
}

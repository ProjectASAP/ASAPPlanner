//! Stage 2's physical candidates: expand each logical candidate into one
//! physical candidate per valid $(\tau, \ell)$ (decision-tree levels 4 and
//! 5, [`configurations`]), and each logical edge into the physical edges
//! between the two candidates' alternatives
//! ([`generate_physical_asap_memo_candidates`]); and realize one selected
//! DAG as a physical plan (#1047). The output type,
//! `PhysicalASAPMemoCandidates`, lives in `asap_types::stages`; Stage 3
//! selects from it.
mod configurations;
mod pruning;
mod realize;
#[cfg(test)]
mod tests;

pub use realize::Stage2AssignmentRealizer;

use std::collections::HashMap;

use asap_types::stages::{
    producer_satisfies_physical_requirements, CandidateForMemoGroupId, LogicalASAPMemoCandidates,
    LogicalEdge, MemoGroupId, PhysicalASAPMemoCandidates, PhysicalCandidate, PhysicalCandidateId,
    PhysicalCandidateKey, PhysicalEdge, PrunedCandidate,
};
use asap_types::workload::DataWorkload;

use configurations::{physical_properties_of, Deployment};

/// Stage 2: expand every logical candidate of `logical` into its physical
/// alternatives (decision-tree levels 4–5: timing, then lifetime) that the
/// data (an ingestion pipeline when `data` keeps arriving) and the
/// deployment (`query_time_results_retained`, `raw_data_retained`) allow,
/// and every logical edge into its physical edges with $\alpha^{\log}$ and
/// $\alpha^{\mathrm{phys}}$. Every alternative not generated is recorded in
/// the output's `pruned_candidates` with its `PruneReason`: first the
/// alternatives a decision-tree prune rule removes, then those that cannot
/// be built or that no query output can read ([`pruning`]). Prices
/// nothing; alignment with each execution context is Stage 3's check
/// (`PhysicalCandidate::delivers_on`).
pub fn generate_physical_asap_memo_candidates(
    logical: &LogicalASAPMemoCandidates,
    data: &DataWorkload,
    query_time_results_retained: bool,
    raw_data_retained: bool,
) -> PhysicalASAPMemoCandidates {
    let memo = &logical.memo;
    let deployment = Deployment {
        has_ingestion_pipeline: data.arrival.rows_per_second().is_some(),
        query_time_results_retained,
        raw_data_retained,
    };
    let mut pruned_candidates = Vec::new();
    // Levels 4 and 5: the offered alternatives, before pruning, and the
    // alternatives of each logical candidate.
    let mut alternatives = Vec::new();
    let mut alternatives_of_logical_candidate: HashMap<CandidateForMemoGroupId, Vec<usize>> =
        HashMap::new();
    for (group_index, group) in memo.groups().iter().enumerate() {
        for (index_in_group, candidate) in group.candidates.iter().enumerate() {
            let logical_candidate_id = CandidateForMemoGroupId {
                group: MemoGroupId(group_index as u32),
                index_in_group,
            };
            let ids = alternatives_of_logical_candidate
                .entry(logical_candidate_id)
                .or_default();
            for alternative in physical_properties_of(candidate, &deployment) {
                match alternative {
                    Ok(physical_properties) => {
                        ids.push(alternatives.len());
                        alternatives.push(PhysicalCandidate {
                            logical_candidate_id,
                            physical_properties,
                        });
                    }
                    Err((physical_properties, reason)) => pruned_candidates.push(PrunedCandidate {
                        group: logical_candidate_id.group,
                        candidate: PhysicalCandidateKey {
                            logical_candidate_id,
                            physical_properties,
                        },
                        reason,
                    }),
                }
            }
        }
    }
    // Every physical edge between the alternatives, grouped by (consumer,
    // input), over provisional IDs (indices into `alternatives`); and each
    // (consumer, input)'s admissible producers, which pruning reads.
    let logical_edges_into_input = logical_edges_into_input(&logical.edges);
    let mut edges = Vec::new();
    let mut admissible_producers_per_input = Vec::with_capacity(alternatives.len());
    for (consumer_index, consumer) in alternatives.iter().enumerate() {
        let input_count = memo
            .candidate(consumer.logical_candidate_id)
            .expression
            .operator
            .children()
            .len();
        let mut admissible_producers = vec![Vec::new(); input_count];
        for (input_index_of_consumer, admissible) in admissible_producers.iter_mut().enumerate() {
            let logical_edges = logical_edges_into_input
                .get(&(consumer.logical_candidate_id, input_index_of_consumer))
                .map_or(&[][..], Vec::as_slice);
            for logical_edge in logical_edges {
                for &producer_index in
                    &alternatives_of_logical_candidate[&logical_edge.producer_candidate_id]
                {
                    let edge = PhysicalEdge {
                        producer_candidate_id: PhysicalCandidateId(producer_index),
                        consumer_candidate_id: PhysicalCandidateId(consumer_index),
                        input_index_of_consumer,
                        satisfies_logical_requirement: logical_edge.satisfies_logical_requirement,
                        satisfies_physical_requirement: producer_satisfies_physical_requirements(
                            &alternatives[producer_index],
                            consumer,
                        ),
                    };
                    if edge.satisfies_logical_requirement && edge.satisfies_physical_requirement {
                        admissible.push(producer_index);
                    }
                    edges.push(edge);
                }
            }
        }
        admissible_producers_per_input.push(admissible_producers);
    }
    // Candidates that cannot be built or that no query output reads.
    let prune_reasons =
        pruning::prune_reasons(&alternatives, &admissible_producers_per_input, memo);
    let mut physical_candidate_id = vec![None; alternatives.len()];
    let mut physical_candidates = Vec::new();
    for ((alternative, reason), id) in alternatives
        .iter()
        .zip(prune_reasons)
        .zip(&mut physical_candidate_id)
    {
        match reason {
            Some(reason) => pruned_candidates.push(PrunedCandidate {
                group: alternative.logical_candidate_id.group,
                candidate: PhysicalCandidateKey {
                    logical_candidate_id: alternative.logical_candidate_id,
                    physical_properties: alternative.physical_properties,
                },
                reason,
            }),
            None => {
                *id = Some(PhysicalCandidateId(physical_candidates.len()));
                physical_candidates.push(*alternative);
            }
        }
    }
    // $\mathcal E$ over $\mathcal C$: the edges between emitted candidates,
    // renumbered.
    let edges = edges
        .into_iter()
        .filter_map(|edge| {
            Some(PhysicalEdge {
                producer_candidate_id: physical_candidate_id[edge.producer_candidate_id.0]?,
                consumer_candidate_id: physical_candidate_id[edge.consumer_candidate_id.0]?,
                ..edge
            })
        })
        .collect();
    let mut physical = PhysicalASAPMemoCandidates::new(
        logical.clone(),
        physical_candidates,
        memo.query_roots.clone(),
        edges,
    );
    physical.pruned_candidates = pruned_candidates;
    physical
}

/// The logical edges into each (consumer, input), in `edges` order.
fn logical_edges_into_input(
    edges: &[LogicalEdge],
) -> HashMap<(CandidateForMemoGroupId, usize), Vec<&LogicalEdge>> {
    let mut into_input: HashMap<_, Vec<&LogicalEdge>> = HashMap::new();
    for edge in edges {
        into_input
            .entry((edge.consumer_candidate_id, edge.input_index_of_consumer))
            .or_default()
            .push(edge);
    }
    into_input
}

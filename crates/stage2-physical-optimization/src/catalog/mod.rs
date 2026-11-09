//! Stage 2's physical candidates: expand each logical candidate into one
//! physical candidate per offered $(\tau, \ell)$ (decision-tree levels 3
//! and 4, [`configurations`]), and each logical edge into the physical
//! edges between the two candidates' alternatives
//! ([`generate_physical_asap_memo_candidates`]). The output type,
//! `PhysicalASAPMemoCandidates`, lives in `asap_types::stages`; Stage 3
//! selects from it.

mod configurations;
mod pruning;
#[cfg(test)]
mod tests;

use std::collections::{HashMap, HashSet};
use std::ops::Range;

use asap_types::deployment_capabilities::DeploymentCapabilities;
use asap_types::stages::{
    producer_satisfies_physical_requirements, CandidateForMemoGroupId, LogicalASAPMemoCandidates,
    MemoGroupId, PhysicalASAPMemoCandidates, PhysicalCandidate, PhysicalCandidateId, PhysicalEdge,
    PrunedCandidate,
};
use asap_types::workload::DataWorkload;

use configurations::enumerate_candidate_configurations;

/// Stage 2: expand every logical candidate of `logical` that survived Pass 2
/// into its physical alternatives (decision-tree levels 3–4: timing and
/// lifetime) the data and the deployment's `capabilities` allow, and every
/// logical edge into its physical edges with $\alpha^{\log}$ and
/// $\alpha^{\mathrm{phys}}$. Every alternative rejected is recorded in the
/// output's `pruned_candidates` with its `PruneReason`: first those a
/// decision-tree prune rule removes, then those that cannot be built or
/// that no query output can read ([`pruning`]). A value outside a
/// dimension's declared value set (`AcrossEvaluations` on a candidate of a
/// group with no known grid) is not offered and not recorded. Costs nothing,
/// and cannot fail; alignment with each execution context is Stage 3's
/// check (`PhysicalCandidate::delivers_on`).
pub fn generate_physical_asap_memo_candidates(
    logical: &LogicalASAPMemoCandidates,
    data: &DataWorkload,
    capabilities: &DeploymentCapabilities,
) -> PhysicalASAPMemoCandidates {
    let memo = &logical.memo;
    let eliminated: HashSet<CandidateForMemoGroupId> = logical
        .pruned
        .iter()
        .map(|pruned| pruned.candidate)
        .collect();
    let ingestion_time_offered = ingestion_time_offered_per_group(logical, &eliminated);
    let mut pruned_candidates = Vec::new();
    // Levels 3 and 4: the offered alternatives, before pruning, and the
    // alternatives of each logical candidate (a range of `alternatives`).
    let mut alternatives = Vec::new();
    let mut alternatives_of_logical_candidate: HashMap<CandidateForMemoGroupId, Range<usize>> =
        HashMap::new();
    for (group_index, group) in memo.groups().iter().enumerate() {
        for index_in_group in 0..group.candidates.len() {
            let logical_candidate_id = CandidateForMemoGroupId {
                group: MemoGroupId(group_index as u32),
                index_in_group,
            };
            if eliminated.contains(&logical_candidate_id) {
                continue;
            }
            let first = alternatives.len();
            for (physical_properties, reason) in enumerate_candidate_configurations(
                logical,
                logical_candidate_id,
                ingestion_time_offered[group_index],
                data,
                capabilities,
            ) {
                let alternative = PhysicalCandidate {
                    logical_candidate_id,
                    physical_properties,
                };
                match reason {
                    Some(reason) => pruned_candidates.push(PrunedCandidate {
                        candidate: alternative,
                        reason,
                    }),
                    None => alternatives.push(alternative),
                }
            }
            alternatives_of_logical_candidate
                .insert(logical_candidate_id, first..alternatives.len());
        }
    }
    // Every physical edge between the alternatives, over provisional IDs
    // (indices into `alternatives`), and each (consumer, input)'s
    // admissible producers, which pruning reads. Pass 2's edges are grouped
    // by (consumer, input), and those into one (c, i) are exactly the
    // candidates of $G_{c,i}$ that survived Pass 2.
    let mut edges = Vec::new();
    let mut admissible_producers_per_input: Vec<Vec<Vec<usize>>> = alternatives
        .iter()
        .map(|alternative| {
            let input_count = memo
                .candidate(alternative.logical_candidate_id)
                .expression
                .operator
                .children()
                .len();
            vec![Vec::new(); input_count]
        })
        .collect();
    for edges_into_input in logical.edges.chunk_by(|a, b| {
        (a.consumer_candidate_id, a.input_index_of_consumer)
            == (b.consumer_candidate_id, b.input_index_of_consumer)
    }) {
        let consumer = edges_into_input[0].consumer_candidate_id;
        let input_index_of_consumer = edges_into_input[0].input_index_of_consumer;
        for consumer_index in alternatives_of_logical_candidate[&consumer].clone() {
            let consumer_timing = alternatives[consumer_index].physical_properties.timing();
            for logical_edge in edges_into_input {
                for producer_index in
                    alternatives_of_logical_candidate[&logical_edge.producer_candidate_id].clone()
                {
                    let edge = PhysicalEdge {
                        producer_candidate_id: PhysicalCandidateId(producer_index),
                        consumer_candidate_id: PhysicalCandidateId(consumer_index),
                        input_index_of_consumer,
                        satisfies_logical_requirement: logical_edge.satisfies_logical_requirement,
                        satisfies_physical_requirement: producer_satisfies_physical_requirements(
                            &alternatives[producer_index],
                            consumer_timing,
                        ),
                    };
                    if edge.is_admissible() {
                        admissible_producers_per_input[consumer_index][input_index_of_consumer]
                            .push(producer_index);
                    }
                    edges.push(edge);
                }
            }
        }
    }
    // Candidates that cannot be built or that no query output reads.
    let prune_reasons =
        pruning::prune_reasons(&alternatives, &admissible_producers_per_input, memo);
    let mut physical_candidate_id = vec![None; alternatives.len()];
    let mut physical_candidates = Vec::new();
    for ((alternative, reason), id) in alternatives
        .into_iter()
        .zip(prune_reasons)
        .zip(&mut physical_candidate_id)
    {
        match reason {
            Some(reason) => pruned_candidates.push(PrunedCandidate {
                candidate: alternative,
                reason,
            }),
            None => {
                *id = Some(PhysicalCandidateId(physical_candidates.len()));
                physical_candidates.push(alternative);
            }
        }
    }
    // $\mathcal E$ over $\mathcal C$: the edges between emitted candidates,
    // renumbered and grouped by (consumer, input).
    let mut edges: Vec<PhysicalEdge> = edges
        .into_iter()
        .filter_map(|edge| {
            Some(PhysicalEdge {
                producer_candidate_id: physical_candidate_id[edge.producer_candidate_id.0]?,
                consumer_candidate_id: physical_candidate_id[edge.consumer_candidate_id.0]?,
                ..edge
            })
        })
        .collect();
    edges.sort_by_key(|edge| (edge.consumer_candidate_id, edge.input_index_of_consumer));
    PhysicalASAPMemoCandidates::new(
        logical.clone(),
        physical_candidates,
        edges,
        pruned_candidates,
    )
}

/// Per group, by `MemoGroupId`: whether level 3 offers its candidates
/// `IngestionTime`. It does when the group is time-independent (it does
/// not depend on the evaluation time, as a chain does not), or when it is
/// an input, directly or through other offered groups, of a candidate of
/// an offered group: an ingestion-time candidate reads only ingestion-time
/// producers. Candidates Pass 2 eliminated read nothing.
fn ingestion_time_offered_per_group(
    logical: &LogicalASAPMemoCandidates,
    eliminated: &HashSet<CandidateForMemoGroupId>,
) -> Vec<bool> {
    let groups = logical.memo.groups();
    let mut offered: Vec<bool> = groups
        .iter()
        .map(|group| !group.group_properties.depends_on_evaluation_time)
        .collect();
    let mut pending: Vec<usize> = (0..groups.len()).filter(|&group| offered[group]).collect();
    while let Some(group) = pending.pop() {
        for (index_in_group, candidate) in groups[group].candidates.iter().enumerate() {
            let id = CandidateForMemoGroupId {
                group: MemoGroupId(group as u32),
                index_in_group,
            };
            if eliminated.contains(&id) {
                continue;
            }
            for input in candidate.expression.operator.children() {
                let input = input.0 as usize;
                if !offered[input] {
                    offered[input] = true;
                    pending.push(input);
                }
            }
        }
    }
    offered
}

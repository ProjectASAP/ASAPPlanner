//! Stage 2, physical ASAP-aware optimization (concepts doc, section 3,
//! "Stage 2: physical alternatives").
//!
//! The planner facade calls [`generate_physical_asap_memo_candidates`] on
//! Pass 2's output (`asap_types::stages::LogicalASAPMemoCandidates`): each
//! logical candidate expanded into one physical candidate per offered
//! $(\tau, \ell)$ (decision-tree levels 3 and 4, [`candidate`], over the
//! per-group facts of [`group`]), each logical edge into the physical edges
//! between the two candidates' alternatives with $\alpha^{\log}$ and
//! $\alpha^{\mathrm{phys}}$ ([`edge`]), and the pruned alternatives
//! ([`pruning`]; `asap_types::stages::PhysicalASAPMemoCandidates`). Stage 3
//! selects from that output and builds the plan. The crate depends on no
//! other stage crate.

mod candidate;
mod edge;
mod group;
mod pruning;
#[cfg(test)]
mod test_fixtures;
#[cfg(test)]
mod tests;

use std::collections::{HashMap, HashSet};
use std::ops::Range;

use asap_types::deployment_capabilities::DeploymentCapabilities;
use asap_types::stages::{
    CandidateForMemoGroupId, LogicalASAPMemoCandidates, MemoGroupId, PhysicalASAPMemoCandidates,
    PhysicalCandidate, PhysicalCandidateId, PhysicalEdge, PrunedCandidate,
};
use asap_types::workload::DataWorkload;

use candidate::{offered_physical_properties, OfferedPhysicalProperties};

/// Stage 2: expand every logical candidate of `logical` that survived Pass 2
/// into its physical alternatives (decision-tree levels 3–4: timing and
/// lifetime) the data and the deployment's `capabilities` allow, and every
/// logical edge into its physical edges with $\alpha^{\log}$ and
/// $\alpha^{\mathrm{phys}}$. Every alternative rejected is recorded in the
/// output's `pruned_candidates` with its `PruneReason`: first those a
/// decision-tree prune rule removes ([`candidate`]), then those that
/// cannot be built or that no query output can read ([`pruning`]). A value
/// outside a dimension's declared value set (`AcrossEvaluations` on a
/// candidate of a time-dependent group with no known grid) is not offered
/// and not recorded. Costs nothing, and cannot fail; alignment with each
/// execution context is Stage 3's check (`PhysicalCandidate::delivers_on`).
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
    let ingestion_time_offered = group::ingestion_time_offered_per_group(logical, &eliminated);
    let across_evaluations_offered =
        group::across_evaluations_offered_per_group(logical, &eliminated);
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
            for OfferedPhysicalProperties {
                physical_properties,
                prune_reason,
            } in offered_physical_properties(
                memo,
                logical_candidate_id,
                ingestion_time_offered[group_index],
                across_evaluations_offered[group_index],
                data,
                capabilities,
            ) {
                let alternative = PhysicalCandidate {
                    logical_candidate_id,
                    physical_properties,
                };
                match prune_reason {
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
    let (edges, admissible_producer_alternatives_per_input) =
        edge::expand_edges(logical, &alternatives, &alternatives_of_logical_candidate);
    // Candidates that cannot be built or that no query output reads.
    let prune_reasons = pruning::prune_reasons(
        &alternatives,
        &admissible_producer_alternatives_per_input,
        memo,
    );
    let mut physical_candidate_id_of_alternative = vec![None; alternatives.len()];
    let mut physical_candidates = Vec::new();
    for ((alternative, reason), id) in alternatives
        .into_iter()
        .zip(prune_reasons)
        .zip(&mut physical_candidate_id_of_alternative)
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
                producer_candidate_id: physical_candidate_id_of_alternative
                    [edge.producer_candidate_id.0]?,
                consumer_candidate_id: physical_candidate_id_of_alternative
                    [edge.consumer_candidate_id.0]?,
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

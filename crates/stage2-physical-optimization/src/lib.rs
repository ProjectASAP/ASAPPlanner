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

use asap_types::deployment_capabilities::DeploymentExclusion;
use asap_types::stages::{
    CandidateForMemoGroupId, LogicalASAPMemoCandidates, MemoGroupId, PhysicalASAPMemoCandidates,
    PhysicalCandidate, PhysicalCandidateId, PhysicalEdge, PrunedCandidate, SearchSpacePruning,
};
use asap_types::workload::DataWorkload;

pub use candidate::assumed_in_order_arrival;
use candidate::{offered_physical_properties, OfferedPhysicalProperties};

/// Stage 2: expand every logical candidate of `logical` that survived Pass 2
/// into its physical alternatives (decision-tree levels 3–4: timing and
/// lifetime) the data and the deployment's `exclusions` allow, and every
/// logical edge into its physical edges with $\alpha^{\log}$ and
/// $\alpha^{\mathrm{phys}}$. Every alternative rejected is recorded in the
/// output's `pruned_candidates` with its `PruneReason`: first those a
/// decision-tree prune rule removes ([`candidate`]; among them those one of
/// the deployment's `exclusions` matches, `DeploymentCapabilities::exclusions`
/// as the facade expanded them, e.g. every query-time scan when raw data is
/// not retained), then those
/// that cannot be built or that no query output can read ([`pruning`]). A value
/// outside a dimension's declared value set (`AcrossEvaluations` on a
/// candidate of a time-dependent group with no known grid) is not offered
/// and not recorded. Costs nothing, and cannot fail; alignment with each
/// execution context is Stage 3's check (`PhysicalASAPMemoCandidates::delivers_on`).
pub fn generate_physical_asap_memo_candidates(
    logical: &LogicalASAPMemoCandidates,
    data: &DataWorkload,
    exclusions: &[DeploymentExclusion],
    search_space_pruning: SearchSpacePruning,
) -> PhysicalASAPMemoCandidates {
    let memo = &logical.memo;
    let eliminated: HashSet<CandidateForMemoGroupId> = logical
        .pruned
        .iter()
        .map(|pruned| pruned.candidate)
        .collect();
    // Rule 5: how many readers each logical candidate has, its satisfied
    // edges into consumers plus the queries whose output its group is.
    let reads =
        (search_space_pruning.one_evaluation_by_readers).then(|| readers_per_candidate(logical));
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
                reads
                    .as_ref()
                    .is_none_or(|reads| reads.get(&logical_candidate_id).copied().unwrap_or(0) > 1),
                data,
                exclusions,
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
    let (edges, admissible_producer_alternatives_per_input) = edge::expand_edges(
        logical,
        &alternatives,
        &alternatives_of_logical_candidate,
        search_space_pruning.context_reachability,
    );
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

/// Per logical candidate, how many readers may read it in one evaluation:
/// each satisfied logical edge into a consumer input (two candidates of
/// one consumer group count twice, as readout copies may both be
/// selected), plus each query whose output its group is. A candidate with
/// at most one reader needs no `OneEvaluation` form
/// (`SearchSpacePruning::one_evaluation_by_readers`).
///
/// Example: in `sum(rate(m[5m])) / count(rate(m[5m]))` the one `rate`
/// candidate has two readers, the `sum` and the `count`.
fn readers_per_candidate(
    logical: &LogicalASAPMemoCandidates,
) -> HashMap<CandidateForMemoGroupId, usize> {
    let mut reads: HashMap<CandidateForMemoGroupId, usize> = HashMap::new();
    for edge in &logical.edges {
        if edge.satisfies_logical_requirement {
            *reads.entry(edge.producer_candidate_id).or_default() += 1;
        }
    }
    let memo = &logical.memo;
    for group in memo.root_groups() {
        let queries = memo.queries_rooted_at(group).len();
        for index_in_group in 0..memo.groups()[group.0 as usize].candidates.len() {
            *reads
                .entry(CandidateForMemoGroupId {
                    group,
                    index_in_group,
                })
                .or_default() += queries;
        }
    }
    reads
}

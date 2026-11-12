//! Stage 2's tests, one module per step. Each builds Pass 2's output, the
//! memo and its logical edges, from `crate::test_fixtures` and reads the
//! physical candidates, edges and pruned alternatives through the checkers
//! below; no other stage crate is called.
mod candidates;
mod edges;
mod pruning;

use asap_types::deployment_capabilities::DeploymentCapabilities;
use asap_types::ir::properties::ExecutionTiming;
use asap_types::stages::{
    CandidateForMemoGroupId, Lifetime, LogicalASAPMemoCandidates, PhysicalASAPMemoCandidates,
    PhysicalCandidateId, PhysicalProperties, PruneReason,
};
use asap_types::workload::DataWorkload;

use crate::generate_physical_asap_memo_candidates;

/// Stage 2 over `logical`, checking what Stage 3 relies on: the edges are
/// grouped by (consumer, input) over emitted candidates, and every emitted
/// candidate has an admissible edge into each input.
fn generate(
    logical: &LogicalASAPMemoCandidates,
    data: &DataWorkload,
    capabilities: &DeploymentCapabilities,
) -> PhysicalASAPMemoCandidates {
    let physical = generate_physical_asap_memo_candidates(logical, data, capabilities);
    let count = physical.physical_candidates.len();
    let mut consumer_inputs = Vec::new();
    for (_, edge) in physical.edges() {
        assert!(edge.producer_candidate_id.0 < count && edge.consumer_candidate_id.0 < count);
        consumer_inputs.push((edge.consumer_candidate_id, edge.input_index_of_consumer));
    }
    assert!(consumer_inputs.is_sorted(), "grouped by (consumer, input)");
    for (id, candidate) in physical.physical_candidates.iter().enumerate() {
        let inputs = logical
            .memo
            .candidate(candidate.logical_candidate_id)
            .expression
            .operator
            .children()
            .len();
        for input in 0..inputs {
            assert!(
                !physical
                    .admissible_edges_into(PhysicalCandidateId(id), input)
                    .is_empty(),
                "{candidate:?} input {input}"
            );
        }
    }
    physical
}

fn properties((timing, lifetime): (ExecutionTiming, Lifetime)) -> PhysicalProperties {
    PhysicalProperties::new(timing, lifetime).unwrap()
}

/// The physical properties of `logical_candidate`'s emitted alternatives.
fn offered(
    physical: &PhysicalASAPMemoCandidates,
    logical_candidate: CandidateForMemoGroupId,
) -> Vec<PhysicalProperties> {
    physical
        .physical_candidates
        .iter()
        .filter(|candidate| candidate.logical_candidate_id == logical_candidate)
        .map(|candidate| candidate.physical_properties)
        .collect()
}

/// The pruned alternatives of `logical_candidate`, with their reasons.
fn pruned(
    physical: &PhysicalASAPMemoCandidates,
    logical_candidate: CandidateForMemoGroupId,
) -> Vec<(PhysicalProperties, PruneReason)> {
    physical
        .pruned_candidates
        .iter()
        .filter(|pruned| pruned.candidate.logical_candidate_id == logical_candidate)
        .map(|pruned| (pruned.candidate.physical_properties, pruned.reason))
        .collect()
}

/// The physical properties of each (τ, ℓ) in `timings_and_lifetimes`.
fn physical_properties_list(
    timings_and_lifetimes: &[(ExecutionTiming, Lifetime)],
) -> Vec<PhysicalProperties> {
    timings_and_lifetimes
        .iter()
        .copied()
        .map(properties)
        .collect()
}

/// Each (τ, ℓ) in `timings_and_lifetimes`, pruned with `reason`.
fn pruned_with(
    timings_and_lifetimes: &[(ExecutionTiming, Lifetime)],
    reason: PruneReason,
) -> Vec<(PhysicalProperties, PruneReason)> {
    timings_and_lifetimes
        .iter()
        .map(|&timing_and_lifetime| (properties(timing_and_lifetime), reason))
        .collect()
}

/// Whether any of `alternatives` is materialized across evaluations.
fn any_across_evaluations<'a>(
    alternatives: impl IntoIterator<Item = &'a PhysicalProperties>,
) -> bool {
    alternatives
        .into_iter()
        .any(|properties| matches!(properties.lifetime(), Lifetime::AcrossEvaluations(_)))
}

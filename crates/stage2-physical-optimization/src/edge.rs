//! Each logical edge expanded into the physical edges between the two
//! candidates' alternatives, with $\alpha^{\log}$ copied and
//! $\alpha^{\mathrm{phys}}$ from the producer's $(\tau, \ell)$ and the
//! consumer's $\tau$.

use std::collections::HashMap;
use std::ops::Range;

use asap_types::stages::{
    producer_satisfies_physical_requirements, CandidateForMemoGroupId, LogicalASAPMemoCandidates,
    PhysicalCandidate, PhysicalCandidateId, PhysicalEdge,
};

/// Every physical edge between `alternatives`, over provisional IDs
/// (indices into `alternatives`), and, per alternative and input, the
/// alternatives on admissible edges into it, which pruning reads.
/// `alternatives_of_logical_candidate` gives each logical candidate's
/// alternatives as a range of `alternatives`. Pass 2's edges are grouped by
/// (consumer, input), and those into one (c, i) are exactly the candidates
/// of $G_{c,i}$ that survived Pass 2.
pub(super) fn expand_edges(
    logical: &LogicalASAPMemoCandidates,
    alternatives: &[PhysicalCandidate],
    alternatives_of_logical_candidate: &HashMap<CandidateForMemoGroupId, Range<usize>>,
) -> (Vec<PhysicalEdge>, Vec<Vec<Vec<usize>>>) {
    let mut edges = Vec::new();
    let mut admissible_producer_alternatives_per_input: Vec<Vec<Vec<usize>>> = alternatives
        .iter()
        .map(|alternative| {
            let input_count = logical
                .memo
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
                    if edge.is_logically_and_physically_admissible() {
                        admissible_producer_alternatives_per_input[consumer_index]
                            [input_index_of_consumer]
                            .push(producer_index);
                    }
                    edges.push(edge);
                }
            }
        }
    }
    (edges, admissible_producer_alternatives_per_input)
}

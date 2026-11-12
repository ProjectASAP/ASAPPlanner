//! Edge scope: the logical edges $(u, c, i)$ with $\alpha^{\log}$.

use crate::stages::CandidateForMemoGroupId;

/// $(u, c, i)$ over logical candidates: `producer_candidate_id` is a
/// candidate of $G_{c,i}$, the group input `input_index_of_consumer` of
/// `consumer_candidate_id` reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct LogicalEdge {
    pub producer_candidate_id: CandidateForMemoGroupId,
    pub consumer_candidate_id: CandidateForMemoGroupId,
    pub input_index_of_consumer: usize,
    /// $\alpha^{\log}_{u,c,i}$.
    pub satisfies_logical_requirement: bool,
}

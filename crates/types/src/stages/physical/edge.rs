//! Edge scope: the physical edges $\mathcal E$ with $\alpha^{\log}$ and
//! $\alpha^{\mathrm{phys}}$ (#1201).

use crate::stages::PhysicalCandidateId;

/// Index into [`PhysicalASAPMemoCandidates::edges`](crate::stages::PhysicalASAPMemoCandidates::edges).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct PhysicalEdgeId(pub usize);

/// $(u, c, i)$ over physical candidates.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct PhysicalEdge {
    pub producer_candidate_id: PhysicalCandidateId,
    pub consumer_candidate_id: PhysicalCandidateId,
    pub input_index_of_consumer: usize,
    /// $\alpha^{\log}_{u,c,i}$.
    pub satisfies_logical_requirement: bool,
    /// $\alpha^{\mathrm{phys}}_{u,c,i}$.
    pub satisfies_physical_requirement: bool,
}

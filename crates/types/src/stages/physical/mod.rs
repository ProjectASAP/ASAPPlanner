//! The Stage 2 → Stage 3 boundary: the physical candidates and edges Stage 2
//! builds from the memo and Stage 3 selects from (#1047, #1054, #1200, #1201).

mod graph;
mod memo_candidates;

pub use graph::{DagOfSelectedCandidates, PhysicalCandidateId, SelectedEdge, SelectedNodeId};
pub use memo_candidates::{
    output_references, satisfies_physical, PhysicalASAPMemoCandidates, PhysicalCandidate,
    PhysicalCandidateKey, PhysicalEdge, PhysicalEdgeId,
};

//! The Stage 2 → Stage 3 boundary: the physical candidates and edges Stage 2
//! builds from the memo and Stage 3 selects from (#1047, #1054, #1200, #1201).

mod execution_context;
mod memo_candidates;

pub use execution_context::{EvaluationGrid, ExecutionContext, ExecutionContextId};
pub use memo_candidates::{
    output_references, producer_satisfies_physical_requirements, PhysicalASAPMemoCandidates,
    PhysicalCandidate, PhysicalCandidateId, PhysicalEdge, PhysicalEdgeId,
};

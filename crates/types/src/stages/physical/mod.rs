//! The Stage 2 → Stage 3 boundary: the physical candidates and edges Stage 2
//! builds from the memo and Stage 3 selects from (#1047, #1054, #1200, #1201),
//! by scope (execution context → candidate → edge → Stage 2's output).

mod candidate;
mod edge;
mod execution_context;
mod memo_candidates;

pub use candidate::{
    producer_satisfies_physical_requirements, PhysicalCandidate, PhysicalCandidateId,
};
pub use edge::{PhysicalEdge, PhysicalEdgeId};
pub use execution_context::{EvaluationGrid, ExecutionContext, ExecutionContextId};
pub use memo_candidates::{output_references, PhysicalASAPMemoCandidates};

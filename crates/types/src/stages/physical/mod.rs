//! The Stage 2 → Stage 3 boundary: the physical candidates and edges Stage 2
//! builds from the memo and Stage 3 selects from, by scope (execution
//! context → candidate → edge → Stage 2's output).
//!
//! - `execution_context` — the contexts a candidate runs in.
//! - `candidate` — a logical candidate's physical alternatives.
//! - `properties` — a candidate's physical properties (timing, lifetime).
//! - `edge` — the physical edges between candidates.
//! - `memo_candidates` — Stage 2's output.

mod candidate;
mod edge;
mod execution_context;
mod memo_candidates;
mod properties;

pub use candidate::{
    producer_satisfies_physical_requirements, PhysicalCandidate, PhysicalCandidateId,
};
pub use edge::{PhysicalEdge, PhysicalEdgeId};
pub use execution_context::{
    anchor_is_evaluation_time, queries_of_context, query_reads, ExecutionContext,
    ExecutionContextId,
};
pub use memo_candidates::{output_references, PhysicalASAPMemoCandidates};
pub use properties::{Lifetime, PhysicalProperties, PhysicalPropertiesError, StorageMedium};

//! The Stage 2 → Stage 3 boundary: the physical candidates Stage 2 builds
//! from the memo and Stage 3 selects from (#1047, #1054).

mod graph;
mod memo_candidates;
mod producers;

pub use graph::{
    DagOfSelectedCandidates, PhysicalCandidateId, ReaderConstraint, SelectedEdge, SelectedNodeId,
};
pub use memo_candidates::{
    output_references, CandidateForMemoGroupId, OperatorNodeOfCandidateForMemoGroup,
    OperatorNodeOfCandidateForMemoGroupId, PhysicalASAPMemoCandidates, PhysicalCandidate,
    PhysicalCandidateKey,
};
#[cfg(any(test, feature = "test-support"))]
pub use producers::{producer_storage_metrics, ProducerStorageMetrics};
pub use producers::{ProducerCandidates, ProducerSetInterner};

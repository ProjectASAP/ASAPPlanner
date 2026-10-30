//! The Stage 2 → Stage 3 boundary: the physical candidates Stage 2 builds
//! from the memo and Stage 3 selects from (#1047, #1054).

mod graph;
mod memo_candidates;
mod suppliers;

pub use graph::{
    validate_selected_connections, PhysicalCandidateId, ProducerConsumerConnection,
    QueryRootAssignment, ReaderConstraint, SelectedCandidateGraph,
};
pub use memo_candidates::{
    output_references, CandidateForMemoGroupId, OperatorNodeOfCandidateForMemoGroup,
    OperatorNodeOfCandidateForMemoGroupId, PhysicalASAPMemoCandidates, PhysicalCandidate,
    PruningMetrics,
};
pub use suppliers::{
    supplier_storage_metrics, SupplierCandidates, SupplierSetInterner, SupplierStorageMetrics,
};

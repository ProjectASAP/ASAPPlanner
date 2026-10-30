//! The Stage 1 → Stage 2 boundary: the memo of logical groups and their
//! implementation candidates that Stage 1 (Pass 1 and Pass 2) produces and
//! Stage 2 implements (#1047, #1054). The Stage 2 → Stage 3 boundary: the
//! physical candidates Stage 2 builds and Stage 3 selects from. Stage
//! crates never depend on each other, so the types more than one stage reads
//! live here.

mod candidates;
mod logical_memo_candidates;
mod memo;
mod physical;

pub use candidates::{CandidateForMemoGroup, CandidateMemo, OriginOfCandidateForMemoGroup};
pub use logical_memo_candidates::LogicalASAPMemoCandidates;
pub use memo::{
    queries_rooted_at, root_groups, CandidateGenerationTimeout, MemoGroup, MemoGroupId,
};
pub use physical::{
    output_references, supplier_storage_metrics, validate_selected_connections,
    CandidateForMemoGroupId, OperatorNodeOfCandidateForMemoGroup,
    OperatorNodeOfCandidateForMemoGroupId, PhysicalASAPMemoCandidates, PhysicalCandidate,
    PhysicalCandidateId, ProducerConsumerConnection, PruningMetrics, QueryRootAssignment,
    ReaderConstraint, SelectedCandidateGraph, SupplierCandidates, SupplierSetInterner,
    SupplierStorageMetrics,
};

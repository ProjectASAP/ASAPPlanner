//! The Stage 0 → Stage 1 boundary: the workload's memo groups
//! ([`DagOfLogicalMemoGroups`]). Inside Stage 1, Pass 1 → Pass 2: the memo
//! groups with their Pass 1 candidates
//! ([`PerComputationLogicalMemoCandidates`]). The Stage 1 → Stage 2 boundary: the memo of logical groups and their
//! implementation candidates that Stage 1 (Pass 1 and Pass 2) produces and
//! Stage 2 implements (#1047, #1054). The Stage 2 → Stage 3 boundary: the
//! physical candidates Stage 2 builds and Stage 3 selects from. Stage
//! crates never depend on each other, so the types more than one stage reads
//! live here.

mod candidates;
mod logical_memo_candidates;
mod memo;
mod per_computation_logical_memo_candidates;
mod physical;
mod pruning;
mod realizer;

pub use candidates::{CandidateForMemoGroup, MemoCandidates, OriginOfCandidateForMemoGroup};
pub use logical_memo_candidates::LogicalASAPMemoCandidates;
pub use memo::{
    queries_rooted_at, root_groups, CandidateGenerationTimeout, DagOfLogicalMemoGroups, MemoGroup,
    MemoGroupId,
};
pub use per_computation_logical_memo_candidates::{
    ComputationTarget, FusedComputation, HavingEntry, HavingPlacement,
    HowToComputeAggregateMeasure, PerComputationLogicalMemoCandidates,
};
pub use physical::{
    output_references, producer_storage_metrics, CandidateForMemoGroupId, DagOfSelectedCandidates,
    OperatorNodeOfCandidateForMemoGroup, OperatorNodeOfCandidateForMemoGroupId,
    PhysicalASAPMemoCandidates, PhysicalCandidate, PhysicalCandidateId, PhysicalCandidateKey,
    ProducerCandidates, ProducerSetInterner, ProducerStorageMetrics, ReaderConstraint,
    SelectedEdge, SelectedNodeId,
};
pub use pruning::{PruneReason, PrunedCandidate};
pub use realizer::{AssignmentRealizer, PhysicalCandidateError, RealizedPlan};

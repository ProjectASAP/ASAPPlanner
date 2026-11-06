//! The Stage 0 → Stage 1 boundary: the workload's memo groups, without
//! candidates ([`Memo`]). Inside Stage 1, Pass 1 → Pass 2: the memo with
//! its Pass 1 candidates. The Stage 1 → Stage 2 boundary: the memo of logical groups and their
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

pub use candidates::{
    parameter_list, satisfies_logical, CandidateExpression, CandidateForMemoGroup,
    CandidateForMemoGroupId, ErrorPropagation, InvalidLogicalProperties, LogicalProperties,
    LogicalRequirement, Memo, OriginOfCandidateForMemoGroup, WindowLayout,
};
pub use logical_memo_candidates::{LogicalASAPMemoCandidates, LogicalEdge};
pub use memo::{queries_rooted_at, root_groups, MemoGroup, MemoGroupId};
pub use per_computation_logical_memo_candidates::{
    ComputationTarget, FusedComputation, HavingEntry, HavingPlacement, HowToComputeAggregateMeasure,
};
pub use physical::{
    output_references, satisfies_physical, DagOfSelectedCandidates, PhysicalASAPMemoCandidates,
    PhysicalCandidate, PhysicalCandidateId, PhysicalCandidateKey, PhysicalEdge, PhysicalEdgeId,
    SelectedEdge, SelectedNodeId,
};
pub use pruning::{PruneReason, PrunedCandidate};
pub use realizer::{AssignmentRealizer, PhysicalCandidateError, RealizedPlan};

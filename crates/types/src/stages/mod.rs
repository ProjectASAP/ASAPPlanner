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
    parameter_list, producer_satisfies_logical_requirements, strongest, weakest, CandidateError,
    CandidateExpression, CandidateForMemoGroup, CandidateForMemoGroupId, ErrorPropagation,
    LogicalProperties, LogicalPropertiesError, LogicalRequirements, Memo,
    OriginOfCandidateForMemoGroup,
};
pub use logical_memo_candidates::{
    LogicalASAPMemoCandidates, LogicalEdge, PruneCause, PrunedLogicalCandidate, RootPruneReason,
};
pub(crate) use memo::MemoGroupAsInput;
pub use memo::{queries_rooted_at, root_groups, GroupProperties, MemoGroup, MemoGroupId};
pub use per_computation_logical_memo_candidates::{
    ComputationTarget, FusedComputation, HavingEntry, HavingPlacement, HowToComputeAggregateMeasure,
};
pub use physical::{
    output_references, producer_satisfies_physical_requirements, DagOfSelectedCandidates,
    ExecutionContext, ExecutionContextId, PhysicalASAPMemoCandidates, PhysicalCandidate,
    PhysicalCandidateId, PhysicalEdge, PhysicalEdgeId, RefreshWindowId, SelectedEdge,
    SelectedNodeId,
};
pub use pruning::{PruneReason, PrunedCandidate};
pub use realizer::{AssignmentRealizer, PhysicalCandidateError, RealizedPlan};

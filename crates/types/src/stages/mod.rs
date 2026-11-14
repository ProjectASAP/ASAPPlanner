//! The types one planning stage hands the next, so that stage crates never
//! depend on each other:
//!
//! - `logical` — Stage 0's and Stage 1's memo ([`Memo`]: groups, queries,
//!   candidates and their edges), and Stage 1's output
//!   ([`LogicalASAPMemoCandidates`]) that Stage 2 implements.
//! - `physical` — Stage 2's output ([`PhysicalASAPMemoCandidates`]): the
//!   physical candidates and edges Stage 3 selects from.
//! - `pruning` — what each stage removed and why ([`PrunedCandidate`],
//!   [`PruneReason`]), one shape for every stage.

mod logical;
mod physical;
mod pruning;

pub use logical::{
    listed_build_parameters, ranked_column_of_group, AggregateCoverage, AggregateDefinition,
    CandidateError, CandidateExpression, CandidateForMemoGroup, CandidateForMemoGroupId,
    ColumnLineage, CoverageError, EvaluationGrid, GroupProperties, LogicalASAPMemoCandidates,
    LogicalEdge, LogicalProperties, LogicalPropertiesError, LogicalRequirements, Memo, MemoGroup,
    MemoGroupError, MemoGroupId, MemoQuery, OriginOfCandidateForMemoGroup, RequiredAccuracy,
    ValueSign,
};
pub use physical::{
    anchor_is_evaluation_time, output_references, producer_satisfies_physical_requirements,
    queries_of_context, query_reads, ExecutionContext, ExecutionContextId, Lifetime,
    PhysicalASAPMemoCandidates, PhysicalCandidate, PhysicalCandidateId, PhysicalEdge,
    PhysicalEdgeId, PhysicalProperties, PhysicalPropertiesError, StorageMedium,
};
pub use pruning::{
    PruneCause, PruneReason, PrunedCandidate, PrunedLogicalCandidate, RootPruneReason,
};

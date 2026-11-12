//! The logical side: Stage 0's and Stage 1's memo, by scope.
//!
//! - `memo` — the memo and its queries.
//! - `group` — a memo group and the properties its candidates share
//!   (column lineage and sign, aggregate coverage).
//! - `candidate` — a group's candidates.
//! - `requirements` — what a candidate requires of each input.
//! - `edge` — the logical edges between candidates.
//! - `memo_candidates` — Stage 1's output.

mod candidate;
mod edge;
mod group;
mod memo;
mod memo_candidates;
mod requirements;

pub use candidate::{
    listed_build_parameters, CandidateError, CandidateExpression, CandidateForMemoGroup,
    CandidateForMemoGroupId, LogicalProperties, LogicalPropertiesError,
    OriginOfCandidateForMemoGroup,
};
pub use edge::LogicalEdge;
pub use group::{
    ranked_column_of_group, raw_data_samples_of_metric_of_group, AggregateCoverage,
    AggregateDefinition, ColumnLineage, CoverageError, GroupProperties, MemoGroup, MemoGroupError,
    MemoGroupId, ValueSign,
};
pub use memo::{Memo, MemoQuery};
pub use memo_candidates::LogicalASAPMemoCandidates;
pub use requirements::{LogicalRequirements, RequiredAccuracy};

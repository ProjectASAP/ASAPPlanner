//! The logical side: Stage 0's and Stage 1's memo, by scope (memo → group
//! → candidate → candidate input → edge → Stage 1's output).

mod aggregate_coverage;
mod candidate;
mod column_properties;
mod edge;
mod group;
mod memo;
mod memo_candidates;
mod requirements;

pub use aggregate_coverage::{AggregateCoverage, AggregateDefinition, CoverageError};
pub use candidate::{
    CandidateError, CandidateExpression, CandidateForMemoGroup, CandidateForMemoGroupId,
    LogicalProperties, LogicalPropertiesError, OriginOfCandidateForMemoGroup,
};
pub use column_properties::ValueSign;
pub use edge::LogicalEdge;
use group::MemoGroupAsInput;
pub use group::{GroupProperties, MemoGroup, MemoGroupId};
pub use memo::{Memo, MemoQuery};
pub use memo_candidates::LogicalASAPMemoCandidates;
pub use requirements::{LogicalRequirements, RequiredAccuracy};

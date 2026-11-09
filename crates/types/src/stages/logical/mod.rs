//! The logical side: Stage 0's and Stage 1's memo, by scope (memo → group
//! → candidate → candidate input → edge → Stage 1's output).

mod candidate;
mod column_properties;
mod edge;
mod group;
mod memo;
mod memo_candidates;
mod requirements;

pub use candidate::{
    CandidateError, CandidateExpression, CandidateForMemoGroup, CandidateForMemoGroupId,
    LogicalProperties, LogicalPropertiesError, OriginOfCandidateForMemoGroup,
};
pub use edge::LogicalEdge;
pub(crate) use group::MemoGroupAsInput;
pub use group::{GroupProperties, MemoGroup, MemoGroupId, ValueSign};
pub use memo::{queries_rooted_at, root_groups, Memo, MemoQuery};
pub use memo_candidates::LogicalASAPMemoCandidates;
pub use requirements::{LogicalRequirements, RequiredAccuracy};

//! The Stage 1 → Stage 2 boundary: the memo of logical groups and their
//! implementation candidates that Stage 1 (Pass 1 and Pass 2) produces and
//! Stage 2 implements (#1047, #1054). Stage crates never depend on each
//! other, so the types more than one stage reads live here.

mod candidates;
mod logical_memo_candidates;
mod memo;

pub use candidates::{CandidateForMemoGroup, CandidateMemo, OriginOfCandidateForMemoGroup};
pub use logical_memo_candidates::LogicalASAPMemoCandidates;
pub use memo::{
    queries_rooted_at, root_groups, CandidateGenerationTimeout, MemoGroup, MemoGroupId,
};

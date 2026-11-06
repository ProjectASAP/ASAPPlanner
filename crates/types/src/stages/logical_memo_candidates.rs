//! Stage 1, Pass 2's output, which Stage 2 expands: the memo after every
//! rule, and the logical edges $(u, c, i)$ with $\alpha^{\log}$ (#1201).

use super::{CandidateForMemoGroupId, Memo};

/// $(u, c, i)$ over logical candidates: `producer` is a candidate of
/// $G_{c,i}$, the group input `input_index_of_consumer` of `consumer` reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct LogicalEdge {
    pub producer: CandidateForMemoGroupId,
    pub consumer: CandidateForMemoGroupId,
    pub input_index_of_consumer: usize,
    /// $\alpha^{\log}_{u,c,i}$.
    pub satisfies_logical_requirement: bool,
}

/// Stage 1, Pass 2's output.
#[derive(Debug, Clone)]
pub struct LogicalASAPMemoCandidates {
    /// The memo after the last rule.
    pub memo: Memo,
    /// Every $(u, c, i)$ with $u \in G_{c,i}$, grouped by $(c, i)$.
    pub edges: Vec<LogicalEdge>,
}

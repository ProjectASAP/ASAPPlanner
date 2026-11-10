//! Stage 1, Pass 2's output, which Stage 2 expands: the memo after every
//! rule, the logical edges and the candidates dead-candidate elimination
//! removed.

use crate::stages::{EvaluationGrid, LogicalEdge, Memo, PrunedLogicalCandidate};

/// Stage 1, Pass 2's output.
#[derive(Debug, Clone)]
pub struct LogicalASAPMemoCandidates {
    /// The memo after the last rule.
    pub memo: Memo,
    /// Every $(u, c, i)$ with $u \in G_{c,i}$ between candidates that
    /// survive dead-candidate elimination, grouped by $(c, i)$.
    pub edges: Vec<LogicalEdge>,
    /// The candidates dead-candidate elimination removed: no logical edge
    /// names them, and Stage 2 does not expand them.
    pub pruned: Vec<PrunedLogicalCandidate>,
    /// Per group, by `MemoGroupId`: the grid a result of the group is
    /// evaluated on, the coarsest grid holding every evaluation time of the
    /// queries reaching it (X = `sum_over_time(m[5m])` read every 60 s and
    /// every 120 s: 60 s); `None` when those times are not on one known
    /// grid. A result of the group materialized across evaluations is
    /// stored at these times.
    pub evaluation_grid_of_group: Vec<Option<EvaluationGrid>>,
}

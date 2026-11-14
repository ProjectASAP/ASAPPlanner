//! Stage 1, Pass 2's output, which Stage 2 expands: the memo after every
//! rule, the logical edges and the candidates dead-candidate elimination
//! removed.

use std::collections::BTreeSet;

use crate::stages::{
    CandidateForMemoGroupId, LogicalEdge, Memo, MemoGroupId, PrunedLogicalCandidate,
};

/// The evaluation times `phase_ms + k · period_ms`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct EvaluationGrid {
    pub period_ms: u64,
    pub phase_ms: u64,
}

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
    /// every 120 s: 60 s; below a PromQL subquery `[10m:30s]`, its 30 s
    /// steps); `None` when those times are not on one known grid (below a
    /// subquery without a resolution). A result of the group materialized across evaluations is
    /// stored at these times.
    pub evaluation_grid_of_group: Vec<Option<EvaluationGrid>>,
    /// Indexed by query: the groups this query reaches through a consumer with no
    /// error contract for its metric; in these groups the query may only use exact candidates.
    /// Q0 = `quantile_over_time(0.9, m[5m]) / quantile_over_time(0.5,
    /// m[5m])` and Q1 = the same ratio `> 1`, both relative 0.05: Q0 may
    /// read the DDSketch readouts, while Q1's comparison has no contract,
    /// so its set holds the ratio group and every group below it. (Both
    /// list the rows below a sketch build, which has no contract.) A query
    /// whose whole-result failure probability has no per-value one ($n_q$
    /// unknown) also lists each readout group another query gave a level,
    /// with the groups below it. A group is listed for a query, not
    /// pruned: other queries may still read its approximate candidates, so
    /// Stage 3 enforces it per query, in each query's selection context
    /// (its MILP's constraint (X)); Pass 2 only lists it. Empty for a query without an accuracy
    /// target.
    pub exact_only_groups_of_query: Vec<BTreeSet<MemoGroupId>>,
}

impl LogicalASAPMemoCandidates {
    /// Whether query `query` must not read `candidate`: it is not exact
    /// (it has sketch parameters θ) and its group is one the query may read
    /// only exactly ([`Self::exact_only_groups_of_query`]). A readout's own
    /// candidate has no θ, but the sketch states it reads are in the same
    /// list, as every group below a listed one is.
    ///
    /// Example: `quantile_over_time(0.9, m[5m])` at a relative target lists
    /// the quantile and every group below it; its KLL state builds (θ) are
    /// candidates it must not read, the exact quantile one it may.
    pub fn query_must_not_read(&self, query: usize, candidate: CandidateForMemoGroupId) -> bool {
        self.exact_only_groups_of_query
            .get(query)
            .is_some_and(|groups| groups.contains(&candidate.group))
            && self
                .memo
                .candidate(candidate)
                .logical_properties
                .parameters()
                .is_some()
    }

    /// The times `candidate` is stored at when materialized across
    /// evaluations, on its own schedule: every window end of a chain (its
    /// width at phase 0), else its group's evaluation grid
    /// ([`Self::evaluation_grid_of_group`]); `None` when neither is known.
    pub fn own_schedule(&self, candidate: CandidateForMemoGroupId) -> Option<EvaluationGrid> {
        match self
            .memo
            .candidate(candidate)
            .expression
            .operator
            .chain_tumbling_window_width_ms()
        {
            Some(width_ms) => Some(EvaluationGrid {
                period_ms: width_ms,
                phase_ms: 0,
            }),
            None => self.evaluation_grid_of_group[candidate.group.0 as usize],
        }
    }
}

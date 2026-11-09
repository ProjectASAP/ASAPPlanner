//! Stage 1, Pass 2's output, which Stage 2 expands: the memo after every
//! rule, the logical edges $(u, c, i)$ with $\alpha^{\log}$ (#1201), and
//! the candidates dead-candidate elimination removed, each with its root
//! causes.

use super::{CandidateForMemoGroupId, EvaluationGrid, Memo, RequiredAccuracy};
use crate::ir::properties::AccuracyMetric;

/// $(u, c, i)$ over logical candidates: `producer_candidate_id` is a
/// candidate of $G_{c,i}$, the group input `input_index_of_consumer` of
/// `consumer_candidate_id` reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct LogicalEdge {
    pub producer_candidate_id: CandidateForMemoGroupId,
    pub consumer_candidate_id: CandidateForMemoGroupId,
    pub input_index_of_consumer: usize,
    /// $\alpha^{\log}_{u,c,i}$.
    pub satisfies_logical_requirement: bool,
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
    /// every 120 s: 60 s); `None` when those times are not on one known
    /// grid. A result of the group materialized across evaluations is
    /// stored at these times.
    pub evaluation_grid_of_group: Vec<Option<EvaluationGrid>>,
}

/// A candidate eliminated by Pass 2.
#[derive(Debug, Clone)]
pub struct PrunedLogicalCandidate {
    pub candidate: CandidateForMemoGroupId,
    /// Every cause: one per failing edge it is cut off by, so mixed causes
    /// (a missed requirement on one edge, the metric rule on another) are
    /// all listed.
    pub causes: Vec<PruneCause>,
}

/// One path from the candidate, through candidates that are themselves
/// eliminated, to the edge that fails. One shortest path per failing edge.
#[derive(Debug, Clone)]
pub struct PruneCause {
    /// The eliminated candidates on the path, nearest first: consumers
    /// above, or, for a candidate cut off from an input, producers below.
    pub through_pruned_candidates: Vec<CandidateForMemoGroupId>,
    pub root: RootPruneReason,
}

/// Why the edge at the end of a [`PruneCause`]'s path fails, or why the
/// candidate has no edge at all. JSON: the snake_case variant name, e.g.
/// `{"below_required_accuracy": {...}}` or `"no_consumer"`.
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RootPruneReason {
    /// The producer's guarantee, at the required failure probability,
    /// exceeds the consumer input's required error.
    BelowRequiredAccuracy {
        consumer: CandidateForMemoGroupId,
        input: usize,
        required: RequiredAccuracy,
    },
    /// The metric rule: the producer is a readout whose metric the
    /// consumer does not accept on that input; or, on a readout's own input,
    /// the readout's metric is not one its output accepts.
    MetricRule {
        consumer: CandidateForMemoGroupId,
        input: usize,
        producer_metric: AccuracyMetric,
        /// The metrics accepted on that input (on a readout's own input: at
        /// its output).
        accepted_metrics: Vec<AccuracyMetric>,
    },
    /// The consumer is a top-k readout whose `k` exceeds the producer's
    /// heap size: the heap cannot hold the k ranked keys, whatever accuracy
    /// is required.
    HeapSmallerThanTopK {
        consumer: CandidateForMemoGroupId,
        input: usize,
        k: usize,
    },
    /// The readout `consumer`, which a query with an accuracy target
    /// reaches, reads a statistic its producer's summary type has no
    /// guarantee for in any metric (e.g. a distinct count from UnivMon):
    /// its error has no bound.
    NoGuarantee {
        consumer: CandidateForMemoGroupId,
        input: usize,
    },
    /// The readout `consumer` is reached by query `query`, whose target sets
    /// a failure probability over the whole result while the number of
    /// values one evaluation returns ($n_q$) is unknown: no per-value
    /// failure probability follows, so no state meets it.
    ValuesPerEvaluationUnknown {
        consumer: CandidateForMemoGroupId,
        query: usize,
    },
    /// The candidate never had a consumer and is not a query output. A
    /// candidate whose consumers were eliminated is traced to their causes.
    NoConsumer,
}

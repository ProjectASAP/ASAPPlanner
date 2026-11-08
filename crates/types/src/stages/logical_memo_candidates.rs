//! Stage 1, Pass 2's output, which Stage 2 expands: the memo after every
//! rule, the logical edges $(u, c, i)$ with $\alpha^{\log}$ (#1201), and
//! the candidates dead-candidate elimination removed, each with its root
//! causes.

use super::{CandidateForMemoGroupId, Memo, RequiredAccuracy};
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

/// One path from the candidate, through consumers that are themselves
/// eliminated, to the edge that fails. One shortest path per failing edge.
#[derive(Debug, Clone)]
pub struct PruneCause {
    /// The eliminated consumers on the path, nearest first.
    pub through_pruned_consumers: Vec<CandidateForMemoGroupId>,
    pub root: RootPruneReason,
}

/// Why the edge at the end of a [`PruneCause`]'s path fails, or why the
/// candidate has no edge at all.
#[derive(Debug, Clone, serde::Serialize)]
pub enum RootPruneReason {
    /// The producer's guarantee, at the required failure probability,
    /// exceeds the consumer input's required error.
    BelowRequiredAccuracy {
        consumer: CandidateForMemoGroupId,
        input: usize,
        required: RequiredAccuracy,
    },
    /// The consumer's error contract carries none of the producer's metrics.
    MetricRule {
        consumer: CandidateForMemoGroupId,
        input: usize,
        producer_metric: AccuracyMetric,
        consumer_carries: Vec<AccuracyMetric>,
    },
    /// The candidate never had a consumer and is not a query output. A
    /// candidate whose consumers were eliminated is traced to their causes.
    NoConsumer,
}

//! What a stage removed, and why: one shape for every stage (#1047 P6).
//! Each stage removes only what its own knowledge rules out (option C):
//! Stage 2 what cannot be built, run or read by a query output. Stage 0 fails with an
//! error, Pass 1 records nothing it leaves out (#1205 will show it), and
//! Pass 2 records its eliminated candidates with their own causes
//! (`PrunedLogicalCandidate`). Stage 3 prunes before
//! its selection MILP (dominated and uncostable candidates); its
//! rejections of whole plans keep their own cause (`RejectionCause` in plan
//! selection). The planner does not check whether an executor implements an
//! operator or summary type, so no reason names it.

use crate::accuracy::AccuracyMetric;
use crate::stages::{CandidateForMemoGroupId, PhysicalCandidateId, RequiredAccuracy};

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

/// One candidate a stage removed, and why: Stage 2 and Stage 3 record the
/// [`PhysicalCandidate`](super::PhysicalCandidate). The candidate names its
/// group through its logical candidate, so there is no separate group field.
#[derive(Debug, Clone, PartialEq)]
pub struct PrunedCandidate<C> {
    pub candidate: C,
    pub reason: PruneReason,
}

/// Why a stage removed a candidate: first Stage 2's (the physical candidate
/// cannot be built, run or read by a query output), then Stage 3's, before
/// its selection MILP.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize)]
pub enum PruneReason {
    /// Stage 2: it runs at ingestion time, and the source has no ingestion
    /// pipeline (its data is at rest or its arrival unknown).
    NoIngestionPipeline,
    /// Stage 2: it scans raw data at query time, and the deployment does
    /// not retain raw data, so no window of it is there to scan.
    RawDataNotRetained,
    /// Stage 2: it is a query-time result materialized (`OneEvaluation` or
    /// `AcrossEvaluations`), and the deployment retains no query-time
    /// results.
    QueryTimeResultsNotRetained,
    /// Stage 2: one of its inputs has no producer candidate that can be
    /// built: none is left, or every one depends, directly or through a
    /// cycle, on an input that cannot be built.
    InputHasNoBuildableProducer,
    /// Stage 2: no query output can read it, directly or through other
    /// candidates, once each candidate's allowed consumers are applied.
    ReachesNoQueryOutput,
    /// Stage 2: a Count-Min build (`Cms`, `CmsWithHeap`, `HydraCms`) whose
    /// weight column's `column_signs` is not `NonNegative`: its estimates are
    /// bounds only for non-negative weights.
    CountMinWeightsNotProvenNonNegative,
    /// Stage 3: the candidate it names, in the same memo group with the
    /// same producers and consumers, is at least as good in every respect
    /// (guarantee, usage, latency, output, held state), so no optimal plan
    /// needs this one.
    DominatedBy(PhysicalCandidateId),
    /// Stage 3: `asap-cost` could not cost it, so the selection MILP leaves
    /// it out.
    UsageNotCostable,
}

//! What a stage removed, and why: one shape for every stage. Each stage
//! removes only what its own knowledge rules out. Pass 2 records its
//! eliminated logical candidates with their causes ([`PrunedLogicalCandidate`],
//! [`PruneCause`], [`RootPruneReason`]); Stage 2 records the physical
//! candidates that cannot be built, run or read by a query output
//! ([`PrunedCandidate`], [`PruneReason`]); Stage 3 prunes dominated and
//! uncostable candidates before its selection, and its rejections of whole
//! plans keep their own cause (`RejectionCause` in plan selection). Stage 0
//! fails with an error and Pass 1 records nothing it leaves out. The planner
//! does not check whether an executor implements an operator or summary
//! type, so no reason names it.

use crate::accuracy::AccuracyMetric;
use crate::deployment_capabilities::ExclusionSource;
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
    /// The producer is a readout with an accuracy metric but no guarantee
    /// evaluable at the required failure probability (an HLL distinct
    /// count, whose failure probability is unknown), or with no guarantee
    /// at all (UnivMon's distinct count), read by a query with an accuracy
    /// target: its error has no bound. Two more cases: a DDSketch quantile
    /// over values of unknown sign (its relative guarantee holds only over
    /// values of one sign), and any sketch readout under an exact target
    /// (δ 0), which no probabilistic guarantee meets.
    NoGuarantee {
        consumer: CandidateForMemoGroupId,
        input: usize,
    },
    /// The readout `consumer` is reached by query `query`, whose target sets
    /// a failure probability over the whole result while the number of
    /// values one evaluation returns ($n_q$) is unknown: no per-value
    /// failure probability follows, so no state meets it; and no other
    /// query reaching it gives it a level.
    ValuesPerEvaluationUnknown {
        consumer: CandidateForMemoGroupId,
        query: usize,
    },
    /// The producer is a candidate over a tumbling-window chain whose width
    /// does not divide the consumer group's evaluation period (or the group
    /// has no known evaluation grid): the chain's windows do not end at the
    /// consumer's evaluation times.
    ChainWidthDoesNotDivideEvaluationGrid {
        consumer: CandidateForMemoGroupId,
        input: usize,
        tumbling_window_width_ms: u64,
        evaluation_period_ms: Option<u64>,
    },
    /// The consumer's input has no producer edge: its group has no
    /// candidate (e.g. a Count-Min state over weights not proven
    /// non-negative, for which `listed_build_parameters` lists no build),
    /// or none with the θ the consumer reads.
    InputHasNoProducer {
        consumer: CandidateForMemoGroupId,
        input: usize,
    },
    /// Search-space pruning rule 1 (`SearchSpacePruning::threshold_theta`):
    /// the producer's θ is larger than the least θ of its shape that also
    /// meets this consumer input (the reader's threshold), and no two
    /// readers of its group need it to share one state.
    AboveReaderThreshold {
        consumer: CandidateForMemoGroupId,
        input: usize,
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
/// its selection MILP. JSON: the snake_case variant name, e.g.
/// `"no_ingestion_pipeline"`, `{"excluded_by_deployment":
/// "raw_data_not_retained"}` or `{"dominated_by": 3}`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PruneReason {
    /// Stage 2: it runs at ingestion time, and the source has no ingestion
    /// pipeline (its data is at rest or its arrival unknown).
    NoIngestionPipeline,
    /// Stage 2: one of its inputs has no producer candidate that can be
    /// built: none is left, or every one depends, directly or through a
    /// cycle, on an input that cannot be built.
    InputHasNoBuildableProducer,
    /// Stage 2: no query output can read it, directly or through other
    /// candidates, once each candidate's allowed consumers are applied.
    ReachesNoQueryOutput,
    /// Stage 2: a Count-Min build (`CountMinSketch`, `CountMinSketchWithHeap`, `HydraCountMinSketch`) whose
    /// weight column's `column_signs` is not `NonNegative`: its estimates are
    /// bounds only for non-negative weights. `listed_build_parameters`
    /// lists no such build, so Stage 2 prunes one only if a later rule adds
    /// it.
    CountMinWeightsNotProvenNonNegative,
    /// Stage 2: the deployment's capabilities exclude it
    /// (`DeploymentCapabilities::exclusions`): the executor cannot run it,
    /// or the caller ruled it out. Names the capability the matching
    /// exclusion comes from, e.g. `RawDataNotRetained` for a query-time
    /// scan when raw data is not retained.
    ExcludedByDeployment(ExclusionSource),
    /// Stage 3: the candidate it names, in the same memo group with the
    /// same producers and consumers, is at least as good in every respect
    /// (guarantee, usage, latency, output, held state), so no optimal plan
    /// needs this one.
    DominatedBy(PhysicalCandidateId),
    /// Stage 3: `asap-cost` could not cost it, so the selection MILP leaves
    /// it out.
    UsageNotCostable,
    /// Stage 3: a query-time computation (a summary build, an aggregate)
    /// that would read `rows` raw rows
    /// where `rows_in_range` arrive over the range it needs, because the
    /// source holds no more (its `rows_held`): it would answer from fewer
    /// samples than its query asks for. A data fact, not a bug; an
    /// ingestion-time build sees every row. Example: a source holding 80
    /// rows read back 60 s at 8 rows a second: `{rows: 80, rows_in_range:
    /// 480}`.
    ReadsFewerSourceRowsThanItsRange { rows: u64, rows_in_range: u64 },
}

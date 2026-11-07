//! What a stage removed, and why: one shape for every stage (#1047 P6).
//! Each stage removes only what its own knowledge rules out (option C):
//! Stage 2 what cannot be built, run or read by a query output. Stage 0 fails with an
//! error, Pass 1 records nothing it leaves out (#1205 will show it), and
//! Pass 2 records its eliminated candidates with their own causes
//! (`PrunedLogicalCandidate`). Stage 3 prunes before
//! its selection MILP (dominated and uncostable candidates); its
//! rejections of whole plans keep their own cause (`RejectionCause` in plan
//! selection). The planner does not check whether an executor implements an
//! operator or family, so no reason names it.

use super::PhysicalCandidateId;

/// One candidate a stage removed, and why. `C` is the stage's candidate
/// type: Stage 2's [`PhysicalCandidate`](super::PhysicalCandidate), Stage
/// 3's [`PhysicalCandidateId`]. The candidate names its group through its
/// logical candidate, so there is no separate group field.
#[derive(Debug, Clone, PartialEq)]
pub struct PrunedCandidate<C> {
    pub candidate: C,
    pub reason: PruneReason,
}

/// Why a stage removed a candidate: first Stage 2's (the physical candidate
/// cannot be built, run or read by a query output), then Stage 3's, before
/// its selection MILP.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
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
    /// Stage 3: the candidate it names, in the same memo group with the
    /// same producers and consumers, is at least as good in every respect
    /// (guarantee, usage, latency, output, held state), so no optimal plan
    /// needs this one.
    DominatedBy(PhysicalCandidateId),
    /// Stage 3: `asap-cost` could not cost it, so the selection MILP leaves
    /// it out.
    UsageNotCostable,
}

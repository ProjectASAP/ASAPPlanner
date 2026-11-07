//! What a stage removed, and why: one shape for every stage (#1047 P6).
//! Each stage removes only what its own knowledge rules out (option C):
//! Stage 2 what cannot be built, run or read by a query output. Stage 0 fails with an
//! error, Pass 1 records nothing it leaves out (#1205 will show it), and
//! Pass 2 only adds candidates. Stage 3 prunes before
//! its selection MILP (dominated and uncostable candidates); its
//! rejections of whole plans keep their own cause (`RejectionCause` in plan
//! selection).

use super::{MemoGroupId, PhysicalCandidateId};

/// One candidate a stage removed from memo group `group`, and why. `C` is
/// the stage's candidate type: Stage 2's
/// [`PhysicalCandidateKey`](super::PhysicalCandidateKey), Stage 3's
/// [`PhysicalCandidateId`].
#[derive(Debug, Clone, PartialEq)]
pub struct PrunedCandidate<C> {
    pub group: MemoGroupId,
    pub candidate: C,
    pub reason: PruneReason,
}

/// Why a stage removed a candidate: first Stage 2's (the physical candidate
/// cannot be built, run or read by a query output), then Stage 3's, before
/// its selection MILP.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PruneReason {
    /// Stage 2: the executor cannot run its operator, an exact per-entity
    /// `Aggregate` (a per-series range reduction over raw data samples)
    /// with more than one measure, a measure without an exact per-entity
    /// kernel (e.g. `stddev_over_time`), a measure filter or a `HAVING`
    /// (`agg_has_exact_per_entity_kernel`). Every timing and lifetime of
    /// the candidate is pruned.
    PerEntityAggregateNotExecutable,
    /// Stage 2, decision-tree level 4: it runs at ingestion time, and the
    /// data workload has no ingestion pipeline (its data is at rest or its
    /// arrival unknown).
    NoIngestionPipeline,
    /// Stage 2: it scans raw data at query time, and the deployment does
    /// not retain raw data, so no window of it is there to scan.
    RawDataNotRetained,
    /// Stage 2, decision-tree level 5: it is a query-time result
    /// materialized across evaluations, and the deployment keeps no
    /// query-time results.
    QueryTimeResultsNotRetained,
    /// Stage 2, decision-tree level 5: it is materialized across
    /// evaluations without a window layout. Only a tumbling-window grid
    /// says when such a result can be read (the concepts doc's alignment
    /// table, section 2), so it is not offered yet.
    AcrossEvaluationsWithoutWindowLayout,
    /// Stage 2: one of its inputs has no admissible producer that can be
    /// built: none is left, or every one depends, directly or through a
    /// cycle, on an input that cannot be built.
    InputHasNoBuildableProducer,
    /// Stage 2: no query output can read it, directly or through other
    /// candidates, over admissible edges.
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

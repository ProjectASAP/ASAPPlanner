//! What a stage removed, and why: one shape for every stage (#1047 P6).
//! Each stage removes only what its own knowledge rules out (option C):
//! Pass 1 what the measure or the deployment cannot compute, Stage 2 what
//! cannot be implemented, built or kept. Stage 0 fails with an error and
//! Pass 2 only adds candidates, so neither prunes. Stage 3 prunes before
//! its selection MILP (dominated and uncostable candidates); its
//! rejections of whole plans keep their own cause (`RejectionCause` in plan
//! selection).

use super::{MemoGroupId, PhysicalCandidateId};

/// One candidate a stage removed from memo group `group`, and why. `C` is
/// the stage's candidate type: Pass 1's way to compute a measure
/// (`HowToComputeAggregateMeasure`), Stage 2's
/// [`PhysicalCandidateKey`](super::PhysicalCandidateKey).
#[derive(Debug, Clone, PartialEq)]
pub struct PrunedCandidate<C> {
    pub group: MemoGroupId,
    pub candidate: C,
    pub reason: PruneReason,
}

/// Why a stage removed a candidate. The first seven are Pass 1's: the
/// candidate cannot compute its measure, the deployment cannot run it, or
/// no query that reads it can meet its accuracy target with it.
/// Then Stage 2's: the physical candidate cannot be built, used or
/// kept. The last two are Stage 3's, before its selection MILP.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PruneReason {
    /// Pass 1: the measure requires an exact answer and the summary is
    /// approximate.
    RequiresExact,
    /// Pass 1: `count` counts rows, so a possibly-null `avg` input changes
    /// the result.
    NullableAvgInput,
    /// Pass 1: frequency sketches do not implement SQL row-count updates.
    /// The exact counter is kept; a fused raw-row top-k has its own keyed
    /// update contract.
    NoRelationalCountSketchRealization,
    /// Pass 1: the deployment's executor cannot build the summary's family
    /// or answer the measure's readout from it (`summary_supported` in
    /// Stage 1).
    UnsupportedByDeployment,
    /// Pass 1: the quantile's φ is outside [0, 1], where PromQL answers -Inf
    /// or +Inf: no sketch estimates that, and the executor's sketches refuse
    /// such a φ, so only the exact answer remains.
    QuantileOutsideUnitInterval,
    /// Pass 1: a PromQL top-k's heap (Count-Min or CountSketch with a heap)
    /// over its own input, an instant vector with one row per ranked series
    /// or group: the heap reads every row and adds nothing to it, so an
    /// exact top-k over the same rows, which keeps only `k` of them, is
    /// exact and no costlier. Only the whole-expression heap over the inner
    /// aggregate's input, where an item's rows repeat, is offered (Pass 1's
    /// fused computation).
    TopKHeapOverOneRowPerItem,
    /// Pass 1: for every query that reads its memo group, the query's
    /// output has no error bound that can meet the query's accuracy target
    /// at any summary size: guarantee propagation has no rule above it
    /// (e.g. a rank error through `avg`, or a value `HAVING` above an
    /// estimate), its bound stays unknown once the data workload's
    /// declarations are applied (e.g. a relative error through `avg` over
    /// samples not declared non-negative), its bound is in a metric the
    /// target does not accept (the deployment's accuracy model decides), or
    /// its failure probability is per value and the target's is over a
    /// whole result whose number of values the data does not bound. Kept
    /// when another aggregate over the same source offers the same family,
    /// which Pass 2 may build from it. Pruned only when the deployment's
    /// accuracy model and evidence cannot accept an output without its
    /// guarantee.
    NoErrorBoundForQueryTarget,
    /// Stage 2: the executor cannot run its operator node, an exact
    /// per-entity `Aggregate` (a per-series range reduction over raw data
    /// samples) with more than one measure, a measure without an exact
    /// per-entity kernel (e.g. `stddev_over_time`), a measure filter or a
    /// `HAVING` (`agg_has_exact_per_entity_kernel`). Every timing and
    /// materialization of the node is pruned.
    PerEntityAggregateNotExecutable,
    /// Stage 2: it scans raw data at query time, and the deployment does
    /// not retain raw data, so no window of it is there to scan.
    RawDataNotRetained,
    /// Stage 2: one of its inputs has no producer candidate that can be
    /// built: none is left, or every one depends, directly or through a
    /// cycle, on an input that cannot be built.
    InputHasNoBuildableProducer,
    /// Stage 2: no query output can read it, directly or through other
    /// candidates, once each candidate's allowed readers are applied.
    ReachesNoQueryOutput,
    /// Stage 2: it is shared within one query evaluation
    /// (`MatAtQuery`), which needs at least two reading queries, and fewer
    /// queries can reach it.
    TooFewQueryReaders,
    /// Stage 3: the candidate it names, in the same memo group with the
    /// same producers and readers, is at least as good in every respect
    /// (guarantee, usage, latency, output, held state), so no optimal plan
    /// needs this one.
    DominatedBy(PhysicalCandidateId),
    /// Stage 3: `asap-cost` could not cost it, so the selection MILP leaves
    /// it out.
    UsageNotCostable,
}

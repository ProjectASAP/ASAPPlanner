//! The computation-target types Stage 1, Pass 1 builds its candidates from.

use super::MemoGroupId;
use crate::ir::schema::{ExactKind, SketchAlgorithm};
use crate::ir::Predicate;

/// How one Pass 1 target is computed: exact or a summary.
///
/// Each target lists one exact option: `ExactAccumulator` when the intent
/// has an exact mergeable state and it can be built, otherwise `Exact`.
/// The plain exact computation is always offered too, as the aggregate
/// group's own expression. Which computation is computed is the target's
/// ([`ComputationTarget`]) or its fused computation's ([`FusedComputation`]).
#[derive(Debug, Clone, PartialEq)]
pub enum HowToComputeAggregateMeasure {
    /// Plain exact execution: the aggregate group's own expression; the
    /// listed exact option only when the intent has no exact accumulator
    /// form or that form is pruned.
    Exact,
    /// One or more exact mergeable accumulator states, by accumulator kind,
    /// finalized and combined into the measure: one state for `sum`,
    /// `count`, `min`, `max`, `rate`, `irate` and `increase`; `[Sum, Count]`,
    /// divided, for `avg`.
    ExactAccumulator(Vec<ExactKind>),
    /// A summary family: its state group gets one build candidate per
    /// configuration of the family's predefined list (`parameter_list`).
    Summary(SketchAlgorithm),
}

/// Where a target's `HAVING` is applied by the candidates Pass 1 adds.
///
/// Only the adjacent form is recorded for push-down: the aggregate's own
/// `having`, or a `Filter` group directly above the aggregate group (the
/// form the SQL frontend lowers `HAVING` to). Multi-level push-down is
/// tracked in #664.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HavingPlacement {
    /// The predicate reads only group keys, so the candidates that compute
    /// the filtered result push it below the aggregate as a row predicate (a
    /// `Filter` on its input, which drops the group).
    BelowAggregate,
    /// The predicate reads aggregate values (or the aggregate is global), so
    /// it stays a `Filter` above the finalize or readout output.
    AboveOutput,
}

/// One `HAVING` over a target's aggregate output.
#[derive(Debug, Clone, PartialEq)]
pub struct HavingEntry {
    /// The consumer `Filter` group that applies it; `None` for the
    /// aggregate's own `having`, which every consumer reads.
    pub filter: Option<MemoGroupId>,
    pub predicate: Predicate<MemoGroupId>,
    pub placement: HavingPlacement,
}

/// One measure of an aggregate with at least one realization besides exact
/// execution. The target retains source, grouping, filters, input
/// expressions and evaluation context.
#[derive(Debug, Clone)]
pub struct ComputationTarget {
    /// The memo group of the `Aggregate` the measure belongs to.
    pub aggregate_group: MemoGroupId,
    /// Index into the aggregate's measures.
    pub measure_index: usize,
    /// The measure's unranked ways to compute it, the exact option first.
    pub ways_to_compute: Vec<HowToComputeAggregateMeasure>,
    /// The measure's own filter; only rows that pass it update the state
    /// (`SummaryAgg.filter`).
    pub measure_filter: Option<Predicate<MemoGroupId>>,
    /// The aggregate's own `having`, then one entry per consumer `Filter`
    /// directly above the aggregate.
    pub having_filters: Vec<HavingEntry>,
    /// The fused computation of this measure together with the target
    /// directly beneath it, offered beside this target's own candidates;
    /// `None` when no fusion applies.
    pub fused_computation: Option<FusedComputation>,
}

/// A target's measure computed together with the targets beneath it, as
/// one operator that replaces their operators in this appearance of the
/// sub-expression (concepts doc §3, "Fused groups"): the whole-expression
/// top-k over a per-item `sum` or `count` (`Summary` of a heap family), or
/// the nested sum, a grouped `sum` over a `sum` (`ExactAccumulator([Sum])`).
/// Its build candidates list the replaced targets' groups in
/// `fused_groups`. It has the outer target's aggregate, measure, filter and
/// `HAVING`.
#[derive(Debug, Clone, PartialEq)]
pub struct FusedComputation {
    /// Indices, into Pass 1's computation-target list, of the targets it
    /// absorbs: the one directly beneath, then, for a whole-expression
    /// heap over the raw data samples beneath a grouped sum of per-series sums or
    /// counts (`topk(k, sum by (G) (sum_over_time(m[W])))`), the per-series
    /// target beneath that, then the targets any of these targets' own fused
    /// computations absorb: the operators it replaces.
    pub absorbed_targets: Vec<usize>,
    /// How the fused computation is computed: exact or a summary. Empty
    /// when Pass 1 pruned every one (today only for a family the deployment
    /// does not support); the fusion is then not offered.
    pub ways_to_compute: Vec<HowToComputeAggregateMeasure>,
}

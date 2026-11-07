//! The computation-target types Stage 1, Pass 1 builds its candidates from.

use super::MemoGroupId;
use crate::ir::schema::{ExactKind, SketchAlgorithm};

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
    /// The fused computation of this measure together with the target
    /// directly beneath it, offered beside this target's own candidates;
    /// `None` when no fusion applies.
    pub fused_computation: Option<FusedComputation>,
}

/// A target's measure computed together with the targets beneath it, as
/// one operator that replaces their operators in this appearance of the
/// sub-expression (concepts doc §3, "Fused groups"): the whole-expression
/// top-k over a per-item `sum` or `count` (`Summary` of a heap family), or
/// a nested distributive aggregate, a grouped `sum`, `min` or `max` over the
/// same reduction (`ExactAccumulator([Sum])`, `[Min]` or `[Max]`).
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

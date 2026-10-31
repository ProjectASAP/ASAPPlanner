//! Stage 1, Pass 1's output, which Pass 2 extends: the memo groups with
//! their Pass 1 candidates, and the computation targets they were built
//! from.

use std::rc::Rc;

use super::{MemoCandidates, MemoGroupId, PrunedCandidate};
use crate::ir::schema::{ExactKind, SketchAlgorithm};
use crate::ir::{OperatorNode, Predicate};

/// Stage 1, Pass 1's output: the workload's memo groups, each with its
/// Pass 1 candidates, and the computation targets they were built from.
/// It is also Pass 2's input.
#[derive(Debug, Clone)]
pub struct PerComputationLogicalMemoCandidates {
    /// The groups moved from the [`DagOfLogicalMemoGroups`](super::DagOfLogicalMemoGroups),
    /// its query roots, and every group's candidates (origin
    /// [`Pass1`](super::OriginOfCandidateForMemoGroup::Pass1)).
    pub memo: MemoCandidates,
    /// One per aggregate measure: what Pass 1 chose among.
    pub computation_targets: Vec<ComputationTarget>,
    /// The ways to compute a target's measure that Pass 1 pruned, each on
    /// its target's aggregate group, in pruning order.
    pub pruned_candidates: Vec<PrunedCandidate<HowToComputeAggregateMeasure>>,
}

/// How one Pass 1 target is computed: exact or a summary.
///
/// Each target has exactly one exact option: `ExactAccumulator` when the
/// intent has an exact mergeable state and it can be built, otherwise
/// `Exact`. The plain form would duplicate "accumulator, not materialized";
/// Stage 2/3 decide materialization and sharing. Which computation is
/// computed is the target's ([`ComputationTarget`]) or its fused
/// computation's ([`FusedComputation`]).
#[derive(Debug, Clone, PartialEq)]
pub enum HowToComputeAggregateMeasure {
    /// Plain exact execution of the original sub-DAG; the exact option only
    /// when the intent has no exact accumulator form or that form is pruned.
    Exact,
    /// One or more exact mergeable accumulator states, by accumulator kind,
    /// finalized and combined into the measure: one state for `sum`,
    /// `count`, `min`, `max`, `rate`, `irate` and `increase`; `[Sum, Count]`,
    /// divided, for `avg`.
    ExactAccumulator(Vec<ExactKind>),
    /// An unsized summary family; sized for the measure's accuracy when Pass 1
    /// attaches it to its memo group.
    Summary(SketchAlgorithm),
}

/// Where a target's `HAVING` is applied. Composition (#575) applies it.
///
/// Only the adjacent form is recorded for push-down: the aggregate's own
/// `having`, or a `Filter` directly above the aggregate (the form the SQL
/// frontend lowers `HAVING` to). A value predicate anywhere above an
/// approximate output gets `guarantee = None` through guarantee propagation
/// (#575); multi-level push-down is tracked in #664.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HavingPlacement {
    /// The predicate reads only group keys and every consumer of the
    /// aggregate applies it, so it is pushed below the aggregate as a row
    /// predicate (a `Filter` on its input, which drops the group).
    BelowAggregate,
    /// The predicate reads aggregate values, so it stays a `Filter` above the
    /// finalize/estimate output; approximate candidates of this target carry
    /// `guarantee = None`, since membership near the threshold may change.
    AboveOutput,
}

/// One `HAVING` over a target's aggregate output.
#[derive(Debug, Clone, PartialEq)]
pub struct HavingEntry {
    /// The consumer `Filter` that applies it; `None` for the aggregate's own
    /// `having`, which every consumer reads.
    pub filter: Option<Rc<OperatorNode>>,
    pub predicate: Predicate,
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
    /// The ones Pass 1 pruned are in
    /// [`PerComputationLogicalMemoCandidates::pruned_candidates`].
    pub ways_to_compute: Vec<HowToComputeAggregateMeasure>,
    /// The measure's own filter; only rows that pass it update the state
    /// (`SummaryAgg.filter`).
    pub measure_filter: Option<Predicate>,
    /// The aggregate's own `having`, then one entry per consumer `Filter`
    /// directly above the aggregate.
    pub having_filters: Vec<HavingEntry>,
    /// The fused computation of this measure together with the target
    /// directly beneath it, offered beside this target's own candidates;
    /// `None` when no fusion applies.
    pub fused_computation: Option<FusedComputation>,
}

/// A target's measure computed together with the targets beneath it, which
/// it absorbs (they are not computed): the whole-expression top-k over a
/// per-item `sum` or `count` (`Summary` of a heap family), or the nested
/// sum, a grouped `sum` over a `sum` (`ExactAccumulator([Sum])`). It has the
/// outer target's aggregate, measure, filter and `HAVING`.
#[derive(Debug, Clone, PartialEq)]
pub struct FusedComputation {
    /// The indices in
    /// [`PerComputationLogicalMemoCandidates::computation_targets`] of the targets
    /// it absorbs: the one directly beneath, then, for a whole-expression
    /// heap over the raw data samples beneath a grouped sum of per-series sums or
    /// counts (`topk(k, sum by (G) (sum_over_time(m[W])))`), the per-series
    /// target beneath that, then the targets any of these targets' own fused
    /// computations absorb. None of them is computed when this is picked.
    pub absorbed_targets: Vec<usize>,
    /// How the fused computation is computed: exact or a summary. Empty
    /// when Pass 1 pruned every one (today only for
    /// [`PruneReason::UnsupportedByDeployment`](super::PruneReason::UnsupportedByDeployment),
    /// recorded in [`PerComputationLogicalMemoCandidates::pruned_candidates`]); the fusion is
    /// then not offered.
    pub ways_to_compute: Vec<HowToComputeAggregateMeasure>,
}

impl ComputationTarget {
    /// How many values this target's digit of a whole-workload choice
    /// takes: its own candidates, then its fused computation's.
    pub fn choice_count(&self) -> usize {
        self.ways_to_compute.len()
            + self
                .fused_computation
                .as_ref()
                .map_or(0, |fused| fused.ways_to_compute.len())
    }

    /// How the measure is computed when this target's digit of a
    /// whole-workload choice is `index`: the candidate, and the targets it
    /// absorbs when the candidate is the fused computation's (past this
    /// target's own candidates). `None` past [`Self::choice_count`].
    pub fn computation_for(
        &self,
        index: usize,
    ) -> Option<(&HowToComputeAggregateMeasure, Option<&[usize]>)> {
        let own = self.ways_to_compute.len();
        match self.ways_to_compute.get(index) {
            Some(candidate) => Some((candidate, None)),
            None => {
                let fused = self.fused_computation.as_ref()?;
                Some((
                    fused.ways_to_compute.get(index - own)?,
                    Some(fused.absorbed_targets.as_slice()),
                ))
            }
        }
    }
}

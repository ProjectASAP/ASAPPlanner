//! Stage 1 (logical ASAP-aware optimization), Pass 1: per-computation
//! candidate generation, as defined in
//! `docs/design_docs/proposals/planner-layering.md`.
//!
//! [`generate_per_computation_candidates`] walks the memo groups of the
//! workload's hash-consed logical DAG ([`DagOfLogicalMemoGroups`]) and
//! returns, for every eligible computation, its unranked candidates. A
//! computation ([`ComputationTarget`]) is one measure of an `Aggregate`
//! group whose intent has a realization besides plain exact execution, once
//! however many queries read the group; nested aggregates and operators
//! read by scalar roots are found too.
//! Pass 1 does not consider sharing across computations; that is Pass 2.
//!
//! - A candidate says how a target is computed: exact or a summary
//!   ([`HowToComputeAggregateMeasure`]). Each target has exactly one exact option:
//!   its exact accumulator states (`ExactAccumulator`; `[Sum, Count]` for
//!   `avg`) when they can be built, otherwise plain `Exact`. Whether they are
//!   materialized is decided in Stage 2.
//! - A computation that fuses a target with the targets beneath it is the
//!   outer target's [`FusedComputation`], which absorbs the ones beneath and
//!   has its own candidates: the whole-expression top-k over a per-item
//!   `sum` or `count` (`Summary` of a heap family) and the nested sum, a
//!   grouped `sum` over a `sum` (`ExactAccumulator([Sum])`).
//! - Summary choices here are unsized families. Attaching them to memo
//!   groups creates sized candidates, one per split of the accuracy budget
//!   (`CandidateForMemoGroup::accuracy_budget_parts`), each sized for ε, its
//!   budget share and δ over the values its readers' targets cover; Pass 2
//!   inherits sizes or takes the strictest shared reader's, and Stage 3 only
//!   checks.
//! - Candidates are pruned only when provably unable to meet the accuracy
//!   requirement or to run, or when the exact candidate provably dominates
//!   them (a PromQL top-k's heap over its instant vector, one row per item:
//!   [`PruneReason::TopKHeapOverOneRowPerItem`]); each pruned candidate
//!   keeps its [`PruneReason`].
//! - A measure filter or `HAVING` prunes nothing. The target records the
//!   measure filter (applied as `SummaryAgg.filter`) and one [`HavingEntry`]
//!   per consumer `Filter` directly above the aggregate: a key-only predicate
//!   applied by every reader of the aggregate's group, across queries, may go
//!   below the aggregate, anything else stays above the output (approximate
//!   outputs then get guarantee `None`). So every candidate of a group
//!   computes the group's own rows.
//!
//! The input must be a logical DAG: ASAP operators, execution timing or a
//! materialization are rejected. No cost or accuracy model is used; ranking
//! and discarding valid candidates belong to Stage 3.
//!
//! # Whole-workload candidates (logical ASAP-aware optimization, end of Pass 1)
//!
//! [`compose_logical_candidate`] is Algorithm 1's
//! `SUBSTITUTE(d, s_1 ↦ c_1, …)`: it picks one per-computation candidate per
//! target (`choice[i]` indexes `targets[i]`'s candidates, then its fused
//! computation's, [`ComputationTarget::computation_for`]) and builds one whole-workload
//! candidate DAG. An aggregate whose chosen candidates are all `Exact` is
//! kept. Otherwise each of its measures is substituted on its own over the
//! aggregate's input:
//!
//! - `ExactAccumulator` with one state: `SummaryAgg` with the exact family →
//!   `FinalizeExactAccumulator`.
//! - `ExactAccumulator([Sum, Count])` of an `avg`: the finalized `sum` and
//!   `count` accumulators divided: a PromQL `BinaryOp`, or for SQL rows a
//!   join on the group keys (NULL keys match) and a SQL division, NULL where
//!   the count is 0.
//! - `Summary`: `SummaryAgg` → `SummaryEstimate`.
//! - `Exact` (or a measure that is no target): a single-measure `Aggregate`.
//!
//! A fused computation's candidate reads the absorbed targets' input instead:
//! the whole-expression top-k is one heap `SummaryAgg` keyed by the ranked
//! item → `SummaryEstimate` (over a grouped sum of per-series sums or counts,
//! it reads the per-series target's input, the raw data samples); the nested sum is `SummaryAgg` with the exact
//! `Sum` family grouped by the outer keys → `FinalizeExactAccumulator`.
//!
//! The substituted output keeps the aggregate's column names. The measures
//! of a multi-measure aggregate are joined back on the group keys (NULL keys
//! match, as grouping treats them). A measure filter becomes
//! `SummaryAgg.filter`, so a group with no matching row stays. A
//! `BelowAggregate` `HAVING` becomes a `Filter` below the state, with its key
//! columns renumbered to the input's, and the consumer `Filter` it came from
//! is dropped; it is not a `SummaryAgg.filter`, which would keep the dropped
//! groups. An `AboveOutput` `HAVING` stays a `Filter` above the output.
//!
//! Summaries are sized here, each for its own measure's accuracy requirement.
//! When a query's value composes several approximate inputs (every sketch
//! summary below its root, when its guarantee is not exact), each is sized
//! for an equal share of that requirement: `ε/n` (a relative error
//! `1 − (1 + ε)^{−1/n}`) and `δ/n` for `n` inputs (#669b). Only the inputs
//! approximate in this candidate count. This composition sizes for ε and δ
//! per output value; attached to the memo groups, each candidate's
//! approximate inputs are sized for the values its readers' targets cover
//! (`δ / (n · V)`,
//! [`generate_per_computation_logical_memo_candidates`](crate::pass1::logical_memo_candidates::generate_per_computation_logical_memo_candidates)),
//! their final sizes. Pass 2 inherits them or takes the strictest shared
//! reader's, and Stage 3 only checks the composed accuracy.
//!
//! Every node of the candidate gets a theoretical guarantee, composed with
//! the built-in accuracy model through operators (#669): exact over exact
//! inputs; a `SummaryEstimate` the sized family's bound where that bound
//! answers the estimate (not for Top-K, where a heap sketch bounds item
//! frequencies, not membership; not for a Count-Min family over weights not
//! proven non-negative); aggregation and arithmetic over approximate values
//! by their composition rules, with statistics left symbolic for plan
//! selection to bind; and `None` where no rule applies (comparisons and
//! value predicates, rank errors). Plan selection's accuracy model decides
//! a candidate without one.
use std::collections::{HashMap, HashSet};
use std::rc::Rc;

use asap_types::ir::operator::operator_properties::{BinaryOpKind, JoinKind, Reduction, Source};
use asap_types::ir::operator::AggIntent;
use asap_types::ir::properties::AccuracyTarget;
use asap_types::ir::scalar::expr_ir::ArithmeticOpKind;
use asap_types::ir::scalar::{resolve_column_ref, ColumnRef, CompareOpKind, ScalarValue};
use asap_types::ir::schema::{DataType, Schema};
use asap_types::ir::schema::{
    EntityIdentity, ExactKind, ExactParams, FieldDataType, GroupingStrategy,
    NonNegativeWeightProof, SketchAlgorithm, SketchKind, StatisticKind, SummaryInputExpr,
    SummaryUpdate, WeightDomain,
};
use asap_types::ir::{
    ASAPOp, BinaryOperator, ExprSemantics, NonASAPOp, Operator, OperatorNode, OperatorResultKind,
    Predicate, ProjectItem, QueryRoot, ScalarExpr, SchemaDerivationError,
};
use asap_types::physical::Materialization;
use thiserror::Error;

use asap_accuracy::composition::parse_raw_data_samples_sign_statistic;
use asap_accuracy::estimators::hll;
use asap_accuracy::propagation::{
    approximate_inputs, bind_guarantees, raw_data_samples_of_metric, root_guarantee,
    with_guarantees,
};
use asap_accuracy::sizing::{accuracy_budget, sized, sizing_target, sketch_statistic};
use asap_accuracy::AccuracyModel;
use asap_types::ir::properties::AccuracyGuarantee;
use asap_types::workload::bounds::{declared_non_negative, series_read, values_per_result};
use asap_types::workload::DataWorkload;

use crate::deployment_capabilities::{summary_supported, SupportedSummary};
use crate::pass1::realization::summary_candidates;
use crate::pass2::memo::DagOfLogicalMemoGroups;
use asap_types::stages::{MemoGroupId, PruneReason};

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

/// `FieldDataType::ExactAggregate` of accumulator `kind`, with its parameters
/// (none has any, so they mirror `kind`).
fn exact_family(kind: ExactKind) -> FieldDataType {
    let params = match kind {
        ExactKind::Sum => ExactParams::Sum,
        ExactKind::Count => ExactParams::Count,
        ExactKind::Min => ExactParams::Min,
        ExactKind::Max => ExactParams::Max,
        ExactKind::Rate => ExactParams::Rate,
        ExactKind::IRate => ExactParams::IRate,
        ExactKind::Increase => ExactParams::Increase,
    };
    FieldDataType::ExactAggregate(kind, params)
}

/// Unranked candidates of one measure and the candidates pruned for it.
#[derive(Debug, Clone, PartialEq)]
pub struct ComputationChoices {
    pub candidates: Vec<HowToComputeAggregateMeasure>,
    pub pruned: Vec<(HowToComputeAggregateMeasure, PruneReason)>,
}

impl ComputationChoices {
    /// Prune matching candidates; a pruned exact accumulator form is replaced by
    /// plain `Exact`, so the target keeps one exact option.
    fn prune(
        &mut self,
        reason: PruneReason,
        applies: impl Fn(&HowToComputeAggregateMeasure) -> bool,
    ) {
        let pruned = prune_own(&mut self.candidates, applies);
        self.pruned
            .extend(pruned.into_iter().map(|choice| (choice, reason)));
    }
}

/// Removes from a measure's own `candidates` the ones `applies` matches
/// (never `Exact`) and returns them; a pruned exact accumulator form is
/// replaced by plain `Exact`, so the measure keeps one exact option.
fn prune_own(
    candidates: &mut Vec<HowToComputeAggregateMeasure>,
    applies: impl Fn(&HowToComputeAggregateMeasure) -> bool,
) -> Vec<HowToComputeAggregateMeasure> {
    let (pruned, kept): (Vec<_>, _) = std::mem::take(candidates)
        .into_iter()
        .partition(|choice| *choice != HowToComputeAggregateMeasure::Exact && applies(choice));
    *candidates = kept;
    if pruned
        .iter()
        .any(|choice| matches!(choice, HowToComputeAggregateMeasure::ExactAccumulator(_)))
    {
        candidates.insert(0, HowToComputeAggregateMeasure::Exact);
    }
    pruned
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
    /// The memo group of the `Aggregate` the measure belongs to; its node is
    /// [`PerComputationCandidates::aggregate`].
    pub aggregate_group: MemoGroupId,
    /// Index into the aggregate's measures.
    pub measure_index: usize,
    /// The measure's unranked ways to compute it, the exact option first.
    /// The ones Pass 1 pruned are in
    /// [`PerComputationCandidates::pruned_ways_to_compute`].
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
    /// The metric whose raw data samples, or plain sums of them, are every value
    /// the aggregate reads ([`raw_data_samples_of_metric`](asap_accuracy::propagation::raw_data_samples_of_metric)); `None` otherwise. A
    /// heap-sketch weight read from them carries the open requirement
    /// [`WeightDomain::RawDataSamplesOfMetric`], which plan selection confirms.
    pub raw_data_samples_of: Option<String>,
}

/// A target's measure computed together with the targets beneath it, which
/// it absorbs (they are not computed): the whole-expression top-k over a
/// per-item `sum` or `count` (`Summary` of a heap family), or the nested
/// sum, a grouped `sum` over a `sum` (`ExactAccumulator([Sum])`). It has the
/// outer target's aggregate, measure, filter and `HAVING`.
#[derive(Debug, Clone, PartialEq)]
pub struct FusedComputation {
    /// The indices in [`PerComputationCandidates::targets`] of the targets
    /// it absorbs: the one directly beneath, then, for a whole-expression
    /// heap over the raw data samples beneath a grouped sum of per-series sums or
    /// counts (`topk(k, sum by (G) (sum_over_time(m[W])))`), the per-series
    /// target beneath that, then the targets any of these targets' own fused
    /// computations absorb. None of them is computed when this is picked.
    pub absorbed_targets: Vec<usize>,
    /// How the fused computation is computed: exact or a summary. Empty
    /// when Pass 1 pruned every one (today only for
    /// [`PruneReason::UnsupportedByDeployment`], recorded in
    /// [`PerComputationCandidates::pruned_ways_to_compute`]); the fusion is
    /// then not offered.
    pub ways_to_compute: Vec<HowToComputeAggregateMeasure>,
}

/// A way to compute a target's measure that Pass 1 pruned, and why.
#[derive(Debug, Clone, PartialEq)]
pub struct PrunedWayToCompute {
    /// The target, by index in [`PerComputationCandidates::targets`].
    pub target: usize,
    /// Whether it was a way of the target's fused computation
    /// ([`ComputationTarget::fused_computation`]) rather than its own.
    pub of_fused_computation: bool,
    pub way_to_compute: HowToComputeAggregateMeasure,
    pub reason: PruneReason,
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

/// Compact Pass 1 inventory over the memo groups it walked: one target per
/// measure of each aggregate group, however many queries read the group.
#[derive(Debug, Clone)]
pub struct PerComputationCandidates {
    /// Each query's root over the memo groups' logical nodes, in workload
    /// order: a group several queries read is one node. Whole-workload
    /// candidates are these roots with chosen candidates substituted.
    pub roots: Vec<QueryRoot>,
    /// Each memo group's logical node, by group index.
    pub(crate) logical_nodes: Rc<[Rc<OperatorNode>]>,
    /// The targets; a whole-workload choice has one digit per target,
    /// ranging over [`ComputationTarget::choice_count`].
    pub targets: Vec<ComputationTarget>,
    /// The ways to compute the targets' measures that Pass 1 pruned, in the
    /// order it pruned them.
    pub pruned_ways_to_compute: Vec<PrunedWayToCompute>,
}

impl PerComputationCandidates {
    /// Whether this inventory walked `dag`, or a DAG built the same way from
    /// the same roots: the same groups (by structural id, in the same order)
    /// and each query's root over the same groups. Its targets' group ids
    /// then name `dag`'s groups.
    pub fn walked(&self, dag: &DagOfLogicalMemoGroups) -> bool {
        use crate::pass2::memo::NodeToStructuralIdCache;
        let mut structural_ids = NodeToStructuralIdCache::default();
        let group_of: HashMap<*const OperatorNode, MemoGroupId> = self
            .logical_nodes
            .iter()
            .enumerate()
            .map(|(g, node)| (Rc::as_ptr(node), MemoGroupId(g as u32)))
            .collect();
        dag.groups.len() == self.logical_nodes.len()
            && dag.query_roots.len() == self.roots.len()
            && dag
                .groups
                .iter()
                .zip(self.logical_nodes.iter())
                .all(|(group, node)| {
                    Rc::ptr_eq(&group.logical_node, node)
                        || structural_ids.structural_id(&group.logical_node)
                            == structural_ids.structural_id(node)
                })
            && dag
                .query_roots
                .iter()
                .zip(&self.roots)
                .all(|(groups, root)| {
                    let nodes = match root {
                        QueryRoot::Operator(node) => vec![node],
                        QueryRoot::Scalar(expr) => expr.operator_refs(),
                    };
                    matches!(groups, QueryRoot::Operator(_))
                        == matches!(root, QueryRoot::Operator(_))
                        && groups.operator_refs().len() == nodes.len()
                        && groups
                            .operator_refs()
                            .into_iter()
                            .zip(nodes)
                            .all(|(g, node)| group_of.get(&Rc::as_ptr(node)) == Some(g))
                })
    }

    /// The `Aggregate` node of `target`'s memo group.
    pub fn aggregate(&self, target: &ComputationTarget) -> &Rc<OperatorNode> {
        &self.logical_nodes[target.aggregate_group.0 as usize]
    }

    /// The number of whole-workload candidates: the size of the Cartesian
    /// product of the targets' candidates, where a target with a fused
    /// computation and the targets beneath that it absorbs count as their
    /// own combinations plus the fused computation's candidates (the
    /// absorbed targets are not computed then), saturating at `usize::MAX`.
    /// A target with `n` candidates and `f` fused ones counts `n · Π m + f`,
    /// the product over the targets it absorbs that no other of them
    /// absorbs, each counted the same way. It is exactly the number of
    /// choices enumeration keeps ([`Self::absorbed_choice_varies`]).
    pub fn combinations(&self) -> usize {
        let absorbed: HashSet<usize> = absorbed_targets(&self.targets).collect();
        (0..self.targets.len())
            .filter(|index| !absorbed.contains(index))
            .map(|index| self.combinations_of(index))
            .fold(1usize, usize::saturating_mul)
    }

    /// [`Self::combinations`] of target `index` with the targets it absorbs.
    fn combinations_of(&self, index: usize) -> usize {
        let target = &self.targets[index];
        let n = target.ways_to_compute.len();
        let Some(fused) = &target.fused_computation else {
            return n;
        };
        let absorbed_by_another = |a: usize| {
            fused.absorbed_targets.iter().any(|&b| {
                self.targets[b]
                    .fused_computation
                    .as_ref()
                    .is_some_and(|inner| inner.absorbed_targets.contains(&a))
            })
        };
        fused
            .absorbed_targets
            .iter()
            .filter(|&&a| !absorbed_by_another(a))
            .map(|&a| self.combinations_of(a))
            .fold(n, usize::saturating_mul)
            .saturating_add(fused.ways_to_compute.len())
    }

    /// Whether `choice` picks a fused computation and a candidate other
    /// than the first for a target it absorbs: the same whole-workload
    /// candidate as the one with the first, so enumeration skips it.
    pub fn absorbed_choice_varies(&self, choice: &[usize]) -> bool {
        self.targets.iter().zip(choice).any(|(target, &index)| {
            target
                .computation_for(index)
                .and_then(|(_, absorbed)| absorbed)
                .is_some_and(|inner| inner.iter().any(|&i| choice[i] != 0))
        })
    }

    /// The ways to compute target `target`'s measure that Pass 1 pruned,
    /// with their reasons, in pruning order: its own, or its fused
    /// computation's when `of_fused_computation`.
    pub fn pruned_of_target(
        &self,
        target: usize,
        of_fused_computation: bool,
    ) -> Vec<(HowToComputeAggregateMeasure, PruneReason)> {
        self.pruned_ways_to_compute
            .iter()
            .filter(|pruned| {
                pruned.target == target && pruned.of_fused_computation == of_fused_computation
            })
            .map(|pruned| (pruned.way_to_compute.clone(), pruned.reason))
            .collect()
    }

    /// This inventory without the summary candidates the executor does not
    /// support (`supported_summaries`, the deployment's
    /// `DeploymentCapabilities::supported_summaries`; `None` supports
    /// every one): a target's own are moved to its pruned candidates with
    /// [`PruneReason::UnsupportedByDeployment`], and so are a fused
    /// computation's (with all of them pruned, the fusion is not offered).
    /// The readout is the statistic the measure's estimate reads (the outer measure's for a
    /// fused computation). With every summary supported it is unchanged.
    pub fn without_unsupported_summaries(
        mut self,
        supported_summaries: Option<&[SupportedSummary]>,
    ) -> Self {
        for (index, target) in self.targets.iter_mut().enumerate() {
            let aggregate = &self.logical_nodes[target.aggregate_group.0 as usize];
            let Some(NonASAPOp::Aggregate { measures, .. }) = aggregate.non_asap() else {
                continue;
            };
            let Some(readout) = sketch_statistic(&measures[target.measure_index]).map(|s| s.kind())
            else {
                continue;
            };
            let unsupported = |candidate: &HowToComputeAggregateMeasure| {
                matches!(candidate, HowToComputeAggregateMeasure::Summary(family)
                    if !summary_supported(supported_summaries, family, readout))
            };
            let pruned = |of_fused_computation| {
                move |way_to_compute| PrunedWayToCompute {
                    target: index,
                    of_fused_computation,
                    way_to_compute,
                    reason: PruneReason::UnsupportedByDeployment,
                }
            };
            let own = prune_own(&mut target.ways_to_compute, unsupported);
            self.pruned_ways_to_compute
                .extend(own.into_iter().map(pruned(false)));
            if let Some(fused) = &mut target.fused_computation {
                let (removed, kept): (Vec<_>, _) = std::mem::take(&mut fused.ways_to_compute)
                    .into_iter()
                    .partition(unsupported);
                fused.ways_to_compute = kept;
                self.pruned_ways_to_compute
                    .extend(removed.into_iter().map(pruned(true)));
            }
        }
        self
    }

    /// This inventory without the summary candidates no query can use
    /// (#1047, accuracy A1 of #1095): a summary of a target (its own, or
    /// its fused computation's) is moved to its pruned candidates with
    /// [`PruneReason::NoErrorBoundForQueryTarget`] when, for every query
    /// whose root reaches the target's aggregate,
    /// [`output_can_meet_target`] says no: the query's output, composed
    /// with that summary and every other target exact, has no error bound
    /// that can meet the query's target (`query_targets[q]`) at any size,
    /// as `model` judges metrics. Every other target exact is the most
    /// favorable combination: an approximate input never adds a guarantee.
    /// A summary some query can use is kept, even when another query
    /// reaching the group cannot; plan selection rejects it for that query.
    ///
    /// Pass 1 cannot see Pass 2's reuse of one group's summary by another
    /// group (rollup, window rollup, finer-aggregate reuse, top-k reuse), so
    /// a summary is also kept whenever another target, over another
    /// aggregate reading one of the same sources, offers the same family
    /// and some query reading that target can use it: Pass 2 may build that
    /// target's state from this one, and the reused state composes through
    /// that target's queries as its own would. This keeps more than those
    /// rules' preconditions need. Exact candidates are never
    /// pruned, and with every fused summary pruned the fusion is not
    /// offered. A summary whose composition fails is kept.
    ///
    /// The facade calls it only when the deployment's accuracy model and
    /// evidence cannot accept an output without its guarantee.
    pub fn without_summaries_no_query_can_use(
        mut self,
        query_targets: &[AccuracyTarget],
        data: &DataWorkload,
        model: &dyn AccuracyModel,
    ) -> Self {
        // The aggregates each query's root reaches.
        let reached: Vec<HashSet<*const OperatorNode>> = self
            .roots
            .iter()
            .map(|root| {
                root.operator_refs()
                    .into_iter()
                    .flat_map(OperatorNode::reachable)
                    .filter(|node| matches!(node.non_asap(), Some(NonASAPOp::Aggregate { .. })))
                    .map(|node| Rc::as_ptr(&node))
                    .collect()
            })
            .collect();
        // Each target's sources, and every family it offers.
        let sources: Vec<Vec<Source>> = self
            .targets
            .iter()
            .map(|target| sources_read(self.aggregate(target)))
            .collect();
        let families = |target: &ComputationTarget| -> Vec<SketchAlgorithm> {
            (0..target.choice_count())
                .filter_map(|choice| match target.computation_for(choice) {
                    Some((HowToComputeAggregateMeasure::Summary(family), _)) => Some(*family),
                    _ => None,
                })
                .collect()
        };
        let offered: Vec<Vec<SketchAlgorithm>> = self.targets.iter().map(families).collect();
        // Per target, its summary choices no query reading its aggregate can
        // use; a target no query reaches, or whose composition fails, keeps
        // every choice.
        let mut unusable_by_own_queries: Vec<Vec<usize>> = vec![Vec::new(); self.targets.len()];
        for (index, target) in self.targets.iter().enumerate() {
            let aggregate = Rc::as_ptr(self.aggregate(target));
            let queries: Vec<usize> = (0..self.roots.len())
                .filter(|&q| reached[q].contains(&aggregate) && q < query_targets.len())
                .collect();
            if queries.is_empty() {
                continue;
            }
            let roots: Vec<QueryRoot> = queries.iter().map(|&q| self.roots[q].clone()).collect();
            for choice in 0..target.choice_count() {
                let Some((HowToComputeAggregateMeasure::Summary(_), _)) =
                    target.computation_for(choice)
                else {
                    continue;
                };
                let mut choices = vec![0; self.targets.len()];
                choices[index] = choice;
                let Ok(composed) = compose_with_budget_share(&self, &roots, &choices, Some(1))
                else {
                    continue;
                };
                if !queries
                    .iter()
                    .zip(&composed)
                    .any(|(&q, root)| output_can_meet_target(root, &query_targets[q], data, model))
                {
                    unusable_by_own_queries[index].push(choice);
                }
            }
        }
        // The families each target's own queries can use.
        let usable_families: Vec<Vec<SketchAlgorithm>> = self
            .targets
            .iter()
            .enumerate()
            .map(|(index, target)| {
                (0..target.choice_count())
                    .filter(|choice| !unusable_by_own_queries[index].contains(choice))
                    .filter_map(|choice| match target.computation_for(choice) {
                        Some((HowToComputeAggregateMeasure::Summary(family), _)) => Some(*family),
                        _ => None,
                    })
                    .collect()
            })
            .collect();
        // Whether Pass 2 may reuse target `index`'s `family` for another
        // target's aggregate whose own queries can use it: a reused state
        // composes through that target's queries as its own would.
        let reusable = |index: usize, family: &SketchAlgorithm| {
            let aggregate = self.targets[index].aggregate_group;
            self.targets.iter().enumerate().any(|(other, target)| {
                target.aggregate_group != aggregate
                    && offered[other].contains(family)
                    && usable_families[other].contains(family)
                    && sources[other]
                        .iter()
                        .any(|source| sources[index].contains(source))
            })
        };
        let mut unusable: Vec<(usize, usize)> = Vec::new();
        for (index, choices) in unusable_by_own_queries.iter().enumerate() {
            for &choice in choices {
                let Some((HowToComputeAggregateMeasure::Summary(family), _)) =
                    self.targets[index].computation_for(choice)
                else {
                    continue;
                };
                if !reusable(index, family) {
                    unusable.push((index, choice));
                }
            }
        }
        for (index, target) in self.targets.iter_mut().enumerate() {
            let own = target.ways_to_compute.len();
            let pruned_choices: Vec<usize> = unusable
                .iter()
                .filter(|(i, _)| *i == index)
                .map(|(_, choice)| *choice)
                .collect();
            if pruned_choices.is_empty() {
                continue;
            }
            let own_pruned: Vec<HowToComputeAggregateMeasure> = pruned_choices
                .iter()
                .filter(|&&choice| choice < own)
                .map(|&choice| target.ways_to_compute[choice].clone())
                .collect();
            let fused_pruned: Vec<HowToComputeAggregateMeasure> = pruned_choices
                .iter()
                .filter(|&&choice| choice >= own)
                .filter_map(|&choice| {
                    let fused = target.fused_computation.as_ref()?;
                    fused.ways_to_compute.get(choice - own).cloned()
                })
                .collect();
            let pruned = |of_fused_computation| {
                move |way_to_compute| PrunedWayToCompute {
                    target: index,
                    of_fused_computation,
                    way_to_compute,
                    reason: PruneReason::NoErrorBoundForQueryTarget,
                }
            };
            let removed = prune_own(&mut target.ways_to_compute, |way| own_pruned.contains(way));
            self.pruned_ways_to_compute
                .extend(removed.into_iter().map(pruned(false)));
            if let Some(fused) = &mut target.fused_computation {
                let (removed, kept): (Vec<_>, _) = std::mem::take(&mut fused.ways_to_compute)
                    .into_iter()
                    .partition(|way| fused_pruned.contains(way));
                fused.ways_to_compute = kept;
                self.pruned_ways_to_compute
                    .extend(removed.into_iter().map(pruned(true)));
            }
        }
        self
    }
}

/// The sources every scan below `node` reads, each once.
fn sources_read(node: &Rc<OperatorNode>) -> Vec<Source> {
    let mut sources = Vec::new();
    for node in OperatorNode::reachable(node) {
        if let Some(NonASAPOp::Scan { source, .. }) = node.non_asap() {
            if !sources.contains(source) {
                sources.push(source.clone());
            }
        }
    }
    sources
}

/// Whether a query's composed output `root` can meet `target` at some size
/// of its summaries: its guarantee is exact, or it has one in a metric
/// `model` says can meet `target` ([`AccuracyModel::metric_can_meet`]; an
/// exact target accepts only exact) whose error, and failure probability
/// when `target` sets one, are known once the statistics Pass 1 left
/// symbolic are bound and the guarantee is stated over the values the
/// target's scope covers, as plan selection states it. The sign of a
/// metric's samples is bound as plan selection binds it, proven only when
/// `data` declares the metric a counter or non-negative
/// ([`declared_non_negative`]). Classic HLL's failure probability is bound
/// from `data`'s declared distinct bound for its source at the largest
/// precision the contract covers: a bound outside the contract there (no
/// certificate, failure probability 1) is outside it at every size, so it
/// stays unbound. Every other statistic (row and series counts, stream
/// norms, an undeclared distinct bound, operand domains) is assumed
/// bindable, so no candidate is pruned for one plan selection may bind.
/// The number of values a whole-result target covers is `data`'s
/// ([`values_per_result`]): when it is not bound, a per-value guarantee
/// that may fail never meets a whole-result failure probability, whatever
/// the size (sizing splits δ over those values).
pub fn output_can_meet_target(
    root: &QueryRoot,
    target: &AccuracyTarget,
    data: &DataWorkload,
    model: &dyn AccuracyModel,
) -> bool {
    let bind = |guarantee: &AccuracyGuarantee| {
        guarantee.bind(&|statistic| {
            if let Some(metric) = parse_raw_data_samples_sign_statistic(statistic) {
                return declared_non_negative(data, metric).then_some(1.0);
            }
            if let Some((_, relative_error, source)) =
                hll::parse_classic_failure_statistic(statistic)
            {
                // At the largest precision the contract covers: no
                // certificate there, none at any size.
                let declared = data
                    .sources
                    .get(source)
                    .and_then(|facts| facts.max_distinct_per_evaluation);
                if let Some(bound) = declared {
                    let best =
                        hll::classic_failure_probability(hll::MAX_PRECISION, relative_error, bound);
                    return (best < 1.0).then_some(best);
                }
            }
            Some(1.0)
        })
    };
    let guarantee = match root_guarantee(root) {
        Some(guarantee) => Some(bind(&guarantee)),
        None => bind_guarantees(root, &bind),
    };
    let Some(guarantee) = guarantee else {
        return false;
    };
    if guarantee.is_exact() {
        return true;
    }
    let values = values_per_result(root, series_read(root, data), data);
    let guarantee = guarantee.over_scope(target.accuracy_scope, values);
    model.metric_can_meet(guarantee.spec.accuracy_metric, target)
        && guarantee.spec.error.evaluate().is_some()
        && (target.failure_probability.is_none()
            || guarantee.spec.failure_probability.evaluate().is_some())
}

/// The indices of `targets` some target's fused computation absorbs.
fn absorbed_targets(targets: &[ComputationTarget]) -> impl Iterator<Item = usize> + '_ {
    targets
        .iter()
        .filter_map(|target| target.fused_computation.as_ref())
        .flat_map(|fused| fused.absorbed_targets.iter().copied())
}

#[derive(Debug, Error)]
pub enum PerComputationCandidateError {
    #[error(transparent)]
    Structure(#[from] SchemaDerivationError),
    #[error("logical candidate input already has assigned execution timing or materialization")]
    AssignedPhysicalAnnotation,
    #[error("logical candidate input already contains ASAP operators")]
    ContainsAsap,
    /// `root` (the query's index in the workload) and `measure` locate the target;
    /// [`computation_candidates_for_intent`] alone leaves them `None`.
    #[error(
        "approximate accuracy of root {}, measure {} requires epsilon and delta in (0, 1)",
        root.as_deref().unwrap_or("?"),
        measure.map_or_else(|| "?".to_string(), |measure| measure.to_string())
    )]
    InvalidAccuracy {
        root: Option<String>,
        measure: Option<usize>,
    },
    #[error("choice must name one listed candidate per target")]
    InvalidChoice,
    #[error("unsupported substitution in logical ASAP-aware optimization: {0}")]
    Unsupported(&'static str),
    /// A Pass 2 rule found more than `max` candidates of one kind, whose
    /// 2^n subsets it would enumerate: the workload is too large to plan
    /// this way, not bad input.
    #[error("found {found} {counted}, more than {max}")]
    TooManyCandidates {
        /// What was counted, e.g. "rollup candidates".
        counted: &'static str,
        /// How many were found, more than `max`.
        found: usize,
        /// The cap that was exceeded.
        max: usize,
    },
}

/// Enumerate exact, rewrite and summary choices of one measure in stable catalog
/// order, without ranking or empirical assessment. A summary is not a claim that
/// a deployment meets the measure's accuracy requirement. The one exact option
/// comes first: the exact accumulator form if the intent has one, else `Exact`.
pub fn computation_candidates_for_intent(
    intent: &AggIntent,
) -> Result<ComputationChoices, PerComputationCandidateError> {
    let states = match intent {
        AggIntent::Count { .. } => vec![ExactKind::Count],
        AggIntent::Sum { .. } => vec![ExactKind::Sum],
        AggIntent::Min { .. } => vec![ExactKind::Min],
        AggIntent::Max { .. } => vec![ExactKind::Max],
        AggIntent::Rate => vec![ExactKind::Rate],
        AggIntent::IRate => vec![ExactKind::IRate],
        AggIntent::Increase => vec![ExactKind::Increase],
        AggIntent::Avg { .. } => vec![ExactKind::Sum, ExactKind::Count],
        _ => vec![],
    };
    let exact = if states.is_empty() {
        HowToComputeAggregateMeasure::Exact
    } else {
        HowToComputeAggregateMeasure::ExactAccumulator(states)
    };
    let mut choices = ComputationChoices {
        candidates: vec![exact],
        pruned: vec![],
    };
    if let Some(target) = sizing_target(intent) {
        if !target.is_exact() {
            let (epsilon, delta) = accuracy_budget(&target);
            let unit = |value: f64| value > 0.0 && value < 1.0;
            if !unit(epsilon) || !unit(delta) {
                return Err(PerComputationCandidateError::InvalidAccuracy {
                    root: None,
                    measure: None,
                });
            }
        }
        let summaries = summary_candidates(intent).into_iter();
        if matches!(intent, AggIntent::Quantile { q, .. } if !(0.0..=1.0).contains(q)) {
            choices.pruned.extend(summaries.map(|algorithm| {
                (
                    HowToComputeAggregateMeasure::Summary(algorithm),
                    PruneReason::QuantileOutsideUnitInterval,
                )
            }));
        } else if target.is_exact() {
            choices.pruned.extend(summaries.map(|algorithm| {
                (
                    HowToComputeAggregateMeasure::Summary(algorithm),
                    PruneReason::RequiresExact,
                )
            }));
        } else {
            choices
                .candidates
                .extend(summaries.map(HowToComputeAggregateMeasure::Summary));
        }
    }
    Ok(choices)
}

/// Whether an `avg` measure's input may be null, so `count` rows differ from
/// counted values. An unresolvable input is treated as possibly null.
fn nullable_avg_input(node: &OperatorNode, intent: &AggIntent) -> bool {
    let Some(NonASAPOp::Aggregate { child, .. }) = node.non_asap() else {
        return true;
    };
    let column = match intent.input_cols().first() {
        Some(column) => Some(*column),
        None => resolve_column_ref(&ColumnRef::SampleValue, &child.schema).ok(),
    };
    column
        .and_then(|column| child.schema.fields.get(column))
        .is_none_or(|field| field.nullable)
}

/// Whether `having` over `aggregate`'s output reads only `by(...)` group keys.
/// A global aggregate (no group keys) never qualifies: its one output row
/// exists even when no input row does, so no row predicate can stand in.
fn key_only_having(aggregate: &OperatorNode, having: &Predicate) -> bool {
    let keys = match aggregate.non_asap() {
        Some(NonASAPOp::Aggregate {
            reduction: Reduction::Reduce(by),
            ..
        }) if !by.is_without() => by.len(),
        _ => 0,
    };
    keys > 0
        && having.0.operator_refs().is_empty()
        && having.0.columns_referenced().iter().all(|id| *id < keys)
}

fn placement(below: bool) -> HavingPlacement {
    if below {
        HavingPlacement::BelowAggregate
    } else {
        HavingPlacement::AboveOutput
    }
}

/// Discover one target per measure of each eligible aggregate memo group of
/// `dag`, including operator plans read by scalar roots: Pass 1 walks the
/// groups, so an aggregate several queries read is one target per measure.
/// An aggregate measure is eligible when it has a realization besides exact
/// execution, even if every such realization is pruned; the prune reasons
/// are reported on the target. Measure filters and `HAVING` keep every
/// candidate; the target records how they are applied. A group's readers
/// are its parent groups and the query roots that read it; a consumer
/// `Filter`'s key-only `HAVING` is pushed below only when every reader of
/// the group applies the same predicate, so every candidate of a shared
/// aggregate computes the group's own rows (#1089).
pub fn generate_per_computation_candidates(
    dag: &DagOfLogicalMemoGroups,
) -> Result<PerComputationCandidates, PerComputationCandidateError> {
    let logical_nodes: Rc<[Rc<OperatorNode>]> = dag
        .groups
        .iter()
        .map(|group| Rc::clone(&group.logical_node))
        .collect();
    let group_of: HashMap<*const OperatorNode, MemoGroupId> = logical_nodes
        .iter()
        .enumerate()
        .map(|(g, node)| (Rc::as_ptr(node), MemoGroupId(g as u32)))
        .collect();
    let roots = dag.roots_over_groups();
    let mut seen = HashSet::new();
    // Each aggregate group with the first query that reaches it.
    let mut aggregates = Vec::new();
    // Every reader of each group's node: `Some(parent)`, or `None` for a
    // query root or scalar reference.
    let mut consumers: HashMap<*const OperatorNode, Vec<Option<Rc<OperatorNode>>>> = HashMap::new();
    for (query, root) in roots.iter().enumerate() {
        root.validate_structure()?;
        let operators = match root {
            QueryRoot::Operator(node) => vec![node],
            QueryRoot::Scalar(expr) => expr.operator_refs(),
        };
        for root in operators {
            if root.contains_asap() {
                return Err(PerComputationCandidateError::ContainsAsap);
            }
            consumers.entry(Rc::as_ptr(root)).or_default().push(None);
            for node in OperatorNode::reachable(root) {
                if !seen.insert(Rc::as_ptr(&node)) {
                    continue;
                }
                if node.timing.is_some() || node.materialization != Materialization::NotMaterialized
                {
                    return Err(PerComputationCandidateError::AssignedPhysicalAnnotation);
                }
                for child in node.children() {
                    consumers
                        .entry(Rc::as_ptr(child))
                        .or_default()
                        .push(Some(Rc::clone(&node)));
                }
                if matches!(node.non_asap(), Some(NonASAPOp::Aggregate { .. })) {
                    aggregates.push((query, node));
                }
            }
        }
    }
    let mut targets = Vec::new();
    let mut pruned_ways_to_compute = Vec::new();
    for (query, node) in aggregates {
        let Some(NonASAPOp::Aggregate {
            measures,
            filters,
            having,
            ..
        }) = node.non_asap()
        else {
            continue;
        };
        let aggregate_group = group_of[&Rc::as_ptr(&node)];
        let mut entries: Vec<HavingEntry> = having
            .iter()
            .map(|having| HavingEntry {
                filter: None,
                predicate: having.clone(),
                placement: placement(key_only_having(&node, having)),
            })
            .collect();
        let parents = &consumers[&Rc::as_ptr(&node)];
        // Only a `Filter` whose input is this aggregate is its `HAVING`; one
        // that reads it from a scalar expression stays a plain consumer.
        let mut filters_above: Vec<(&Rc<OperatorNode>, &Predicate)> = Vec::new();
        for parent in parents.iter().flatten() {
            if let Some(NonASAPOp::Filter { child, pred }) = parent.non_asap() {
                if Rc::ptr_eq(child, &node)
                    && !filters_above
                        .iter()
                        .any(|(filter, _)| Rc::ptr_eq(filter, parent))
                {
                    filters_above.push((parent, pred));
                }
            }
        }
        for (filter, predicate) in &filters_above {
            // Every reader of the group, across queries, applies it.
            let shared_by_all = filters_above.len() == parents.len()
                && filters_above.iter().all(|(_, other)| other == predicate);
            entries.push(HavingEntry {
                filter: Some(Rc::clone(filter)),
                predicate: (*predicate).clone(),
                placement: placement(shared_by_all && key_only_having(&node, predicate)),
            });
        }
        for (measure, intent) in measures.iter().enumerate() {
            let mut choices =
                computation_candidates_for_intent(intent).map_err(|error| match error {
                    PerComputationCandidateError::InvalidAccuracy { .. } => {
                        PerComputationCandidateError::InvalidAccuracy {
                            root: Some(query.to_string()),
                            measure: Some(measure),
                        }
                    }
                    error => error,
                })?;
            if choices.candidates == [HowToComputeAggregateMeasure::Exact]
                && choices.pruned.is_empty()
            {
                continue;
            }
            if matches!(intent, AggIntent::Avg { .. }) && nullable_avg_input(&node, intent) {
                choices.prune(PruneReason::NullableAvgInput, |choice| {
                    matches!(choice, HowToComputeAggregateMeasure::ExactAccumulator(_))
                });
            }
            if matches!(intent, AggIntent::Count { .. })
                && node.children()[0].result_kind == OperatorResultKind::Relation
            {
                choices.prune(PruneReason::NoRelationalCountSketchRealization, |choice| {
                    matches!(choice, HowToComputeAggregateMeasure::Summary(_))
                });
            }
            pruned_ways_to_compute.extend(choices.pruned.into_iter().map(
                |(way_to_compute, reason)| PrunedWayToCompute {
                    target: targets.len(),
                    of_fused_computation: false,
                    way_to_compute,
                    reason,
                },
            ));
            targets.push(ComputationTarget {
                aggregate_group,
                measure_index: measure,
                ways_to_compute: choices.candidates,
                measure_filter: filters.get(measure).cloned().flatten(),
                having_filters: entries.clone(),
                fused_computation: None,
                raw_data_samples_of: node.non_asap().and_then(|op| match op {
                    NonASAPOp::Aggregate { child, .. } => raw_data_samples_of_metric(child),
                    _ => None,
                }),
            });
        }
    }
    add_whole_expression_targets(&mut targets, &logical_nodes);
    prune_topk_heaps_over_one_row_per_item(
        &mut targets,
        &logical_nodes,
        &mut pruned_ways_to_compute,
    );
    add_nested_sum_targets(&mut targets, &logical_nodes);
    close_absorptions(&mut targets);
    Ok(PerComputationCandidates {
        roots,
        logical_nodes,
        targets,
        pruned_ways_to_compute,
    })
}

/// Adds to each fused computation's `absorbed_targets` the targets its absorbed
/// targets' own fused computations absorb, transitively: none of them is
/// computed when the fused computation is picked, so enumeration keeps only
/// their first choice ([`PerComputationCandidates::absorbed_choice_varies`])
/// and [`PerComputationCandidates::combinations`] counts exactly those.
fn close_absorptions(targets: &mut [ComputationTarget]) {
    loop {
        let mut changed = false;
        for t in 0..targets.len() {
            let Some(fused) = &targets[t].fused_computation else {
                continue;
            };
            let beneath: Vec<usize> = fused
                .absorbed_targets
                .iter()
                .filter_map(|&a| targets[a].fused_computation.as_ref())
                .flat_map(|inner| inner.absorbed_targets.iter().copied())
                .filter(|b| !fused.absorbed_targets.contains(b))
                .collect();
            if let Some(fused) = targets[t]
                .fused_computation
                .as_mut()
                .filter(|_| !beneath.is_empty())
            {
                for b in beneath {
                    if !fused.absorbed_targets.contains(&b) {
                        fused.absorbed_targets.push(b);
                    }
                }
                changed = true;
            }
        }
        if !changed {
            return;
        }
    }
}

/// Nested-sum merge: a grouped `sum` over the value of a `sum` target
/// beneath it gets a fused computation ([`ComputationTarget::fused_computation`])
/// with the one candidate `ExactAccumulator([Sum])`: one exact `Sum`
/// accumulator over the inner sum's input, grouped by the outer keys (the
/// sum of the inner groups' sums is the sum of their rows), absorbing the
/// inner target. Offered whatever else reads the inner sum: it is one more
/// candidate of the outer sum's group, and the inner sum's group keeps its
/// own candidates for its other readers. Applied when neither has a filter
/// or `HAVING`
/// ([`nested_sum_input`]), and when the outer sum is not itself absorbed by
/// a merge and has no fused computation yet, and the inner sum has none, so
/// merges do not stack. An outer sum a whole-expression top-k absorbs
/// (`topk(k, sum by (G) (sum_over_time(m[W])))`) keeps its merge for the
/// top-k's own candidates.
fn add_nested_sum_targets(targets: &mut [ComputationTarget], logical_nodes: &[Rc<OperatorNode>]) {
    let aggregate = |target: &ComputationTarget| &logical_nodes[target.aggregate_group.0 as usize];
    // The targets a merge absorbs.
    let mut absorbed: HashSet<usize> = HashSet::new();
    for t in 0..targets.len() {
        let target = &targets[t];
        if absorbed.contains(&t)
            || target.fused_computation.is_some()
            || !target.having_filters.is_empty()
        {
            continue;
        }
        let Some(NonASAPOp::Aggregate { child, .. }) = aggregate(target).non_asap() else {
            continue;
        };
        let Some(inner) = targets
            .iter()
            .position(|other| Rc::ptr_eq(aggregate(other), child))
        else {
            continue;
        };
        // Merges do not stack: the inner sum must not have a fused
        // computation of its own.
        if !targets[inner].having_filters.is_empty()
            || targets[inner].fused_computation.is_some()
            || nested_sum_input(aggregate(target)).is_none()
        {
            continue;
        }
        absorbed.insert(inner);
        let merge = HowToComputeAggregateMeasure::ExactAccumulator(vec![ExactKind::Sum]);
        targets[t].fused_computation = Some(FusedComputation {
            absorbed_targets: vec![inner],
            ways_to_compute: vec![merge],
        });
    }
}

/// The input rows, the inner sum and the outer grouping (moved onto the
/// input rows by column, [`partitions_over`]) of a nested-sum merge of
/// `target`, or `None` when it does not apply: `target` is a grouped `sum`
/// (not by `without`) of the value column of a per-series or grouped `sum`
/// directly beneath it, neither filtered nor with `HAVING`, and not grouped
/// by the metric name a per-series inner sum drops.
fn nested_sum_input(target: &OperatorNode) -> Option<(Rc<OperatorNode>, AggIntent, Reduction)> {
    use asap_types::ir::operator::non_asap::any_measure_filtered;
    let Some(NonASAPOp::Aggregate {
        child: inner,
        reduction: reduction @ Reduction::Reduce(by),
        measures,
        filters,
        having: None,
        ..
    }) = target.non_asap()
    else {
        return None;
    };
    let [AggIntent::Sum { col }] = measures.as_slice() else {
        return None;
    };
    let Some(NonASAPOp::Aggregate {
        child: rows,
        reduction: inner_reduction,
        measures: inner_measures,
        filters: inner_filters,
        having: None,
        ..
    }) = inner.non_asap()
    else {
        return None;
    };
    let [inner_sum @ AggIntent::Sum { .. }] = inner_measures.as_slice() else {
        return None;
    };
    if by.is_without() || any_measure_filtered(filters) || any_measure_filtered(inner_filters) {
        return None;
    }
    // The inner sum's value column: a per-series sum keeps its labels and
    // carries the value as the sample value; a grouped one ends with it.
    let value = match inner_reduction {
        Reduction::PerEntity => resolve_column_ref(&ColumnRef::SampleValue, &inner.schema).ok()?,
        Reduction::Reduce(keys) if !keys.is_without() => keys.len(),
        Reduction::Reduce(_) => return None,
    };
    let summed = match col {
        None => resolve_column_ref(&ColumnRef::SampleValue, &inner.schema).ok()?,
        Some(column) => *column,
    };
    if summed != value || by.keys().contains(&value) {
        return None;
    }
    // A per-series sum drops the metric name, which the raw rows keep:
    // grouping them by it would name groups the nested sums leave unnamed.
    if matches!(inner_reduction, Reduction::PerEntity)
        && by.keys().iter().any(|&key| {
            inner
                .schema
                .fields
                .get(key)
                .is_some_and(|f| f.name == "__name__")
        })
    {
        return None;
    }
    let partitions = partitions_over(reduction, &inner.schema, &rows.schema)?;
    Some((Rc::clone(rows), inner_sum.clone(), partitions))
}

/// Whole-expression top-k (Example 1's Q2): a top-k over a per-item `sum`
/// or `count` that is itself a target gets a fused computation
/// ([`ComputationTarget::fused_computation`]) absorbing the inner target, whose
/// candidates are the top-k's heap summaries (Count-Min and CountSketch
/// with a heap), each one heap over the inner aggregate's input, keyed by
/// the ranked item and weighted by the summed value. Those heaps over the
/// exact per-item result leave the top-k's own candidates: the fused
/// computation replaces them. Over a grouped sum of per-series sums or
/// counts of raw data samples, the heap reads those samples and absorbs the
/// per-series target too. Offered whatever else reads the inner aggregate:
/// it is one more candidate of the top-k's group, and the absorbed groups
/// keep their own candidates for their other readers. Applied when neither
/// has a filter or `HAVING`, and when the keyed-additive rule realizes it
/// ([`whole_expression_input`]).
fn add_whole_expression_targets(
    targets: &mut [ComputationTarget],
    logical_nodes: &[Rc<OperatorNode>],
) {
    let aggregate = |target: &ComputationTarget| &logical_nodes[target.aggregate_group.0 as usize];
    for t in 0..targets.len() {
        let target = &targets[t];
        let Some(NonASAPOp::Aggregate { child, .. }) = aggregate(target).non_asap() else {
            continue;
        };
        let Some(inner) = targets
            .iter()
            .position(|other| Rc::ptr_eq(aggregate(other), child))
        else {
            continue;
        };
        if target.measure_filter.is_some()
            || !target.having_filters.is_empty()
            || targets[inner].measure_filter.is_some()
            || !targets[inner].having_filters.is_empty()
        {
            continue;
        }
        let Some((input, ..)) = whole_expression_input(aggregate(target)) else {
            continue;
        };
        // A heap over the raw data samples beneath a grouped sum of per-series
        // sums or counts also absorbs the per-series target.
        let mut absorbs = vec![inner];
        if let Some(NonASAPOp::Aggregate {
            child: per_series, ..
        }) = aggregate(&targets[inner]).non_asap()
        {
            let reads_below = per_series
                .children()
                .iter()
                .any(|&rows| Rc::ptr_eq(rows, &input));
            if reads_below {
                absorbs.extend(
                    targets
                        .iter()
                        .position(|other| Rc::ptr_eq(aggregate(other), per_series)),
                );
            }
        }
        let (heaps, own): (Vec<_>, Vec<_>) =
            targets[t]
                .ways_to_compute
                .iter()
                .cloned()
                .partition(|candidate| {
                    matches!(candidate, HowToComputeAggregateMeasure::Summary(family)
                    if family.capability().answers(StatisticKind::TopK))
                });
        if heaps.is_empty() {
            continue;
        }
        let target = &mut targets[t];
        target.ways_to_compute = own;
        target.fused_computation = Some(FusedComputation {
            absorbed_targets: absorbs,
            ways_to_compute: heaps,
        });
    }
}

/// Prunes the own heap candidates of each PromQL top-k
/// ([`PruneReason::TopKHeapOverOneRowPerItem`]); a fused whole-expression
/// heap ([`add_whole_expression_targets`]) has already moved out of them.
fn prune_topk_heaps_over_one_row_per_item(
    targets: &mut [ComputationTarget],
    logical_nodes: &[Rc<OperatorNode>],
    pruned_ways_to_compute: &mut Vec<PrunedWayToCompute>,
) {
    for (index, target) in targets.iter_mut().enumerate() {
        let Some(NonASAPOp::Aggregate {
            measures, child, ..
        }) = logical_nodes[target.aggregate_group.0 as usize].non_asap()
        else {
            continue;
        };
        if !matches!(
            measures.get(target.measure_index),
            Some(AggIntent::TopK { .. })
        ) {
            continue;
        }
        // A PromQL top-k ranks an instant vector: one row per series or
        // group. (A SQL top-k keeps its heaps.)
        if !reads_only_time_series(child) {
            continue;
        }
        let pruned = prune_own(&mut target.ways_to_compute, |candidate| {
            matches!(candidate, HowToComputeAggregateMeasure::Summary(family)
                if family.capability().answers(StatisticKind::TopK))
        });
        pruned_ways_to_compute.extend(pruned.into_iter().map(|way_to_compute| {
            PrunedWayToCompute {
                target: index,
                of_fused_computation: false,
                way_to_compute,
                reason: PruneReason::TopKHeapOverOneRowPerItem,
            }
        }));
    }
}

/// Whether every scan below `node` reads a time series (PromQL rows).
fn reads_only_time_series(node: &OperatorNode) -> bool {
    match node.non_asap() {
        Some(NonASAPOp::Scan { source, .. }) => matches!(source, Source::TimeSeries { .. }),
        _ => node.children().iter().all(|c| reads_only_time_series(c)),
    }
}

/// The input, update and partitions of a whole-expression top-k over
/// `target`'s inner aggregate, by the keyed-additive rule, or `None` when it
/// does not apply. Rows that carry the full series identity rank it as a
/// column, as [`summary_update`] does. The partitions are `target`'s, moved
/// onto the input the heap reads ([`partitions_over`]).
fn whole_expression_input(
    target: &OperatorNode,
) -> Option<(Rc<OperatorNode>, SummaryUpdate, Reduction)> {
    use crate::pass1::realization::realize_keyed_additive_summary_input;
    let Some(NonASAPOp::Aggregate {
        child,
        reduction,
        measures,
        filters,
        having: None,
        ..
    }) = target.non_asap()
    else {
        return None;
    };
    let [intent @ AggIntent::TopK { .. }] = measures.as_slice() else {
        return None;
    };
    if asap_types::ir::operator::non_asap::any_measure_filtered(filters) {
        return None;
    }
    // The rule reads only the family's algorithm, and rejects Count-Min over
    // weights it cannot prove non-negative. Ask with CountSketch so one
    // update serves both heap families; plan selection decides whether
    // Count-Min is admissible, as for every other Count-Min candidate.
    let family = FieldDataType::Sketch(
        SketchKind::new(
            SketchAlgorithm::CountSketchWithHeap,
            asap_types::ir::schema::SketchParams::CountSketchWithHeap {
                width: 1,
                depth: 1,
                heap_size: 1,
            },
        ),
        GroupingStrategy::default(),
    );
    let realized = realize_keyed_additive_summary_input(intent, &family, reduction, child)?;
    let mut input = realized.input;
    let per_series = matches!(
        child.non_asap(),
        Some(NonASAPOp::Aggregate {
            reduction: Reduction::PerEntity,
            ..
        })
    );
    let partitions = partitions_over(reduction, &child.schema, &realized.child.schema)?;
    if per_series && realized.child.schema.has_promql_series_identity() {
        input.item = Some(asap_types::ir::schema::topk::series_identity_item(
            &realized.child.schema,
            partitions.group_keys().map_or(&[][..], |keys| keys.keys()),
        ));
    } else if realized.child.schema.closed
        && matches!(input.item, Some(SummaryInputExpr::EntityIdentity(_)))
    {
        // An encoded label set needs an open PromQL schema. PromQL rows that
        // carry the series identity are closed, but their inner group labels
        // are item columns. SQL group keys are explicit columns too, so their
        // keyed heap can consume raw rows without a per-key aggregate first.
        return None;
    }
    Some((realized.child, input, partitions))
}

/// `reduction`'s keys, which index `inner` (the absorbed aggregate's
/// output), as indices into `input` (the rows the heap reads), matched by
/// column. `None` when a key is missing or ambiguous in `input`, or the keys
/// are given by `without`: `input` carries columns `inner` dropped.
pub(crate) fn partitions_over(
    reduction: &Reduction,
    inner: &Schema,
    input: &Schema,
) -> Option<Reduction> {
    let Reduction::Reduce(keys) = reduction else {
        return Some(Reduction::PerEntity);
    };
    if keys.is_without() {
        return None;
    }
    let column = |schema: &Schema, index: usize| {
        crate::pass1::realization::column_ref(&schema.fields[index])
    };
    let keys = keys
        .keys()
        .iter()
        .map(|&index| {
            inner.fields.get(index)?;
            let key = column(inner, index);
            let mut matches = (0..input.fields.len()).filter(|&i| column(input, i) == key);
            match (matches.next(), matches.next()) {
                (Some(i), None) => Some(i),
                _ => None,
            }
        })
        .collect::<Option<_>>()?;
    Some(Reduction::by(keys))
}

/// `update` with the open requirement that its weights are samples of
/// `metric` ([`WeightDomain::RawDataSamplesOfMetric`]), when it is a keyed heap
/// update whose weights are not otherwise proven.
fn with_raw_data_samples_requirement(
    mut update: SummaryUpdate,
    metric: Option<&str>,
) -> SummaryUpdate {
    if let Some(metric) = metric {
        if update.item.is_some() && update.weight_domain == WeightDomain::UnknownOrSigned {
            update.weight_domain = WeightDomain::RawDataSamplesOfMetric {
                metric: metric.to_string(),
            };
        }
    }
    update
}

/// Build one whole-workload candidate (end of Pass 1): `choice[i]` picks
/// `inventory.targets[i]`'s candidate, or past its own candidates its fused
/// computation's ([`ComputationTarget::computation_for`]). An aggregate with
/// a chosen candidate other than `Exact` is replaced measure by measure: an
/// exact accumulator by `SummaryAgg` → `FinalizeExactAccumulator`, a summary
/// by `SummaryAgg` → `SummaryEstimate`, `avg` by its `sum` and `count`
/// accumulators divided, a fused computation by one state over the absorbed
/// target's input, and any other measure by its own single-measure
/// aggregate; several measures are joined back on the group keys. A summary
/// is sized here for its measure's accuracy requirement; Pass 1 leaves it
/// unsized; several approximate inputs of one query share its budget
/// equally ([`budget_shares`]). A measure filter becomes `SummaryAgg.filter`;
/// `HAVING` is applied as its target records. A sub-DAG shared between roots
/// stays shared. Every node carries the guarantee `asap_accuracy::propagation::composed_guarantee`
/// gives it.
pub fn compose_logical_candidate(
    inventory: &PerComputationCandidates,
    choice: &[usize],
) -> Result<Vec<QueryRoot>, PerComputationCandidateError> {
    compose_with_budget_share(inventory, &inventory.roots, choice, None)
}

/// [`compose_logical_candidate`] with every sketch summary sized for one of
/// `share` equal shares of its measure's budget, whatever the roots
/// compose: the size a summary gets when it is one of `share` approximate
/// inputs of a query ([`budget_shares`]). `share` 1 sizes for the whole
/// budget. The sub-expression memo composes one aggregate alone this way,
/// once per share its readers may give it.
pub fn compose_logical_candidate_with_budget_share(
    inventory: &PerComputationCandidates,
    choice: &[usize],
    share: usize,
) -> Result<Vec<QueryRoot>, PerComputationCandidateError> {
    compose_with_budget_share(inventory, &inventory.roots, choice, Some(share))
}

/// [`compose_logical_candidate`] of `roots` (some of `inventory.roots`),
/// each summary sized for `budget_share` shares of its budget, or as
/// [`budget_shares`] splits it when `None`.
fn compose_with_budget_share(
    inventory: &PerComputationCandidates,
    roots: &[QueryRoot],
    choice: &[usize],
    budget_share: Option<usize>,
) -> Result<Vec<QueryRoot>, PerComputationCandidateError> {
    if choice.len() != inventory.targets.len() {
        return Err(PerComputationCandidateError::InvalidChoice);
    }
    let mut chosen: Chosen = HashMap::new();
    for (target, &index) in inventory.targets.iter().zip(choice) {
        let (candidate, absorbs) = target
            .computation_for(index)
            .ok_or(PerComputationCandidateError::InvalidChoice)?;
        chosen
            .entry(Rc::as_ptr(inventory.aggregate(target)))
            .or_default()
            .push(ChosenComputation {
                target,
                candidate,
                fused: absorbs.is_some(),
            });
    }
    // Consumer `Filter`s whose key-only `HAVING` moves below a substituted
    // aggregate: each is replaced by its input.
    let pushed_below: HashSet<_> = chosen
        .values()
        .filter(|targets| substituted(targets))
        .flat_map(|targets| &targets[0].target.having_filters)
        .filter(|entry| entry.placement == HavingPlacement::BelowAggregate)
        .filter_map(|entry| entry.filter.as_ref().map(Rc::as_ptr))
        .collect();
    // Each summary is first sized for its measure's whole budget. A query
    // whose value composes several approximate inputs is then composed
    // again with each sized for an equal share ([`budget_shares`]).
    let whole = Sizing::default();
    let composed = compose_roots(roots, &chosen, &pushed_below, &whole)?;
    let shares = match budget_share {
        None => budget_shares(&composed, &whole.built.into_inner()),
        Some(share) if share > 1 => whole
            .built
            .into_inner()
            .into_iter()
            .map(|(_, key)| (key, share))
            .collect(),
        Some(_) => HashMap::new(),
    };
    if shares.is_empty() {
        return Ok(composed);
    }
    let shared = Sizing {
        shares,
        ..Sizing::default()
    };
    compose_roots(roots, &chosen, &pushed_below, &shared)
}

/// One measure of an aggregate: the aggregate and the measure's index.
type MeasureKey = (*const OperatorNode, usize);

/// How [`compose_logical_candidate`] sizes summaries in one composition.
#[derive(Default)]
struct Sizing {
    /// How many approximate inputs each measure's summary shares its
    /// query's budget with; a measure not listed has the whole budget.
    shares: HashMap<MeasureKey, usize>,
    /// Every sketch summary state built, with the measure it sizes.
    built: std::cell::RefCell<Vec<(Rc<OperatorNode>, MeasureKey)>>,
}

/// The equal budget split among the approximate inputs of each query
/// (#669b): a query whose value carries a guarantee that is not exact
/// composes every sketch summary state below its root
/// ([`approximate_inputs`]), so with `n ≥ 2` of them each is sized for one
/// of `n` equal shares of the query's target. A state several queries read
/// takes the largest `n`. Only states `built` lists (by the measure they
/// size) are split. Empty when no query composes two approximate inputs.
fn budget_shares(
    roots: &[QueryRoot],
    built: &[(Rc<OperatorNode>, MeasureKey)],
) -> HashMap<MeasureKey, usize> {
    let measure: HashMap<_, _> = built
        .iter()
        .map(|(state, key)| (Rc::as_ptr(state), *key))
        .collect();
    let mut shares = HashMap::new();
    for root in roots {
        let inputs = approximate_inputs(root);
        if inputs.len() < 2 {
            continue;
        }
        for state in &inputs {
            if let Some(key) = measure.get(&Rc::as_ptr(state)) {
                let share = shares.entry(*key).or_insert(1);
                *share = inputs.len().max(*share);
            }
        }
    }
    shares
}

/// Every root of `roots` with the chosen candidates substituted, each
/// summary sized as `sizing` says, and every node with its guarantee.
fn compose_roots(
    roots: &[QueryRoot],
    chosen: &Chosen<'_>,
    pushed_below: &HashSet<*const OperatorNode>,
    sizing: &Sizing,
) -> Result<Vec<QueryRoot>, PerComputationCandidateError> {
    let (mut cache, mut guaranteed) = (HashMap::new(), HashMap::new());
    let mut rewrite = |node: &Rc<OperatorNode>| {
        let node = rewrite(node, chosen, pushed_below, sizing, &mut cache)?;
        with_guarantees(&node, &mut guaranteed).map_err(PerComputationCandidateError::from)
    };
    let roots = roots
        .iter()
        .map(|root| {
            let root = match root {
                QueryRoot::Operator(node) => QueryRoot::Operator(rewrite(node)?),
                QueryRoot::Scalar(expr) => {
                    let mut rewritten = HashMap::new();
                    for node in expr.operator_refs() {
                        rewritten.insert(Rc::as_ptr(node), rewrite(node)?);
                    }
                    QueryRoot::Scalar(
                        expr.map_operator_refs(&mut |node| rewritten[&Rc::as_ptr(node)].clone()),
                    )
                }
            };
            // A substitution this module cannot express is an error, never an
            // invalid DAG.
            root.validate_structure()?;
            Ok(root)
        })
        .collect::<Result<Vec<_>, PerComputationCandidateError>>()?;
    // The built states, as the roots hold them: with their guarantees.
    for (state, _) in sizing.built.borrow_mut().iter_mut() {
        if let Some(with_guarantee) = guaranteed.get(&Rc::as_ptr(state)) {
            *state = with_guarantee.clone();
        }
    }
    Ok(roots)
}

/// One target's chosen computation in a whole-workload choice.
struct ChosenComputation<'a> {
    /// The target whose measure it computes.
    target: &'a ComputationTarget,
    /// How it is computed.
    candidate: &'a HowToComputeAggregateMeasure,
    /// Whether the candidate is the target's fused computation's, which
    /// absorbs the targets beneath; `false` for one of its own candidates.
    fused: bool,
}

/// Each aggregate's targets with their chosen computations.
type Chosen<'a> = HashMap<*const OperatorNode, Vec<ChosenComputation<'a>>>;
type NodeToRewrittenNodeCache = HashMap<*const OperatorNode, Rc<OperatorNode>>;

/// Whether any measure of the aggregate is substituted by something other than
/// its plain exact computation.
fn substituted(targets: &[ChosenComputation<'_>]) -> bool {
    targets
        .iter()
        .any(|chosen| *chosen.candidate != HowToComputeAggregateMeasure::Exact)
}

fn rewrite(
    node: &Rc<OperatorNode>,
    chosen: &Chosen<'_>,
    pushed_below: &HashSet<*const OperatorNode>,
    sizing: &Sizing,
    cache: &mut NodeToRewrittenNodeCache,
) -> Result<Rc<OperatorNode>, PerComputationCandidateError> {
    if let Some(done) = cache.get(&Rc::as_ptr(node)) {
        return Ok(done.clone());
    }
    for child in node.children() {
        rewrite(child, chosen, pushed_below, sizing, cache)?;
    }
    let changed = node
        .children()
        .iter()
        .any(|child| !Rc::ptr_eq(child, &cache[&Rc::as_ptr(child)]));
    let rebuilt = match (chosen.get(&Rc::as_ptr(node)), node.non_asap()) {
        (Some(targets), _) if substituted(targets) => substitute(node, targets, sizing, cache)?,
        (_, Some(NonASAPOp::Filter { child, .. })) if pushed_below.contains(&Rc::as_ptr(node)) => {
            cache[&Rc::as_ptr(child)].clone()
        }
        _ if changed => Rc::new(node.with_new_children(|child| cache[&Rc::as_ptr(child)].clone())?),
        _ => node.clone(),
    };
    cache.insert(Rc::as_ptr(node), rebuilt.clone());
    Ok(rebuilt)
}

/// Replace an aggregate whose measures are `targets`' (or exact, for a
/// measure that is no target) by its substitution, with the same output
/// columns.
fn substitute(
    target: &Rc<OperatorNode>,
    targets: &[ChosenComputation<'_>],
    sizing: &Sizing,
    cache: &NodeToRewrittenNodeCache,
) -> Result<Rc<OperatorNode>, PerComputationCandidateError> {
    let Some(NonASAPOp::Aggregate {
        child,
        reduction,
        measures,
        filters,
        ..
    }) = target.non_asap()
    else {
        return Err(PerComputationCandidateError::Unsupported(
            "target is not an aggregate",
        ));
    };
    // A key-only `HAVING` is a row predicate over the input: rows of a
    // dropped group never reach the state.
    let having = &targets[0].target.having_filters;
    // Operator refs in a predicate (a subquery) are this aggregate's
    // children, already rewritten.
    let rewritten = |pred: &Predicate| {
        Predicate(
            pred.0
                .map_operator_refs(&mut |node| cache[&Rc::as_ptr(node)].clone()),
        )
    };
    let mut input = cache[&Rc::as_ptr(child)].clone();
    for entry in having {
        if entry.placement == HavingPlacement::BelowAggregate {
            let keys = reduction.group_keys().map_or(&[][..], |by| by.keys());
            let pred = Predicate(entry.predicate.0.map_columns(&mut |column| keys[column]));
            input = OperatorNode::new_shared(Operator::NonASAP(NonASAPOp::Filter {
                pred,
                child: input,
            }))?;
        }
    }
    let parts = measures
        .iter()
        .enumerate()
        .map(|(measure, intent)| {
            // `fused`: the candidate is the target's fused computation's,
            // which absorbs the target beneath.
            let (candidate, raw_data_samples_of, fused) = targets
                .iter()
                .find(|chosen| chosen.target.measure_index == measure)
                .map_or(
                    (&HowToComputeAggregateMeasure::Exact, None, false),
                    |chosen| {
                        (
                            chosen.candidate,
                            chosen.target.raw_data_samples_of.as_deref(),
                            chosen.fused,
                        )
                    },
                );
            let filter = filters.get(measure).and_then(Option::as_ref).map(rewritten);
            let key = (Rc::as_ptr(target), measure);
            if fused {
                return match candidate {
                    HowToComputeAggregateMeasure::ExactAccumulator(_) => nested_sum(target, cache),
                    HowToComputeAggregateMeasure::Summary(algorithm) => whole_expression(
                        target,
                        intent,
                        algorithm,
                        raw_data_samples_of,
                        (sizing, key),
                        cache,
                    ),
                    HowToComputeAggregateMeasure::Exact => {
                        Err(PerComputationCandidateError::Unsupported(
                            "plain exact execution of a fused computation",
                        ))
                    }
                };
            }
            substitute_measure(
                target,
                intent,
                candidate,
                filter,
                &input,
                raw_data_samples_of,
                (sizing, key),
            )
        })
        .collect::<Result<Vec<_>, _>>()?;
    let mut output = match <[_; 1]>::try_from(parts) {
        Ok([part]) => with_names_of(target, part)?,
        Err(parts) => join_measures(target, parts)?,
    };
    for entry in having {
        if entry.placement == HavingPlacement::AboveOutput && entry.filter.is_none() {
            output = OperatorNode::new_shared(Operator::NonASAP(NonASAPOp::Filter {
                pred: rewritten(&entry.predicate),
                child: output,
            }))?;
        }
    }
    Ok(output)
}

/// `output` with `target`'s column names: an estimate or accumulator names
/// its value after the operation (`quantile`, `sum`), not the aggregate's
/// output column.
fn with_names_of(
    target: &OperatorNode,
    output: Rc<OperatorNode>,
) -> Result<Rc<OperatorNode>, PerComputationCandidateError> {
    let names = |node: &OperatorNode| -> Vec<String> {
        node.schema
            .fields
            .iter()
            .map(|field| field.name.clone())
            .collect()
    };
    // A top-k's ranked rows may carry their item columns after the
    // aggregate's ([`ranked_as_topk`]).
    if names(&output).starts_with(&names(target)) {
        return Ok(with_aggregate_qualifiers(target, output));
    }
    if output.schema.fields.len() != target.schema.fields.len() {
        return Err(PerComputationCandidateError::Unsupported(
            "substituted output with other columns than the aggregate",
        ));
    }
    let cols = names(target)
        .into_iter()
        .enumerate()
        .map(|(column, name)| ProjectItem {
            alias: Some(name),
            expr: ScalarExpr::Column(column),
        })
        .collect();
    Ok(with_aggregate_qualifiers(
        target,
        OperatorNode::new_shared(Operator::NonASAP(NonASAPOp::Project {
            cols,
            qualifier: None,
            child: output,
        }))?,
    ))
}

/// An equivalent aggregate replacement must retain SQL field identity. These
/// explicit qualifiers survive child rebinding; types and keys remain derived
/// from executable operators rather than copied from the requested output.
fn with_aggregate_qualifiers(target: &OperatorNode, output: Rc<OperatorNode>) -> Rc<OperatorNode> {
    if output
        .schema
        .fields
        .iter()
        .zip(&target.schema.fields)
        .all(|(field, expected)| field.table == expected.table)
    {
        return output;
    }
    let mut output = output.as_ref().clone();
    for (field, expected) in output.schema.fields.iter_mut().zip(&target.schema.fields) {
        field.table = expected.table.clone();
    }
    Rc::new(output)
}

/// A whole-expression top-k for `target` ([`whole_expression_input`]): one
/// heap summary of `algorithm` over the absorbed aggregate's rewritten
/// input, partitioned as `target`.
fn whole_expression(
    target: &OperatorNode,
    intent: &AggIntent,
    algorithm: &SketchAlgorithm,
    raw_data_samples_of: Option<&str>,
    (sizing, key): (&Sizing, MeasureKey),
    cache: &NodeToRewrittenNodeCache,
) -> Result<Rc<OperatorNode>, PerComputationCandidateError> {
    let (input, update, partitions) = whole_expression_input(target).ok_or(
        PerComputationCandidateError::Unsupported("whole-expression top-k input"),
    )?;
    let state = OperatorNode::new_shared(Operator::ASAP(ASAPOp::SummaryAgg {
        child: cache[&Rc::as_ptr(&input)].clone(),
        input: with_raw_data_samples_requirement(update, raw_data_samples_of),
        family: FieldDataType::Sketch(
            sized(algorithm, intent, sizing.share(key)),
            GroupingStrategy::default(),
        ),
        reduction: partitions,
        grouping: GroupingStrategy::default(),
        filter: None,
    }))?;
    sizing.built.borrow_mut().push((state.clone(), key));
    let estimate = OperatorNode::new_shared(Operator::ASAP(ASAPOp::SummaryEstimate {
        summary_input: state,
        query: sketch_statistic(intent).ok_or(PerComputationCandidateError::Unsupported(
            "sketch evaluation",
        ))?,
    }))?;
    ranked_as_topk(target, estimate)
}

/// A heap's top-k `estimate` (`[partitions.., items.., topk]`, one row per
/// ranked item) in `target`'s columns: its `[keys.., topk_k]`, with
/// `topk_k` the estimated Float64 weight, then the item columns. This is
/// the exact top-k's physical shape (its ranked rows' value, then the
/// columns naming each ranked item: PromQL labels, SQL group keys), so
/// readers of `[keys.., topk_k]` find those columns where they were.
fn ranked_as_topk(
    target: &OperatorNode,
    estimate: Rc<OperatorNode>,
) -> Result<Rc<OperatorNode>, PerComputationCandidateError> {
    let Some(NonASAPOp::Aggregate { reduction, .. }) = target.non_asap() else {
        return Err(PerComputationCandidateError::Unsupported(
            "top-k target is not an aggregate",
        ));
    };
    let ranked_index = reduction.group_keys().map_or(0, |keys| keys.len());
    let fields = &estimate.schema.fields;
    let estimated = fields.len() - 1;
    let cols = target
        .schema
        .fields
        .iter()
        .enumerate()
        .map(|(index, field)| {
            let source = if index == ranked_index {
                estimated
            } else {
                fields[..estimated]
                    .iter()
                    .position(|input| input.name == field.name)
                    .ok_or(PerComputationCandidateError::Unsupported(
                        "top-k estimate without a ranked-row identity column",
                    ))?
            };
            Ok(ProjectItem {
                alias: Some(field.name.clone()),
                expr: ScalarExpr::Column(source),
            })
        })
        .collect::<Result<Vec<_>, PerComputationCandidateError>>()?;
    Ok(OperatorNode::new_shared(Operator::NonASAP(
        NonASAPOp::Project {
            cols,
            qualifier: None,
            child: estimate,
        },
    ))?)
}

/// A nested-sum merge for `target` ([`nested_sum_input`]): one exact `Sum`
/// accumulator over the absorbed sum's rewritten input, grouped as
/// `target`, finalized.
fn nested_sum(
    target: &OperatorNode,
    cache: &NodeToRewrittenNodeCache,
) -> Result<Rc<OperatorNode>, PerComputationCandidateError> {
    let (rows, intent, reduction) = nested_sum_input(target).ok_or(
        PerComputationCandidateError::Unsupported("nested-sum merge input"),
    )?;
    let state = exact_accumulator(
        ExactKind::Sum,
        &intent,
        &cache[&Rc::as_ptr(&rows)],
        reduction,
    )?;
    Ok(OperatorNode::new_shared(Operator::ASAP(
        ASAPOp::FinalizeExactAccumulator { child: state },
    ))?)
}

/// An unfiltered exact accumulator of `kind` for `intent` over `input`,
/// grouped by `reduction`.
pub(crate) fn exact_accumulator(
    kind: ExactKind,
    intent: &AggIntent,
    input: &Rc<OperatorNode>,
    reduction: Reduction,
) -> Result<Rc<OperatorNode>, PerComputationCandidateError> {
    let family = exact_family(kind);
    let read = measure_input(intent, input, None)?;
    Ok(OperatorNode::new_shared(Operator::ASAP(
        ASAPOp::SummaryAgg {
            input: summary_update(intent, &family, &reduction, &input.schema, read)?,
            child: Rc::clone(input),
            family,
            reduction,
            grouping: GroupingStrategy::default(),
            filter: None,
        },
    ))?)
}

/// One measure of `target` over `input`: its group keys, then its value.
fn substitute_measure(
    target: &OperatorNode,
    intent: &AggIntent,
    candidate: &HowToComputeAggregateMeasure,
    filter: Option<Predicate>,
    input: &Rc<OperatorNode>,
    raw_data_samples_of: Option<&str>,
    (sizing, key): (&Sizing, MeasureKey),
) -> Result<Rc<OperatorNode>, PerComputationCandidateError> {
    let Some(NonASAPOp::Aggregate {
        reduction,
        output_names,
        ..
    }) = target.non_asap()
    else {
        unreachable!("substitute checked the aggregate")
    };
    let state = |family: FieldDataType, intent: &AggIntent, read: SummaryInputExpr| {
        Ok::<_, PerComputationCandidateError>(OperatorNode::new_shared(Operator::ASAP(
            ASAPOp::SummaryAgg {
                child: input.clone(),
                input: with_raw_data_samples_requirement(
                    summary_update(intent, &family, reduction, &input.schema, read)?,
                    raw_data_samples_of,
                ),
                family,
                reduction: reduction.clone(),
                grouping: GroupingStrategy::default(),
                filter: filter.clone(),
            },
        ))?)
    };
    let finalized = |kind: &ExactKind, intent: &AggIntent, read| {
        let state = state(exact_family(*kind), intent, read)?;
        Ok::<_, PerComputationCandidateError>(OperatorNode::new_shared(Operator::ASAP(
            ASAPOp::FinalizeExactAccumulator { child: state },
        ))?)
    };
    Ok(match candidate {
        HowToComputeAggregateMeasure::Exact => {
            OperatorNode::new_shared(Operator::NonASAP(NonASAPOp::Aggregate {
                child: input.clone(),
                reduction: reduction.clone(),
                measures: vec![intent.clone()],
                // One name per measure, as the target names this one (an
                // empty name is the intent's synthetic one).
                output_names: vec![output_names.get(key.1).cloned().unwrap_or_default()],
                filters: vec![filter.clone()],
                having: None,
            }))?
        }
        HowToComputeAggregateMeasure::ExactAccumulator(kinds) if kinds.len() == 1 => {
            let read = measure_input(intent, input, filter.as_ref())?;
            finalized(&kinds[0], intent, read)?
        }
        HowToComputeAggregateMeasure::Summary(algorithm) => {
            let family = FieldDataType::Sketch(
                sized(algorithm, intent, sizing.share(key)),
                GroupingStrategy::default(),
            );
            let summary_input = state(
                family,
                intent,
                measure_input(intent, input, filter.as_ref())?,
            )?;
            sizing.built.borrow_mut().push((summary_input.clone(), key));
            let estimate = OperatorNode::new_shared(Operator::ASAP(ASAPOp::SummaryEstimate {
                summary_input,
                query: sketch_statistic(intent).ok_or(
                    PerComputationCandidateError::Unsupported("sketch evaluation"),
                )?,
            }))?;
            match intent {
                AggIntent::TopK { .. } => ranked_as_topk(target, estimate)?,
                _ => estimate,
            }
        }
        HowToComputeAggregateMeasure::ExactAccumulator(kinds) => {
            let (AggIntent::Avg { col }, [ExactKind::Sum, ExactKind::Count]) =
                (intent, kinds.as_slice())
            else {
                return Err(PerComputationCandidateError::Unsupported(
                    "several exact states other than avg's sum then count",
                ));
            };
            let sum_intent = AggIntent::Sum { col: *col };
            // The count counts the averaged column, as `avg` does: it is
            // non-null (a nullable input prunes this candidate), so it counts
            // the rows the sum reads.
            let read = measure_input(&sum_intent, input, filter.as_ref())?;
            let sum = finalized(&ExactKind::Sum, &sum_intent, read.clone())?;
            let count = finalized(
                &ExactKind::Count,
                &AggIntent::Count {
                    accuracy: AccuracyTarget::exact(),
                },
                read,
            )?;
            if input.result_kind == OperatorResultKind::Relation {
                return sql_avg(reduction, filter.is_some(), sum, count);
            }
            // `avg` is Float64 whatever the summed type, as `Avg`'s output is,
            // and a PromQL binary operator reads Float64 sample values. A
            // grouped sum or count ends with its value, cast when not Float64
            // (a count is Int64). A per-series sum keeps the input's labels,
            // so its value is the sample-value column; a per-series count is
            // Float64.
            let (sum, count) = match reduction {
                Reduction::Reduce(by) => (
                    last_as_float64(by.len(), sum)?,
                    last_as_float64(by.len(), count)?,
                ),
                Reduction::PerEntity => {
                    let value = resolve_column_ref(&ColumnRef::SampleValue, &sum.schema)
                        .ok()
                        .and_then(|column| sum.schema.fields.get(column));
                    if value.map(|field| &field.dtype)
                        != Some(&FieldDataType::Plain(DataType::Float64))
                    {
                        return Err(PerComputationCandidateError::Unsupported(
                            "non-Float64 per-series avg",
                        ));
                    }
                    (sum, count)
                }
            };
            OperatorNode::new_shared(Operator::NonASAP(NonASAPOp::BinaryOp {
                operator: BinaryOperator {
                    checked_relative_division: false,
                    // A per-series window may hold no finite sum to divide.
                    checked_finite_division: matches!(reduction, Reduction::PerEntity),
                    kind: BinaryOpKind::Arithmetic(ArithmeticOpKind::Div),
                    vector_match: None,
                },
                return_bool: false,
                lhs: sum,
                rhs: count,
            }))?
        }
    })
}

/// `node`, a grouped aggregate's `keys` group keys and then its value, with
/// the value cast to Float64 unless it already is one.
fn last_as_float64(
    keys: usize,
    node: Rc<OperatorNode>,
) -> Result<Rc<OperatorNode>, PerComputationCandidateError> {
    if node.schema.fields.last().map(|field| &field.dtype)
        == Some(&FieldDataType::Plain(DataType::Float64))
    {
        return Ok(node);
    }
    let mut cols = key_items(keys);
    cols.push(ProjectItem {
        alias: None,
        expr: ScalarExpr::Cast {
            expr: Box::new(ScalarExpr::Column(keys)),
            to: DataType::Float64,
            try_cast: false,
        },
    });
    Ok(OperatorNode::new_shared(Operator::NonASAP(
        NonASAPOp::Project {
            cols,
            qualifier: None,
            child: node,
        },
    ))?)
}

/// SQL `avg` from its finalized `sum` and `count`: joined on the group keys
/// (NULL keys match), then `sum / count` as Float64. A group with no row
/// passing the measure filter, or an empty global input, counts 0 and its
/// `avg` is NULL.
fn sql_avg(
    reduction: &Reduction,
    filtered: bool,
    sum: Rc<OperatorNode>,
    count: Rc<OperatorNode>,
) -> Result<Rc<OperatorNode>, PerComputationCandidateError> {
    let Reduction::Reduce(by) = reduction else {
        return Err(PerComputationCandidateError::Unsupported(
            "SQL avg without `by` keys",
        ));
    };
    if by.is_without() {
        return Err(PerComputationCandidateError::Unsupported(
            "SQL avg without `by` keys",
        ));
    }
    let keys = by.len();
    let float = |column| {
        Box::new(ScalarExpr::Cast {
            expr: Box::new(ScalarExpr::Column(column)),
            to: DataType::Float64,
            try_cast: false,
        })
    };
    let count_column = keys + 1 + keys;
    let mut avg = ScalarExpr::Arithmetic {
        op: ArithmeticOpKind::Div,
        left: float(keys),
        right: float(count_column),
        semantics: ExprSemantics::Sql,
    };
    if filtered || keys == 0 {
        avg = ScalarExpr::Case {
            operand: None,
            branches: vec![(
                ScalarExpr::Compare {
                    left: Box::new(ScalarExpr::Column(count_column)),
                    op: CompareOpKind::Eq,
                    right: Box::new(ScalarExpr::Literal(ScalarValue::Int64(0))),
                    semantics: ExprSemantics::Sql,
                },
                ScalarExpr::Literal(ScalarValue::Null),
            )],
            else_expr: Some(Box::new(avg)),
        };
    }
    let mut cols = key_items(keys);
    cols.push(ProjectItem {
        alias: None,
        expr: avg,
    });
    Ok(OperatorNode::new_shared(Operator::NonASAP(
        NonASAPOp::Project {
            cols,
            qualifier: None,
            child: join_on_keys(keys, sum, count)?,
        },
    ))?)
}

/// `left` joined with `right` on their leading `keys` columns; NULL keys
/// match each other, as grouping treats them.
fn join_on_keys(
    keys: usize,
    left: Rc<OperatorNode>,
    right: Rc<OperatorNode>,
) -> Result<Rc<OperatorNode>, PerComputationCandidateError> {
    let width = left.schema.fields.len();
    let column = |id| Box::new(ScalarExpr::Column(id));
    let matches = (0..keys)
        .map(|key| {
            let equal = ScalarExpr::Compare {
                left: column(key),
                op: CompareOpKind::Eq,
                right: column(width + key),
                semantics: ExprSemantics::Sql,
            };
            if right.schema.fields[key].nullable {
                ScalarExpr::BoolOr(vec![
                    equal,
                    ScalarExpr::BoolAnd(vec![
                        ScalarExpr::IsNull(column(key)),
                        ScalarExpr::IsNull(column(width + key)),
                    ]),
                ])
            } else {
                equal
            }
        })
        .collect::<Vec<_>>();
    let (kind, pred) = match matches.len() {
        0 => (
            JoinKind::Cross,
            ScalarExpr::Literal(ScalarValue::Boolean(true)),
        ),
        _ => (JoinKind::Inner, ScalarExpr::BoolAnd(matches)),
    };
    Ok(OperatorNode::new_shared(Operator::NonASAP(
        NonASAPOp::Join {
            kind,
            pred: Predicate(pred),
            left,
            right,
        },
    ))?)
}

/// Join single-measure `parts` (each: group keys, then one value) on their
/// group keys and project `target`'s output columns. Every part has the same
/// groups, so an inner join keeps them all; NULL keys match each other, as
/// grouping treats them.
fn join_measures(
    target: &OperatorNode,
    parts: Vec<Rc<OperatorNode>>,
) -> Result<Rc<OperatorNode>, PerComputationCandidateError> {
    let Some(NonASAPOp::Aggregate {
        reduction: Reduction::Reduce(by),
        ..
    }) = target.non_asap()
    else {
        return Err(PerComputationCandidateError::Unsupported(
            "multi-measure aggregate without `by` keys",
        ));
    };
    if by.is_without() {
        return Err(PerComputationCandidateError::Unsupported(
            "multi-measure aggregate without `by` keys",
        ));
    }
    let keys = by.len();
    let width = keys + 1;
    let mut parts = parts.into_iter();
    let mut joined = parts.next().expect("an aggregate has measures");
    for part in parts {
        joined = join_on_keys(keys, joined, part)?;
    }
    let mut cols = key_items(keys);
    cols.extend(
        (0..target.schema.fields.len() - keys).map(|measure| ProjectItem {
            alias: None,
            expr: ScalarExpr::Column(measure * width + keys),
        }),
    );
    for (item, field) in cols.iter_mut().zip(&target.schema.fields) {
        item.alias = Some(field.name.clone());
    }
    Ok(OperatorNode::new_shared(Operator::NonASAP(
        NonASAPOp::Project {
            cols,
            qualifier: None,
            child: joined,
        },
    ))?)
}

fn key_items(keys: usize) -> Vec<ProjectItem> {
    (0..keys)
        .map(|key| ProjectItem {
            alias: None,
            expr: ScalarExpr::Column(key),
        })
        .collect()
}

impl Sizing {
    /// How many approximate inputs `key`'s summary shares its query's
    /// budget with.
    fn share(&self, key: MeasureKey) -> usize {
        self.shares.get(&key).copied().unwrap_or(1)
    }
}

/// What a measure's state reads from each row of `input`, resolvable there.
/// A SQL `count` reads [`counted_column`]; any other measure, a PromQL count
/// included, reads its input column (the sample value when it names none).
fn measure_input(
    intent: &AggIntent,
    input: &OperatorNode,
    filter: Option<&Predicate>,
) -> Result<SummaryInputExpr, PerComputationCandidateError> {
    let schema = &input.schema;
    let read = match intent {
        AggIntent::Count { .. } if input.result_kind == OperatorResultKind::Relation => {
            SummaryInputExpr::Column(counted_column(schema, filter)?)
        }
        // A top-k ranks its child's aggregate output, not a column of its own.
        AggIntent::TopK { .. } => SummaryInputExpr::Column(
            crate::pass1::realization::ranked_value(input)
                .map_err(|_| {
                    PerComputationCandidateError::Unsupported("Top-K has no ranked column")
                })?
                .1,
        ),
        _ => crate::pass1::realization::summarised_input(intent, schema).ok_or(
            PerComputationCandidateError::Unsupported("input column outside child schema"),
        )?,
    };
    if let SummaryInputExpr::Column(column) = &read {
        resolve_column_ref(column, schema).map_err(|_| {
            PerComputationCandidateError::Unsupported("summary input column does not resolve")
        })?;
    }
    Ok(read)
}

/// The column a SQL `count` state updates on. The runtime skips NULL updates,
/// so it is a column non-null on every counted row: one the measure filter
/// proves non-null (`count(col)` lowers to the filter `col IS NOT NULL`), else
/// a non-nullable one, Float64 first (the runtime's numeric update).
fn counted_column(
    schema: &Schema,
    filter: Option<&Predicate>,
) -> Result<ColumnRef, PerComputationCandidateError> {
    fn not_null(expr: &ScalarExpr) -> Vec<usize> {
        match expr {
            ScalarExpr::IsNotNull(inner) => match **inner {
                ScalarExpr::Column(column) => vec![column],
                _ => vec![],
            },
            ScalarExpr::BoolAnd(conjuncts) => conjuncts.iter().flat_map(not_null).collect(),
            _ => vec![],
        }
    }
    let non_nullable = |float: bool| {
        schema
            .fields
            .iter()
            .enumerate()
            .filter_map(move |(id, field)| {
                (!field.nullable
                    && (!float || field.dtype == FieldDataType::Plain(DataType::Float64)))
                .then_some(id)
            })
    };
    let reference = |id: usize| {
        schema
            .fields
            .get(id)
            .map(crate::pass1::realization::column_ref)
    };
    filter
        .map_or_else(Vec::new, |filter| not_null(&filter.0))
        .into_iter()
        .chain(non_nullable(true))
        .chain(non_nullable(false))
        .find_map(|id| {
            reference(id).filter(|column| resolve_column_ref(column, schema).ok() == Some(id))
        })
        .ok_or(PerComputationCandidateError::Unsupported(
            "SQL count over rows with no column known to be non-null",
        ))
}

/// What each input row contributes, given what the measure reads
/// ([`measure_input`]), following the legacy realization rules: heap sketches
/// rank series identities, frequency sketches count values, and every other
/// family reads the measure's input.
fn summary_update(
    intent: &AggIntent,
    family: &FieldDataType,
    reduction: &Reduction,
    child: &Schema,
    weight: SummaryInputExpr,
) -> Result<SummaryUpdate, PerComputationCandidateError> {
    let algorithm = match family {
        FieldDataType::Sketch(kind, _) => Some(kind.algorithm()),
        _ => None,
    };
    Ok(match (intent, algorithm) {
        (AggIntent::TopK { .. }, Some(_)) => {
            if reduction.group_keys().is_some_and(|keys| keys.is_without()) {
                return Err(PerComputationCandidateError::Unsupported(
                    "Top-K partitions given by `without`",
                ));
            }
            let groups: Vec<usize> = reduction
                .group_keys()
                .into_iter()
                .flat_map(|keys| keys.iter().copied())
                .collect();
            let item = if child.has_promql_series_identity() {
                asap_types::ir::schema::topk::series_identity_item(child, &groups)
            } else if child.closed {
                // A closed child (an aggregate's output, say) lists every
                // column: the item is its columns outside the partition and
                // the ranked value, as the legacy current-series and counter
                // rules rank them.
                let SummaryInputExpr::Column(ranked) = &weight else {
                    return Err(PerComputationCandidateError::Unsupported(
                        "Top-K ranked value is not a column",
                    ));
                };
                let value = resolve_column_ref(ranked, child).map_err(|_| {
                    PerComputationCandidateError::Unsupported("Top-K ranked value does not resolve")
                })?;
                let items: Vec<_> = child
                    .fields
                    .iter()
                    .enumerate()
                    .filter(|(index, _)| *index != value && !groups.contains(index))
                    .map(|(_, field)| {
                        SummaryInputExpr::Column(crate::pass1::realization::column_ref(field))
                    })
                    .collect();
                if items.is_empty() {
                    return Err(PerComputationCandidateError::Unsupported(
                        "Top-K item has no label outside its partition",
                    ));
                }
                SummaryInputExpr::Tuple(items)
            } else {
                let excluding = groups
                    .iter()
                    .filter_map(|&index| child.fields.get(index))
                    .map(crate::pass1::realization::column_ref)
                    .collect();
                SummaryInputExpr::EntityIdentity(EntityIdentity::PromqlLabelSet { excluding })
            };
            SummaryUpdate {
                item: Some(item),
                // The ranked value ([`measure_input`]).
                weight,
                // Not proven non-negative; selection decides whether CMS is admissible.
                weight_domain: WeightDomain::UnknownOrSigned,
            }
        }
        (_, Some(SketchAlgorithm::UnivMon))
        | (AggIntent::Count { .. }, Some(SketchAlgorithm::Cms)) => SummaryUpdate {
            item: Some(weight),
            weight: SummaryInputExpr::Constant(1.0),
            weight_domain: WeightDomain::NonNegative {
                proof: NonNegativeWeightProof::UnitCount,
            },
        },
        _ => SummaryUpdate {
            item: None,
            weight,
            weight_domain: WeightDomain::UnknownOrSigned,
        },
    })
}

#[cfg(test)]
mod tests {
    /// Aggregate replacement qualifiers survive both renaming and later input rebinding.
    #[test]
    fn aggregate_replacement_preserves_sql_field_identity() {
        let original =
            crate::test_support::lower_promql("sum by (job) (m)", AccuracyTarget::exact());
        let mut target = original.as_ref().clone();
        target.schema.fields[0].table = Some("metrics".into());
        let projected = OperatorNode::new_shared(Operator::NonASAP(NonASAPOp::Project {
            cols: target
                .schema
                .fields
                .iter()
                .enumerate()
                .map(|(column, field)| ProjectItem {
                    alias: Some(field.name.clone()),
                    expr: ScalarExpr::Column(column),
                })
                .collect(),
            qualifier: None,
            child: original,
        }))
        .unwrap();
        assert_eq!(projected.schema.fields[0].table, None);
        let replaced = with_names_of(&target, projected).unwrap();
        assert_eq!(replaced.schema, target.schema);
        let rebound = replaced.with_new_children(Rc::clone).unwrap();
        assert_eq!(rebound.schema, target.schema);
    }

    use super::*;
    use asap_accuracy::composition::{StreamNorm, PRODUCT_OPERAND_DOMAINS};
    use asap_accuracy::propagation::operand_guarantee;
    use asap_accuracy::test_support::{
        composed_guarantee, estimate_bound_over_raw_data_samples, estimate_has_bound,
        estimates_raw_data_samples_of, fold_stats, local_guarantee,
        raw_data_samples_sign_statistic, scalar_guarantee, sign_statistic,
        source_distinct_guarantee, stream_norm, QUOTIENT_OPERAND_DOMAINS,
    };
    use asap_types::ir::properties::guarantee::{
        AccuracyGuarantee, AccuracyMetric, BoundExpr, ProbabilityExpr,
    };
    use asap_types::ir::properties::AccuracyTarget;
    use asap_types::ir::schema::{SketchParams, SketchStatistic};
    /// Approximate count keeps one exact option, its accumulator, beside the
    /// universal summary; plain exact execution is not offered.
    #[test]
    fn approximate_count_keeps_one_exact_accumulator_and_universal_choices() {
        let choices = computation_candidates_for_intent(&AggIntent::Count {
            accuracy: AccuracyTarget::max_error(0.05).failure_probability(0.01),
        })
        .unwrap()
        .candidates;
        assert!(!choices.contains(&HowToComputeAggregateMeasure::Exact));
        assert_eq!(
            choices[0],
            HowToComputeAggregateMeasure::ExactAccumulator(vec![ExactKind::Count])
        );
        assert!(choices.contains(&HowToComputeAggregateMeasure::Summary(
            SketchAlgorithm::UnivMon
        )));
    }

    /// Count Sketch's signed counters give no total, so Pass 1 offers no
    /// Count Sketch for a count, and every count candidate it offers
    /// answers the total.
    #[test]
    fn a_count_offers_no_count_sketch() {
        let choices = computation_candidates_for_intent(&AggIntent::Count {
            accuracy: AccuracyTarget::max_error(0.05).failure_probability(0.01),
        })
        .unwrap()
        .candidates;
        assert!(!choices.contains(&HowToComputeAggregateMeasure::Summary(
            SketchAlgorithm::CountSketch
        )));
        let total = sketch_statistic(&AggIntent::Count {
            accuracy: AccuracyTarget::exact(),
        })
        .unwrap();
        for choice in &choices {
            if let HowToComputeAggregateMeasure::Summary(algorithm) = choice {
                assert!(
                    asap_types::ir::schema::FamilyCapability::of_per_instance_sketch(algorithm)
                        .answers(&total),
                    "{algorithm:?}"
                );
            }
        }
    }

    /// Guarantee composition through scalar and vector arithmetic (#669).
    mod guarantee_composition {
        use super::*;
        use crate::test_support::lower_promql;
        use asap_types::ir::properties::AccuracyTarget;

        fn with_bound(metric: AccuracyMetric, bound: f64, delta: f64) -> AccuracyGuarantee {
            AccuracyGuarantee::new(
                metric,
                BoundExpr::Constant { value: bound },
                ProbabilityExpr::Constant { value: delta },
                vec![],
            )
        }
        fn abs(bound: f64, delta: f64) -> AccuracyGuarantee {
            with_bound(AccuracyMetric::ABSOLUTE, bound, delta)
        }
        fn rel(bound: f64) -> AccuracyGuarantee {
            with_bound(AccuracyMetric::Relative, bound, 0.0)
        }
        /// `sum(m)` carrying `guarantee`.
        fn node(guarantee: AccuracyGuarantee) -> Rc<OperatorNode> {
            node_over("sum(m)", guarantee)
        }
        /// `query` carrying `guarantee`.
        fn node_over(query: &str, guarantee: AccuracyGuarantee) -> Rc<OperatorNode> {
            let node = lower_promql(query, AccuracyTarget::exact());
            Rc::new((*node).clone().with_guarantee(Some(guarantee)))
        }
        fn scalar(guarantee: AccuracyGuarantee) -> ScalarExpr {
            ScalarExpr::PromqlScalarFromVector(node(guarantee))
        }
        fn lit(value: f64) -> ScalarExpr {
            ScalarExpr::literal_f64(value)
        }
        fn arith(op: ArithmeticOpKind, left: ScalarExpr, right: ScalarExpr) -> ScalarExpr {
            ScalarExpr::Arithmetic {
                op,
                left: Box::new(left),
                right: Box::new(right),
                semantics: ExprSemantics::Promql,
            }
        }
        fn call(name: &str, args: Vec<ScalarExpr>) -> ScalarExpr {
            ScalarExpr::FunctionCall {
                name: name.into(),
                args,
            }
        }
        fn of(expr: &ScalarExpr) -> Option<AccuracyGuarantee> {
            scalar_guarantee(expr, &|_| None)
        }
        fn bound(g: &AccuracyGuarantee) -> f64 {
            g.spec.error.evaluate().expect("a resolved bound")
        }
        fn delta(g: &AccuracyGuarantee) -> f64 {
            g.spec
                .failure_probability
                .evaluate()
                .expect("a resolved probability")
        }
        fn close(a: f64, b: f64) -> bool {
            (a - b).abs() < 1e-12
        }

        /// Absolute errors add through `x ± y`, with the union bound on the
        /// failure probability; `−x` and `x ± c` keep the error, `c · x` and
        /// `x / c` scale it by `|c|` and `1/|c|`, division by a literal zero
        /// has none.
        #[test]
        fn absolute_errors_compose_through_arithmetic() {
            use ArithmeticOpKind::*;
            let (x, y) = (scalar(abs(0.01, 0.001)), scalar(abs(0.02, 0.002)));
            for op in [Add, Sub] {
                let g = of(&arith(op.clone(), x.clone(), y.clone())).unwrap();
                assert_eq!(g.spec.accuracy_metric, AccuracyMetric::ABSOLUTE, "{op:?}");
                assert!(close(bound(&g), 0.03) && close(delta(&g), 0.003), "{op:?}");
                for shifted in [
                    arith(op.clone(), x.clone(), lit(5.0)),
                    arith(op.clone(), lit(5.0), x.clone()),
                    arith(op.clone(), x.clone(), ScalarExpr::EvalTimestamp),
                ] {
                    let g = of(&shifted).unwrap();
                    assert!(close(bound(&g), 0.01) && close(delta(&g), 0.001), "{op:?}");
                }
            }
            let negated = ScalarExpr::Negative {
                expr: Box::new(x.clone()),
                semantics: ExprSemantics::Promql,
            };
            assert!(close(bound(&of(&negated).unwrap()), 0.01));
            for (expr, expected) in [
                (arith(Mul, lit(-3.0), x.clone()), 0.03),
                (arith(Mul, x.clone(), lit(3.0)), 0.03),
                (arith(Div, x.clone(), lit(-4.0)), 0.0025),
            ] {
                let g = of(&expr).unwrap();
                assert!(
                    close(bound(&g), expected) && close(delta(&g), 0.001),
                    "{expr:?}"
                );
            }
            for none in [
                arith(Div, x.clone(), lit(0.0)),
                arith(Mul, x.clone(), ScalarExpr::EvalTimestamp),
                arith(Pow, x.clone(), lit(2.0)),
            ] {
                assert_eq!(of(&none), None, "{none:?}");
            }
        }

        /// Count-Min and Count Sketch errors are additive: through
        /// arithmetic they compose as absolute errors, each normalized bound
        /// times the (symbolic) stream norm of its summary's source, here
        /// over each source's raw value column, for plan selection to bind.
        #[test]
        fn normalized_additive_errors_compose_as_absolute_errors() {
            let cms = with_bound(AccuracyMetric::ABSOLUTE_STREAM_L1, 0.01, 0.001);
            let count_sketch = with_bound(AccuracyMetric::ABSOLUTE_STREAM_L2, 0.02, 0.002);
            // Summaries of each source's raw value column.
            let raw = |query: &str, guarantee: AccuracyGuarantee| {
                ScalarExpr::PromqlScalarFromVector(crate::test_support::estimate_over(
                    crate::test_support::weighted(
                        crate::test_support::cms_summary(query),
                        SummaryInputExpr::Column(ColumnRef::SampleValue),
                    ),
                    guarantee,
                ))
            };
            let g = of(&arith(
                ArithmeticOpKind::Sub,
                raw("count by (job) (m)", cms.clone()),
                raw("count by (job) (n)", count_sketch),
            ))
            .unwrap();
            assert_eq!(g.spec.accuracy_metric, AccuracyMetric::ABSOLUTE);
            assert!(close(delta(&g), 0.003));
            let norms = |statistic: &str| match statistic {
                "stream_l1_norm[m]" => Some(100.0),
                "stream_l2_norm[n]" => Some(10.0),
                _ => None,
            };
            assert!(g.spec.error.has_unknown());
            assert!(close(g.bind(&norms).spec.error.evaluate().unwrap(), 1.2));
            let doubled = of(&arith(
                ArithmeticOpKind::Mul,
                lit(2.0),
                raw("count by (job) (m)", cms),
            ))
            .unwrap();
            assert!(close(
                doubled.bind(&norms).spec.error.evaluate().unwrap(),
                2.0
            ));
        }

        /// A product of two estimates has an absolute bound
        /// `|x|·e_y + |y|·e_x + e_x·e_y`, and a quotient with an approximate
        /// denominator one that needs the denominator bounded away from
        /// zero: both need operand domains, so Stage 1 leaves them symbolic
        /// for plan selection to bind. Relative errors need no domain: a
        /// product has the closed form `(1 + a)(1 + b) − 1`, a quotient
        /// `(a + b) / (1 − b)` (`a = 0` for an exact numerator). The failure
        /// probability is the union bound. A rank error composes to none.
        #[test]
        fn products_and_quotients_of_estimates_have_closed_or_symbolic_bounds() {
            use ArithmeticOpKind::*;
            let unknown = |statistic: &str| BoundExpr::Unknown {
                statistic: statistic.into(),
            };
            let (x, y) = (scalar(abs(0.01, 0.001)), scalar(abs(0.02, 0.002)));
            let product = of(&arith(Mul, x.clone(), y.clone())).unwrap();
            assert_eq!(product.spec.accuracy_metric, AccuracyMetric::ABSOLUTE);
            assert_eq!(product.spec.error, unknown(PRODUCT_OPERAND_DOMAINS));
            assert!(close(delta(&product), 0.003));
            for quotient in [
                arith(Div, x.clone(), y.clone()),
                arith(Div, lit(1.0), x.clone()),
                arith(Div, x.clone(), scalar(rel(0.01))),
            ] {
                let g = of(&quotient).unwrap();
                assert_eq!(
                    g.spec.accuracy_metric,
                    AccuracyMetric::ABSOLUTE,
                    "{quotient:?}"
                );
                assert_eq!(
                    g.spec.error,
                    unknown(QUOTIENT_OPERAND_DOMAINS),
                    "{quotient:?}"
                );
            }
            let ratio = of(&arith(
                Div,
                scalar(with_bound(AccuracyMetric::Relative, 0.01, 0.001)),
                scalar(with_bound(AccuracyMetric::Relative, 0.02, 0.002)),
            ))
            .unwrap();
            assert_eq!(ratio.spec.accuracy_metric, AccuracyMetric::Relative);
            assert!(close(bound(&ratio), (0.01 + 0.02) / (1.0 - 0.02)));
            assert!(close(delta(&ratio), 0.003));
            let reciprocal = of(&arith(Div, lit(2.0), scalar(rel(0.02)))).unwrap();
            assert_eq!(reciprocal.spec.accuracy_metric, AccuracyMetric::Relative);
            assert!(close(bound(&reciprocal), 0.02 / (1.0 - 0.02)));
            // A denominator bound of 1 or more leaves the ratio unbounded.
            assert_eq!(of(&arith(Div, scalar(rel(0.01)), scalar(rel(1.0)))), None);
            // A relative bound still symbolic in Stage 1 has no ratio rule.
            let symbolic = AccuracyGuarantee::new(
                AccuracyMetric::Relative,
                unknown("values_sign"),
                ProbabilityExpr::Zero,
                vec![],
            );
            assert_eq!(of(&arith(Div, scalar(symbolic), scalar(rel(0.01)))), None);
            let relative = of(&arith(Mul, scalar(rel(0.01)), scalar(rel(0.02)))).unwrap();
            assert_eq!(relative.spec.accuracy_metric, AccuracyMetric::Relative);
            assert!(close(bound(&relative), 0.01 + 0.02 + 0.0002));
            let rank = scalar(with_bound(AccuracyMetric::Rank, 0.01, 0.001));
            assert_eq!(of(&arith(Mul, rank.clone(), x.clone())), None);
            assert_eq!(of(&arith(Div, x, rank)), None);
        }

        /// A relative error survives negation and scaling by an exact value,
        /// but not a shift (`x + c`) or a sum or difference of estimates
        /// (cancellation); a rank error composes to no guarantee (a rank
        /// interval counts as none in the MVP).
        #[test]
        fn relative_and_rank_errors_compose_only_where_sound() {
            use ArithmeticOpKind::*;
            let x = scalar(rel(0.01));
            for kept in [
                ScalarExpr::Negative {
                    expr: Box::new(x.clone()),
                    semantics: ExprSemantics::Promql,
                },
                arith(Mul, x.clone(), lit(-2.0)),
                arith(Div, x.clone(), ScalarExpr::EvalTimestamp),
            ] {
                let g = of(&kept).unwrap();
                assert_eq!(g.spec.accuracy_metric, AccuracyMetric::Relative, "{kept:?}");
                assert!(close(bound(&g), 0.01), "{kept:?}");
            }
            for none in [
                arith(Add, x.clone(), lit(1.0)),
                arith(Sub, x.clone(), scalar(rel(0.01))),
            ] {
                assert_eq!(of(&none), None, "{none:?}");
            }
            let rank = scalar(with_bound(AccuracyMetric::Rank, 0.01, 0.001));
            for none in [
                arith(Sub, rank.clone(), rank.clone()),
                arith(Mul, rank.clone(), lit(2.0)),
                call("promql_clamp_max", vec![rank, lit(1.0)]),
            ] {
                assert_eq!(of(&none), None, "{none:?}");
            }
        }

        /// `clamp`, `clamp_min`, `clamp_max` and `abs` are 1-Lipschitz and
        /// monotone in each argument: the result keeps the largest absolute
        /// (or relative) error of its arguments, with the union bound on the
        /// failure probability.
        #[test]
        fn clamp_and_abs_keep_the_largest_argument_error() {
            let x = scalar(abs(0.01, 0.001));
            for expr in [
                call("promql_clamp", vec![x.clone(), lit(0.0), lit(10.0)]),
                call("promql_clamp_min", vec![x.clone(), lit(0.0)]),
                call("promql_clamp_max", vec![x.clone(), lit(10.0)]),
                call("promql_abs", vec![x.clone()]),
                call("abs", vec![x.clone()]),
            ] {
                let g = of(&expr).unwrap();
                assert_eq!(g.spec.accuracy_metric, AccuracyMetric::ABSOLUTE, "{expr:?}");
                assert!(
                    close(bound(&g), 0.01) && close(delta(&g), 0.001),
                    "{expr:?}"
                );
            }
            let bounded = of(&call(
                "promql_clamp",
                vec![x.clone(), scalar(abs(0.03, 0.002)), lit(10.0)],
            ))
            .unwrap();
            assert!(close(bound(&bounded), 0.03) && close(delta(&bounded), 0.003));
            let relative = of(&call(
                "promql_clamp",
                vec![scalar(rel(0.01)), scalar(rel(0.02)), lit(10.0)],
            ))
            .unwrap();
            assert_eq!(relative.spec.accuracy_metric, AccuracyMetric::Relative);
            assert!(close(bound(&relative), 0.02));
            let mixed = call("promql_clamp_min", vec![x, scalar(rel(0.02))]);
            assert_eq!(of(&mixed), None);
            assert_eq!(of(&call("promql_floor", vec![scalar(rel(0.01))])), None);
        }

        /// A comparison over an estimate has no guarantee on its output (a
        /// band guarantee is later work); over exact operands it is exact.
        #[test]
        fn comparisons_over_estimates_have_no_guarantee() {
            let compare = |left: ScalarExpr| ScalarExpr::Compare {
                left: Box::new(left),
                op: CompareOpKind::Gt,
                right: Box::new(lit(5.0)),
                semantics: ExprSemantics::Promql,
            };
            assert_eq!(of(&compare(scalar(abs(0.01, 0.001)))), None);
            assert!(of(&compare(scalar(AccuracyGuarantee::exact("exact"))))
                .is_some_and(|g| g.is_exact()));
        }

        /// A SQL scalar subquery inherits its relation's guarantee, as
        /// PromQL `scalar(v)` does.
        #[test]
        fn a_scalar_subquery_inherits_its_guarantee() {
            let g = abs(0.01, 0.001);
            let expr = arith(
                ArithmeticOpKind::Add,
                ScalarExpr::ScalarSubquery(node(g.clone())),
                lit(1.0),
            );
            assert!(close(bound(&of(&expr).unwrap()), 0.01));
        }

        /// Two estimates count their failure probabilities once each (union
        /// bound), even when they read one shared summary: the built-in
        /// guarantees hold per estimate, not simultaneously. The same
        /// planned estimate read twice fails or holds as one event, so its
        /// failure probability counts once.
        #[test]
        fn failure_probability_counts_once_per_distinct_estimate() {
            let shared = node(abs(0.01, 0.001));
            // Equal to `shared`, but planned separately.
            let other = node(abs(0.01, 0.001));
            let same = ScalarExpr::PromqlScalarFromVector(shared.clone());
            let twice = of(&arith(ArithmeticOpKind::Add, same.clone(), same.clone())).unwrap();
            assert!(close(bound(&twice), 0.02) && close(delta(&twice), 0.001));
            let distinct = of(&arith(
                ArithmeticOpKind::Add,
                same,
                ScalarExpr::PromqlScalarFromVector(other.clone()),
            ))
            .unwrap();
            assert!(close(bound(&distinct), 0.02) && close(delta(&distinct), 0.002));
            let vector = |lhs: &Rc<OperatorNode>, rhs: &Rc<OperatorNode>| {
                let node = OperatorNode::new(Operator::NonASAP(NonASAPOp::BinaryOp {
                    operator: BinaryOperator {
                        checked_relative_division: false,
                        checked_finite_division: false,
                        kind: BinaryOpKind::Arithmetic(ArithmeticOpKind::Sub),
                        vector_match: None,
                    },
                    return_bool: false,
                    lhs: lhs.clone(),
                    rhs: rhs.clone(),
                }))
                .unwrap();
                composed_guarantee(&node).unwrap()
            };
            assert!(close(delta(&vector(&shared, &shared)), 0.001));
            assert!(close(delta(&vector(&shared, &other)), 0.002));
        }

        /// `left JOIN right ON pred`, composed.
        fn join(
            kind: JoinKind,
            left: &Rc<OperatorNode>,
            right: &Rc<OperatorNode>,
            pred: ScalarExpr,
        ) -> OperatorNode {
            let mut node = OperatorNode::new(Operator::NonASAP(NonASAPOp::Join {
                kind,
                pred: Predicate(pred),
                left: left.clone(),
                right: right.clone(),
            }))
            .unwrap();
            node.guarantee = composed_guarantee(&node);
            node
        }
        fn column(id: usize) -> ScalarExpr {
            ScalarExpr::Column(id)
        }
        fn equals(left: ScalarExpr, right: ScalarExpr) -> ScalarExpr {
            ScalarExpr::Compare {
                left: Box::new(left),
                op: CompareOpKind::Eq,
                right: Box::new(right),
                semantics: ExprSemantics::Sql,
            }
        }
        /// `CASE WHEN condition THEN NULL ELSE otherwise END` over `child`,
        /// composed.
        fn case_over(
            child: &OperatorNode,
            condition: ScalarExpr,
            otherwise: ScalarExpr,
        ) -> Option<AccuracyGuarantee> {
            let node = OperatorNode::new(Operator::NonASAP(NonASAPOp::Project {
                cols: vec![ProjectItem {
                    alias: Some("v".into()),
                    expr: ScalarExpr::Case {
                        operand: None,
                        branches: vec![(condition, ScalarExpr::Literal(ScalarValue::Null))],
                        else_expr: Some(Box::new(otherwise)),
                    },
                }],
                qualifier: None,
                child: Rc::new(child.clone()),
            }))
            .unwrap();
            composed_guarantee(&node)
        }
        /// An approximate node and an exact one, and the approximate one's
        /// column count (its columns come first in a join).
        fn approximate_and_exact() -> (Rc<OperatorNode>, Rc<OperatorNode>, usize) {
            let approximate = node(rel(0.01));
            let exact = node_over("count(m)", AccuracyGuarantee::exact("test"));
            let width = approximate.schema.fields.len();
            (approximate, exact, width)
        }

        /// A join whose other input is exact and whose predicate reads only
        /// exact columns keeps the approximate input's guarantee: a cross
        /// join, or a key equality on the exact input's columns.
        #[test]
        fn join_beside_an_exact_input_on_exact_columns_keeps_the_guarantee() {
            let (approximate, exact, width) = approximate_and_exact();
            for (kind, pred) in [
                (
                    JoinKind::Cross,
                    ScalarExpr::Literal(ScalarValue::Boolean(true)),
                ),
                (JoinKind::Inner, equals(column(width), lit(0.0))),
                (JoinKind::Left, equals(column(width), lit(0.0))),
            ] {
                assert_eq!(
                    join(kind.clone(), &approximate, &exact, pred).guarantee,
                    Some(rel(0.01)),
                    "{kind:?}"
                );
            }
            // Either side.
            assert_eq!(
                join(
                    JoinKind::Cross,
                    &exact,
                    &approximate,
                    ScalarExpr::Literal(ScalarValue::Boolean(true))
                )
                .guarantee,
                Some(rel(0.01))
            );
        }

        /// A join keyed on an approximate input's column, a semi-join, or
        /// two approximate inputs have no guarantee (column-level tracking
        /// is #689).
        #[test]
        fn join_on_an_approximate_key_has_no_guarantee() {
            let (approximate, exact, width) = approximate_and_exact();
            assert_eq!(
                join(
                    JoinKind::Inner,
                    &approximate,
                    &exact,
                    equals(column(0), column(width))
                )
                .guarantee,
                None
            );
            assert_eq!(
                join(
                    JoinKind::Semi,
                    &exact,
                    &approximate,
                    ScalarExpr::Literal(ScalarValue::Boolean(true))
                )
                .guarantee,
                None
            );
            assert_eq!(
                join(
                    JoinKind::Cross,
                    &approximate,
                    &approximate,
                    ScalarExpr::Literal(ScalarValue::Boolean(true))
                )
                .guarantee,
                None
            );
        }

        /// A `CASE` whose condition reads only exact columns keeps each
        /// branch's guarantee (a NULL branch is exact); one testing an
        /// approximate column has none.
        #[test]
        fn case_on_exact_columns_keeps_the_branch_guarantee() {
            let (approximate, exact, width) = approximate_and_exact();
            let joined = join(
                JoinKind::Cross,
                &approximate,
                &exact,
                ScalarExpr::Literal(ScalarValue::Boolean(true)),
            );
            let value = approximate.schema.fields.len() - 1;
            assert_eq!(
                case_over(&joined, equals(column(width), lit(0.0)), column(value)),
                Some(rel(0.01))
            );
            assert_eq!(
                case_over(&joined, equals(column(value), lit(0.0)), column(value)),
                None
            );
        }

        /// `node` projecting `cols` over `child`, composed.
        fn project(child: &OperatorNode, cols: Vec<ScalarExpr>) -> OperatorNode {
            let mut node = OperatorNode::new(Operator::NonASAP(NonASAPOp::Project {
                cols: cols
                    .into_iter()
                    .enumerate()
                    .map(|(i, expr)| ProjectItem {
                        alias: Some(format!("c{i}")),
                        expr,
                    })
                    .collect(),
                qualifier: None,
                child: Rc::new(child.clone()),
            }))
            .unwrap();
            node.guarantee = composed_guarantee(&node);
            node
        }

        /// Over a join, a projection reads each column's own guarantee,
        /// not the child's sample value (which may be an exact input's
        /// column): `sum(m) * 1000` beside the exact side's `value` has
        /// the scaled error 1000, not the approximate side's 1. A
        /// projection over that computed projection has no per-column
        /// guarantees, so it has none (#689).
        #[test]
        fn projection_over_a_join_reads_each_columns_guarantee() {
            let approximate = node(abs(1.0, 0.01));
            let exact = node_over("m", AccuracyGuarantee::exact("test"));
            let joined = join(
                JoinKind::Cross,
                &approximate,
                &exact,
                ScalarExpr::Literal(ScalarValue::Boolean(true)),
            );
            assert_eq!(joined.guarantee, Some(abs(1.0, 0.01)));
            let sum = approximate.schema.fields.len() - 1;
            let value = approximate.schema.fields.len() + exact.schema.fields.len() - 1;
            let scaled = project(
                &joined,
                vec![
                    arith(ArithmeticOpKind::Mul, column(sum), lit(1000.0)),
                    column(value),
                ],
            );
            assert_eq!(scaled.guarantee.as_ref().map(bound), Some(1000.0));
            let stacked = project(
                &scaled,
                vec![arith(ArithmeticOpKind::Add, column(1), lit(1.0)), column(0)],
            );
            assert_eq!(stacked.guarantee, None);
        }
    }

    /// Which stream norm bounds a Count-Min or Count Sketch estimate (#669).
    mod stream_norms {
        use super::*;
        use crate::test_support::{cms_summary, weighted};
        use asap_accuracy::composition::STREAM_L1_NORM;
        use asap_accuracy::test_support::{source_norm_statistic, STREAM_ROW_COUNT};

        /// A count (constant weight) is bounded by its weight times the
        /// source's row count; a sum of the source's raw value column by
        /// the source's norm; a sum of a computed value (`m * 1000`,
        /// `rate(…)`) by nothing that can be bound.
        #[test]
        fn a_norm_follows_the_update_weight() {
            let count = Rc::new(cms_summary("count by (job) (m)"));
            assert_eq!(
                stream_norm(&count),
                Some(StreamNorm::ConstantWeight {
                    weight: 1.0,
                    source: "m".into()
                })
            );
            let raw = weighted(
                cms_summary("count by (job) (m)"),
                SummaryInputExpr::Column(ColumnRef::SampleValue),
            );
            assert_eq!(stream_norm(&raw), Some(StreamNorm::Source("m".into())));
            for computed in ["count by (job) (m * 1000)", "count by (job) (rate(m[1m]))"] {
                let summary = weighted(
                    cms_summary(computed),
                    SummaryInputExpr::Column(ColumnRef::SampleValue),
                );
                assert_eq!(stream_norm(&summary), None, "{computed}");
            }
        }

        /// The absolute form of a Count-Min bound names the statistic its
        /// update weight allows: the source's norm for its raw value
        /// column, the weight times the source's row count for a count.
        #[test]
        fn the_absolute_bound_names_the_weighted_norm() {
            let frequency = AccuracyGuarantee::new(
                AccuracyMetric::ABSOLUTE_STREAM_L1,
                BoundExpr::Constant { value: 0.01 },
                ProbabilityExpr::Constant { value: 0.001 },
                vec![],
            );
            let estimate = |summary: Rc<OperatorNode>| {
                operand_guarantee(&crate::test_support::estimate_over(
                    summary,
                    frequency.clone(),
                ))
                .unwrap()
            };
            let raw = estimate(weighted(
                cms_summary("count by (job) (m)"),
                SummaryInputExpr::Column(ColumnRef::SampleValue),
            ));
            let l1 = source_norm_statistic(STREAM_L1_NORM, "m");
            assert_eq!(
                raw.bind(&|s| (s == l1).then_some(100.0))
                    .spec
                    .error
                    .evaluate(),
                Some(1.0)
            );
            let count = estimate(Rc::new(cms_summary("count by (job) (m)")));
            assert!(count
                .bind(&|s| (s == l1).then_some(100.0))
                .spec
                .error
                .has_unknown());
            let rows = source_norm_statistic(STREAM_ROW_COUNT, "m");
            assert_eq!(
                count
                    .bind(&|s| (s == rows).then_some(100.0))
                    .spec
                    .error
                    .evaluate(),
                Some(1.0)
            );
        }
    }

    fn inventory(query: &str) -> PerComputationCandidates {
        let root = crate::test_support::lower_promql(
            query,
            AccuracyTarget::max_error(0.01).failure_probability(0.001),
        );
        // Per-series state ranks each series' identity, which the rows carry
        // as a column; a query over aggregated rows is planned over labels.
        let root = asap_types::ir::schema::schema_support::with_promql_series_identity(&root)
            .unwrap_or(root);
        generate_per_computation_candidates(
            &crate::pass2::memo::DagOfLogicalMemoGroups::from_roots(&[(
                0,
                QueryRoot::Operator(root),
            )]),
        )
        .unwrap()
    }

    /// Every candidate of target `t`, then of its fused computation, composed
    /// with every other target at its first candidate, with the candidate.
    fn composed_with(
        inventory: &PerComputationCandidates,
        t: usize,
    ) -> Vec<(HowToComputeAggregateMeasure, Rc<OperatorNode>)> {
        (0..inventory.targets[t].choice_count())
            .map(|c| {
                let mut choice = vec![0; inventory.targets.len()];
                choice[t] = c;
                let composed = compose_logical_candidate(inventory, &choice).unwrap();
                let QueryRoot::Operator(root) = &composed[0] else {
                    panic!("operator root")
                };
                let (candidate, _) = inventory.targets[t].computation_for(c).unwrap();
                (candidate.clone(), root.clone())
            })
            .collect()
    }

    fn heap_states(root: &Rc<OperatorNode>) -> Vec<Rc<OperatorNode>> {
        OperatorNode::reachable(root)
            .into_iter()
            .filter(|node| {
                matches!(node.asap(), Some(ASAPOp::SummaryAgg {
                    family: FieldDataType::Sketch(kind, _), ..
                }) if kind.algorithm().capability().answers(StatisticKind::TopK))
            })
            .collect()
    }

    fn topk_target(inventory: &PerComputationCandidates) -> usize {
        inventory
            .targets
            .iter()
            .position(|t| {
                matches!(
                    inventory.aggregate(t).non_asap(),
                    Some(NonASAPOp::Aggregate { measures, .. })
                        if matches!(measures.as_slice(), [AggIntent::TopK { .. }])
                )
            })
            .expect("a top-k target")
    }

    /// Example 1's Q2: the top-k over a per-series `sum_over_time` keeps
    /// only its exact candidate and gets a fused whole-expression
    /// computation with a Count-Min and a CountSketch heap, which replace the
    /// heaps over the exact sum. The fused computation absorbs the inner sum: composed, no
    /// aggregate is computed and the heap reads the range selector's raw
    /// samples.
    #[test]
    fn whole_expression_topk_replaces_the_heap_over_the_exact_sum() {
        let inventory = inventory("topk by (job) (10, sum_over_time(m[1m]))");
        let topk = topk_target(&inventory);
        let target = &inventory.targets[topk];
        assert_eq!(
            target.ways_to_compute,
            [HowToComputeAggregateMeasure::Exact]
        );
        let fused = target
            .fused_computation
            .as_ref()
            .expect("a fused computation");
        assert_eq!(
            fused.ways_to_compute,
            [
                HowToComputeAggregateMeasure::Summary(SketchAlgorithm::CmsWithHeap),
                HowToComputeAggregateMeasure::Summary(SketchAlgorithm::CountSketchWithHeap),
            ]
        );
        let inner = fused.absorbed_targets[0];
        assert_eq!(inventory.targets[inner].ways_to_compute.len(), 1);
        assert_eq!(inventory.combinations(), 3);
        for (candidate, root) in composed_with(&inventory, topk) {
            if candidate == HowToComputeAggregateMeasure::Exact {
                continue;
            }
            assert!(
                !OperatorNode::reachable(&root).iter().any(|n| matches!(
                    n.non_asap(),
                    Some(NonASAPOp::Aggregate { .. })
                ) || matches!(
                    n.asap(),
                    Some(ASAPOp::FinalizeExactAccumulator { .. })
                )),
                "{candidate:?}: the inner sum is not computed"
            );
            let [state] = &heap_states(&root)[..] else {
                panic!("one heap")
            };
            let Some(ASAPOp::SummaryAgg { child, .. }) = state.asap() else {
                unreachable!()
            };
            assert!(matches!(
                child.non_asap(),
                Some(NonASAPOp::TimeRange { .. })
            ));
        }
    }

    /// The whole-expression heap reads the inner sum's input, so the outer
    /// top-k's partition keys (indices into the sum's output) are moved onto
    /// that input by column.
    #[test]
    fn whole_expression_topk_partitions_index_the_heap_input() {
        let root = crate::test_support::lower_promql(
            "topk by (job) (2, sum by (service, job) (rate(m[1m])))",
            AccuracyTarget::max_error(0.1),
        );
        let inventory = generate_per_computation_candidates(
            &crate::pass2::memo::DagOfLogicalMemoGroups::from_roots(&[(
                0,
                QueryRoot::Operator(root),
            )]),
        )
        .unwrap();
        let topk = topk_target(&inventory);
        let mut heaps = 0;
        for (candidate, root) in composed_with(&inventory, topk) {
            // The top-k's own candidate is exact; its summaries are the
            // fused computation's.
            if !matches!(candidate, HowToComputeAggregateMeasure::Summary(_)) {
                continue;
            }
            for state in heap_states(&root) {
                let Some(ASAPOp::SummaryAgg {
                    child, reduction, ..
                }) = state.asap()
                else {
                    unreachable!()
                };
                let names: Vec<_> = reduction
                    .group_keys()
                    .expect("partitioned by job")
                    .keys()
                    .iter()
                    .map(|&index| child.schema.fields[index].name.as_str())
                    .collect();
                assert_eq!(names, ["job"]);
                heaps += 1;
            }
        }
        assert_eq!(
            heaps, 2,
            "a whole-expression Count-Min and CountSketch heap"
        );
    }

    /// How many whole-workload choices enumeration keeps: every digit over
    /// its target's `choice_count`, less those [`absorbed_choice_varies`]
    /// skips.
    ///
    /// [`absorbed_choice_varies`]: PerComputationCandidates::absorbed_choice_varies
    fn enumerated_choices(inventory: &PerComputationCandidates) -> usize {
        let mut enumerated = 0;
        let mut choice = vec![0; inventory.targets.len()];
        loop {
            if !inventory.absorbed_choice_varies(&choice) {
                enumerated += 1;
            }
            let mut carry = true;
            for (digit, target) in choice.iter_mut().zip(&inventory.targets).rev() {
                *digit += 1;
                if *digit < target.choice_count() {
                    carry = false;
                    break;
                }
                *digit = 0;
            }
            if carry {
                return enumerated;
            }
        }
    }

    /// `topk(k, sum by (G) (sum_over_time(m[W])))` and its `count_over_time`
    /// form rank the groups `G` by the sum (count) of their raw data samples: the
    /// top-k keeps its exact candidate and gets the fused whole-expression
    /// Count-Min and CountSketch heaps, which absorb both the grouped sum and
    /// the per-series sum (count) beneath it. Composed, no aggregate is
    /// computed: the heap reads the range selector's raw data samples, keyed by
    /// `G` (less the top-k's own `by` labels, its partitions), weighted by
    /// the sample value (a sum, an open requirement on `m`) or by one (a
    /// count, proven non-negative).
    #[test]
    fn whole_expression_topk_over_a_grouped_sum_of_range_sums_reads_the_raw_data_samples() {
        let samples = WeightDomain::RawDataSamplesOfMetric { metric: "m".into() };
        let unit = WeightDomain::NonNegative {
            proof: NonNegativeWeightProof::UnitCount,
        };
        let value = SummaryInputExpr::Column(ColumnRef::SampleValue);
        let one = SummaryInputExpr::Constant(1.0);
        for (query, weight, domain, partitions) in [
            (
                "topk(3, sum by (region) (sum_over_time(m[1m])))",
                &value,
                &samples,
                &[][..],
            ),
            (
                "topk(3, sum by (region) (count_over_time(m[1m])))",
                &one,
                &unit,
                &[],
            ),
            (
                "topk by (job) (3, sum by (job, region) (sum_over_time(m[1m])))",
                &value,
                &samples,
                &["job"],
            ),
        ] {
            let inventory = inventory(query);
            let topk = topk_target(&inventory);
            let target = &inventory.targets[topk];
            assert_eq!(
                target.ways_to_compute,
                [HowToComputeAggregateMeasure::Exact],
                "{query}"
            );
            let fused = target
                .fused_computation
                .as_ref()
                .expect("a fused computation");
            assert_eq!(
                fused.ways_to_compute,
                [
                    HowToComputeAggregateMeasure::Summary(SketchAlgorithm::CmsWithHeap),
                    HowToComputeAggregateMeasure::Summary(SketchAlgorithm::CountSketchWithHeap),
                ],
                "{query}"
            );
            assert_eq!(
                fused.absorbed_targets.len(),
                2,
                "{query}: both aggregates beneath"
            );
            assert_eq!(
                inventory.combinations(),
                enumerated_choices(&inventory),
                "{query}"
            );
            let mut heaps = 0;
            for (candidate, root) in composed_with(&inventory, topk) {
                if candidate == HowToComputeAggregateMeasure::Exact {
                    continue;
                }
                assert!(
                    !OperatorNode::reachable(&root).iter().any(|n| matches!(
                        n.non_asap(),
                        Some(NonASAPOp::Aggregate { .. })
                    ) || matches!(
                        n.asap(),
                        Some(ASAPOp::FinalizeExactAccumulator { .. })
                    )),
                    "{query} {candidate:?}: no sum is computed"
                );
                let [state] = &heap_states(&root)[..] else {
                    panic!("{query}: one heap")
                };
                let Some(ASAPOp::SummaryAgg {
                    child,
                    input,
                    reduction,
                    ..
                }) = state.asap()
                else {
                    unreachable!()
                };
                assert!(
                    matches!(child.non_asap(), Some(NonASAPOp::TimeRange { .. })),
                    "{query}: the heap reads the raw data samples"
                );
                assert_eq!(
                    input.item,
                    Some(SummaryInputExpr::Column(ColumnRef::Named("region".into()))),
                    "{query}"
                );
                assert_eq!(&input.weight, weight, "{query}");
                assert_eq!(&input.weight_domain, domain, "{query}");
                let names: Vec<_> = reduction
                    .group_keys()
                    .map_or(&[][..], |keys| keys.keys())
                    .iter()
                    .map(|&index| child.schema.fields[index].name.as_str())
                    .collect();
                assert_eq!(names, partitions, "{query}");
                // The answer has the top-k's columns, `region` among them.
                assert!(
                    root.schema.fields.iter().any(|f| f.name == "region"),
                    "{query}: {:?}",
                    root.schema
                );
                heaps += 1;
            }
            assert_eq!(heaps, 2, "{query}");
        }
    }

    /// The weight domain of every heap-sketch update in every candidate of
    /// `query`'s top-k.
    fn heap_weight_domains(query: &str) -> Vec<WeightDomain> {
        let inventory = inventory(query);
        let topk = topk_target(&inventory);
        composed_with(&inventory, topk)
            .iter()
            .flat_map(|(_, root)| heap_states(root))
            .map(|state| match state.asap() {
                Some(ASAPOp::SummaryAgg { input, .. }) => input.weight_domain.clone(),
                _ => unreachable!(),
            })
            .collect()
    }

    /// Pass 1 does not read the metric's type: heap weights that are raw
    /// samples of `m` carry the open requirement that `m` is a counter, under
    /// any name (`m_total` included), for plan selection to confirm.
    #[test]
    fn heap_weights_over_raw_data_samples_record_an_open_requirement() {
        for (query, metric) in [
            ("topk by (job) (10, sum_over_time(m[1m]))", "m"),
            ("topk by (job) (10, sum_over_time(m_total[1m]))", "m_total"),
        ] {
            let domains = heap_weight_domains(query);
            assert_eq!(domains.len(), 2, "{query}");
            assert!(
                domains.iter().all(|d| *d
                    == WeightDomain::RawDataSamplesOfMetric {
                        metric: metric.into()
                    }),
                "{query}: {domains:?}"
            );
        }
    }

    /// Only raw data samples and plain sums of them: arithmetic, `rate` and other
    /// functions end the requirement.
    #[test]
    fn raw_data_samples_of_metric_ends_at_any_other_operator() {
        for (query, metric) in [
            ("sum_over_time(m[1m])", Some("m")),
            ("sum by (job) (sum_over_time(m[1m]))", Some("m")),
            ("sum_over_time(m[1m]) - 100", None),
            ("-sum_over_time(m[1m])", None),
            ("rate(m[1m])", None),
            ("max_over_time(m[1m])", None),
        ] {
            let root = crate::test_support::lower_promql(query, AccuracyTarget::exact());
            assert_eq!(
                raw_data_samples_of_metric(&root).as_deref(),
                metric,
                "{query}"
            );
        }
    }

    /// An avg's exact states are its sum then its count: any other order
    /// is refused rather than divided the wrong way round.
    #[test]
    fn avg_states_out_of_order_are_refused() {
        let mut inventory = inventory("avg by (job) (m)");
        assert_eq!(
            inventory.targets[0].ways_to_compute,
            [HowToComputeAggregateMeasure::ExactAccumulator(vec![
                ExactKind::Sum,
                ExactKind::Count
            ])]
        );
        assert!(compose_logical_candidate(&inventory, &[0]).is_ok());
        inventory.targets[0].ways_to_compute = vec![
            HowToComputeAggregateMeasure::ExactAccumulator(vec![ExactKind::Count, ExactKind::Sum]),
        ];
        assert!(matches!(
            compose_logical_candidate(&inventory, &[0]),
            Err(PerComputationCandidateError::Unsupported(_))
        ));
    }

    /// The target whose fused computation is a nested-sum merge.
    fn nested_sum_target(inventory: &PerComputationCandidates) -> Option<usize> {
        let merge = HowToComputeAggregateMeasure::ExactAccumulator(vec![ExactKind::Sum]);
        inventory.targets.iter().position(|t| {
            t.fused_computation
                .as_ref()
                .is_some_and(|fused| fused.ways_to_compute == [merge.clone()])
        })
    }

    /// `sum by (job) (sum_over_time(m[5m]))` gets a fused nested sum
    /// beside its own accumulator: one exact Sum accumulator over the range's
    /// raw data samples, grouped by `job`, which absorbs the inner per-series sum.
    /// Composed, no per-series state is built, the value is exact and the
    /// output columns are the outer sum's.
    #[test]
    fn nested_sum_merges_into_one_exact_accumulator() {
        let inventory = inventory("sum by (job) (sum_over_time(m[5m]))");
        let outer = nested_sum_target(&inventory).expect("a nested-sum merge");
        let target = &inventory.targets[outer];
        let sum = HowToComputeAggregateMeasure::ExactAccumulator(vec![ExactKind::Sum]);
        assert_eq!(target.ways_to_compute, std::slice::from_ref(&sum));
        let fused = target
            .fused_computation
            .as_ref()
            .expect("a fused computation");
        assert_eq!(fused.ways_to_compute, [sum]);
        let inner = fused.absorbed_targets[0];
        assert_eq!(inventory.targets[inner].ways_to_compute.len(), 1);
        assert_eq!(inventory.combinations(), 2);
        let composed = composed_with(&inventory, outer);
        let (_, separate) = &composed[0];
        let (_, merged) = &composed[1];
        let states: Vec<_> = OperatorNode::reachable(merged)
            .into_iter()
            .filter(|n| matches!(n.asap(), Some(ASAPOp::SummaryAgg { .. })))
            .collect();
        let [state] = states.as_slice() else {
            panic!("one accumulator: {states:?}")
        };
        let Some(ASAPOp::SummaryAgg {
            child,
            family,
            reduction,
            ..
        }) = state.asap()
        else {
            unreachable!()
        };
        assert_eq!(
            *family,
            FieldDataType::ExactAggregate(ExactKind::Sum, ExactParams::Sum)
        );
        assert!(matches!(
            child.non_asap(),
            Some(NonASAPOp::TimeRange { .. })
        ));
        let Reduction::Reduce(keys) = reduction else {
            panic!("grouped by job: {reduction:?}")
        };
        assert_eq!(
            keys.keys()
                .iter()
                .map(|&k| child.schema.fields[k].name.as_str())
                .collect::<Vec<_>>(),
            ["job"]
        );
        assert!(merged
            .guarantee
            .as_ref()
            .is_some_and(AccuracyGuarantee::is_exact));
        let names = |node: &OperatorNode| {
            node.schema
                .fields
                .iter()
                .map(|f| f.name.clone())
                .collect::<Vec<_>>()
        };
        assert_eq!(names(merged), names(separate));
        merged.validate_structure().unwrap();
    }

    /// The merge needs a sum over a sum's value: an outer sum over a rate,
    /// an outer count, or an outer sum over an inner `max` gets none.
    #[test]
    fn nested_sum_merge_needs_a_sum_of_sums() {
        for query in [
            "sum by (job) (rate(m[5m]))",
            "count by (job) (sum_over_time(m[5m]))",
            "sum by (job) (max_over_time(m[5m]))",
        ] {
            assert_eq!(nested_sum_target(&inventory(query)), None, "{query}");
        }
    }

    /// A per-series sum drops the metric name, but the raw data samples keep it:
    /// grouping the merge by `__name__` would answer the metric's name where
    /// the nested sums answer none, so no merge is offered.
    #[test]
    fn nested_sum_merge_never_groups_by_the_dropped_metric_name() {
        let query = "sum by (__name__) (sum_over_time(m[5m]))";
        assert_eq!(nested_sum_target(&inventory(query)), None);
    }

    /// Merges do not stack: in `sum(sum by (job) (sum_over_time(m[5m])))`
    /// a target with a fused computation is never absorbed, and `combinations`
    /// counts exactly the choices enumeration keeps.
    #[test]
    fn nested_sum_merges_do_not_stack() {
        let inventory = inventory("sum(sum by (job) (sum_over_time(m[5m])))");
        let absorbed: Vec<usize> = absorbed_targets(&inventory.targets).collect();
        assert_eq!(absorbed.len(), 1, "one merge");
        for &a in &absorbed {
            assert!(
                inventory.targets[a].fused_computation.is_none(),
                "an absorbed target has a fused computation"
            );
        }
        let enumerated = enumerated_choices(&inventory);
        assert_eq!(inventory.combinations(), enumerated);
    }

    /// The shapes a heap over the raw data samples cannot answer do not get one:
    /// `bottomk` stays a sort and limit (no top-k target), and a grouped sum
    /// `without` labels or by the metric name (which the per-series sum drops)
    /// gets no heap that absorbs the per-series sum and reads the samples.
    #[test]
    fn grouped_range_sums_a_raw_data_samples_heap_cannot_rank_get_no_fused_heap() {
        let is_topk = |inventory: &PerComputationCandidates, t: &ComputationTarget| {
            matches!(
                inventory.aggregate(t).non_asap(),
                Some(NonASAPOp::Aggregate { measures, .. })
                    if matches!(measures.as_slice(), [AggIntent::TopK { .. }])
            )
        };
        let bottom = inventory("bottomk(3, sum by (region) (sum_over_time(m[1m])))");
        assert!(
            !bottom.targets.iter().any(|t| is_topk(&bottom, t)),
            "bottomk has no top-k target"
        );
        for query in [
            "topk(3, sum without (instance) (sum_over_time(m[1m])))",
            "topk(3, sum by (__name__) (sum_over_time(m[1m])))",
        ] {
            let inventory = inventory(query);
            for target in inventory.targets.iter().filter(|t| is_topk(&inventory, t)) {
                assert!(
                    target
                        .fused_computation
                        .as_ref()
                        .is_none_or(|f| f.absorbed_targets.len() == 1),
                    "{query}: {:?}",
                    target.fused_computation
                );
            }
        }
    }

    /// A fused computation picked leaves every target beneath it uncomputed,
    /// also one that an absorbed target's own merge absorbs: Pass 1 closes
    /// `absorbed_targets` over those, so enumeration keeps only their first choice and
    /// `combinations` still counts exactly the choices kept. Here the top-k
    /// heap is made to absorb the grouped sum only, whose merge absorbs the
    /// per-series sum.
    #[test]
    fn a_fused_computation_absorbs_what_its_absorbed_targets_merges_absorb() {
        let mut inventory = inventory("topk(10, sum by (job) (sum_over_time(m[1m])))");
        let topk = topk_target(&inventory);
        let fused = inventory.targets[topk]
            .fused_computation
            .as_mut()
            .expect("a fused heap");
        let grouped = fused.absorbed_targets[0];
        fused.absorbed_targets.truncate(1);
        let per_series = inventory.targets[grouped]
            .fused_computation
            .as_ref()
            .expect("the grouped sum's merge")
            .absorbed_targets[0];
        close_absorptions(&mut inventory.targets);
        assert_eq!(
            inventory.targets[topk]
                .fused_computation
                .as_ref()
                .unwrap()
                .absorbed_targets,
            vec![grouped, per_series]
        );
        assert_eq!(inventory.combinations(), enumerated_choices(&inventory));
    }

    /// `combinations` counts each choice once: picking a fused
    /// whole-expression candidate with any but the first candidate of the
    /// target it absorbs is the same candidate. An absorbed target with three
    /// candidates (an approximate inner count: its accumulator, Count-Min and
    /// UnivMon) gives 1 exact · 3 + 2 whole-expression = 5, not 3 · 3.
    #[test]
    fn combinations_skip_choices_a_whole_expression_candidate_absorbs() {
        let inventory = inventory("topk by (job) (10, count_over_time(m[1m]))");
        let topk = topk_target(&inventory);
        let target = &inventory.targets[topk];
        let Some(fused) = &target.fused_computation else {
            panic!("a fused computation: {:?}", target.ways_to_compute)
        };
        let inner = fused.absorbed_targets[0];
        let n = target.ways_to_compute.len();
        let whole = fused.ways_to_compute.len();
        let m = inventory.targets[inner].ways_to_compute.len();
        let enumerated = enumerated_choices(&inventory);
        assert!(whole > 0 && m > 1, "{n} {whole} {m}");
        assert_eq!(inventory.combinations(), n * m + whole);
        assert_eq!(inventory.combinations(), 5);
        assert_eq!(inventory.combinations(), enumerated);
    }

    /// A quantile estimate of a metric's samples carries their sign only for
    /// φ in [0, 1]: outside it PromQL answers -Inf or +Inf. Pass 1 offers
    /// no sketch for such a φ.
    #[test]
    fn a_quantile_estimate_shares_its_raw_data_samples_sign_only_inside_the_unit_interval() {
        let estimate_of = |query: &str| {
            let inventory = inventory(query);
            (0..inventory.targets.len())
                .flat_map(|t| composed_with(&inventory, t))
                .find_map(|(_, root)| {
                    OperatorNode::reachable(&root)
                        .into_iter()
                        .find(|n| matches!(n.asap(), Some(ASAPOp::SummaryEstimate { .. })))
                })
        };
        let inside = estimate_of("quantile_over_time(0.5, m[1m])").expect("a sketch estimate");
        assert_eq!(estimates_raw_data_samples_of(&inside).as_deref(), Some("m"));
        let Some(ASAPOp::SummaryEstimate { summary_input, .. }) = inside.asap() else {
            unreachable!()
        };
        for q in [-1.0, 2.0] {
            let outside = OperatorNode::with_schema(
                Operator::ASAP(ASAPOp::SummaryEstimate {
                    summary_input: Rc::clone(summary_input),
                    query: SketchStatistic::Quantile { q },
                }),
                inside.schema.clone(),
            );
            assert_eq!(estimates_raw_data_samples_of(&outside), None, "q = {q}");
        }
        for query in [
            "quantile_over_time(-1, m[1m])",
            "quantile_over_time(2, m[1m])",
        ] {
            assert!(estimate_of(query).is_none(), "{query}: a sketch is offered");
        }
    }

    /// A quantile read from merged tumbling panes of a metric's samples is
    /// still one of the samples (or between two), so it carries their sign
    /// as the same quantile over the raw window does; φ outside [0, 1]
    /// still carries none.
    #[test]
    fn a_quantile_of_merged_panes_shares_its_raw_data_samples_sign() {
        let inventory = inventory("quantile_over_time(0.99, m[100s])");
        let estimate = (0..inventory.targets.len())
            .flat_map(|t| composed_with(&inventory, t))
            .find_map(|(_, root)| {
                OperatorNode::reachable(&root)
                    .into_iter()
                    .find(|n| matches!(n.asap(), Some(ASAPOp::SummaryEstimate { .. })))
            })
            .expect("a sketch estimate");
        let Some(ASAPOp::SummaryEstimate { summary_input, .. }) = estimate.asap() else {
            unreachable!()
        };
        let Some(ASAPOp::SummaryAgg {
            child: range,
            family,
            input,
            reduction,
            grouping,
            filter,
        }) = summary_input.asap()
        else {
            panic!("{summary_input:?}")
        };
        let chain = Rc::new(
            OperatorNode::new(Operator::ASAP(ASAPOp::PaneChain {
                child: Rc::clone(range.children()[0]),
                family: family.clone(),
                input: input.clone(),
                reduction: reduction.clone(),
                grouping: grouping.clone(),
                filter: filter.clone(),
                pane_width_ms: 10_000,
                pane_count: 10,
            }))
            .unwrap(),
        );
        let merged = Rc::new(
            OperatorNode::new(Operator::ASAP(ASAPOp::SummaryMerge {
                children: vec![chain],
                newest_panes: None,
                group_by: None,
            }))
            .unwrap(),
        );
        let read = |q: f64| {
            OperatorNode::with_schema(
                Operator::ASAP(ASAPOp::SummaryEstimate {
                    summary_input: Rc::clone(&merged),
                    query: SketchStatistic::Quantile { q },
                }),
                estimate.schema.clone(),
            )
        };
        assert_eq!(
            estimates_raw_data_samples_of(&read(0.99)).as_deref(),
            Some("m")
        );
        assert_eq!(
            sign_statistic(&fold_stats(&Reduction::by(vec![]), &Rc::new(read(0.99))).unwrap()),
            Some(raw_data_samples_sign_statistic("m").as_str())
        );
        for q in [-1.0, 2.0] {
            assert_eq!(estimates_raw_data_samples_of(&read(q)), None, "q = {q}");
        }
    }

    /// The metric whose raw data samples, or plain sums of them, a composed node's
    /// values estimate: a whole-expression heap's estimates and an exact
    /// sum over the samples are; a rate or any other function is not. A
    /// fold over them names its unknown sign by that metric.
    #[test]
    fn values_estimating_raw_data_samples_name_their_sign_by_the_metric() {
        let inventory = inventory("topk by (job) (10, sum_over_time(m[1m]))");
        let topk = topk_target(&inventory);
        for (candidate, root) in composed_with(&inventory, topk) {
            let estimate = OperatorNode::reachable(&root)
                .into_iter()
                .find(|n| {
                    matches!(
                        n.asap(),
                        Some(
                            ASAPOp::SummaryEstimate { .. }
                                | ASAPOp::FinalizeExactAccumulator { .. }
                        )
                    )
                })
                .unwrap();
            assert_eq!(
                estimates_raw_data_samples_of(&estimate).as_deref(),
                Some("m"),
                "{candidate:?}"
            );
            let stats = fold_stats(&Reduction::by(vec![]), &estimate).unwrap();
            assert_eq!(
                asap_accuracy::test_support::sign_statistic(&stats),
                Some(asap_accuracy::test_support::raw_data_samples_sign_statistic("m").as_str())
            );
        }
        let rate = inventory_root("sum by (job) (rate(m[1m]))");
        let finalized = OperatorNode::reachable(&rate)
            .into_iter()
            .filter(|n| matches!(n.asap(), Some(ASAPOp::FinalizeExactAccumulator { .. })))
            .collect::<Vec<_>>();
        assert!(!finalized.is_empty());
        for node in finalized {
            assert_eq!(estimates_raw_data_samples_of(&node), None);
        }
        let stats = fold_stats(&Reduction::by(vec![]), &rate).unwrap();
        assert_eq!(sign_statistic(&stats), None);
    }

    /// The sketch parameters of every summary state below `root`.
    fn sketch_params(root: &Rc<OperatorNode>) -> Vec<SketchParams> {
        OperatorNode::reachable(root)
            .into_iter()
            .filter_map(|node| match node.asap() {
                Some(ASAPOp::SummaryAgg {
                    family: FieldDataType::Sketch(kind, _),
                    ..
                }) => Some(kind.params().clone()),
                _ => None,
            })
            .collect()
    }

    /// `query` composed with target `t` at the candidate `pick(t, its
    /// candidates)` returns the position of.
    fn composed_picking(
        query: &str,
        pick: impl Fn(usize, &[HowToComputeAggregateMeasure]) -> usize,
    ) -> Rc<OperatorNode> {
        let inventory = inventory(query);
        let choice: Vec<usize> = inventory
            .targets
            .iter()
            .enumerate()
            .map(|(t, target)| pick(t, &target.ways_to_compute))
            .collect();
        let composed = compose_logical_candidate(&inventory, &choice).unwrap();
        let QueryRoot::Operator(root) = &composed[0] else {
            panic!("operator root")
        };
        root.clone()
    }

    fn position_of(
        candidates: &[HowToComputeAggregateMeasure],
        candidate: HowToComputeAggregateMeasure,
    ) -> usize {
        candidates
            .iter()
            .position(|c| *c == candidate)
            .expect("the candidate is offered")
    }

    fn cms(eps: f64, delta: f64) -> SketchParams {
        SketchParams::Cms {
            width: asap_accuracy::test_support::cms_width(eps),
            depth: asap_accuracy::test_support::cms_depth(delta),
        }
    }

    /// A combination of approximate values splits the query's budget
    /// equally among them (#669b): the difference of two Count-Min counts
    /// sizes each for ε/2 and δ/2 of the query's ε = 0.01, δ = 0.001.
    #[test]
    fn approximate_inputs_of_a_combination_share_the_budget_equally() {
        let both = composed_picking("count by (job) (a) - count by (job) (b)", |_, c| {
            position_of(
                c,
                HowToComputeAggregateMeasure::Summary(SketchAlgorithm::Cms),
            )
        });
        assert!(both.guarantee.as_ref().is_some_and(|g| !g.is_exact()));
        assert_eq!(sketch_params(&both), vec![cms(0.005, 0.0005); 2]);
    }

    /// Only the inputs that are approximate in the combination share the
    /// budget: with one count exact, the other gets all of it.
    #[test]
    fn an_exact_input_takes_no_share_of_the_budget() {
        let one = composed_picking("count by (job) (a) - count by (job) (b)", |t, c| {
            if t == 0 {
                position_of(
                    c,
                    HowToComputeAggregateMeasure::Summary(SketchAlgorithm::Cms),
                )
            } else {
                0
            }
        });
        assert_eq!(sketch_params(&one), vec![cms(0.01, 0.001)]);
    }

    /// The first composed candidate of `query`.
    fn inventory_root(query: &str) -> Rc<OperatorNode> {
        let inventory = inventory(query);
        let composed =
            compose_logical_candidate(&inventory, &vec![0; inventory.targets.len()]).unwrap();
        let QueryRoot::Operator(root) = &composed[0] else {
            panic!("operator root")
        };
        root.clone()
    }

    /// A Count-Min estimate has a bound only over weights proven
    /// non-negative: an open requirement (`RawDataSamplesOfMetric`) is not a proof.
    #[test]
    fn count_min_estimate_has_no_bound_over_an_open_requirement() {
        let family = FieldDataType::Sketch(
            SketchKind::new(
                SketchAlgorithm::Cms,
                asap_types::ir::schema::SketchParams::Cms { width: 8, depth: 2 },
            ),
            GroupingStrategy::default(),
        );
        let query = SketchStatistic::PointCount {
            key: ColumnRef::SampleValue,
            value: None,
        };
        let with = |weight_domain| SummaryUpdate {
            item: Some(SummaryInputExpr::Column(ColumnRef::SampleValue)),
            weight: SummaryInputExpr::Column(ColumnRef::SampleValue),
            weight_domain,
        };
        assert!(!estimate_has_bound(
            &family,
            &with(WeightDomain::RawDataSamplesOfMetric { metric: "m".into() }),
            &query
        ));
        assert!(!estimate_has_bound(
            &family,
            &with(WeightDomain::UnknownOrSigned),
            &query
        ));
        assert!(estimate_has_bound(
            &family,
            &with(WeightDomain::NonNegative {
                proof: NonNegativeWeightProof::CounterRawDataSamples
            }),
            &query
        ));
    }

    /// A Count-Min estimate over samples of `m` gets its bound gated on
    /// `m`'s sign: unknown until plan selection binds it, the local bound
    /// once bound to 1.
    #[test]
    fn count_min_estimate_over_raw_data_samples_is_gated_on_the_metric() {
        let family = FieldDataType::Sketch(
            SketchKind::new(
                SketchAlgorithm::Cms,
                asap_types::ir::schema::SketchParams::Cms {
                    width: 272,
                    depth: 5,
                },
            ),
            GroupingStrategy::default(),
        );
        let query = SketchStatistic::PointCount {
            key: ColumnRef::SampleValue,
            value: None,
        };
        let update = SummaryUpdate {
            item: Some(SummaryInputExpr::Column(ColumnRef::SampleValue)),
            weight: SummaryInputExpr::Column(ColumnRef::SampleValue),
            weight_domain: WeightDomain::RawDataSamplesOfMetric { metric: "m".into() },
        };
        let gated = estimate_bound_over_raw_data_samples(&family, &update, &query).expect("gated");
        assert_eq!(gated.spec.error.evaluate(), None);
        let local = local_guarantee(&family, &query).unwrap();
        let statistic = raw_data_samples_sign_statistic("m");
        // Every other statistic (a stream norm) bound alike.
        let others = |s: &str| Some(if s == statistic { 1.0 } else { 1_000.0 });
        let bound = gated.bind(&others).spec.error.evaluate();
        assert!(bound.is_some());
        assert_eq!(bound, local.bind(&others).spec.error.evaluate());
        let unconfirmed = gated
            .bind(&|s| (s != statistic).then_some(1_000.0))
            .spec
            .error
            .evaluate();
        assert_eq!(unconfirmed, None);
    }

    /// Classic HLL's failure probability is left symbolic in a source's
    /// distinct bound only for one instance per group over that source's
    /// own rows (#693's scope condition): not over a join of two sources,
    /// not over computed rows, and not for one Hydra structure shared by
    /// every group, whose distinct items the source's bound does not bound.
    #[test]
    fn classic_hll_confidence_needs_one_instance_over_a_sources_own_rows() {
        use asap_types::ir::schema::{HydraKind, HydraParams};
        let hll = |grouping| {
            FieldDataType::Sketch(
                SketchKind::new(
                    SketchAlgorithm::Hll,
                    SketchParams::Hll {
                        precision: 15,
                        relative_error: Some(0.05),
                    },
                ),
                grouping,
            )
        };
        let input = |query| crate::test_support::lower_promql(query, AccuracyTarget::exact());
        let guarantee = |family: &FieldDataType, query| {
            source_distinct_guarantee(family, &SketchStatistic::Cardinality, &input(query))
        };
        let own = hll(GroupingStrategy::PerSubpopulationInstance);
        let symbolic = guarantee(&own, "a").expect("a source's own rows");
        assert_eq!(
            symbolic.spec.failure_probability,
            ProbabilityExpr::Unknown {
                statistic: asap_accuracy::estimators::hll::classic_failure_statistic(15, 0.05, "a")
            }
        );
        for (query, over) in [("a * on() b", "a join"), ("a * 2", "computed rows")] {
            assert!(guarantee(&own, query).is_none(), "{over}: {query}");
        }
        let hydra = hll(GroupingStrategy::SharedMultiSubpopulation {
            kind: HydraKind::HydraCms,
            params: HydraParams::HydraCms {
                width: 8,
                depth: 2,
                shared_rows: 2,
                shared_columns: 8,
            },
        });
        assert!(guarantee(&hydra, "a").is_none());
    }

    /// A deployment whose executor supports only `supported`: each family
    /// with its readouts.
    fn supporting(supported: &[(SketchAlgorithm, &[StatisticKind])]) -> Vec<SupportedSummary> {
        supported
            .iter()
            .map(|(family, readouts)| SupportedSummary {
                family: *family,
                readouts: readouts.to_vec(),
            })
            .collect()
    }

    /// A family the deployment cannot build is pruned from Pass 1 with
    /// `UnsupportedByDeployment`; the supported family and the exact option
    /// stay. With every summary supported the inventory is unchanged.
    #[test]
    fn a_family_the_deployment_cannot_build_is_pruned() {
        let query = "quantile_over_time(0.99, m[5m])";
        let unrestricted = inventory(query);
        let restricted = inventory(query).without_unsupported_summaries(Some(&supporting(&[(
            SketchAlgorithm::Kll,
            &[StatisticKind::Quantile],
        )])));
        let target = &restricted.targets[0];
        assert_eq!(
            target.ways_to_compute,
            [
                HowToComputeAggregateMeasure::Exact,
                HowToComputeAggregateMeasure::Summary(SketchAlgorithm::Kll)
            ]
        );
        assert_eq!(
            restricted.pruned_of_target(0, false),
            [(
                HowToComputeAggregateMeasure::Summary(SketchAlgorithm::DDSketch),
                PruneReason::UnsupportedByDeployment
            )]
        );
        let default = inventory(query).without_unsupported_summaries(None);
        assert_eq!(
            default.targets[0].ways_to_compute,
            unrestricted.targets[0].ways_to_compute
        );
    }

    /// A family the deployment builds but cannot answer the measure's
    /// readout from is pruned: UnivMon, supported for distinct counts and
    /// L2 only, is no count candidate; Count-Min, supported for the total,
    /// stays.
    #[test]
    fn a_readout_the_deployment_cannot_answer_prunes_its_family() {
        let restricted = inventory("count_over_time(m[1m])").without_unsupported_summaries(Some(
            &supporting(&[
                (SketchAlgorithm::Cms, &[StatisticKind::Total]),
                (
                    SketchAlgorithm::UnivMon,
                    &[StatisticKind::Distinct, StatisticKind::FrequencyL2],
                ),
            ]),
        ));
        let target = &restricted.targets[0];
        assert_eq!(
            target.ways_to_compute,
            [
                HowToComputeAggregateMeasure::ExactAccumulator(vec![ExactKind::Count]),
                HowToComputeAggregateMeasure::Summary(SketchAlgorithm::Cms),
            ]
        );
        assert_eq!(
            restricted.pruned_of_target(0, false),
            [(
                HowToComputeAggregateMeasure::Summary(SketchAlgorithm::UnivMon),
                PruneReason::UnsupportedByDeployment
            )]
        );
    }

    /// A fused whole-expression top-k loses the heap families the
    /// deployment cannot answer top-k from, each recorded with its reason,
    /// and is not offered once it has none left: the target and the inner
    /// sum keep their exact options.
    #[test]
    fn a_fused_computation_loses_its_unsupported_heaps() {
        let query = "topk by (job) (10, sum_over_time(m[1m]))";
        let one_heap = inventory(query).without_unsupported_summaries(Some(&supporting(&[(
            SketchAlgorithm::CountSketchWithHeap,
            &[StatisticKind::TopK],
        )])));
        let topk = topk_target(&one_heap);
        assert_eq!(
            one_heap.targets[topk]
                .fused_computation
                .as_ref()
                .unwrap()
                .ways_to_compute,
            [HowToComputeAggregateMeasure::Summary(
                SketchAlgorithm::CountSketchWithHeap
            )]
        );
        assert_eq!(
            one_heap.pruned_of_target(topk, true),
            [(
                HowToComputeAggregateMeasure::Summary(SketchAlgorithm::CmsWithHeap),
                PruneReason::UnsupportedByDeployment
            )]
        );
        assert_eq!(one_heap.combinations(), 2);
        // Count-Min with a heap answers item frequencies, not top-k.
        let none = inventory(query).without_unsupported_summaries(Some(&supporting(&[(
            SketchAlgorithm::CmsWithHeap,
            &[StatisticKind::ItemFrequency],
        )])));
        let topk = topk_target(&none);
        let fused = none.targets[topk].fused_computation.as_ref().unwrap();
        assert!(fused.ways_to_compute.is_empty());
        assert_eq!(
            none.pruned_of_target(topk, true),
            [
                (
                    HowToComputeAggregateMeasure::Summary(SketchAlgorithm::CmsWithHeap),
                    PruneReason::UnsupportedByDeployment
                ),
                (
                    HowToComputeAggregateMeasure::Summary(SketchAlgorithm::CountSketchWithHeap),
                    PruneReason::UnsupportedByDeployment
                ),
            ]
        );
        assert_eq!(
            none.targets[topk].ways_to_compute,
            [HowToComputeAggregateMeasure::Exact]
        );
        assert_eq!(none.combinations(), 1);
    }

    /// Mergeable intents give exactly their accumulator (`avg`: sum, count); other exact-only intents give `Exact`.
    #[test]
    fn intent_candidate_matrix() {
        use asap_types::ir::operator::agg_intent::{MathFunc, TimeFunc};
        use AggIntent as A;
        use ExactKind as E;
        let accumulator = |kinds: Vec<E>| HowToComputeAggregateMeasure::ExactAccumulator(kinds);
        let exact_count = A::Count {
            accuracy: AccuracyTarget::exact(),
        };
        let mut matrix: Vec<(A, ComputationChoices)> = [
            (A::Min { col: None }, vec![E::Min]),
            (A::Max { col: None }, vec![E::Max]),
            (A::IRate, vec![E::IRate]),
            (A::Increase, vec![E::Increase]),
            (A::Sum { col: None }, vec![E::Sum]),
            (A::Rate, vec![E::Rate]),
            (A::Avg { col: None }, vec![E::Sum, E::Count]),
        ]
        .into_iter()
        .map(|(intent, kinds)| {
            (
                intent,
                ComputationChoices {
                    candidates: vec![accumulator(kinds)],
                    pruned: vec![],
                },
            )
        })
        .collect();
        // An exact count prunes its summaries for requiring an exact answer.
        matrix.push((
            exact_count.clone(),
            ComputationChoices {
                candidates: vec![accumulator(vec![E::Count])],
                pruned: summary_candidates(&exact_count)
                    .iter()
                    .map(|a| {
                        (
                            HowToComputeAggregateMeasure::Summary(*a),
                            PruneReason::RequiresExact,
                        )
                    })
                    .collect(),
            },
        ));
        let exact_only = [
            A::PearsonCorr { left: 0, right: 1 },
            A::Extension {
                ext_kind: "frequency".into(),
                payload: serde_json::json!({ "item": "checkout" }),
            },
            A::HistogramQuantile { q: 0.99, le: 0 },
            A::Changes,
            A::Delta,
            A::IDelta,
            A::Deriv,
            A::Resets,
            A::PredictLinear { seconds: 60.0 },
            A::DoubleExpSmoothing {
                smoothing: 0.5,
                trend: 0.5,
            },
            A::HistogramCount,
            A::HistogramSum,
            A::HistogramAvg,
            A::HistogramStdDev,
            A::HistogramStdVar,
            A::HistogramFraction {
                lower: 0.0,
                upper: 1.0,
            },
            A::Math(MathFunc::Abs),
            A::TimeFn(TimeFunc::Hour),
            A::Absent {
                labels: Default::default(),
            },
            A::AbsentOverTime,
            A::PresentOverTime,
            A::Group,
            A::CountValues { label: "v".into() },
            A::LastOverTime,
            A::FirstOverTime,
            A::MadOverTime,
            A::TsOfMinOverTime,
            A::TsOfMaxOverTime,
            A::TsOfFirstOverTime,
            A::TsOfLastOverTime,
        ];
        matrix.extend(exact_only.into_iter().map(|intent| {
            (
                intent,
                ComputationChoices {
                    candidates: vec![HowToComputeAggregateMeasure::Exact],
                    pruned: vec![],
                },
            )
        }));
        for (intent, expected) in &matrix {
            assert_eq!(
                &computation_candidates_for_intent(intent).unwrap(),
                expected,
                "{intent:?}"
            );
        }
        // Correlation never gets a single-input summary.
        assert!(summary_candidates(&A::PearsonCorr { left: 0, right: 1 }).is_empty());
    }

    /// `avg_over_time` divides with a finite-division guard; a grouped `avg` does not.
    #[test]
    fn temporal_average_divides_with_a_finite_division_guard() {
        let division_guard = |query: &str| {
            let root = inventory_root(query);
            let divisions: Vec<bool> = OperatorNode::reachable(&root)
                .iter()
                .filter_map(|node| match node.non_asap() {
                    Some(NonASAPOp::BinaryOp { operator, .. })
                        if operator.kind == BinaryOpKind::Arithmetic(ArithmeticOpKind::Div) =>
                    {
                        Some(operator.checked_finite_division)
                    }
                    _ => None,
                })
                .collect();
            let [guarded] = divisions[..] else {
                panic!("{query}: one sum / count division: {root:#?}")
            };
            guarded
        };
        assert!(division_guard("avg_over_time(a[5m])"));
        assert!(!division_guard("avg by (job) (m)"));
    }

    /// The candidates of `query` lowered at `accuracy`, over rows without the
    /// series-identity column (open PromQL schemas): over identity-carrying
    /// (closed) rows a grouped top-k gets no whole-expression heap.
    fn inventory_at(query: &str, accuracy: AccuracyTarget) -> PerComputationCandidates {
        let root = crate::test_support::lower_promql(query, accuracy);
        generate_per_computation_candidates(
            &crate::pass2::memo::DagOfLogicalMemoGroups::from_roots(&[(
                0,
                QueryRoot::Operator(root),
            )]),
        )
        .unwrap()
    }

    fn open_inventory(query: &str) -> PerComputationCandidates {
        inventory_at(
            query,
            AccuracyTarget::max_error(0.01).failure_probability(0.001),
        )
    }

    /// The estimate and state of the top-k's fused whole-expression heap of
    /// `algorithm`.
    fn fused_heap(
        inventory: &PerComputationCandidates,
        algorithm: SketchAlgorithm,
    ) -> (SketchStatistic, Rc<OperatorNode>) {
        let topk = topk_target(inventory);
        let own = inventory.targets[topk].ways_to_compute.len();
        let (_, root) = composed_with(inventory, topk)
            .into_iter()
            .skip(own)
            .find(|(candidate, _)| *candidate == HowToComputeAggregateMeasure::Summary(algorithm))
            .unwrap_or_else(|| panic!("a fused {algorithm:?} candidate"));
        let [state] = &heap_states(&root)[..] else {
            panic!("one heap: {root:#?}")
        };
        let estimate = OperatorNode::reachable(&root)
            .into_iter()
            .find_map(|node| match node.asap() {
                Some(ASAPOp::SummaryEstimate {
                    summary_input,
                    query,
                }) if Rc::ptr_eq(summary_input, state) => Some(query.clone()),
                _ => None,
            })
            .expect("the heap's estimate");
        (estimate, state.clone())
    }

    fn heap_update(state: &OperatorNode) -> &SummaryUpdate {
        match state.asap() {
            Some(ASAPOp::SummaryAgg { input, .. }) => input,
            _ => unreachable!(),
        }
    }

    /// A top-k over a `count` fuses into one global unit-weight heap over the raw rows, ranking the label.
    #[test]
    fn count_ranked_topk_fuses_to_one_global_heap_sketch() {
        let inventory = open_inventory("topk(10, count by (service) (m))");
        let topk = topk_target(&inventory);
        let fused = inventory.targets[topk]
            .fused_computation
            .as_ref()
            .expect("a fused heap");
        assert!(fused
            .ways_to_compute
            .contains(&HowToComputeAggregateMeasure::Summary(
                SketchAlgorithm::CmsWithHeap
            )));
        let (estimate, state) = fused_heap(&inventory, SketchAlgorithm::CmsWithHeap);
        assert_eq!(estimate, SketchStatistic::TopK { k: 10 });
        let update = heap_update(&state);
        assert_eq!(
            update.item,
            Some(SummaryInputExpr::Column(ColumnRef::Named("service".into())))
        );
        assert_eq!(update.weight, SummaryInputExpr::Constant(1.0));
        assert_eq!(
            update.weight_domain,
            WeightDomain::NonNegative {
                proof: NonNegativeWeightProof::UnitCount,
            }
        );
        let Some(ASAPOp::SummaryAgg { child, .. }) = state.asap() else {
            unreachable!()
        };
        assert!(!child.contains_asap());
    }

    /// The ranked item over several group keys is their tuple, in the lowered (name-sorted) key order.
    #[test]
    fn multidimensional_ranked_item_is_a_structured_tuple() {
        let inventory = open_inventory("topk(10, count by (service, region) (m))");
        let (_, state) = fused_heap(&inventory, SketchAlgorithm::CmsWithHeap);
        let column = |name: &str| SummaryInputExpr::Column(ColumnRef::Named(name.into()));
        assert_eq!(
            heap_update(&state).item,
            Some(SummaryInputExpr::Tuple(vec![
                column("region"),
                column("service")
            ]))
        );
    }

    /// The top-k's own partition keys partition the heap and are left out of the ranked tuple.
    #[test]
    fn subpopulation_columns_are_not_part_of_the_ranked_item_tuple() {
        let inventory =
            open_inventory("topk by (region) (10, count by (service, method, region) (m))");
        let (_, state) = fused_heap(&inventory, SketchAlgorithm::CmsWithHeap);
        let column = |name: &str| SummaryInputExpr::Column(ColumnRef::Named(name.into()));
        assert_eq!(
            heap_update(&state).item,
            Some(SummaryInputExpr::Tuple(vec![
                column("method"),
                column("service")
            ]))
        );
        let Some(ASAPOp::SummaryAgg {
            child, reduction, ..
        }) = state.asap()
        else {
            unreachable!()
        };
        let keys: Vec<_> = reduction
            .group_keys()
            .expect("partitioned by region")
            .keys()
            .iter()
            .map(|&k| child.schema.fields[k].name.as_str())
            .collect();
        assert_eq!(keys, ["region"]);
    }

    /// A per-series top-k ranks series identity (column, else label set) weighted by sample value.
    #[test]
    fn temporal_per_entity_topk_uses_series_identity_and_sample_value() {
        let query = "topk by (service) (5, sum_over_time(m[1m]))";
        let with_identity = inventory(query);
        let without_identity = open_inventory(query);
        for (inventory, item) in [
            (
                with_identity,
                SummaryInputExpr::Column(ColumnRef::Named(
                    asap_types::ir::schema::PROMQL_SERIES_IDENTITY.into(),
                )),
            ),
            (
                without_identity,
                SummaryInputExpr::EntityIdentity(EntityIdentity::PromqlLabelSet {
                    excluding: vec![ColumnRef::Named("service".into())],
                }),
            ),
        ] {
            let (_, state) = fused_heap(&inventory, SketchAlgorithm::CountSketchWithHeap);
            let update = heap_update(&state);
            assert_eq!(update.item, Some(item));
            assert_eq!(
                update.weight,
                SummaryInputExpr::Column(ColumnRef::SampleValue)
            );
        }
    }

    // Identity realization must preserve named labels and every top-k candidate's output contract.
    #[test]
    fn topk_keeps_known_labels_beside_series_identity() {
        let inventory = inventory("topk by (service) (5, sum_over_time(m{instance=\"a\"}[1m]))");
        let topk = topk_target(&inventory);
        let expected = &inventory.aggregate(&inventory.targets[topk]).schema;
        assert!(expected.fields.iter().any(|field| field.name == "instance"));
        assert!(expected
            .fields
            .iter()
            .any(|field| field.name == asap_types::ir::schema::PROMQL_SERIES_IDENTITY));
        for (_, root) in composed_with(&inventory, topk) {
            assert_eq!(root.schema.fields, expected.fields);
            assert_eq!(root.schema.time_index, expected.time_index);
            assert_eq!(root.schema.closed, expected.closed);
            // A physical heap may prove an additional partition/item key.
            // Every required uniqueness fact must still follow from its keys.
            for required in &expected.unique_keys {
                assert!(
                    root.schema.unique_keys.iter().any(|supplied| {
                        supplied.iter().all(|column| required.contains(column))
                    }),
                    "required key {required:?} is not implied by {:?}",
                    root.schema.unique_keys
                );
            }
        }
        let (_, state) = fused_heap(&inventory, SketchAlgorithm::CountSketchWithHeap);
        assert!(matches!(
            heap_update(&state).item,
            Some(SummaryInputExpr::Tuple(_))
        ));
    }

    /// A KLL quantile over an exact sum accumulator composes per node, the sum adding no error.
    #[test]
    fn nested_quantile_over_an_exact_sum_composes_per_node() {
        let sum = HowToComputeAggregateMeasure::ExactAccumulator(vec![ExactKind::Sum]);
        let kll = HowToComputeAggregateMeasure::Summary(SketchAlgorithm::Kll);
        let inventory = inventory_at(
            "quantile(0.9, sum by (job) (m))",
            AccuracyTarget::max_error(0.01).failure_probability(0.01),
        );
        let choice: Vec<usize> = inventory
            .targets
            .iter()
            .map(|target| {
                let candidates = &target.ways_to_compute;
                if candidates.contains(&kll) {
                    position_of(candidates, kll.clone())
                } else {
                    position_of(candidates, sum.clone())
                }
            })
            .collect();
        let composed = compose_logical_candidate(&inventory, &choice).unwrap();
        let QueryRoot::Operator(root) = &composed[0] else {
            panic!("operator root")
        };
        let estimate = OperatorNode::reachable(root)
            .into_iter()
            .find(|node| matches!(node.asap(), Some(ASAPOp::SummaryEstimate { .. })))
            .expect("a quantile estimate");
        let Some(ASAPOp::SummaryEstimate { summary_input, .. }) = estimate.asap() else {
            unreachable!()
        };
        let Some(ASAPOp::SummaryAgg { child, family, .. }) = summary_input.asap() else {
            panic!("a KLL state: {summary_input:#?}")
        };
        let FieldDataType::Sketch(kind, _) = family else {
            panic!("a sketch: {family:?}")
        };
        assert_eq!(kind.algorithm(), &SketchAlgorithm::Kll);
        assert_eq!(
            kind.params(),
            &SketchParams::Kll {
                k: 269,
                failure_probability: None
            }
        );
        let Some(ASAPOp::FinalizeExactAccumulator { child: sum_state }) = child.asap() else {
            panic!("the finalized sum: {child:#?}")
        };
        let Some(ASAPOp::SummaryAgg {
            family: sum_family,
            child: leaf,
            ..
        }) = sum_state.asap()
        else {
            panic!("the sum's accumulator: {sum_state:#?}")
        };
        assert_eq!(
            sum_family,
            &FieldDataType::ExactAggregate(ExactKind::Sum, ExactParams::Sum)
        );
        assert!(!leaf.contains_asap());
        assert!(child
            .guarantee
            .as_ref()
            .is_some_and(AccuracyGuarantee::is_exact));
        let guarantee = root.guarantee.as_ref().expect("a rank bound");
        assert_eq!(guarantee.spec.accuracy_metric, AccuracyMetric::Rank);
        assert_eq!(
            guarantee.spec.error.evaluate(),
            Some(asap_accuracy::test_support::kll_rank_error_99(269))
        );
    }

    /// Pass 1's inventory of `queries`, each lowered with its target, less
    /// the summaries no query can use over `data`.
    fn usable_inventory(
        queries: &[(&str, AccuracyTarget)],
        data: &DataWorkload,
    ) -> PerComputationCandidates {
        let roots: Vec<(usize, QueryRoot)> = queries
            .iter()
            .enumerate()
            .map(|(id, (query, target))| {
                let root = crate::test_support::lower_promql(query, target.clone());
                (id, QueryRoot::Operator(root))
            })
            .collect();
        let targets: Vec<AccuracyTarget> =
            queries.iter().map(|(_, target)| target.clone()).collect();
        generate_per_computation_candidates(
            &crate::pass2::memo::DagOfLogicalMemoGroups::from_roots(&roots),
        )
        .unwrap()
        .without_summaries_no_query_can_use(
            &targets,
            data,
            &asap_accuracy::BuiltinAccuracyModel,
        )
    }

    /// A per-value target at ε 0.05, δ 0.01 in `metric`.
    fn target_in(metric: AccuracyMetric) -> AccuracyTarget {
        AccuracyTarget::max_error(0.05)
            .failure_probability(0.01)
            .accuracy_metric(metric)
            .accuracy_scope(asap_types::ir::properties::AccuracyScope::PerValue)
    }

    /// Metric `x`, its samples declared non-negative or not.
    fn x_data(non_negative: bool) -> DataWorkload {
        let mut data =
            DataWorkload::sampled_every(["x".to_string()], asap_types::workload::DurationMs(1_000));
        data.sources.get_mut("x").unwrap().values_non_negative = non_negative;
        data
    }

    /// The summary families pruned for `reason`, and those still offered.
    fn pruned_and_offered(
        inventory: &PerComputationCandidates,
        reason: PruneReason,
    ) -> (Vec<SketchAlgorithm>, Vec<SketchAlgorithm>) {
        let summary = |way: &HowToComputeAggregateMeasure| match way {
            HowToComputeAggregateMeasure::Summary(algorithm) => Some(*algorithm),
            _ => None,
        };
        let pruned = inventory
            .pruned_ways_to_compute
            .iter()
            .filter(|pruned| pruned.reason == reason)
            .filter_map(|pruned| summary(&pruned.way_to_compute))
            .collect();
        let offered = inventory
            .targets
            .iter()
            .flat_map(|target| &target.ways_to_compute)
            .filter_map(summary)
            .collect();
        (pruned, offered)
    }

    /// An average of per-series p99s at a relative target: a relative error
    /// passes through `avg` only over samples declared non-negative, and a
    /// rank error never meets a relative target. Over an undeclared gauge
    /// every quantile sketch is pruned with
    /// [`PruneReason::NoErrorBoundForQueryTarget`]; declared non-negative,
    /// DDSketch is kept and KLL still pruned. At a rank target, a rank
    /// error has no rule through `avg`, so KLL is pruned too.
    #[test]
    fn a_relative_bound_through_avg_is_pruned_without_a_sign_declaration() {
        let query = "avg by (g) (quantile_over_time(0.99, x[100s]))";
        let reason = PruneReason::NoErrorBoundForQueryTarget;
        let relative = target_in(AccuracyMetric::Relative);
        let undeclared = usable_inventory(&[(query, relative.clone())], &x_data(false));
        let (pruned, offered) = pruned_and_offered(&undeclared, reason);
        assert!(pruned.contains(&SketchAlgorithm::DDSketch), "{pruned:?}");
        assert!(pruned.contains(&SketchAlgorithm::Kll), "{pruned:?}");
        assert!(offered.is_empty(), "{offered:?}");
        let declared = usable_inventory(&[(query, relative)], &x_data(true));
        let (pruned, offered) = pruned_and_offered(&declared, reason);
        assert_eq!(offered, vec![SketchAlgorithm::DDSketch], "{pruned:?}");
        assert!(pruned.contains(&SketchAlgorithm::Kll), "{pruned:?}");
        let rank = usable_inventory(&[(query, target_in(AccuracyMetric::Rank))], &x_data(true));
        let (pruned, offered) = pruned_and_offered(&rank, reason);
        assert!(pruned.contains(&SketchAlgorithm::Kll), "{pruned:?}");
        assert!(offered.is_empty(), "{offered:?}");
    }

    /// A summary one of the queries reading its group can use is kept: the
    /// per-series p99s are both a query's result, which a KLL rank bound
    /// meets, and the input of an average, which no rank bound reaches.
    #[test]
    fn a_summary_one_of_two_queries_can_use_is_kept() {
        let rank = target_in(AccuracyMetric::Rank);
        let alone = usable_inventory(
            &[("avg(quantile_over_time(0.99, x[5m]))", rank.clone())],
            &x_data(false),
        );
        let (pruned, _) = pruned_and_offered(&alone, PruneReason::NoErrorBoundForQueryTarget);
        assert!(pruned.contains(&SketchAlgorithm::Kll), "{pruned:?}");
        let shared = usable_inventory(
            &[
                ("quantile_over_time(0.99, x[5m])", rank.clone()),
                ("avg(quantile_over_time(0.99, x[5m]))", rank),
            ],
            &x_data(false),
        );
        let (pruned, offered) =
            pruned_and_offered(&shared, PruneReason::NoErrorBoundForQueryTarget);
        assert!(offered.contains(&SketchAlgorithm::Kll), "{offered:?}");
        assert!(!pruned.contains(&SketchAlgorithm::Kll), "{pruned:?}");
    }

    /// Exact candidates are never pruned: every target keeps its exact
    /// option first, and only summaries are pruned for the target.
    #[test]
    fn exact_candidates_are_never_pruned_for_the_target() {
        let rank = target_in(AccuracyMetric::Rank);
        for query in [
            "avg by (g) (quantile_over_time(0.99, x[100s]))",
            "avg(quantile_over_time(0.99, x[5m])) > 3",
            "sum(count_over_time(x[5m])) / avg(quantile_over_time(0.5, x[5m]))",
        ] {
            let inventory = usable_inventory(&[(query, rank.clone())], &x_data(false));
            assert!(!inventory.targets.is_empty(), "{query}");
            for target in &inventory.targets {
                assert!(
                    matches!(
                        target.ways_to_compute.first(),
                        Some(
                            HowToComputeAggregateMeasure::Exact
                                | HowToComputeAggregateMeasure::ExactAccumulator(_)
                        )
                    ),
                    "{query}: {:?}",
                    target.ways_to_compute
                );
            }
            assert!(
                inventory
                    .pruned_ways_to_compute
                    .iter()
                    .filter(|pruned| pruned.reason == PruneReason::NoErrorBoundForQueryTarget)
                    .all(|pruned| matches!(
                        pruned.way_to_compute,
                        HowToComputeAggregateMeasure::Summary(_)
                    )),
                "{query}"
            );
        }
    }

    /// A summary another aggregate over the same source also offers is
    /// kept, though no query reading its own group can use it: Pass 2 may
    /// build the other group's state from it (a rollup or window rollup),
    /// which Pass 1 does not see.
    #[test]
    fn a_summary_pass2_may_reuse_for_another_aggregate_is_kept() {
        let rank = target_in(AccuracyMetric::Rank);
        let reason = PruneReason::NoErrorBoundForQueryTarget;
        let with_other = usable_inventory(
            &[
                ("avg(quantile_over_time(0.99, x[5m]))", rank.clone()),
                ("quantile_over_time(0.99, x[10m])", rank),
            ],
            &x_data(false),
        );
        let (pruned, _) = pruned_and_offered(&with_other, reason);
        assert!(!pruned.contains(&SketchAlgorithm::Kll), "{pruned:?}");
    }

    /// A whole-result target with a failure probability over a number of
    /// values the data does not bound cannot be met by a per-value
    /// guarantee that may fail, whatever its size: KLL is pruned. With the
    /// series count declared, it is kept. DDSketch's relative bound never
    /// fails, so it is not pruned for the scope (only for the metric under
    /// a rank target).
    #[test]
    fn a_whole_result_target_over_unbound_values_prunes_a_failing_guarantee() {
        let whole = target_in(AccuracyMetric::Rank)
            .accuracy_scope(asap_types::ir::properties::AccuracyScope::WholeResult);
        let query = "quantile_over_time(0.99, x[5m])";
        let reason = PruneReason::NoErrorBoundForQueryTarget;
        let unbound = usable_inventory(&[(query, whole.clone())], &x_data(false));
        let (pruned, _) = pruned_and_offered(&unbound, reason);
        assert!(pruned.contains(&SketchAlgorithm::Kll), "{pruned:?}");
        let bound = usable_inventory(&[(query, whole)], &x_data(false).with_series_count(10_000));
        let (pruned, offered) = pruned_and_offered(&bound, reason);
        assert!(offered.contains(&SketchAlgorithm::Kll), "{pruned:?}");
    }

    /// The reuse escape keeps a summary only for another target whose own
    /// queries can use the family: two averages of p99s over different
    /// windows of one metric at a rank target both offer KLL, and neither
    /// can use it (a rank error has no rule through `avg`), so it is
    /// pruned for both. Before, each kept it for the other.
    #[test]
    fn a_summary_no_reusing_target_can_use_is_pruned() {
        let rank = target_in(AccuracyMetric::Rank);
        let inventory = usable_inventory(
            &[
                ("avg(quantile_over_time(0.99, x[5m]))", rank.clone()),
                ("avg(quantile_over_time(0.99, x[10m]))", rank),
            ],
            &x_data(false),
        );
        let (pruned, offered) =
            pruned_and_offered(&inventory, PruneReason::NoErrorBoundForQueryTarget);
        assert_eq!(
            pruned
                .iter()
                .filter(|&&family| family == SketchAlgorithm::Kll)
                .count(),
            2,
            "{pruned:?}"
        );
        assert!(!offered.contains(&SketchAlgorithm::Kll), "{offered:?}");
    }

    /// Classic HLL is pruned when its source's declared distinct bound is
    /// outside the contract at every precision (above 2^17 distinct items):
    /// its failure probability is 1 at any size. Under the bound it is kept,
    /// and with no declared bound too (plan selection may bind one). Two
    /// queries over the same metric do not keep it for each other.
    #[test]
    fn hll_over_a_distinct_bound_outside_its_contract_is_pruned() {
        let relative = target_in(AccuracyMetric::Relative);
        let queries = [
            ("distinct_over_time(x[5m])", relative.clone()),
            ("distinct_over_time(x[10m])", relative),
        ];
        let with_bound = |bound: Option<u64>| {
            let mut data = x_data(true);
            data.sources
                .get_mut("x")
                .unwrap()
                .max_distinct_per_evaluation = bound;
            data
        };
        let reason = PruneReason::NoErrorBoundForQueryTarget;
        let (_, offered) =
            pruned_and_offered(&usable_inventory(&queries, &with_bound(None)), reason);
        assert!(offered.contains(&SketchAlgorithm::Hll), "{offered:?}");
        let (_, offered) = pruned_and_offered(
            &usable_inventory(&queries, &with_bound(Some(1_000))),
            reason,
        );
        assert!(offered.contains(&SketchAlgorithm::Hll), "{offered:?}");
        let (pruned, offered) = pruned_and_offered(
            &usable_inventory(&queries, &with_bound(Some(1_000_000))),
            reason,
        );
        assert!(pruned.contains(&SketchAlgorithm::Hll), "{pruned:?}");
        assert!(!offered.contains(&SketchAlgorithm::Hll), "{offered:?}");
    }
}

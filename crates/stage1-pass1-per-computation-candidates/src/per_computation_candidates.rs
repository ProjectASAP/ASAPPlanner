//! Stage 1 (logical ASAP-aware optimization), Pass 1: per-computation
//! candidate generation, as defined in
//! `docs/design_docs/proposals/planner-layering.md`.
//!
//! [`generate_per_computation_candidates`] walks the memo groups of the
//! workload's memo groups ([`Memo`]) and
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
//! - Summary choices here are unsized families. Adding them to memo groups
//!   creates one build candidate per listed parameter configuration
//!   (`parameter_list`); Stage 3 checks each query's composed accuracy at its
//!   root.
//! - Candidates are pruned only when provably unable to meet the accuracy
//!   requirement or to run, or when the exact candidate provably dominates
//!   them (a PromQL top-k's heap over its instant vector, one row per item).
//! - A measure filter or `HAVING` prunes nothing. The target records the
//!   measure filter (applied as `SummaryAgg.filter`) and one [`HavingEntry`]
//!   per consumer `Filter` directly above the aggregate: a key-only predicate
//!   applied by every consumer of the aggregate's group, across queries, may go
//!   below the aggregate, anything else stays above the output (approximate
//!   outputs then get guarantee `None`). So every candidate of a group
//!   computes the group's own rows.
//!
//! The input must be a logical DAG: ASAP operators are rejected. A logical
//! DAG has no physical properties (#1200). No cost model is used. The accuracy model
//! is used only to prune summaries no query reading them can use
//! ([`PerComputationCandidates::without_summaries_no_query_can_use`]);
//! ranking and discarding valid candidates belong to Stage 3.
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
//! approximate inputs are sized for the values its consumers' targets cover
//! (`δ / (n · V)`,
//! [`pass1_memo`](crate::logical_memo_candidates::pass1_memo)),
//! their final sizes. Pass 2 inherits them or takes the strictest shared
//! consumer's, and Stage 3 only checks the composed accuracy.
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
    EntityIdentity, ExactKind, FieldDataType, GroupingStrategy, NonNegativeWeightProof,
    SketchAlgorithm, StatisticKind, SummaryInputExpr, SummaryUpdate, WeightDomain,
};
use asap_types::ir::{
    ASAPOp, BinaryOperator, ExprSemantics, NonASAPOp, Operator, OperatorNode, OperatorResultKind,
    Predicate, ProjectItem, QueryRoot, ScalarExpr, SchemaDerivationError,
};
use thiserror::Error;

use asap_accuracy::composition::parse_raw_data_samples_sign_statistic;
use asap_accuracy::estimators::hll;
use asap_accuracy::propagation::{
    bind_guarantees, raw_data_samples_of_metric, root_guarantee, with_guarantees,
};
use asap_accuracy::sizing::{accuracy_budget, sizing_target, sketch_statistic};
use asap_accuracy::AccuracyModel;
use asap_types::ir::properties::AccuracyGuarantee;
use asap_types::workload::bounds::{declared_non_negative, series_read, values_per_result};
use asap_types::workload::DataWorkload;

use crate::realization::summary_candidates;
use asap_types::deployment_capabilities::summary_supported;
use asap_types::ir::schema::SupportedSummary;
use asap_types::stages::{
    ComputationTarget, FusedComputation, HavingEntry, HavingPlacement,
    HowToComputeAggregateMeasure, Memo, MemoGroupId,
};

/// Unranked candidates of one measure and the candidates pruned for it.
#[derive(Debug, Clone, PartialEq)]
pub(super) struct ComputationChoices {
    pub candidates: Vec<HowToComputeAggregateMeasure>,
    pub pruned: Vec<HowToComputeAggregateMeasure>,
}

impl ComputationChoices {
    /// Prune matching candidates; a pruned exact accumulator form is replaced by
    /// plain `Exact`, so the target keeps one exact option.
    fn prune(&mut self, applies: impl Fn(&HowToComputeAggregateMeasure) -> bool) {
        let pruned = prune_own(&mut self.candidates, applies);
        self.pruned.extend(pruned);
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

/// Compact Pass 1 inventory over the memo groups it walked: one target per
/// measure of each aggregate group, however many queries read the group.
/// Pass 1's working form, not its output:
/// [`pass1_memo`](crate::pass1_memo)
/// builds, prunes and attaches it to the memo groups. Public only to tests
/// (feature `test-support`).
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
}

impl PerComputationCandidates {
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
    #[cfg(any(test, feature = "test-support"))]
    pub fn combinations(&self) -> usize {
        let absorbed: HashSet<usize> = absorbed_targets(&self.targets).collect();
        (0..self.targets.len())
            .filter(|index| !absorbed.contains(index))
            .map(|index| self.combinations_of(index))
            .fold(1usize, usize::saturating_mul)
    }

    /// [`Self::combinations`] of target `index` with the targets it absorbs.
    #[cfg(any(test, feature = "test-support"))]
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
    #[cfg(any(test, feature = "test-support"))]
    pub fn absorbed_choice_varies(&self, choice: &[usize]) -> bool {
        self.targets.iter().zip(choice).any(|(target, &index)| {
            target
                .computation_for(index)
                .and_then(|(_, absorbed)| absorbed)
                .is_some_and(|inner| inner.iter().any(|&i| choice[i] != 0))
        })
    }

    /// This inventory without the summary candidates the executor does not
    /// support (`supported_summaries`, the deployment's
    /// `DeploymentCapabilities::supported_summaries`; `None` supports
    /// every one): a target's own are removed, and so are a fused
    /// computation's (with all of them removed, the fusion is not offered).
    /// The readout is the statistic the measure's estimate reads (the outer measure's for a
    /// fused computation). With every summary supported it is unchanged.
    pub fn without_unsupported_summaries(
        mut self,
        supported_summaries: Option<&[SupportedSummary]>,
    ) -> Self {
        for target in self.targets.iter_mut() {
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
            prune_own(&mut target.ways_to_compute, unsupported);
            if let Some(fused) = &mut target.fused_computation {
                fused.ways_to_compute.retain(|way| !unsupported(way));
            }
        }
        self
    }

    /// This inventory without the summary candidates no query can use
    /// (#1047, accuracy A1 of #1095): a summary of a target (its own, or
    /// its fused computation's) is removed when, for every query
    /// whose root reaches the target's aggregate,
    /// `output_can_meet_target` says no: the query's output, composed
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
    /// Pass 1's entry (`pass1_memo`)
    /// calls it only when the deployment's accuracy model and evidence
    /// cannot accept an output without its guarantee.
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
                let Ok(composed) = compose(&self, &roots, &choices) else {
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
            prune_own(&mut target.ways_to_compute, |way| own_pruned.contains(way));
            if let Some(fused) = &mut target.fused_computation {
                fused
                    .ways_to_compute
                    .retain(|way| !fused_pruned.contains(way));
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
pub(crate) fn output_can_meet_target(
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
#[cfg(any(test, feature = "test-support"))]
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
    #[error("logical candidate input already contains ASAP operators")]
    ContainsAsap,
    /// `root` (the query's index in the workload) and `measure` locate the target;
    /// `computation_candidates_for_intent` alone leaves them `None`.
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
}

/// Enumerate exact, rewrite and summary choices of one measure in stable catalog
/// order, without ranking or empirical assessment. A summary is not a claim that
/// a deployment meets the measure's accuracy requirement. The one exact option
/// comes first: the exact accumulator form if the intent has one, else `Exact`.
pub(super) fn computation_candidates_for_intent(
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
        if matches!(intent, AggIntent::Quantile { q, .. } if !(0.0..=1.0).contains(q))
            || target.is_exact()
        {
            choices
                .pruned
                .extend(summaries.map(HowToComputeAggregateMeasure::Summary));
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

/// Each memo group's expression as an operator-node tree, by group id: the
/// trees target discovery still walks. `Memo::logical_node_trees` is
/// deleted; discovery reads the groups in #1207 PR 3.
fn logical_node_trees(memo: &Memo) -> Vec<Rc<OperatorNode>> {
    let _ = memo;
    todo!("#1207 PR 3: discover targets over the memo groups, not node trees")
}

/// Discover one target per measure of each eligible aggregate memo group of
/// `dag`, including operator plans read by scalar roots: Pass 1 walks the
/// groups, so an aggregate several queries read is one target per measure.
/// An aggregate measure is eligible when it has a realization besides exact
/// execution, even if every such realization is pruned; the prune reasons
/// are reported on the target. Measure filters and `HAVING` keep every
/// candidate; the target records how they are applied. A group's consumers
/// are its parent groups and the query roots that read it; a consumer
/// `Filter`'s key-only `HAVING` is pushed below only when every consumer of
/// the group applies the same predicate, so every candidate of a shared
/// aggregate computes the group's own rows (#1089).
pub fn generate_per_computation_candidates(
    memo: &Memo,
) -> Result<PerComputationCandidates, PerComputationCandidateError> {
    let logical_nodes: Rc<[Rc<OperatorNode>]> = logical_node_trees(memo).into();
    let group_of: HashMap<*const OperatorNode, MemoGroupId> = logical_nodes
        .iter()
        .enumerate()
        .map(|(g, node)| (Rc::as_ptr(node), MemoGroupId(g as u32)))
        .collect();
    // Each query's root over the groups' logical nodes, in workload order:
    // a group several queries read is one node.
    let node = |g: &MemoGroupId| Rc::clone(&logical_nodes[g.0 as usize]);
    let roots: Vec<QueryRoot> = memo
        .query_roots
        .iter()
        .map(|root| match root {
            QueryRoot::Operator(g) => QueryRoot::Operator(node(g)),
            QueryRoot::Scalar(expr) => QueryRoot::Scalar(expr.map_operator_refs(&mut |g| node(g))),
        })
        .collect();
    let mut seen = HashSet::new();
    // Each aggregate group with the first query that reaches it.
    let mut aggregates = Vec::new();
    // Every consumer of each group's node: `Some(parent)`, or `None` for a
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
            // Every consumer of the group, across queries, applies it.
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
                choices.prune(|choice| {
                    matches!(choice, HowToComputeAggregateMeasure::ExactAccumulator(_))
                });
            }
            if matches!(intent, AggIntent::Count { .. })
                && node.children()[0].result_kind == OperatorResultKind::Relation
            {
                choices.prune(|choice| matches!(choice, HowToComputeAggregateMeasure::Summary(_)));
            }
            targets.push(ComputationTarget {
                aggregate_group,
                measure_index: measure,
                ways_to_compute: choices.candidates,
                measure_filter: filters.get(measure).cloned().flatten(),
                having_filters: entries.clone(),
                fused_computation: None,
            });
        }
    }
    add_whole_expression_targets(&mut targets, &logical_nodes);
    prune_topk_heaps_over_one_row_per_item(&mut targets, &logical_nodes);
    add_nested_sum_targets(&mut targets, &logical_nodes);
    close_absorptions(&mut targets);
    Ok(PerComputationCandidates {
        roots,
        logical_nodes,
        targets,
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
/// own candidates for its other consumers. Applied when neither has a filter
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
/// input rows by column, [`Reduction::partitions_over`]) of a nested-sum merge of
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
    let partitions = reduction.partitions_over(&inner.schema, &rows.schema)?;
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
/// keep their own candidates for their other consumers. Applied when neither
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

/// Prunes the own heap candidates of each PromQL top-k: the heap reads
/// one row per item, so exact is no costlier; a fused whole-expression
/// heap ([`add_whole_expression_targets`]) has already moved out of them.
fn prune_topk_heaps_over_one_row_per_item(
    targets: &mut [ComputationTarget],
    logical_nodes: &[Rc<OperatorNode>],
) {
    for target in targets.iter_mut() {
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
        prune_own(&mut target.ways_to_compute, |candidate| {
            matches!(candidate, HowToComputeAggregateMeasure::Summary(family)
                if family.capability().answers(StatisticKind::TopK))
        });
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
/// onto the input the heap reads ([`Reduction::partitions_over`]).
fn whole_expression_input(
    target: &OperatorNode,
) -> Option<(Rc<OperatorNode>, SummaryUpdate, Reduction)> {
    use crate::realization::realize_keyed_additive_summary_input;
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
        SketchAlgorithm::CountSketchWithHeap,
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
    let partitions = reduction.partitions_over(&child.schema, &realized.child.schema)?;
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
/// is sized here for its measure's accuracy requirement. A measure filter
/// becomes `SummaryAgg.filter`;
/// `HAVING` is applied as its target records. A sub-DAG shared between roots
/// stays shared. Every node carries the guarantee `asap_accuracy::propagation::composed_guarantee`
/// gives it.
#[cfg(any(test, feature = "test-support"))]
pub fn compose_logical_candidate(
    inventory: &PerComputationCandidates,
    choice: &[usize],
) -> Result<Vec<QueryRoot>, PerComputationCandidateError> {
    compose(inventory, &inventory.roots, choice)
}

/// [`compose_logical_candidate`] of `roots` (some of `inventory.roots`),
/// each summary sized for its measure's whole budget.
fn compose(
    inventory: &PerComputationCandidates,
    roots: &[QueryRoot],
    choice: &[usize],
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
    compose_roots(roots, &chosen, &pushed_below, &Sizing::default())
}

/// One measure of an aggregate: the aggregate and the measure's index.
type MeasureKey = (*const OperatorNode, usize);

/// How [`compose_logical_candidate`] sizes summaries in one composition.
#[derive(Default)]
struct Sizing {
    /// Every sketch summary state built, with the measure it sizes.
    built: std::cell::RefCell<Vec<(Rc<OperatorNode>, MeasureKey)>>,
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
    // The metric whose raw data samples, or plain sums of them, are every
    // value the aggregate reads: a heap-sketch weight read from them carries
    // the open requirement `WeightDomain::RawDataSamplesOfMetric`, which
    // plan selection confirms.
    let raw_data_samples = raw_data_samples_of_metric(child);
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
                    |chosen| (chosen.candidate, raw_data_samples.as_deref(), chosen.fused),
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
        return Ok(output);
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
    Ok(OperatorNode::new_shared(Operator::NonASAP(
        NonASAPOp::Project {
            cols,
            child: output,
        },
    ))?)
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
        family: FieldDataType::Sketch(*algorithm, GroupingStrategy::default()),
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
/// consumers of `[keys.., topk_k]` find those columns where they were.
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
    let family = FieldDataType::exact(kind);
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
        let state = state(FieldDataType::exact(*kind), intent, read)?;
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
            let family = FieldDataType::Sketch(*algorithm, GroupingStrategy::default());
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
        NonASAPOp::Project { cols, child: node },
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

/// What a measure's state reads from each row of `input`, resolvable there.
/// A SQL `count` reads [`counted_column`]; any other measure, a PromQL count
/// included, reads its input column (the sample value when it names none).
fn measure_input(
    intent: &AggIntent,
    input: &Rc<OperatorNode>,
    filter: Option<&Predicate>,
) -> Result<SummaryInputExpr, PerComputationCandidateError> {
    let schema = &input.schema;
    let read = match intent {
        AggIntent::Count { .. } if input.result_kind == OperatorResultKind::Relation => {
            SummaryInputExpr::Column(counted_column(schema, filter)?)
        }
        // A top-k ranks its child's aggregate output, not a column of its own.
        AggIntent::TopK { .. } => SummaryInputExpr::Column(
            crate::realization::ranked_value(input)
                .map_err(|_| {
                    PerComputationCandidateError::Unsupported("Top-K has no ranked column")
                })?
                .1,
        ),
        _ => crate::realization::summarised_input(intent, schema).ok_or(
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
            .map(asap_types::ir::schema::Field::column_ref)
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
        FieldDataType::Sketch(kind, _) => Some(kind),
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
                    .map(|(_, field)| SummaryInputExpr::Column(field.column_ref()))
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
                    .map(asap_types::ir::schema::Field::column_ref)
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

    use super::*;

    use asap_accuracy::test_support::{
        estimate_bound_over_raw_data_samples, estimate_has_bound, local_guarantee,
        raw_data_samples_sign_statistic,
    };

    use asap_types::ir::properties::AccuracyTarget;
    use asap_types::ir::schema::SketchStatistic;
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

    /// A Count-Min estimate has a bound only over weights proven
    /// non-negative: an open requirement (`RawDataSamplesOfMetric`) is not a proof.
    #[test]
    fn count_min_estimate_has_no_bound_over_an_open_requirement() {
        let family = FieldDataType::Sketch(SketchAlgorithm::Cms, GroupingStrategy::default());
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
        let family = FieldDataType::Sketch(SketchAlgorithm::Cms, GroupingStrategy::default());
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
}

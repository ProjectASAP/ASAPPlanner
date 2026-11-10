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
//!   (`parameter_list`); Pass 2 gives readouts their required accuracy.
//! - Candidates are pruned only when provably unable to run, or when the
//!   exact candidate provably dominates them (a PromQL top-k's heap over its
//!   instant vector, one row per item); accuracy prunes nothing here (Pass
//!   2's dead-candidate elimination does).
//! - A measure filter or `HAVING` prunes nothing. The target records the
//!   measure filter (applied as `SummaryAgg.filter`) and one [`HavingEntry`]
//!   per consumer `Filter` directly above the aggregate: a key-only predicate
//!   applied by every consumer of the aggregate's group, across queries, may go
//!   below the aggregate, anything else stays above the output (approximate
//!   outputs then get guarantee `None`). So every candidate of a group
//!   computes the group's own rows.
//!
//! The input must be a logical DAG: ASAP operators are rejected. A logical
//! DAG has no physical properties (#1200). No cost or accuracy model is
//! used; ranking and discarding valid candidates belong to Stage 3.
use std::collections::{HashMap, HashSet};
use std::rc::Rc;

use asap_types::ir::operator::arguments::{Reduction, Source};
use asap_types::ir::operator::AggIntent;
use asap_types::ir::scalar::{resolve_column_ref, ColumnRef};
use asap_types::ir::schema::FieldDataType;
use asap_types::ir::{
    NonASAPOp, OperatorNode, OperatorResultKind, Predicate, QueryRoot, SchemaDerivationError,
};
use asap_types::summary_type::{
    ExactKind, SketchAlgorithm, StatisticKind, SummaryInputExpr, SummaryUpdate,
};
use thiserror::Error;

use asap_accuracy::sizing::sketch_statistic;

use crate::summary_realization::summary_candidates;
use asap_types::deployment_capabilities::{summary_supported, SupportedSummary};
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
    /// candidates are these roots with chosen candidates substituted
    /// (test support only).
    #[cfg(any(test, feature = "test-support"))]
    pub roots: Vec<QueryRoot>,
    /// Each memo group's logical node, by group index.
    pub(crate) logical_nodes: Rc<[Rc<OperatorNode>]>,
    /// The targets; a whole-workload choice has one digit per target,
    /// ranging over [`ComputationTarget::choice_count`].
    pub targets: Vec<ComputationTarget>,
}

impl PerComputationCandidates {
    /// The `Aggregate` node of `target`'s memo group.
    #[cfg(any(test, feature = "test-support"))]
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
    #[error("choice must name one listed candidate per target")]
    InvalidChoice,
    #[error("unsupported substitution in logical ASAP-aware optimization: {0}")]
    Unsupported(&'static str),
}

/// Enumerate exact, rewrite and summary choices of one measure in stable catalog
/// order, without ranking or empirical assessment. A summary is not a claim that
/// a deployment meets the measure's accuracy requirement. The one exact option
/// comes first: the exact accumulator form if the intent has one, else `Exact`.
pub(super) fn computation_candidates_for_intent(intent: &AggIntent) -> ComputationChoices {
    let states = match intent {
        AggIntent::Count => vec![ExactKind::Count],
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
    if sketch_statistic(intent).is_some() {
        let summaries = summary_candidates(intent).into_iter();
        if matches!(intent, AggIntent::Quantile { q, .. } if !(0.0..=1.0).contains(q)) {
            choices
                .pruned
                .extend(summaries.map(HowToComputeAggregateMeasure::Summary));
        } else {
            choices
                .candidates
                .extend(summaries.map(HowToComputeAggregateMeasure::Summary));
        }
    }
    choices
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
        .queries
        .iter()
        .map(|query| match &query.root {
            QueryRoot::Operator(g) => QueryRoot::Operator(node(g)),
            QueryRoot::Scalar(expr) => QueryRoot::Scalar(expr.map_operator_refs(&mut |g| node(g))),
        })
        .collect();
    let mut seen = HashSet::new();
    // Each aggregate group.
    let mut aggregates = Vec::new();
    // Every consumer of each group's node: `Some(parent)`, or `None` for a
    // query root or scalar reference.
    let mut consumers: HashMap<*const OperatorNode, Vec<Option<Rc<OperatorNode>>>> = HashMap::new();
    for root in &roots {
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
                    aggregates.push(node);
                }
            }
        }
    }
    let mut targets = Vec::new();
    for node in aggregates {
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
            let mut choices = computation_candidates_for_intent(intent);
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
            if matches!(intent, AggIntent::Count)
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
        #[cfg(any(test, feature = "test-support"))]
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
pub(super) fn nested_sum_input(
    target: &OperatorNode,
) -> Option<(Rc<OperatorNode>, AggIntent, Reduction)> {
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
pub(super) fn whole_expression_input(
    target: &OperatorNode,
) -> Option<(Rc<OperatorNode>, SummaryUpdate, Reduction)> {
    use crate::summary_realization::realize_keyed_additive_summary_input;
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
    let family = FieldDataType::Sketch(SketchAlgorithm::CountSketchWithHeap);
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

#[cfg(test)]
mod tests {

    use super::*;

    use asap_types::summary_type::{NonNegativeWeightProof, WeightDomain};

    use asap_accuracy::test_support::estimate_has_bound;

    use asap_types::summary_type::SketchStatistic;
    /// Approximate count keeps one exact option, its accumulator, beside the
    /// universal summary; plain exact execution is not offered.
    #[test]
    fn approximate_count_keeps_one_exact_accumulator_and_universal_choices() {
        let choices = computation_candidates_for_intent(&AggIntent::Count).candidates;
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
        let choices = computation_candidates_for_intent(&AggIntent::Count).candidates;
        assert!(!choices.contains(&HowToComputeAggregateMeasure::Summary(
            SketchAlgorithm::CountSketch
        )));
        let total = sketch_statistic(&AggIntent::Count).unwrap();
        for choice in &choices {
            if let HowToComputeAggregateMeasure::Summary(algorithm) = choice {
                assert!(
                    asap_types::summary_type::SummaryTypeCapability::of(&FieldDataType::Sketch(
                        *algorithm
                    ))
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
        let family = FieldDataType::Sketch(SketchAlgorithm::Cms);
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
}

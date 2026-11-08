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
//! [`pass1_memo`](crate::pass1::logical_memo_candidates::pass1_memo)),
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

use asap_types::ir::operator::operator_properties::{Reduction, Source};
use asap_types::ir::operator::AggIntent;
use asap_types::ir::scalar::{resolve_column_ref, ColumnRef};
use asap_types::ir::schema::{
    ExactKind, FieldDataType, SketchAlgorithm, StatisticKind, SummaryInputExpr, SummaryUpdate,
};
use asap_types::ir::{
    NonASAPOp, OperatorNode, OperatorResultKind, Predicate, QueryRoot, SchemaDerivationError,
};
use thiserror::Error;

use asap_accuracy::sizing::sketch_statistic;

use crate::pass1::realization::summary_candidates;
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
/// [`pass1_memo`](crate::pass1::pass1_memo)
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
        .query_roots
        .iter()
        .map(|root| match root {
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
    use crate::pass1::composition::*;
    use asap_types::ir::operator::operator_properties::{BinaryOpKind, JoinKind};
    use asap_types::ir::properties::{AccuracyGuarantee, AccuracyTarget};
    use asap_types::ir::scalar::expr_ir::ArithmeticOpKind;
    use asap_types::ir::scalar::{CompareOpKind, ScalarValue};
    use asap_types::ir::schema::{EntityIdentity, NonNegativeWeightProof, WeightDomain};
    use asap_types::ir::{
        ASAPOp, BinaryOperator, ExprSemantics, Operator, ProjectItem, ScalarExpr,
    };

    use asap_accuracy::propagation::raw_data_samples_of_metric;

    use asap_accuracy::composition::{StreamNorm, PRODUCT_OPERAND_DOMAINS};
    use asap_accuracy::propagation::operand_guarantee;
    use asap_accuracy::test_support::{
        composed_guarantee, estimate_has_bound, estimates_raw_data_samples_of, fold_stats,
        raw_data_samples_sign_statistic, scalar_guarantee, sign_statistic,
        source_distinct_guarantee, stream_norm, QUOTIENT_OPERAND_DOMAINS,
    };
    use asap_types::ir::properties::guarantee::{AccuracyMetric, BoundExpr, ProbabilityExpr};

    use asap_types::ir::schema::SketchStatistic;
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
                    asap_types::ir::schema::SummaryTypeCapability::of(&FieldDataType::Sketch(
                        *algorithm
                    ))
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
            &crate::stage0::memo_of_queries(
                &[QueryRoot::Operator(root)],
                &[None],
                &Default::default(),
            )
            .unwrap(),
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
                    summary_type: FieldDataType::Sketch(kind), ..
                }) if kind.capability().answers(StatisticKind::TopK))
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
            &crate::stage0::memo_of_queries(
                &[QueryRoot::Operator(root)],
                &[None],
                &Default::default(),
            )
            .unwrap(),
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
            summary_type: family,
            reduction,
            ..
        }) = state.asap()
        else {
            unreachable!()
        };
        assert_eq!(
            *family,
            FieldDataType::ExactAggregate(ExactKind::Sum, asap_types::ir::schema::ExactParams::Sum)
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

    /// A quantile read from merged tumbling windows of a metric's samples is
    /// still one of the samples (or between two), so it carries their sign
    /// as the same quantile over the raw window does; φ outside [0, 1]
    /// still carries none.
    #[test]
    fn a_quantile_of_merged_tumbling_windows_shares_its_raw_data_samples_sign() {
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
            summary_type: family,
            input,
            reduction,
            filter,
        }) = summary_input.asap()
        else {
            panic!("{summary_input:?}")
        };
        let chain = Rc::new(
            OperatorNode::new(Operator::ASAP(ASAPOp::SummaryTumblingWindowChain {
                child: Rc::clone(range.children()[0]),
                summary_type: family.clone(),
                input: input.clone(),
                reduction: reduction.clone(),
                filter: filter.clone(),
                tumbling_window_width_ms: 10_000,
            }))
            .unwrap(),
        );
        let merged = Rc::new(
            OperatorNode::new(Operator::ASAP(ASAPOp::SummaryMerge {
                children: vec![chain],
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

    /// Classic HLL's failure probability is left symbolic in a source's
    /// distinct bound only for one instance per group over that source's
    /// own rows (#693's scope condition): not over a join of two sources,
    /// not over computed rows, and not for one Hydra structure shared by
    /// every group, whose distinct items the source's bound does not bound.
    #[test]
    fn classic_hll_confidence_needs_one_instance_over_a_sources_own_rows() {
        let hll = FieldDataType::Sketch;
        let input = |query| crate::test_support::lower_promql(query, AccuracyTarget::exact());
        let guarantee = |family: &FieldDataType, query| {
            source_distinct_guarantee(family, &SketchStatistic::Cardinality, &input(query))
        };
        let own = hll(SketchAlgorithm::Hll);
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
        let hydra = hll(SketchAlgorithm::HydraHll);
        assert!(guarantee(&hydra, "a").is_none());
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
            &crate::stage0::memo_of_queries(
                &[QueryRoot::Operator(root)],
                &[None],
                &Default::default(),
            )
            .unwrap(),
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
}

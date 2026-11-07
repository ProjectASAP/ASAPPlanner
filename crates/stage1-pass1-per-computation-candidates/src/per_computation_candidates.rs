//! Stage 1, Pass 1: each memo group's initial candidates (concepts doc
//! `memo-groups-properties-requirements.md`, section 3, "Stage 1, Pass 1";
//! #1203, #1207).
//!
//! [`computation_targets`] walks the memo groups and finds, for every
//! aggregate group, one target ([`ComputationTarget`]) per measure whose
//! intent has a realization besides plain exact execution, once however
//! many queries read the group. [`add_pass1_candidates`] then adds each way
//! to compute a group as candidates, through `Memo::find_or_add_group` and
//! `Memo::add_candidate`:
//!
//! - **Exact.** Every group's own expression is a candidate of it.
//! - **Exact accumulator.** An accumulator-state group
//!   (`SummaryAgg` of an exact family) and a `FinalizeExactAccumulator`
//!   candidate over it; `avg` finalizes its `sum` and `count` states and
//!   divides them.
//! - **Summary.** One state group per family (`SummaryAgg` of the family),
//!   with one build candidate per configuration of the family's predefined
//!   list (`parameter_list`), and a readout (`SummaryEstimate`) group over
//!   it, read by the result group.
//! - **Fused builds.** A whole-expression top-k (a heap over the rows below
//!   a per-item `sum` or `count`) and a nested sum (one exact `Sum` state
//!   over the rows below a `sum` of a `sum`): each computes several
//!   operators of one appearance as one operator, and its builds list the
//!   groups of the operators it replaces in `fused_groups` (concepts doc
//!   §3, "Fused groups").
//!
//! Every intermediate result is its own group, and every candidate is one
//! operator over input groups. A candidate outputs its group's schema
//! exactly (`add_candidate` checks it), so a readout whose column names
//! differ from the group's is read through a renaming `Project` group.
//! $\boldsymbol\theta$ lives only in `LogicalProperties::parameters`, never
//! in an operator.
//!
//! Each candidate's first input requirements ([`input_requirements`]) carry
//! only the direct need of its operator: a top-k readout needs a heap of at
//! least `k` items. Pass 1 has no other parameter floor: the consumers that
//! read a state group are readouts, finalizers and merges, whose error
//! propagation is linear, so no validity cap applies to a state group's
//! input (concepts doc, section 4). The metric rule and the pruning caps are
//! decided per edge, after the last Pass 2 rule (Pass 2's logical edges).
//!
//! A measure filter or `HAVING` prunes nothing. The target records the
//! measure filter (applied as `SummaryAgg.filter`) and one [`HavingEntry`]
//! per `HAVING`: the aggregate's own, and each consumer `Filter` group
//! directly above the aggregate group. A key-only predicate is pushed below
//! the aggregate by the candidates that compute the filtered result; any
//! other stays a `Filter` above the output.
//!
//! The input must be a logical memo: a group whose expression is an ASAP
//! operator is rejected. No cost model and no accuracy target is read.
use std::collections::BTreeMap;

use asap_types::ir::operator::non_asap::any_measure_filtered;
use asap_types::ir::operator::operator_properties::{BinaryOpKind, JoinKind, Reduction, Source};
use asap_types::ir::operator::AggIntent;
use asap_types::ir::properties::AccuracyTarget;
use asap_types::ir::scalar::expr_ir::ArithmeticOpKind;
use asap_types::ir::scalar::{resolve_column_ref, ColumnRef, CompareOpKind, ScalarValue};
use asap_types::ir::schema::{
    DataType, EntityIdentity, ExactKind, FieldDataType, GroupingStrategy, NonNegativeWeightProof,
    Schema, SketchAlgorithm, SketchParams, SketchStatistic, StatisticKind, SummaryInputExpr,
    SummaryUpdate, SupportedSummary, WeightDomain,
};
use asap_types::ir::{
    ASAPOp, BinaryOperator, ExprSemantics, NonASAPOp, Operator, OperatorResultKind, Predicate,
    ProjectItem, ScalarExpr, SchemaDerivationError,
};
use asap_types::stages::{
    parameter_list, CandidateError, CandidateExpression, CandidateForMemoGroup, ComputationTarget,
    FusedComputation, HavingEntry, HavingPlacement, HowToComputeAggregateMeasure,
    LogicalProperties, LogicalPropertiesError, LogicalRequirements, Memo, MemoGroupId,
    OriginOfCandidateForMemoGroup,
};
use thiserror::Error;

use asap_accuracy::sizing::{accuracy_budget, sizing_target, sketch_statistic};
use asap_types::deployment_capabilities::summary_supported;

use crate::realization::{
    non_asap, raw_data_samples_of_metric, realize_keyed_additive_summary_input, schema,
    summary_candidates,
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

/// Why Pass 1 could not add a group's candidates.
#[derive(Debug, Error)]
pub enum PerComputationCandidateError {
    #[error(transparent)]
    Structure(#[from] SchemaDerivationError),
    #[error("logical candidate input already contains ASAP operators")]
    ContainsAsap,
    /// `root` (the first query reaching the aggregate) and `measure` locate
    /// the target; `computation_candidates_for_intent` alone leaves them
    /// `None`.
    #[error(
        "approximate accuracy of root {}, measure {} requires epsilon and delta in (0, 1)",
        root.as_deref().unwrap_or("?"),
        measure.map_or_else(|| "?".to_string(), |measure| measure.to_string())
    )]
    InvalidAccuracy {
        root: Option<String>,
        measure: Option<usize>,
    },
    /// A candidate Pass 1 built breaks the node rules or does not output
    /// its group's result: a bug in the rule that built it.
    #[error(transparent)]
    Candidate(#[from] CandidateError),
    /// A candidate Pass 1 built has logical properties that cannot go
    /// together: a bug in the rule that built it.
    #[error(transparent)]
    LogicalProperties(#[from] LogicalPropertiesError),
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
fn nullable_avg_input(input: &Schema, intent: &AggIntent) -> bool {
    let column = match intent.input_cols().first() {
        Some(column) => Some(*column),
        None => resolve_column_ref(&ColumnRef::SampleValue, input).ok(),
    };
    column
        .and_then(|column| input.fields.get(column))
        .is_none_or(|field| field.nullable)
}

/// Whether `having` over an aggregate's output reads only its `by(...)`
/// group keys. A global aggregate (no group keys) never qualifies: its one
/// output row exists even when no input row does, so no row predicate can
/// stand in.
fn key_only_having(reduction: &Reduction, having: &Predicate<MemoGroupId>) -> bool {
    let keys = match reduction {
        Reduction::Reduce(by) if !by.is_without() => by.len(),
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

/// The expression of group `group`.
fn expression(memo: &Memo, group: MemoGroupId) -> &Operator<MemoGroupId> {
    &memo.groups()[group.0 as usize].expression
}

/// Discover one target per measure of each eligible aggregate group of
/// `memo`, nested aggregates and those read by scalar roots included:
/// Pass 1 walks the groups, so an aggregate several queries read is one
/// target per measure. An aggregate measure is eligible when it has a
/// realization besides exact execution, even if every such realization is
/// pruned. The summaries the deployment does not support
/// (`supported_summaries`, the deployment's
/// `DeploymentCapabilities::supported_summaries`; `None` supports every
/// one) are dropped: a target's own, and a fused computation's (with all
/// of them dropped, the fusion is not offered). Measure filters and
/// `HAVING` keep every candidate; the target records how they are applied:
/// the aggregate's own `having`, then each consumer `Filter` group whose
/// input is the aggregate group, key-only predicates below the aggregate.
pub(crate) fn computation_targets(
    memo: &Memo,
    supported_summaries: Option<&[SupportedSummary]>,
) -> Result<Vec<ComputationTarget>, PerComputationCandidateError> {
    if memo
        .groups()
        .iter()
        .any(|group| matches!(group.expression, Operator::ASAP(_)))
    {
        return Err(PerComputationCandidateError::ContainsAsap);
    }
    let mut targets = Vec::new();
    for (index, group) in memo.groups().iter().enumerate() {
        let Operator::NonASAP(NonASAPOp::Aggregate {
            child,
            reduction,
            measures,
            filters,
            having,
            ..
        }) = &group.expression
        else {
            continue;
        };
        let aggregate_group = MemoGroupId(index as u32);
        let mut entries: Vec<HavingEntry> = having
            .iter()
            .map(|having| HavingEntry {
                filter: None,
                predicate: having.clone(),
                placement: placement(key_only_having(reduction, having)),
            })
            .collect();
        // Only a `Filter` whose input is this aggregate is its `HAVING`; one
        // that reads it from a scalar expression stays a plain consumer.
        for &parent in group.parent_groups() {
            if let Operator::NonASAP(NonASAPOp::Filter { child, pred }) = expression(memo, parent) {
                if *child == aggregate_group {
                    entries.push(HavingEntry {
                        filter: Some(parent),
                        predicate: pred.clone(),
                        placement: placement(key_only_having(reduction, pred)),
                    });
                }
            }
        }
        for (measure, intent) in measures.iter().enumerate() {
            let mut choices =
                computation_candidates_for_intent(intent).map_err(|error| match error {
                    PerComputationCandidateError::InvalidAccuracy { .. } => {
                        PerComputationCandidateError::InvalidAccuracy {
                            root: group.reached_by_queries().first().map(usize::to_string),
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
            if matches!(intent, AggIntent::Avg { .. })
                && nullable_avg_input(schema(memo, *child), intent)
            {
                choices.prune(|choice| {
                    matches!(choice, HowToComputeAggregateMeasure::ExactAccumulator(_))
                });
            }
            if matches!(intent, AggIntent::Count { .. })
                && memo.groups()[child.0 as usize].group_properties.result_kind
                    == OperatorResultKind::Relation
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
    add_whole_expression_targets(memo, &mut targets);
    prune_topk_heaps_over_one_row_per_item(memo, &mut targets);
    add_nested_sum_targets(memo, &mut targets);
    close_absorptions(&mut targets);
    without_unsupported_summaries(memo, &mut targets, supported_summaries);
    Ok(targets)
}

/// Drops from `targets` the summary candidates the executor does not
/// support (`supported_summaries`; `None` supports every one): a target's
/// own, and its fused computation's. The readout is the statistic the
/// measure's estimate reads (the outer measure's for a fused computation).
fn without_unsupported_summaries(
    memo: &Memo,
    targets: &mut [ComputationTarget],
    supported_summaries: Option<&[SupportedSummary]>,
) {
    for target in targets.iter_mut() {
        let Operator::NonASAP(NonASAPOp::Aggregate { measures, .. }) =
            expression(memo, target.aggregate_group)
        else {
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
}

/// Adds to each fused computation's `absorbed_targets` the targets its
/// absorbed targets' own fused computations absorb, transitively: the
/// fused computation replaces all their operators, so its builds list all
/// their groups in `fused_groups`.
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
/// inner target. Offered whatever else reads the inner sum: the inner sum's
/// group keeps its own candidates for its other consumers. Applied when
/// neither has a filter or `HAVING` ([`nested_sum_input`]), and when the
/// outer sum is not itself absorbed by a merge and has no fused computation
/// yet, and the inner sum has none, so merges do not stack. An outer sum a
/// whole-expression top-k absorbs (`topk(k, sum by (G)
/// (sum_over_time(m[W])))`) keeps its merge for the top-k's own candidates.
fn add_nested_sum_targets(memo: &Memo, targets: &mut [ComputationTarget]) {
    // The targets a merge absorbs.
    let mut absorbed: Vec<usize> = Vec::new();
    for t in 0..targets.len() {
        let target = &targets[t];
        if absorbed.contains(&t)
            || target.fused_computation.is_some()
            || !target.having_filters.is_empty()
        {
            continue;
        }
        let Operator::NonASAP(NonASAPOp::Aggregate { child, .. }) =
            expression(memo, target.aggregate_group)
        else {
            continue;
        };
        let Some(inner) = targets
            .iter()
            .position(|other| other.aggregate_group == *child)
        else {
            continue;
        };
        // Merges do not stack: the inner sum must not have a fused
        // computation of its own.
        if !targets[inner].having_filters.is_empty()
            || targets[inner].fused_computation.is_some()
            || nested_sum_input(memo, target.aggregate_group).is_none()
        {
            continue;
        }
        absorbed.push(inner);
        let merge = HowToComputeAggregateMeasure::ExactAccumulator(vec![ExactKind::Sum]);
        targets[t].fused_computation = Some(FusedComputation {
            absorbed_targets: vec![inner],
            ways_to_compute: vec![merge],
        });
    }
}

/// The input rows, the inner sum and the outer grouping (moved onto the
/// input rows by column, [`Reduction::partitions_over`]) of a nested-sum
/// merge of `target`, or `None` when it does not apply: `target` is a
/// grouped `sum` (not by `without`) of the value column of a per-series or
/// grouped `sum` directly beneath it, neither filtered nor with `HAVING`,
/// and not grouped by the metric name a per-series inner sum drops.
fn nested_sum_input(
    memo: &Memo,
    target: MemoGroupId,
) -> Option<(MemoGroupId, AggIntent, Reduction)> {
    let Some(NonASAPOp::Aggregate {
        child: inner,
        reduction: reduction @ Reduction::Reduce(_),
        measures,
        filters,
        having: None,
        ..
    }) = non_asap(memo, target)
    else {
        return None;
    };
    let Reduction::Reduce(by) = &reduction else {
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
    }) = non_asap(memo, inner)
    else {
        return None;
    };
    let [inner_sum @ AggIntent::Sum { .. }] = inner_measures.as_slice() else {
        return None;
    };
    if by.is_without() || any_measure_filtered(&filters) || any_measure_filtered(&inner_filters) {
        return None;
    }
    // The inner sum's value column: a per-series sum keeps its labels and
    // carries the value as the sample value; a grouped one ends with it.
    let value = match &inner_reduction {
        Reduction::PerEntity => {
            resolve_column_ref(&ColumnRef::SampleValue, schema(memo, inner)).ok()?
        }
        Reduction::Reduce(keys) if !keys.is_without() => keys.len(),
        Reduction::Reduce(_) => return None,
    };
    let summed = match col {
        None => resolve_column_ref(&ColumnRef::SampleValue, schema(memo, inner)).ok()?,
        Some(column) => *column,
    };
    if summed != value || by.keys().contains(&value) {
        return None;
    }
    // A per-series sum drops the metric name, which the raw rows keep:
    // grouping them by it would name groups the nested sums leave unnamed.
    if matches!(inner_reduction, Reduction::PerEntity)
        && by.keys().iter().any(|&key| {
            schema(memo, inner)
                .fields
                .get(key)
                .is_some_and(|f| f.name == "__name__")
        })
    {
        return None;
    }
    let partitions = reduction.partitions_over(schema(memo, inner), schema(memo, rows))?;
    Some((rows, inner_sum.clone(), partitions))
}

/// Whole-expression top-k (Example 1's Q2): a top-k over a per-item `sum`
/// or `count` that is itself a target gets a fused computation
/// ([`ComputationTarget::fused_computation`]) absorbing the inner target,
/// whose candidates are the top-k's heap summaries (Count-Min and
/// CountSketch with a heap), each one heap over the inner aggregate's
/// input, keyed by the ranked item and weighted by the summed value. Those
/// heaps over the exact per-item result leave the top-k's own candidates:
/// the fused computation replaces them. Over a grouped sum of per-series
/// sums or counts of raw data samples, the heap reads those samples and
/// absorbs the per-series target too. Offered whatever else reads the inner
/// aggregate: the absorbed groups keep their own candidates for their other
/// consumers. Applied when neither has a filter or `HAVING`, and when the
/// keyed-additive rule realizes it ([`whole_expression_input`]).
fn add_whole_expression_targets(memo: &Memo, targets: &mut [ComputationTarget]) {
    for t in 0..targets.len() {
        let target = &targets[t];
        let Operator::NonASAP(NonASAPOp::Aggregate { child, .. }) =
            expression(memo, target.aggregate_group)
        else {
            continue;
        };
        let Some(inner) = targets
            .iter()
            .position(|other| other.aggregate_group == *child)
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
        let Some((input, ..)) = whole_expression_input(memo, target.aggregate_group) else {
            continue;
        };
        // A heap over the raw data samples beneath a grouped sum of per-series
        // sums or counts also absorbs the per-series target.
        let mut absorbs = vec![inner];
        if let Operator::NonASAP(NonASAPOp::Aggregate {
            child: per_series, ..
        }) = expression(memo, targets[inner].aggregate_group)
        {
            let reads_below = expression(memo, *per_series).children().contains(&&input);
            if reads_below {
                absorbs.extend(
                    targets
                        .iter()
                        .position(|other| other.aggregate_group == *per_series),
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
fn prune_topk_heaps_over_one_row_per_item(memo: &Memo, targets: &mut [ComputationTarget]) {
    for target in targets.iter_mut() {
        let Operator::NonASAP(NonASAPOp::Aggregate {
            measures, child, ..
        }) = expression(memo, target.aggregate_group)
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
        if !reads_only_time_series(memo, *child) {
            continue;
        }
        prune_own(&mut target.ways_to_compute, |candidate| {
            matches!(candidate, HowToComputeAggregateMeasure::Summary(family)
                if family.capability().answers(StatisticKind::TopK))
        });
    }
}

/// Whether every scan below `input` reads a time series (PromQL rows).
fn reads_only_time_series(memo: &Memo, input: MemoGroupId) -> bool {
    match expression(memo, input) {
        Operator::NonASAP(NonASAPOp::Scan { source, .. }) => {
            matches!(source, Source::TimeSeries { .. })
        }
        operator => operator
            .children()
            .into_iter()
            .all(|&child| reads_only_time_series(memo, child)),
    }
}

/// The input, update and partitions of a whole-expression top-k over
/// `target`'s inner aggregate, by the keyed-additive rule, or `None` when it
/// does not apply. Rows that carry the full series identity rank it as a
/// column, as [`summary_update`] does. The partitions are `target`'s, moved
/// onto the input the heap reads ([`Reduction::partitions_over`]).
fn whole_expression_input(
    memo: &Memo,
    target: MemoGroupId,
) -> Option<(MemoGroupId, SummaryUpdate, Reduction)> {
    let Some(NonASAPOp::Aggregate {
        child,
        reduction,
        measures,
        filters,
        having: None,
        ..
    }) = non_asap(memo, target)
    else {
        return None;
    };
    let [intent @ AggIntent::TopK { .. }] = measures.as_slice() else {
        return None;
    };
    if any_measure_filtered(&filters) {
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
    let realized = realize_keyed_additive_summary_input(memo, intent, &family, &reduction, child)?;
    let mut input = realized.input;
    let per_series = matches!(
        non_asap(memo, child),
        Some(NonASAPOp::Aggregate {
            reduction: Reduction::PerEntity,
            ..
        })
    );
    let partitions =
        reduction.partitions_over(schema(memo, child), schema(memo, realized.child))?;
    if per_series && schema(memo, realized.child).has_promql_series_identity() {
        input.item = Some(asap_types::ir::schema::topk::series_identity_item(
            schema(memo, realized.child),
            partitions.group_keys().map_or(&[][..], |keys| keys.keys()),
        ));
    } else if schema(memo, realized.child).closed
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

/// Adds Pass 1's candidates to `memo` (Stage 0's, without candidates):
/// every group's own expression, then, for each aggregate group with
/// `targets`, each way to compute it. A consumer `Filter` group whose
/// key-only `HAVING` is pushed below the aggregate gets the candidates that
/// compute its filtered rows. Every group and candidate is added through
/// `Memo::find_or_add_group` and `Memo::add_candidate`.
pub(crate) fn add_pass1_candidates(
    memo: &mut Memo,
    targets: &[ComputationTarget],
) -> Result<(), PerComputationCandidateError> {
    for index in 0..memo.groups().len() {
        let group = MemoGroupId(index as u32);
        let own = expression(memo, group).clone();
        add_candidate(memo, group, own, None, Vec::new())?;
    }
    let mut targets_of: BTreeMap<MemoGroupId, Vec<&ComputationTarget>> = BTreeMap::new();
    for target in targets {
        targets_of
            .entry(target.aggregate_group)
            .or_default()
            .push(target);
    }
    for (&aggregate_group, group_targets) in &targets_of {
        let entries = &group_targets[0].having_filters;
        let own = |placement: HavingPlacement| -> Vec<Predicate<MemoGroupId>> {
            entries
                .iter()
                .filter(|entry| entry.filter.is_none() && entry.placement == placement)
                .map(|entry| entry.predicate.clone())
                .collect()
        };
        let (below, above) = (
            own(HavingPlacement::BelowAggregate),
            own(HavingPlacement::AboveOutput),
        );
        let ways = AggregateWays {
            aggregate_group,
            result_group: aggregate_group,
            below: &below,
            above: &above,
            targets: group_targets,
            all_targets: targets,
        };
        add_aggregate_candidates(memo, &ways)?;
        for entry in entries {
            if let (Some(filter), HavingPlacement::BelowAggregate) = (entry.filter, entry.placement)
            {
                let mut with_consumer = below.clone();
                with_consumer.push(entry.predicate.clone());
                let filtered = AggregateWays {
                    result_group: filter,
                    below: &with_consumer,
                    ..ways
                };
                add_aggregate_candidates(memo, &filtered)?;
            }
        }
    }
    Ok(())
}

/// The ways to compute one result from an aggregate group's targets.
struct AggregateWays<'a> {
    /// The aggregate group whose measures are computed.
    aggregate_group: MemoGroupId,
    /// The group the candidates compute: the aggregate group, or a consumer
    /// `Filter` group above it whose key-only `HAVING` goes below.
    result_group: MemoGroupId,
    /// Key-only predicates over the aggregate's output, applied to its
    /// input rows (their key columns renumbered to the input's).
    below: &'a [Predicate<MemoGroupId>],
    /// Predicates applied as `Filter`s above the output.
    above: &'a [Predicate<MemoGroupId>],
    /// The aggregate's targets, one per measure that has one.
    targets: &'a [&'a ComputationTarget],
    /// Every target, for the groups a fused computation absorbs.
    all_targets: &'a [ComputationTarget],
}

/// One measure of an aggregate, computed on its own over `input`.
struct SingleMeasure<'a> {
    input: MemoGroupId,
    reduction: &'a Reduction,
    intent: &'a AggIntent,
    filter: Option<&'a Predicate<MemoGroupId>>,
    /// The metric whose raw data samples, or plain sums of them, are every
    /// value the aggregate reads: a heap-sketch weight read from them
    /// carries the open requirement `WeightDomain::RawDataSamplesOfMetric`.
    raw_data_samples_of: Option<&'a str>,
}

/// Adds `ways.result_group`'s candidates that compute it from
/// `ways.aggregate_group`'s measures: for a single measure, each of its
/// target's ways and fused ways; for several, one candidate joining the
/// measures' own groups (a single-measure aggregate each, with its own
/// candidates) on the group keys.
fn add_aggregate_candidates(
    memo: &mut Memo,
    ways: &AggregateWays<'_>,
) -> Result<(), PerComputationCandidateError> {
    let Operator::NonASAP(NonASAPOp::Aggregate {
        child,
        reduction,
        measures,
        output_names,
        filters,
        ..
    }) = expression(memo, ways.aggregate_group).clone()
    else {
        return Err(PerComputationCandidateError::Unsupported(
            "target is not an aggregate",
        ));
    };
    let keys: Vec<usize> = reduction
        .group_keys()
        .map_or_else(Vec::new, |by| by.keys().to_vec());
    let mut input = child;
    for predicate in ways.below {
        let pred = Predicate(predicate.0.map_columns(&mut |column| keys[column]));
        input = add_group(
            memo,
            Operator::NonASAP(NonASAPOp::Filter { pred, child: input }),
        )?;
    }
    let raw_data_samples = raw_data_samples_of_metric(memo, child);
    let measure = |index: usize| SingleMeasure {
        input,
        reduction: &reduction,
        intent: &measures[index],
        filter: filters.get(index).and_then(Option::as_ref),
        raw_data_samples_of: raw_data_samples.as_deref(),
    };
    let target_of = |index: usize| {
        ways.targets
            .iter()
            .find(|target| target.measure_index == index)
    };
    if measures.len() == 1 {
        let Some(target) = target_of(0) else {
            return Ok(());
        };
        for way in &target.ways_to_compute {
            if *way == HowToComputeAggregateMeasure::Exact {
                continue;
            }
            unless_unsupported(
                measure_output(memo, &measure(0), way, ways.result_group).and_then(|output| {
                    add_output_candidate(memo, ways.result_group, output, ways.above)
                }),
            )?;
        }
        if let Some(fused) = &target.fused_computation {
            let absorbed_groups: Vec<MemoGroupId> = fused
                .absorbed_targets
                .iter()
                .map(|&absorbed| ways.all_targets[absorbed].aggregate_group)
                .collect();
            for way in &fused.ways_to_compute {
                unless_unsupported(add_fused_candidate(
                    memo,
                    ways,
                    &measure(0),
                    way,
                    &absorbed_groups,
                ))?;
            }
        }
        return Ok(());
    }
    // The measures are joined back on their group keys: SQL rows grouped
    // `by` keys (PromQL aggregates have one measure each).
    if !matches!(&reduction, Reduction::Reduce(by) if !by.is_without())
        || memo.groups()[input.0 as usize].group_properties.result_kind
            != OperatorResultKind::Relation
    {
        return Ok(());
    }
    let mut parts = Vec::with_capacity(measures.len());
    for (index, intent) in measures.iter().enumerate() {
        let part = add_group(
            memo,
            Operator::NonASAP(NonASAPOp::Aggregate {
                child: input,
                reduction: reduction.clone(),
                measures: vec![intent.clone()],
                output_names: vec![output_names.get(index).cloned().unwrap_or_default()],
                filters: vec![filters.get(index).cloned().flatten()],
                having: None,
            }),
        )?;
        if let Some(target) = target_of(index) {
            for way in &target.ways_to_compute {
                if *way == HowToComputeAggregateMeasure::Exact {
                    continue;
                }
                unless_unsupported(
                    measure_output(memo, &measure(index), way, part)
                        .and_then(|output| add_output_candidate(memo, part, output, &[])),
                )?;
            }
        }
        parts.push(part);
    }
    let output = join_measures(memo, &reduction, ways.result_group, parts)?;
    add_output_candidate(memo, ways.result_group, output, ways.above)
}

/// `added`, with a way Pass 1 cannot build (`Unsupported`) not offered:
/// its checks run before the groups it reads are added, so the memo is
/// unchanged, except for the rare checks that need a group's schema (a
/// top-k estimate without its item columns, a per-series `avg` that is not
/// Float64), which leave groups no query reaches.
fn unless_unsupported(
    added: Result<(), PerComputationCandidateError>,
) -> Result<(), PerComputationCandidateError> {
    match added {
        Err(PerComputationCandidateError::Unsupported(_)) => Ok(()),
        added => added,
    }
}

/// Adds the fused computation `way` of `ways`' single measure: a whole-
/// expression heap (a summary way) or a nested-sum merge (an exact
/// accumulator way). Its builds list `absorbed_groups`, the groups of the
/// operators it replaces, in `fused_groups`.
fn add_fused_candidate(
    memo: &mut Memo,
    ways: &AggregateWays<'_>,
    measure: &SingleMeasure<'_>,
    way: &HowToComputeAggregateMeasure,
    absorbed_groups: &[MemoGroupId],
) -> Result<(), PerComputationCandidateError> {
    match way {
        HowToComputeAggregateMeasure::Summary(algorithm) => {
            let (rows, update, partitions) = whole_expression_input(memo, ways.aggregate_group)
                .ok_or(PerComputationCandidateError::Unsupported(
                    "whole-expression top-k input",
                ))?;
            let query = sketch_statistic(measure.intent).ok_or(
                PerComputationCandidateError::Unsupported("sketch evaluation"),
            )?;
            let state = add_state_group(
                memo,
                Operator::ASAP(ASAPOp::SummaryAgg {
                    child: rows,
                    input: with_raw_data_samples_requirement(update, measure.raw_data_samples_of),
                    family: FieldDataType::Sketch(*algorithm, GroupingStrategy::default()),
                    reduction: partitions,
                    grouping: GroupingStrategy::default(),
                    filter: None,
                }),
                absorbed_groups,
            )?;
            let estimate = add_group(
                memo,
                Operator::ASAP(ASAPOp::SummaryEstimate {
                    summary_input: state,
                    query,
                }),
            )?;
            let output = ranked_as_topk(memo, measure.reduction, ways.result_group, estimate)?;
            add_candidate(memo, ways.result_group, output, None, Vec::new())?;
        }
        HowToComputeAggregateMeasure::ExactAccumulator(_) => {
            // The merged build computes the outer sum's own accumulator
            // state, from the rows below the inner sum: a candidate of that
            // state group (#1203, fused builds).
            let (rows, inner_sum, partitions) = nested_sum_input(memo, ways.aggregate_group)
                .ok_or(PerComputationCandidateError::Unsupported(
                    "nested-sum merge input",
                ))?;
            let outer_state = accumulator_state(memo, ExactKind::Sum, measure)?;
            let merged =
                exact_accumulator_build(memo, ExactKind::Sum, &inner_sum, rows, partitions)?;
            add_candidate(memo, outer_state, merged, None, absorbed_groups.to_vec())?;
        }
        HowToComputeAggregateMeasure::Exact => {
            return Err(PerComputationCandidateError::Unsupported(
                "plain exact execution of a fused computation",
            ))
        }
    }
    Ok(())
}

/// The operator that computes `measure` by `way` (not `Exact`) for
/// `result_group`: its value after its group keys, its column names not
/// yet the group's ([`add_output_candidate`] renames them). Every group it
/// reads is added with its candidates.
fn measure_output(
    memo: &mut Memo,
    measure: &SingleMeasure<'_>,
    way: &HowToComputeAggregateMeasure,
    result_group: MemoGroupId,
) -> Result<Operator<MemoGroupId>, PerComputationCandidateError> {
    let finalized = |memo: &mut Memo, kind: ExactKind, measure: &SingleMeasure<'_>| {
        let state = accumulator_state(memo, kind, measure)?;
        Ok::<_, PerComputationCandidateError>(Operator::ASAP(ASAPOp::FinalizeExactAccumulator {
            child: state,
        }))
    };
    Ok(match way {
        HowToComputeAggregateMeasure::Exact => {
            return Err(PerComputationCandidateError::Unsupported(
                "plain exact execution is the group's own expression",
            ))
        }
        HowToComputeAggregateMeasure::ExactAccumulator(kinds) if kinds.len() == 1 => {
            finalized(memo, kinds[0], measure)?
        }
        HowToComputeAggregateMeasure::Summary(algorithm) => {
            let query = sketch_statistic(measure.intent).ok_or(
                PerComputationCandidateError::Unsupported("sketch evaluation"),
            )?;
            let family = FieldDataType::Sketch(*algorithm, GroupingStrategy::default());
            let read = measure_input(measure.intent, memo, measure.input, measure.filter)?;
            let update = with_raw_data_samples_requirement(
                summary_update(
                    measure.intent,
                    &family,
                    measure.reduction,
                    schema(memo, measure.input),
                    read,
                )?,
                measure.raw_data_samples_of,
            );
            let state = add_state_group(
                memo,
                Operator::ASAP(ASAPOp::SummaryAgg {
                    child: measure.input,
                    input: update,
                    family,
                    reduction: measure.reduction.clone(),
                    grouping: GroupingStrategy::default(),
                    filter: measure.filter.cloned(),
                }),
                &[],
            )?;
            let estimate = Operator::ASAP(ASAPOp::SummaryEstimate {
                summary_input: state,
                query,
            });
            match measure.intent {
                AggIntent::TopK { .. } => {
                    let estimate = add_group(memo, estimate)?;
                    ranked_as_topk(memo, measure.reduction, result_group, estimate)?
                }
                _ => estimate,
            }
        }
        HowToComputeAggregateMeasure::ExactAccumulator(kinds) => {
            let (AggIntent::Avg { col }, [ExactKind::Sum, ExactKind::Count]) =
                (measure.intent, kinds.as_slice())
            else {
                return Err(PerComputationCandidateError::Unsupported(
                    "several exact states other than avg's sum then count",
                ));
            };
            // The count counts the averaged column, as `avg` does: it is
            // non-null (a nullable input prunes this candidate), so it counts
            // the rows the sum reads.
            let sum_intent = AggIntent::Sum { col: *col };
            let count_intent = AggIntent::Count {
                accuracy: AccuracyTarget::exact(),
            };
            let read = measure_input(&sum_intent, memo, measure.input, measure.filter)?;
            let sum = finalized_with(memo, ExactKind::Sum, &sum_intent, measure, read.clone())?;
            let count = finalized_with(memo, ExactKind::Count, &count_intent, measure, read)?;
            let sum = add_group(memo, sum)?;
            let count = add_group(memo, count)?;
            if memo.groups()[measure.input.0 as usize]
                .group_properties
                .result_kind
                == OperatorResultKind::Relation
            {
                return sql_avg(
                    memo,
                    measure.reduction,
                    measure.filter.is_some(),
                    sum,
                    count,
                );
            }
            // `avg` is Float64 whatever the summed type, as `Avg`'s output is,
            // and a PromQL binary operator reads Float64 sample values. A
            // grouped sum or count ends with its value, cast when not Float64
            // (a count is Int64). A per-series sum keeps the input's labels,
            // so its value is the sample-value column; a per-series count is
            // Float64.
            let (sum, count) = match measure.reduction {
                Reduction::Reduce(by) => (
                    last_as_float64(memo, by.len(), sum)?,
                    last_as_float64(memo, by.len(), count)?,
                ),
                Reduction::PerEntity => {
                    let sum_schema = schema(memo, sum);
                    let value = resolve_column_ref(&ColumnRef::SampleValue, sum_schema)
                        .ok()
                        .and_then(|column| sum_schema.fields.get(column));
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
            Operator::NonASAP(NonASAPOp::BinaryOp {
                operator: BinaryOperator {
                    checked_relative_division: false,
                    // A per-series window may hold no finite sum to divide.
                    checked_finite_division: matches!(measure.reduction, Reduction::PerEntity),
                    kind: BinaryOpKind::Arithmetic(ArithmeticOpKind::Div),
                    vector_match: None,
                },
                return_bool: false,
                lhs: sum,
                rhs: count,
            })
        }
    })
}

/// The accumulator-state group of `kind` for `measure` (its own intent and
/// input), with its build candidate.
fn accumulator_state(
    memo: &mut Memo,
    kind: ExactKind,
    measure: &SingleMeasure<'_>,
) -> Result<MemoGroupId, PerComputationCandidateError> {
    let read = measure_input(measure.intent, memo, measure.input, measure.filter)?;
    let build = accumulator_build(memo, kind, measure.intent, measure, read)?;
    add_state_group(memo, build, &[])
}

/// `FinalizeExactAccumulator` over the accumulator state of `kind` for
/// `intent` (one of `measure`'s states, such as `avg`'s `sum`), reading
/// `read` from `measure`'s input.
fn finalized_with(
    memo: &mut Memo,
    kind: ExactKind,
    intent: &AggIntent,
    measure: &SingleMeasure<'_>,
    read: SummaryInputExpr,
) -> Result<Operator<MemoGroupId>, PerComputationCandidateError> {
    let build = accumulator_build(memo, kind, intent, measure, read)?;
    let state = add_state_group(memo, build, &[])?;
    Ok(Operator::ASAP(ASAPOp::FinalizeExactAccumulator {
        child: state,
    }))
}

/// The accumulator build of `kind` for `intent` over `measure`'s input,
/// filtered and grouped as `measure`.
fn accumulator_build(
    memo: &Memo,
    kind: ExactKind,
    intent: &AggIntent,
    measure: &SingleMeasure<'_>,
    read: SummaryInputExpr,
) -> Result<Operator<MemoGroupId>, PerComputationCandidateError> {
    let family = FieldDataType::exact(kind);
    Ok(Operator::ASAP(ASAPOp::SummaryAgg {
        input: summary_update(
            intent,
            &family,
            measure.reduction,
            schema(memo, measure.input),
            read,
        )?,
        child: measure.input,
        family,
        reduction: measure.reduction.clone(),
        grouping: GroupingStrategy::default(),
        filter: measure.filter.cloned(),
    }))
}

/// An unfiltered exact accumulator build of `kind` for `intent` over
/// `input`, grouped by `reduction`.
fn exact_accumulator_build(
    memo: &Memo,
    kind: ExactKind,
    intent: &AggIntent,
    input: MemoGroupId,
    reduction: Reduction,
) -> Result<Operator<MemoGroupId>, PerComputationCandidateError> {
    let family = FieldDataType::exact(kind);
    let read = measure_input(intent, memo, input, None)?;
    Ok(Operator::ASAP(ASAPOp::SummaryAgg {
        input: summary_update(intent, &family, &reduction, schema(memo, input), read)?,
        child: input,
        family,
        reduction,
        grouping: GroupingStrategy::default(),
        filter: None,
    }))
}

/// Adds `output` as a candidate of `result_group`, then the `above` filters
/// over it. When its column names differ from the group's,
/// `output` becomes its own group, read through a `Project` that names its
/// columns as the group does ([`renaming_project`]). With `above`
/// predicates the output (named as the group) is a group, read by one
/// `Filter` group per predicate but the last, whose `Filter` is the
/// candidate.
fn add_output_candidate(
    memo: &mut Memo,
    result_group: MemoGroupId,
    output: Operator<MemoGroupId>,
    above: &[Predicate<MemoGroupId>],
) -> Result<(), PerComputationCandidateError> {
    let Some((last, filters)) = above.split_last() else {
        match add_candidate(memo, result_group, output.clone(), None, Vec::new()) {
            Err(PerComputationCandidateError::Candidate(
                CandidateError::OutputDiffersFromGroup { .. },
            )) => {}
            added => return added.map(|_| ()),
        }
        let output = add_group(memo, output)?;
        let renamed = renaming_project(memo, output, result_group)?;
        add_candidate(memo, result_group, renamed, None, Vec::new())?;
        return Ok(());
    };
    let mut named = add_group(memo, output)?;
    if schema(memo, named) != schema(memo, result_group) {
        let renamed = renaming_project(memo, named, result_group)?;
        named = add_group(memo, renamed)?;
    }
    for pred in filters {
        named = add_group(
            memo,
            Operator::NonASAP(NonASAPOp::Filter {
                pred: pred.clone(),
                child: named,
            }),
        )?;
    }
    add_candidate(
        memo,
        result_group,
        Operator::NonASAP(NonASAPOp::Filter {
            pred: last.clone(),
            child: named,
        }),
        None,
        Vec::new(),
    )?;
    Ok(())
}

/// A `Project` over `output` that names its columns as `result_group`'s. `output` must have the group's number of columns,
/// and the group's schema must be closed (list every column its rows carry).
fn renaming_project(
    memo: &Memo,
    output: MemoGroupId,
    result_group: MemoGroupId,
) -> Result<Operator<MemoGroupId>, PerComputationCandidateError> {
    let (from, to) = (schema(memo, output), schema(memo, result_group));
    // An open schema's rows carry columns it does not list (a PromQL
    // series whose identity is not realized as a column): a `Project` lists
    // its columns, so it would drop them, and its rows are not the group's.
    if !to.closed {
        return Err(PerComputationCandidateError::Unsupported(
            "a rename drops the columns an open schema does not list",
        ));
    }
    if from.fields.len() != to.fields.len() {
        return Err(PerComputationCandidateError::Unsupported(
            "substituted output with other columns than the aggregate",
        ));
    }
    let cols = to
        .fields
        .iter()
        .enumerate()
        .map(|(column, field)| ProjectItem {
            alias: Some(field.name.clone()),
            expr: ScalarExpr::Column(column),
        })
        .collect();
    Ok(Operator::NonASAP(NonASAPOp::Project {
        cols,
        child: output,
    }))
}

/// A heap's top-k `estimate` group (`[partitions.., items.., topk]`, one
/// row per ranked item) in `result_group`'s columns, as a `Project`: its
/// `[keys.., topk_k]`, with `topk_k` the estimated Float64 weight, then the
/// item columns. This is the exact top-k's physical shape (its ranked rows'
/// value, then the columns naming each ranked item: PromQL labels, SQL
/// group keys), so consumers of `[keys.., topk_k]` find those columns where
/// they were.
fn ranked_as_topk(
    memo: &Memo,
    reduction: &Reduction,
    result_group: MemoGroupId,
    estimate: MemoGroupId,
) -> Result<Operator<MemoGroupId>, PerComputationCandidateError> {
    let ranked_index = reduction.group_keys().map_or(0, |keys| keys.len());
    let fields = &schema(memo, estimate).fields;
    let estimated = fields.len() - 1;
    let to = schema(memo, result_group);
    let cols = to
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
            // The estimated weight is Float64; an exact top-k of a SQL
            // count ranks Int64 values, so its column is cast to them.
            let expr = match (&fields[source].dtype, &field.dtype) {
                (FieldDataType::Plain(from), FieldDataType::Plain(to)) if from != to => {
                    ScalarExpr::Cast {
                        expr: Box::new(ScalarExpr::Column(source)),
                        to: to.clone(),
                        try_cast: false,
                    }
                }
                _ => ScalarExpr::Column(source),
            };
            Ok(ProjectItem {
                alias: Some(field.name.clone()),
                expr,
            })
        })
        .collect::<Result<Vec<_>, PerComputationCandidateError>>()?;
    Ok(Operator::NonASAP(NonASAPOp::Project {
        cols,
        child: estimate,
    }))
}

/// The group of `operator`, whatever adds it, with `operator` as its
/// candidate: an intermediate result is its own group.
fn add_group(
    memo: &mut Memo,
    operator: Operator<MemoGroupId>,
) -> Result<MemoGroupId, PerComputationCandidateError> {
    let group = memo.find_or_add_group(operator.clone())?;
    add_candidate(memo, group, operator, None, Vec::new())?;
    Ok(group)
}

/// The state group of `build` (a `SummaryAgg`) with its build candidates:
/// one per configuration of a sketch family's predefined list
/// (`parameter_list`), or the one exact accumulator build. Each lists
/// `fused_groups`.
fn add_state_group(
    memo: &mut Memo,
    build: Operator<MemoGroupId>,
    fused_groups: &[MemoGroupId],
) -> Result<MemoGroupId, PerComputationCandidateError> {
    let group = memo.find_or_add_group(build.clone())?;
    let configurations: Vec<Option<SketchParams>> = match &build {
        Operator::ASAP(ASAPOp::SummaryAgg {
            family: FieldDataType::Sketch(algorithm, _),
            ..
        }) => parameter_list(*algorithm)
            .iter()
            .cloned()
            .map(Some)
            .collect(),
        _ => vec![None],
    };
    for parameters in configurations {
        add_candidate(
            memo,
            group,
            build.clone(),
            parameters,
            fused_groups.to_vec(),
        )?;
    }
    Ok(group)
}

/// Adds the candidate `operator` with `parameters` to `group`, with its
/// first input requirements ([`input_requirements`]).
fn add_candidate(
    memo: &mut Memo,
    group: MemoGroupId,
    operator: Operator<MemoGroupId>,
    parameters: Option<SketchParams>,
    fused_groups: Vec<MemoGroupId>,
) -> Result<(), PerComputationCandidateError> {
    let expression = CandidateExpression { operator };
    let logical_properties = LogicalProperties::new(&expression, parameters, None)?;
    let input_requirements = input_requirements(memo, &expression.operator);
    memo.add_candidate(
        group,
        CandidateForMemoGroup {
            expression,
            logical_properties,
            input_requirements,
            fused_groups,
            origin: OriginOfCandidateForMemoGroup::Pass1,
        },
    )?;
    Ok(())
}

/// $r^{\log}_{c,i}$ of a Pass 1 candidate `operator`, one per input group:
/// a top-k readout of a heap state needs a heap of at least `k` items (the
/// weakest listed width and depth, `heap_size = k`); every other input has
/// no parameter floor. Pass 1's consumers of state groups propagate error
/// linearly, so no validity cap sets a floor here (concepts doc, section 4).
pub(crate) fn input_requirements(
    memo: &Memo,
    operator: &Operator<MemoGroupId>,
) -> Vec<LogicalRequirements> {
    let floor = match operator {
        Operator::ASAP(ASAPOp::SummaryEstimate {
            summary_input,
            query: SketchStatistic::TopK { k },
        }) => heap_floor(expression(memo, *summary_input), *k),
        _ => None,
    };
    operator
        .children()
        .into_iter()
        .enumerate()
        .map(|(index, _)| LogicalRequirements::new(if index == 0 { floor.clone() } else { None }))
        .collect()
}

/// The parameter floor of a top-k readout of `k` items over a heap state
/// `build`: the family's weakest listed width and depth with
/// `heap_size = k`. `None` when `build` is no heap family's build.
fn heap_floor(build: &Operator<MemoGroupId>, k: usize) -> Option<SketchParams> {
    let Operator::ASAP(ASAPOp::SummaryAgg {
        family: FieldDataType::Sketch(algorithm, _),
        ..
    }) = build
    else {
        return None;
    };
    let heap_size = u32::try_from(k).unwrap_or(u32::MAX);
    match parameter_list(*algorithm).first()? {
        SketchParams::CmsWithHeap { width, depth, .. } => Some(SketchParams::CmsWithHeap {
            width: *width,
            depth: *depth,
            heap_size,
        }),
        SketchParams::CountSketchWithHeap { width, depth, .. } => {
            Some(SketchParams::CountSketchWithHeap {
                width: *width,
                depth: *depth,
                heap_size,
            })
        }
        _ => None,
    }
}

/// `group`, a grouped aggregate's `keys` group keys and then its value,
/// with the value cast to Float64 unless it already is one (a `Project`
/// group over it).
fn last_as_float64(
    memo: &mut Memo,
    keys: usize,
    group: MemoGroupId,
) -> Result<MemoGroupId, PerComputationCandidateError> {
    if schema(memo, group).fields.last().map(|field| &field.dtype)
        == Some(&FieldDataType::Plain(DataType::Float64))
    {
        return Ok(group);
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
    add_group(
        memo,
        Operator::NonASAP(NonASAPOp::Project { cols, child: group }),
    )
}

/// SQL `avg` from its finalized `sum` and `count` groups: joined on the
/// group keys (NULL keys match), then `sum / count` as Float64. A group
/// with no row passing the measure filter, or an empty global input, counts
/// 0 and its `avg` is NULL.
fn sql_avg(
    memo: &mut Memo,
    reduction: &Reduction,
    filtered: bool,
    sum: MemoGroupId,
    count: MemoGroupId,
) -> Result<Operator<MemoGroupId>, PerComputationCandidateError> {
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
    let joined = add_group(memo, join_on_keys(memo, keys, sum, count))?;
    Ok(Operator::NonASAP(NonASAPOp::Project {
        cols,
        child: joined,
    }))
}

/// `left` joined with `right` on their leading `keys` columns; NULL keys
/// match each other, as grouping treats them.
fn join_on_keys(
    memo: &Memo,
    keys: usize,
    left: MemoGroupId,
    right: MemoGroupId,
) -> Operator<MemoGroupId> {
    let width = schema(memo, left).fields.len();
    let right_schema = schema(memo, right);
    let column = |id| Box::new(ScalarExpr::Column(id));
    let matches = (0..keys)
        .map(|key| {
            let equal = ScalarExpr::Compare {
                left: column(key),
                op: CompareOpKind::Eq,
                right: column(width + key),
                semantics: ExprSemantics::Sql,
            };
            if right_schema.fields[key].nullable {
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
    Operator::NonASAP(NonASAPOp::Join {
        kind,
        pred: Predicate(pred),
        left,
        right,
    })
}

/// Joins single-measure `parts` (each a group: group keys, then one value)
/// on their group keys, and projects `result_group`'s output columns. Every
/// part has the same groups, so an inner join keeps them all; NULL keys
/// match each other, as grouping treats them.
fn join_measures(
    memo: &mut Memo,
    reduction: &Reduction,
    result_group: MemoGroupId,
    parts: Vec<MemoGroupId>,
) -> Result<Operator<MemoGroupId>, PerComputationCandidateError> {
    let Reduction::Reduce(by) = reduction else {
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
        let join = join_on_keys(memo, keys, joined, part);
        joined = add_group(memo, join)?;
    }
    let to = schema(memo, result_group);
    let mut cols = key_items(keys);
    cols.extend((0..to.fields.len() - keys).map(|measure| ProjectItem {
        alias: None,
        expr: ScalarExpr::Column(measure * width + keys),
    }));
    for (item, field) in cols.iter_mut().zip(&to.fields) {
        item.alias = Some(field.name.clone());
    }
    Ok(Operator::NonASAP(NonASAPOp::Project {
        cols,
        child: joined,
    }))
}

fn key_items(keys: usize) -> Vec<ProjectItem<MemoGroupId>> {
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
    memo: &Memo,
    input: MemoGroupId,
    filter: Option<&Predicate<MemoGroupId>>,
) -> Result<SummaryInputExpr, PerComputationCandidateError> {
    let schema = schema(memo, input);
    let read = match intent {
        AggIntent::Count { .. }
            if memo.groups()[input.0 as usize].group_properties.result_kind
                == OperatorResultKind::Relation =>
        {
            SummaryInputExpr::Column(counted_column(schema, filter)?)
        }
        // A top-k ranks its child's aggregate output, not a column of its own.
        AggIntent::TopK { .. } => SummaryInputExpr::Column(
            crate::realization::ranked_value(memo, input)
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
    filter: Option<&Predicate<MemoGroupId>>,
) -> Result<ColumnRef, PerComputationCandidateError> {
    fn not_null(expr: &ScalarExpr<MemoGroupId>) -> Vec<usize> {
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

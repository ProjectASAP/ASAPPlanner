//! Stage 1, Pass 1's computation targets over the memo groups (concepts doc
//! `memo-groups-properties-requirements.md`, section 3, "Stage 1, Pass 1").
//!
//! [`computation_targets`](fn@crate::computation_targets::computation_targets) walks the memo groups and finds, for every
//! aggregate group, one target ([`ComputationTarget`](crate::computation_targets::ComputationTarget)) per measure whose
//! intent has a realization besides plain exact execution, once however
//! many queries read the group, with the ways to compute it and its fused
//! computations: a whole-expression top-k (a heap over the rows below a
//! per-item `sum` or `count`) and a nested distributive aggregate (one
//! exact `Sum`, `Min` or `Max` state over the rows below a `sum` of a
//! `sum`, a `min` of a `min` or a `max` of a `max`). The candidates that
//! compute the targets are `crate::candidates`'.
//!
//! The input must be a logical memo: a group whose expression is an ASAP
//! operator is rejected. No cost model and no accuracy target is read.

use asap_types::ir::operator::arguments::Reduction;
use asap_types::ir::operator::arguments::Source;
use asap_types::ir::operator::non_asap::any_measure_filtered;
use asap_types::ir::operator::AggIntent;
use asap_types::ir::scalar::{resolve_column_ref, ColumnRef};
use asap_types::ir::schema::{ColumnId, Schema};
use asap_types::ir::{NonASAPOp, Operator, OperatorResultKind, ScalarExpr};
use asap_types::stages::{Memo, MemoGroupId};
use asap_types::summary_type::{
    ExactKind, SketchAlgorithm, SketchStatistic, StatisticKind, SummaryInputExpr, SummaryUpdate,
};

use asap_types::deployment_capabilities::{summary_supported, SupportedSummary};

use crate::candidates::having::has_having;
use crate::summary_realization::{realize_keyed_additive_summary_input, summary_candidates};
use crate::{expression, schema, PerComputationCandidateError};

/// How one Pass 1 target is computed besides plain exact execution: by
/// exact accumulator states or a summary. Plain exact execution is not a
/// way here: it is always offered, as the aggregate group's own
/// expression.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum HowToComputeAggregateMeasure {
    /// One or more exact mergeable accumulator states, by accumulator kind,
    /// finalized and combined into the measure: one state for `sum`,
    /// `count`, `min`, `max`, `rate`, `irate` and `increase`; `[Sum, Count]`,
    /// divided, for `avg`.
    ExactAccumulator(Vec<ExactKind>),
    /// A sketch algorithm: its state group gets one build candidate per
    /// configuration of the algorithm's predefined list (`parameter_list`).
    Summary(SketchAlgorithm),
}

/// One measure of an aggregate group with at least one realization besides
/// exact execution.
#[derive(Debug)]
pub(crate) struct ComputationTarget {
    /// The memo group of the `Aggregate` the measure belongs to.
    pub aggregate_group: MemoGroupId,
    /// Index into the aggregate's measures.
    pub measure_index: usize,
    /// The measure's unranked ways to compute it besides plain exact
    /// execution, the exact accumulator first when it has one; empty when
    /// Pass 1 pruned every one.
    pub ways_to_compute: Vec<HowToComputeAggregateMeasure>,
    /// The fused computation of this measure together with the target
    /// directly beneath it, offered beside this target's own candidates;
    /// `None` when no fusion applies.
    pub fused_computation: Option<FusedComputation>,
}

/// A target's measure computed together with the targets beneath it, as
/// one operator that replaces their operators in this appearance of the
/// sub-expression (concepts doc §3, "Fused groups"): the whole-expression
/// top-k over a per-item `sum` or `count` (`Summary` of a heap algorithm), or
/// a nested distributive aggregate, a grouped `sum`, `min` or `max` over the
/// same reduction (`ExactAccumulator([Sum])`, `[Min]` or `[Max]`). It has
/// the outer target's aggregate, measure, filter and `HAVING`.
#[derive(Debug)]
pub(crate) struct FusedComputation {
    /// The groups of the operators it replaces, which its builds list in
    /// `fused_groups`: the aggregate beneath (after the column-renaming
    /// `Project` between them, when there is one), then, for a
    /// whole-expression heap over the raw data samples beneath a grouped sum
    /// of per-series sums or counts (`topk(k, sum by (G)
    /// (sum_over_time(m[W])))`), the per-series aggregate beneath that, then
    /// the groups any of these targets' own fused computations replace.
    pub absorbed_groups: Vec<MemoGroupId>,
    /// How the fused computation is computed: by an exact accumulator or a
    /// summary. Empty
    /// when Pass 1 pruned every one (today only for an algorithm the deployment
    /// does not support); the fusion is then not offered.
    pub ways_to_compute: Vec<HowToComputeAggregateMeasure>,
}

/// Enumerate the exact accumulator and summary choices of one measure in
/// stable catalog order, without ranking or empirical assessment. A summary
/// is not a claim that a deployment meets the measure's accuracy
/// requirement. The exact accumulator form comes first when the intent has
/// one; plain exact execution is not listed (it is the group's own
/// expression). A quantile outside [0, 1] gets no summary: its answer is
/// -Inf or +Inf.
pub(crate) fn ways_to_compute_intent(intent: &AggIntent) -> Vec<HowToComputeAggregateMeasure> {
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
    let mut choices = Vec::new();
    if !states.is_empty() {
        choices.push(HowToComputeAggregateMeasure::ExactAccumulator(states));
    }
    let quantile_outside_unit_interval =
        matches!(intent, AggIntent::Quantile { q, .. } if !(0.0..=1.0).contains(q));
    if SketchStatistic::for_intent(intent).is_some() && !quantile_outside_unit_interval {
        choices.extend(
            summary_candidates(intent)
                .into_iter()
                .map(HowToComputeAggregateMeasure::Summary),
        );
    }
    choices
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
/// `HAVING` keep every candidate.
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
            child, measures, ..
        }) = &group.expression
        else {
            continue;
        };
        let aggregate_group = MemoGroupId(index as u32);
        for (measure, intent) in measures.iter().enumerate() {
            let mut choices = ways_to_compute_intent(intent);
            if choices.is_empty() {
                continue;
            }
            if matches!(intent, AggIntent::Avg { .. })
                && nullable_avg_input(schema(memo, *child), intent)
            {
                choices.retain(|choice| {
                    !matches!(choice, HowToComputeAggregateMeasure::ExactAccumulator(_))
                });
            }
            if matches!(intent, AggIntent::Count)
                && memo.groups()[child.0 as usize].group_properties.result_kind
                    == OperatorResultKind::Relation
            {
                choices
                    .retain(|choice| !matches!(choice, HowToComputeAggregateMeasure::Summary(_)));
            }
            targets.push(ComputationTarget {
                aggregate_group,
                measure_index: measure,
                ways_to_compute: choices,
                fused_computation: None,
            });
        }
    }
    add_whole_expression_targets(memo, &mut targets);
    prune_topk_heaps_over_one_row_per_item(memo, &mut targets);
    add_nested_distributive_targets(memo, &mut targets);
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
        let Some(readout) =
            SketchStatistic::for_intent(&measures[target.measure_index]).map(|s| s.kind())
        else {
            continue;
        };
        let unsupported = |candidate: &HowToComputeAggregateMeasure| {
            matches!(candidate, HowToComputeAggregateMeasure::Summary(algorithm)
                if !summary_supported(supported_summaries, algorithm, readout))
        };
        target.ways_to_compute.retain(|way| !unsupported(way));
        if let Some(fused) = &mut target.fused_computation {
            fused.ways_to_compute.retain(|way| !unsupported(way));
        }
    }
}

/// Adds to each fused computation's `absorbed_groups` the groups its
/// absorbed targets' own fused computations replace, transitively: the
/// fused computation replaces all their operators, so its builds list all
/// their groups in `fused_groups`.
fn close_absorptions(targets: &mut [ComputationTarget]) {
    loop {
        let mut changed = false;
        for t in 0..targets.len() {
            let Some(fused) = &targets[t].fused_computation else {
                continue;
            };
            let beneath: Vec<MemoGroupId> = fused
                .absorbed_groups
                .iter()
                .filter_map(|&group| {
                    targets
                        .iter()
                        .find(|target| target.aggregate_group == group)?
                        .fused_computation
                        .as_ref()
                })
                .flat_map(|inner| inner.absorbed_groups.iter().copied())
                .filter(|b| !fused.absorbed_groups.contains(b))
                .collect();
            if let Some(fused) = targets[t]
                .fused_computation
                .as_mut()
                .filter(|_| !beneath.is_empty())
            {
                for b in beneath {
                    if !fused.absorbed_groups.contains(&b) {
                        fused.absorbed_groups.push(b);
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

/// Nested distributive merge: a grouped `sum`, `min` or `max` over the
/// value of a target of the same reduction beneath it (`sum` of `sum`,
/// `min` of `min`, `max` of `max`) gets a fused computation
/// ([`ComputationTarget::fused_computation`]) with the one candidate
/// `ExactAccumulator([kind])`: one exact accumulator of that kind over the
/// inner aggregate's input, grouped by the outer keys (the sum, min or max
/// of the inner groups' values is that of their rows), absorbing the inner
/// target. Offered whatever else reads the inner aggregate: its group keeps
/// its own candidates for its other consumers. Applied when neither has a
/// filter or `HAVING` ([`nested_distributive_input`]), and when neither the
/// outer aggregate nor the inner one has a fused computation yet, so merges
/// do not stack. Targets follow the memo's group order, inputs first, so an
/// outer aggregate is reached before any merge above it. An outer sum a
/// whole-expression top-k absorbs (`topk(k, sum by (G)
/// (sum_over_time(m[W])))`) keeps its merge for the top-k's own candidates.
fn add_nested_distributive_targets(memo: &Memo, targets: &mut [ComputationTarget]) {
    for t in 0..targets.len() {
        let target = &targets[t];
        if target.fused_computation.is_some() || has_having(memo, target.aggregate_group) {
            continue;
        }
        let Some(nested) = nested_aggregates(memo, target.aggregate_group) else {
            continue;
        };
        let Some(inner) = targets
            .iter()
            .position(|other| other.aggregate_group == nested.inner)
        else {
            continue;
        };
        // Merges do not stack: the inner aggregate must not have a fused
        // computation of its own.
        if has_having(memo, nested.inner) || targets[inner].fused_computation.is_some() {
            continue;
        }
        let Some((_, _, _, kind)) = nested_distributive_input(memo, target.aggregate_group) else {
            continue;
        };
        targets[t].fused_computation = Some(FusedComputation {
            absorbed_groups: nested.renaming.into_iter().chain([nested.inner]).collect(),
            ways_to_compute: vec![HowToComputeAggregateMeasure::ExactAccumulator(vec![kind])],
        });
    }
}

/// The accumulator kind and input column of a distributive measure, one
/// whose value over a union of groups is that of their values: `sum`, `min`
/// and `max`.
fn distributive(intent: &AggIntent) -> Option<(ExactKind, Option<usize>)> {
    match intent {
        AggIntent::Sum { col } => Some((ExactKind::Sum, *col)),
        AggIntent::Min { col } => Some((ExactKind::Min, *col)),
        AggIntent::Max { col } => Some((ExactKind::Max, *col)),
        _ => None,
    }
}

/// An outer grouped aggregate of one measure over the value of a
/// one-measure aggregate beneath it ([`nested_aggregates`]).
pub(crate) struct NestedAggregates<'m> {
    /// The outer aggregate's measure.
    pub(crate) outer_measure: &'m AggIntent,
    /// The outer aggregate's `by` keys, as columns of the inner aggregate's
    /// output.
    pub(crate) by: Vec<ColumnId>,
    /// The group of a `Project` between them that only renames or reorders
    /// columns (a SQL subquery's), or `None` when the outer aggregate reads
    /// the inner one directly.
    pub(crate) renaming: Option<MemoGroupId>,
    /// The inner aggregate's group.
    pub(crate) inner: MemoGroupId,
    pub(crate) inner_reduction: &'m Reduction,
    pub(crate) inner_measure: &'m AggIntent,
    /// The inner aggregate's input.
    pub(crate) rows: MemoGroupId,
}

/// `outer` as a grouped aggregate (not by `without`) of one unfiltered
/// measure without `HAVING`, over the value column of a per-series or
/// grouped (not by `without`) aggregate of one unfiltered measure without
/// `HAVING` beneath it, read directly or through a `Project` that only
/// renames or reorders columns and keeps the key names; not grouped by the
/// value column, nor, over a per-series aggregate, by the metric name.
/// `None` otherwise.
///
/// A per-series aggregate drops the metric name `__name__`, which the raw
/// rows keep: an outer aggregate grouped by it over the per-series rows has
/// one unnamed group where the same grouping of the raw rows has one per
/// metric, so it is never one aggregate over the raw rows. A key whose
/// column cannot be read is treated as the metric name.
pub(crate) fn nested_aggregates(memo: &Memo, outer: MemoGroupId) -> Option<NestedAggregates<'_>> {
    let Operator::NonASAP(NonASAPOp::Aggregate {
        child,
        reduction: Reduction::Reduce(by),
        measures,
        filters,
        having: None,
        ..
    }) = expression(memo, outer)
    else {
        return None;
    };
    let [outer_measure] = measures.as_slice() else {
        return None;
    };
    if by.is_without() || any_measure_filtered(filters) {
        return None;
    }
    // The inner aggregate's column each column the outer one reads is.
    let (renaming, inner, inner_column): (Option<MemoGroupId>, MemoGroupId, Vec<ColumnId>) =
        match expression(memo, *child) {
            Operator::NonASAP(NonASAPOp::Project { cols, child: inner }) => (
                Some(*child),
                *inner,
                cols.iter()
                    .map(|item| match item.expr {
                        ScalarExpr::Column(column) => Some(column),
                        _ => None,
                    })
                    .collect::<Option<_>>()?,
            ),
            _ => (
                None,
                *child,
                (0..schema(memo, *child).fields.len()).collect(),
            ),
        };
    let Operator::NonASAP(NonASAPOp::Aggregate {
        child: rows,
        reduction: inner_reduction,
        measures: inner_measures,
        filters: inner_filters,
        having: None,
        ..
    }) = expression(memo, inner)
    else {
        return None;
    };
    let [inner_measure] = inner_measures.as_slice() else {
        return None;
    };
    if any_measure_filtered(inner_filters) {
        return None;
    }
    // The inner aggregate's value column: a per-series one keeps its labels
    // and carries the value as the sample value; a grouped one ends with it.
    let value = match inner_reduction {
        Reduction::PerEntity => {
            resolve_column_ref(&ColumnRef::SampleValue, schema(memo, inner)).ok()?
        }
        Reduction::Reduce(keys) if !keys.is_without() => keys.len(),
        Reduction::Reduce(_) => return None,
    };
    let read = match outer_measure.input_cols().first() {
        Some(&column) => column,
        None => resolve_column_ref(&ColumnRef::SampleValue, schema(memo, *child)).ok()?,
    };
    if inner_column.get(read) != Some(&value) {
        return None;
    }
    let outer_keys = by.keys();
    let by: Vec<ColumnId> = outer_keys
        .iter()
        .map(|&key| inner_column.get(key).copied())
        .collect::<Option<_>>()?;
    if by.contains(&value) {
        return None;
    }
    // A renaming keeps the key names, so a state over its rows and one over
    // the raw rows have the same columns.
    let name = |group: MemoGroupId, column: ColumnId| {
        schema(memo, group)
            .fields
            .get(column)
            .map(|field| field.name.as_str())
    };
    if outer_keys
        .iter()
        .zip(&by)
        .any(|(&outer_key, &inner_key)| name(*child, outer_key) != name(inner, inner_key))
    {
        return None;
    }
    if matches!(inner_reduction, Reduction::PerEntity)
        && by
            .iter()
            .any(|&key| name(inner, key).is_none_or(|name| name == "__name__"))
    {
        return None;
    }
    Some(NestedAggregates {
        outer_measure,
        by,
        renaming,
        inner,
        inner_reduction,
        inner_measure,
        rows: *rows,
    })
}

/// The input rows, the inner measure, the outer grouping (moved onto the
/// input rows by column, [`Reduction::partitions_over`]) and the
/// accumulator kind of a nested distributive merge of `target`, or `None`
/// when it does not apply: `target` and the aggregate beneath it
/// ([`nested_aggregates`]) are both `sum`, both `min` or both `max`
/// ([`distributive`]).
pub(crate) fn nested_distributive_input(
    memo: &Memo,
    target: MemoGroupId,
) -> Option<(MemoGroupId, AggIntent, Reduction, ExactKind)> {
    let nested = nested_aggregates(memo, target)?;
    let (kind, _) = distributive(nested.outer_measure)?;
    if distributive(nested.inner_measure).map(|(inner_kind, _)| inner_kind) != Some(kind) {
        return None;
    }
    let partitions = Reduction::by(nested.by)
        .partitions_over(schema(memo, nested.inner), schema(memo, nested.rows))?;
    Some((nested.rows, nested.inner_measure.clone(), partitions, kind))
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
        if !targets.iter().any(|other| other.aggregate_group == *child) {
            continue;
        }
        // Measure filters: `whole_expression_input` refuses them.
        if has_having(memo, target.aggregate_group) || has_having(memo, *child) {
            continue;
        }
        let Some((input, ..)) = whole_expression_input(memo, target.aggregate_group) else {
            continue;
        };
        // A heap over the raw data samples beneath a grouped sum of per-series
        // sums or counts also absorbs the per-series target.
        let mut absorbs = vec![*child];
        if let Operator::NonASAP(NonASAPOp::Aggregate {
            child: per_series, ..
        }) = expression(memo, *child)
        {
            let reads_below = expression(memo, *per_series).children().contains(&&input);
            if reads_below
                && targets
                    .iter()
                    .any(|other| other.aggregate_group == *per_series)
            {
                absorbs.push(*per_series);
            }
        }
        let (heaps, own): (Vec<_>, Vec<_>) =
            targets[t]
                .ways_to_compute
                .iter()
                .cloned()
                .partition(|candidate| {
                    matches!(candidate, HowToComputeAggregateMeasure::Summary(algorithm)
                    if algorithm.capability().answers(StatisticKind::TopK))
                });
        if heaps.is_empty() {
            continue;
        }
        let target = &mut targets[t];
        target.ways_to_compute = own;
        target.fused_computation = Some(FusedComputation {
            absorbed_groups: absorbs,
            ways_to_compute: heaps,
        });
    }
}

/// Prunes the own heap candidates of each PromQL top-k: the heap reads
/// one row per item, so exact is no costlier; a fused whole-expression
/// heap ([`add_whole_expression_targets`]) has already moved out of them.
fn prune_topk_heaps_over_one_row_per_item(memo: &Memo, targets: &mut [ComputationTarget]) {
    for target in targets.iter_mut() {
        let Operator::NonASAP(NonASAPOp::Aggregate { measures, .. }) =
            expression(memo, target.aggregate_group)
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
        // group. (A SQL top-k keeps its heaps.) The aggregate scans what its
        // input does.
        if !memo.groups()[target.aggregate_group.0 as usize]
            .group_properties
            .scanned_sources
            .iter()
            .all(Source::is_time_series)
        {
            continue;
        }
        target.ways_to_compute.retain(|candidate| {
            !matches!(candidate, HowToComputeAggregateMeasure::Summary(algorithm)
                if algorithm.capability().answers(StatisticKind::TopK))
        });
    }
}

/// The input, update and partitions of a whole-expression top-k over
/// `target`'s inner aggregate, by the keyed-additive rule, or `None` when it
/// does not apply. Rows that carry the full series identity rank it as a
/// column, as `candidates::summary_update` does. The partitions are `target`'s, moved
/// onto the input the heap reads ([`Reduction::partitions_over`]).
pub(crate) fn whole_expression_input(
    memo: &Memo,
    target: MemoGroupId,
) -> Option<(MemoGroupId, SummaryUpdate, Reduction)> {
    let Operator::NonASAP(NonASAPOp::Aggregate {
        child,
        reduction,
        measures,
        filters,
        having: None,
        ..
    }) = expression(memo, target)
    else {
        return None;
    };
    let child = *child;
    let [AggIntent::TopK { .. }] = measures.as_slice() else {
        return None;
    };
    if any_measure_filtered(filters) {
        return None;
    }
    let (rows, mut input) = realize_keyed_additive_summary_input(memo, reduction, child)?;
    let per_series = matches!(
        expression(memo, child),
        Operator::NonASAP(NonASAPOp::Aggregate {
            reduction: Reduction::PerEntity,
            ..
        })
    );
    let partitions = reduction.partitions_over(schema(memo, child), schema(memo, rows))?;
    if per_series && schema(memo, rows).has_promql_series_identity() {
        input.item = Some(asap_types::ir::schema::topk::series_identity_item(
            schema(memo, rows),
            partitions.group_keys().map_or(&[][..], |keys| keys.keys()),
        ));
    } else if schema(memo, rows).closed
        && matches!(input.item, Some(SummaryInputExpr::EntityIdentity(_)))
    {
        // An encoded label set needs an open PromQL schema. PromQL rows that
        // carry the series identity are closed, but their inner group labels
        // are item columns. SQL group keys are explicit columns too, so their
        // keyed heap can consume raw rows without a per-key aggregate first.
        return None;
    }
    Some((rows, input, partitions))
}

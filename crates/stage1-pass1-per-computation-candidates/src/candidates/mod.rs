//! Stage 1, Pass 1's candidates: [`add_pass1_candidates`] adds each way to
//! compute a group as candidates, through `Memo::find_or_add_group` and
//! `Memo::add_candidate`, from the targets `crate::computation_targets`
//! finds:
//!
//! - **Exact.** Every group's own expression is a candidate of it.
//! - **Exact accumulator.** An accumulator-state group
//!   (`SummaryAgg` of an exact summary type) and a `FinalizeExactAccumulator`
//!   candidate over it; `avg` finalizes its `sum` and `count` states and
//!   divides them.
//! - **Summary.** One state group per sketch algorithm (`SummaryAgg` of
//!   it), with one build candidate per configuration of the algorithm's predefined
//!   list (`parameter_list`), and a readout (`SummaryEstimate`) group over
//!   it, read by the result group.
//! - **Fused builds.** A whole-expression top-k (a heap over the rows below
//!   a per-item `sum` or `count`) and a nested distributive aggregate (one
//!   exact `Sum`, `Min` or `Max` state over the rows below a `sum` of a
//!   `sum`, a `min` of a `min` or a `max` of a `max`): each computes several
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
//! Pass 1 sets no input requirement: each candidate's are empty, and Pass 2
//! gives each readout's input its required accuracy (a top-k readout's
//! `heap_size` ≥ k is part of that rule; concepts doc, section 2).
//!
//! A measure filter or `HAVING` prunes nothing. The measure filter is
//! applied as `SummaryAgg.filter`. A `HAVING` is the aggregate's own
//! `having` or a consumer `Filter` group directly above the aggregate group,
//! read from the memo: its key-only conjuncts (`key_only_having`) are pushed
//! below the aggregate by the candidates that compute the filtered result
//! (the aggregate group, or that `Filter` group); the other conjuncts stay a
//! `Filter` above the output.
//!
//! Each way is built pending (`Pending`) and added only once
//! every check that can find it unsupported has passed, so an unsupported
//! way adds nothing to the memo.

mod measure_output;
mod pending;

use std::collections::BTreeMap;

use asap_types::ir::operator::arguments::Reduction;
use asap_types::ir::operator::AggIntent;
use asap_types::ir::schema::FieldDataType;
use asap_types::ir::{ASAPOp, NonASAPOp, Operator, OperatorResultKind, Predicate, ScalarExpr};
use asap_types::stages::{raw_data_samples_of_metric_of_group, Memo, MemoGroupId};

use asap_accuracy::sizing::sketch_statistic;

use crate::computation_targets::{
    having_filter_groups, nested_distributive_input, whole_expression_input, ComputationTarget,
    HowToComputeAggregateMeasure,
};
use crate::summary_realization::{measure_input, with_raw_data_samples_requirement};
use crate::{expression, schema, PerComputationCandidateError};
use measure_output::{
    accumulator_build, add_output_candidate, join_measures, measure_output, ranked_as_topk,
};
pub(crate) use pending::Pending;
use pending::{add_candidate, commit_pending};

/// Adds Pass 1's candidates to `memo` (Stage 0's, without candidates):
/// every group's own expression, then, for each aggregate group with
/// `targets`, each way to compute it. A consumer `Filter` group whose
/// `HAVING` has key-only conjuncts gets the candidates that compute its
/// rows with those conjuncts pushed below the aggregate. Every group and
/// candidate is added through `Memo::find_or_add_group` and
/// `Memo::add_candidate`.
pub(crate) fn add_pass1_candidates(
    memo: &mut Memo,
    targets: &[ComputationTarget],
) -> Result<(), PerComputationCandidateError> {
    // Stage 0's groups: a consumer `Filter` among them is a query's
    // `HAVING`; the `Filter` groups Pass 1 adds are parts of its candidates.
    let stage0_groups = memo.groups().len();
    for index in 0..stage0_groups {
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
        let Operator::NonASAP(NonASAPOp::Aggregate {
            reduction, having, ..
        }) = expression(memo, aggregate_group)
        else {
            continue;
        };
        let (below, above) = having
            .as_ref()
            .map_or_else(Default::default, |having| split_having(reduction, having));
        // Each consumer `Filter` group whose `HAVING` has key-only conjuncts
        // gets the candidates that compute its rows with them pushed below;
        // its other conjuncts stay above.
        let pushed_down: Vec<_> = having_filter_groups(memo, aggregate_group)
            .filter(|&(filter, _)| (filter.0 as usize) < stage0_groups)
            .map(|(filter, pred)| {
                let (key_only, rest) = split_having(reduction, pred);
                (filter, key_only, rest)
            })
            .filter(|(_, key_only, _)| !key_only.is_empty())
            .collect();
        let ways = AggregateWays {
            aggregate_group,
            result_group: aggregate_group,
            below: &below,
            above: &above,
            targets: group_targets,
        };
        add_aggregate_candidates(memo, &ways)?;
        for (filter, key_only, rest) in pushed_down {
            let below = [below.clone(), key_only].concat();
            let above = [above.clone(), rest].concat();
            let filtered = AggregateWays {
                result_group: filter,
                below: &below,
                above: &above,
                ..ways
            };
            add_aggregate_candidates(memo, &filtered)?;
        }
    }
    Ok(())
}

/// `having`'s conjuncts over an aggregate's output, split into those that
/// read only its group keys ([`key_only_having`]), which can filter its
/// input rows, and the others, as one predicate (none when every conjunct
/// is key-only).
fn split_having(
    reduction: &Reduction,
    having: &Predicate<MemoGroupId>,
) -> (Vec<Predicate<MemoGroupId>>, Vec<Predicate<MemoGroupId>>) {
    let conjuncts = match &having.0 {
        ScalarExpr::BoolAnd(conjuncts) => conjuncts.clone(),
        conjunct => vec![conjunct.clone()],
    };
    let (key_only, mut rest): (Vec<_>, Vec<_>) = conjuncts
        .into_iter()
        .map(Predicate)
        .partition(|conjunct| key_only_having(reduction, conjunct));
    let rest = match rest.len() {
        0 | 1 => rest,
        _ => vec![Predicate(ScalarExpr::BoolAnd(
            rest.drain(..).map(|conjunct| conjunct.0).collect(),
        ))],
    };
    (key_only, rest)
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

/// The ways to compute one result from an aggregate group's targets.
struct AggregateWays<'a> {
    /// The aggregate group whose measures are computed.
    aggregate_group: MemoGroupId,
    /// The group the candidates compute: the aggregate group, or a consumer
    /// `Filter` group above it whose key-only `HAVING` conjuncts go below.
    result_group: MemoGroupId,
    /// Key-only predicates over the aggregate's output, applied to its
    /// input rows (their key columns renumbered to the input's).
    below: &'a [Predicate<MemoGroupId>],
    /// Predicates applied as `Filter`s above the output.
    above: &'a [Predicate<MemoGroupId>],
    /// The aggregate's targets, one per measure that has one.
    targets: &'a [&'a ComputationTarget],
}

/// One measure of an aggregate, computed on its own over `input`.
pub(crate) struct SingleMeasure<'a> {
    /// The rows the measure reads: a memo group, or a pending `Filter` of
    /// the key-only `HAVING` conjuncts over `unfiltered_input`.
    pub(crate) input: MemoGroupId,
    /// The memo group `input` filters, or `input` itself: it has `input`'s
    /// schema and kind, and is ranked as `input` is.
    pub(crate) unfiltered_input: MemoGroupId,
    pub(crate) reduction: &'a Reduction,
    pub(crate) intent: &'a AggIntent,
    pub(crate) filter: Option<&'a Predicate<MemoGroupId>>,
    /// The metric whose raw data samples, or plain sums of them, are every
    /// value the aggregate reads: a heap-sketch weight read from them
    /// carries the open requirement `WeightDomain::RawDataSamplesOfMetric`.
    pub(crate) raw_data_samples_of: Option<&'a str>,
}

/// Adds `ways.result_group`'s candidates that compute it from
/// `ways.aggregate_group`'s measures: for a single measure, each of its
/// target's ways and fused ways; for several, one candidate joining the
/// measures' own groups (a single-measure aggregate each, with its own
/// candidates) on the group keys. Each way starts pending with the
/// key-only `HAVING` conjuncts as `Filter`s over the aggregate's input.
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
    // A way over the aggregate's input rows, filtered by the key-only
    // conjuncts, pending.
    let start_way = |memo: &Memo| -> Result<(Pending, MemoGroupId), PerComputationCandidateError> {
        let mut pending = Pending::new(memo);
        let mut input = child;
        for predicate in ways.below {
            let pred = Predicate(predicate.0.map_columns(&mut |column| keys[column]));
            input = pending.push(
                memo,
                Operator::NonASAP(NonASAPOp::Filter { pred, child: input }),
            )?;
        }
        Ok((pending, input))
    };
    let raw_data_samples = raw_data_samples_of_metric_of_group(memo, child);
    let measure = |index: usize, input: MemoGroupId| SingleMeasure {
        input,
        unfiltered_input: child,
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
            if *way != HowToComputeAggregateMeasure::Exact {
                unless_unsupported(start_way(memo).and_then(|(pending, input)| {
                    add_measure_way(
                        memo,
                        pending,
                        &measure(0, input),
                        way,
                        ways.result_group,
                        ways.above,
                    )
                }))?;
            }
        }
        if let Some(fused) = &target.fused_computation {
            for way in &fused.ways_to_compute {
                unless_unsupported(start_way(memo).and_then(|(pending, input)| {
                    add_fused_candidate(
                        memo,
                        pending,
                        ways,
                        &measure(0, input),
                        way,
                        &fused.absorbed_groups,
                    )
                }))?;
            }
        }
        return Ok(());
    }
    // The measures are joined back on their group keys: SQL rows grouped
    // `by` keys (PromQL aggregates have one measure each).
    if !matches!(&reduction, Reduction::Reduce(by) if !by.is_without())
        || memo.groups()[child.0 as usize].group_properties.result_kind
            != OperatorResultKind::Relation
    {
        return Ok(());
    }
    let joined = unless_unsupported(start_way(memo).and_then(|(mut pending, input)| {
        let parts = (0..measures.len())
            .map(|index| {
                pending.push(
                    memo,
                    single_measure_part(
                        &reduction,
                        &measures,
                        &output_names,
                        &filters,
                        index,
                        input,
                    ),
                )
            })
            .collect::<Result<Vec<_>, _>>()?;
        let output = join_measures(memo, &mut pending, &reduction, ways.result_group, &parts)?;
        let group_of = add_output_candidate(memo, ways.result_group, pending, output, ways.above)?;
        Ok((
            group_of(input),
            parts.into_iter().map(group_of).collect::<Vec<_>>(),
        ))
    }))?;
    // Each measure's own group gets its ways once the join adds it.
    let Some((input, parts)) = joined else {
        return Ok(());
    };
    for (index, part) in parts.into_iter().enumerate() {
        if let Some(target) = target_of(index) {
            for way in &target.ways_to_compute {
                if *way != HowToComputeAggregateMeasure::Exact {
                    unless_unsupported(add_measure_way(
                        memo,
                        Pending::new(memo),
                        &measure(index, input),
                        way,
                        part,
                        &[],
                    ))?;
                }
            }
        }
    }
    Ok(())
}

/// Measure `index` of a multi-measure aggregate over `input` as its own
/// single-measure aggregate, written as the frontends write one: its name,
/// and its filter only when it has one (an unfiltered aggregate lists none),
/// so it is the same group as an equal Stage 0 aggregate.
fn single_measure_part(
    reduction: &Reduction,
    measures: &[AggIntent],
    output_names: &[String],
    filters: &[Option<Predicate<MemoGroupId>>],
    index: usize,
    input: MemoGroupId,
) -> Operator<MemoGroupId> {
    Operator::NonASAP(NonASAPOp::Aggregate {
        child: input,
        reduction: reduction.clone(),
        measures: vec![measures[index].clone()],
        output_names: output_names
            .get(index)
            .map_or_else(Vec::new, |name| vec![name.clone()]),
        filters: filters
            .get(index)
            .cloned()
            .flatten()
            .map_or_else(Vec::new, |filter| vec![Some(filter)]),
        having: None,
    })
}

/// `added`, or `None` when the way is one Pass 1 cannot build
/// (`Unsupported`), which is not offered. Every check that returns
/// `Unsupported` runs before the way adds its first group or candidate (its
/// groups are pending until then), so an unsupported way leaves the memo
/// unchanged.
fn unless_unsupported<T>(
    added: Result<T, PerComputationCandidateError>,
) -> Result<Option<T>, PerComputationCandidateError> {
    match added {
        Err(PerComputationCandidateError::Unsupported(_)) => Ok(None),
        added => added.map(Some),
    }
}

/// Adds `measure`'s way `way` (not `Exact`), started in `pending`, as
/// candidates of `result_group`, then the `above` filters over it: its
/// groups are built pending ([`measure_output`](fn@measure_output)), checked, and only then
/// added.
pub(crate) fn add_measure_way(
    memo: &mut Memo,
    mut pending: Pending,
    measure: &SingleMeasure<'_>,
    way: &HowToComputeAggregateMeasure,
    result_group: MemoGroupId,
    above: &[Predicate<MemoGroupId>],
) -> Result<(), PerComputationCandidateError> {
    let output = measure_output(memo, &mut pending, measure, way, result_group)?;
    add_output_candidate(memo, result_group, pending, output, above).map(|_| ())
}

/// Adds the fused computation `way` of `ways`' single measure, started in
/// `pending`: a whole-expression heap (a summary way) or a nested
/// distributive merge (an exact accumulator way). Its builds list
/// `absorbed_groups`, the groups of the operators it replaces, in
/// `fused_groups`.
fn add_fused_candidate(
    memo: &mut Memo,
    mut pending: Pending,
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
            let state = pending.push(
                memo,
                Operator::ASAP(ASAPOp::SummaryAgg {
                    child: rows,
                    input: with_raw_data_samples_requirement(update, measure.raw_data_samples_of),
                    summary_type: FieldDataType::Sketch(*algorithm),
                    reduction: partitions,
                    filter: None,
                }),
            )?;
            let estimate = pending.push(
                memo,
                Operator::ASAP(ASAPOp::SummaryEstimate {
                    summary_input: state,
                    query,
                }),
            )?;
            let output = ranked_as_topk(
                measure.reduction,
                schema(memo, ways.result_group),
                pending.schema(memo, estimate),
                estimate,
            )?;
            let group_of = commit_pending(memo, pending, absorbed_groups)?;
            let output = output.map_children(|&input| group_of(input));
            add_candidate(memo, ways.result_group, output, None, Vec::new())?;
        }
        HowToComputeAggregateMeasure::ExactAccumulator(_) => {
            // The merged build computes the outer aggregate's own
            // accumulator state, from the rows below the inner aggregate: a
            // candidate of that state group.
            let (rows, inner, partitions, kind) =
                nested_distributive_input(memo, ways.aggregate_group).ok_or(
                    PerComputationCandidateError::Unsupported("nested distributive merge input"),
                )?;
            let outer_build = accumulator_build(
                memo,
                kind,
                measure.intent,
                measure,
                measure_input(
                    measure.intent,
                    memo,
                    measure.unfiltered_input,
                    measure.filter,
                )?,
            )?;
            let below_inner = SingleMeasure {
                input: rows,
                unfiltered_input: rows,
                reduction: &partitions,
                intent: &inner,
                filter: None,
                raw_data_samples_of: None,
            };
            let merged = accumulator_build(
                memo,
                kind,
                &inner,
                &below_inner,
                measure_input(&inner, memo, rows, None)?,
            )?;
            let outer_state = pending.push(memo, outer_build)?;
            let group_of = commit_pending(memo, pending, &[])?;
            add_candidate(
                memo,
                group_of(outer_state),
                merged,
                None,
                absorbed_groups.to_vec(),
            )?;
        }
        HowToComputeAggregateMeasure::Exact => {
            unreachable!(
                "a fused computation is a summary or an exact accumulator, never plain exact"
            )
        }
    }
    Ok(())
}

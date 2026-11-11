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
//! read from the memo: a key-only predicate (`key_only_having`) is pushed
//! below the aggregate by the candidates that compute the filtered result
//! (the aggregate group, or that `Filter` group); any other stays a `Filter`
//! above the output.
use std::collections::BTreeMap;

use asap_types::ir::operator::arguments::{BinaryOpKind, JoinKind, Reduction};
use asap_types::ir::operator::AggIntent;
use asap_types::ir::scalar::expr_ir::ArithmeticOpKind;
use asap_types::ir::scalar::{resolve_column_ref, ColumnRef, CompareOpKind, ScalarValue};
use asap_types::ir::schema::{DataType, FieldDataType, Schema};
use asap_types::ir::{
    ASAPOp, BinaryOperator, ExprSemantics, NonASAPOp, Operator, OperatorResultKind, Predicate,
    ProjectItem, ScalarExpr,
};
use asap_types::stages::{
    raw_data_samples_of_metric_of_group, CandidateError, CandidateExpression,
    CandidateForMemoGroup, LogicalProperties, LogicalRequirements, Memo, MemoGroupId,
    OriginOfCandidateForMemoGroup,
};
use asap_types::summary_type::parameter_list;
use asap_types::summary_type::{
    EntityIdentity, ExactKind, NonNegativeWeightProof, SketchAlgorithm, SketchParams,
    SummaryInputExpr, SummaryUpdate, WeightDomain,
};

use asap_accuracy::sizing::sketch_statistic;

use crate::computation_targets::{
    having_filter_groups, nested_distributive_input, whole_expression_input, ComputationTarget,
    HowToComputeAggregateMeasure,
};
use crate::summary_realization::{ranked_value, summarised_input};
use crate::{expression, schema, PerComputationCandidateError};

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
        let (below, above): (Vec<_>, Vec<_>) = having
            .iter()
            .cloned()
            .partition(|having| key_only_having(reduction, having));
        // Each consumer `Filter` group whose `HAVING` is key-only gets the
        // candidates that compute its rows with the predicate pushed below.
        let pushed_down: Vec<(MemoGroupId, Predicate<MemoGroupId>)> =
            having_filter_groups(memo, aggregate_group)
                .filter(|&(filter, pred)| {
                    (filter.0 as usize) < stage0_groups && key_only_having(reduction, pred)
                })
                .map(|(filter, pred)| (filter, pred.clone()))
                .collect();
        let ways = AggregateWays {
            aggregate_group,
            result_group: aggregate_group,
            below: &below,
            above: &above,
            targets: group_targets,
        };
        add_aggregate_candidates(memo, &ways)?;
        for (filter, pred) in pushed_down {
            let mut with_consumer = below.clone();
            with_consumer.push(pred);
            let filtered = AggregateWays {
                result_group: filter,
                below: &with_consumer,
                ..ways
            };
            add_aggregate_candidates(memo, &filtered)?;
        }
    }
    Ok(())
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
    /// `Filter` group above it whose key-only `HAVING` goes below.
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
    let raw_data_samples = raw_data_samples_of_metric_of_group(memo, child);
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
            for way in &fused.ways_to_compute {
                unless_unsupported(add_fused_candidate(
                    memo,
                    ways,
                    &measure(0),
                    way,
                    &fused.absorbed_groups,
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
    unless_unsupported(
        join_measures(memo, &reduction, ways.result_group, parts)
            .and_then(|output| add_output_candidate(memo, ways.result_group, output, ways.above)),
    )
}

/// `added`, with a way Pass 1 cannot build (`Unsupported`) not offered:
/// its checks run before the groups it reads are added, so the memo is
/// unchanged, except for the rare checks that need a group's schema (a
/// top-k estimate without its item columns, a per-series `avg` that is not
/// Float64, an output a `Project` cannot rename to the group's columns),
/// which leave groups no query reaches.
fn unless_unsupported(
    added: Result<(), PerComputationCandidateError>,
) -> Result<(), PerComputationCandidateError> {
    match added {
        Err(PerComputationCandidateError::Unsupported(_)) => Ok(()),
        added => added,
    }
}

/// Adds the fused computation `way` of `ways`' single measure: a whole-
/// expression heap (a summary way) or a nested distributive merge (an
/// exact accumulator way). Its builds list `absorbed_groups`, the groups of the
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
                    summary_type: FieldDataType::Sketch(*algorithm),
                    reduction: partitions,
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
            // The merged build computes the outer aggregate's own
            // accumulator state, from the rows below the inner aggregate: a
            // candidate of that state group.
            let (rows, inner, partitions, kind) =
                nested_distributive_input(memo, ways.aggregate_group).ok_or(
                    PerComputationCandidateError::Unsupported("nested distributive merge input"),
                )?;
            let outer_state = accumulator_state(
                memo,
                kind,
                measure.intent,
                measure,
                measure_input(measure.intent, memo, measure.input, measure.filter)?,
            )?;
            let below_inner = SingleMeasure {
                input: rows,
                reduction: &partitions,
                intent: &inner,
                filter: None,
                raw_data_samples_of: None,
            };
            let read = measure_input(&inner, memo, rows, None)?;
            let merged = accumulator_build(memo, kind, &inner, &below_inner, read)?;
            add_candidate(memo, outer_state, merged, None, absorbed_groups.to_vec())?;
        }
        HowToComputeAggregateMeasure::Exact => {
            unreachable!(
                "a fused computation is a summary or an exact accumulator, never plain exact"
            )
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
    Ok(match way {
        HowToComputeAggregateMeasure::Exact => {
            unreachable!(
                "plain exact execution is the group's own expression, not a measure output"
            )
        }
        HowToComputeAggregateMeasure::ExactAccumulator(kinds) if kinds.len() == 1 => {
            let read = measure_input(measure.intent, memo, measure.input, measure.filter)?;
            Operator::ASAP(ASAPOp::FinalizeExactAccumulator {
                child: accumulator_state(memo, kinds[0], measure.intent, measure, read)?,
            })
        }
        HowToComputeAggregateMeasure::Summary(algorithm) => {
            let query = sketch_statistic(measure.intent).ok_or(
                PerComputationCandidateError::Unsupported("sketch evaluation"),
            )?;
            let summary_type = FieldDataType::Sketch(*algorithm);
            let read = measure_input(measure.intent, memo, measure.input, measure.filter)?;
            let update = with_raw_data_samples_requirement(
                summary_update(
                    measure.intent,
                    &summary_type,
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
                    summary_type,
                    reduction: measure.reduction.clone(),
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
            let count_intent = AggIntent::Count;
            let read = measure_input(&sum_intent, memo, measure.input, measure.filter)?;
            let sum = accumulator_state(memo, ExactKind::Sum, &sum_intent, measure, read.clone())?;
            let count = accumulator_state(memo, ExactKind::Count, &count_intent, measure, read)?;
            let sum = add_group(
                memo,
                Operator::ASAP(ASAPOp::FinalizeExactAccumulator { child: sum }),
            )?;
            let count = add_group(
                memo,
                Operator::ASAP(ASAPOp::FinalizeExactAccumulator { child: count }),
            )?;
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

/// The accumulator-state group of `kind` for `intent` (`measure`'s own, or
/// one of its states, such as `avg`'s `sum`), reading `read` from
/// `measure`'s input, with its build candidate.
fn accumulator_state(
    memo: &mut Memo,
    kind: ExactKind,
    intent: &AggIntent,
    measure: &SingleMeasure<'_>,
    read: SummaryInputExpr,
) -> Result<MemoGroupId, PerComputationCandidateError> {
    let build = accumulator_build(memo, kind, intent, measure, read)?;
    add_state_group(memo, build, &[])
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
    let summary_type = FieldDataType::exact(kind);
    Ok(Operator::ASAP(ASAPOp::SummaryAgg {
        input: summary_update(
            intent,
            &summary_type,
            measure.reduction,
            schema(memo, measure.input),
            read,
        )?,
        child: measure.input,
        summary_type,
        reduction: measure.reduction.clone(),
        filter: measure.filter.cloned(),
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
/// one per configuration of a sketch algorithm's predefined list
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
            summary_type: FieldDataType::Sketch(algorithm),
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

/// Adds the candidate `operator` with `parameters` to `group`. Its input
/// requirements are empty: Pass 2 sets each readout's required accuracy
/// (`Memo::set_candidate_input_requirements`).
fn add_candidate(
    memo: &mut Memo,
    group: MemoGroupId,
    operator: Operator<MemoGroupId>,
    parameters: Option<SketchParams>,
    fused_groups: Vec<MemoGroupId>,
) -> Result<(), PerComputationCandidateError> {
    let expression = CandidateExpression { operator };
    let logical_properties = LogicalProperties::new(&expression, parameters)?;
    let input_requirements =
        vec![LogicalRequirements::default(); expression.operator.children().len()];
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
    let Some(by) = reduction.group_keys().filter(|by| !by.is_without()) else {
        return Err(PerComputationCandidateError::Unsupported(
            "SQL avg without `by` keys",
        ));
    };
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
    let Some(by) = reduction.group_keys().filter(|by| !by.is_without()) else {
        return Err(PerComputationCandidateError::Unsupported(
            "multi-measure aggregate without `by` keys",
        ));
    };
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
        AggIntent::Count
            if memo.groups()[input.0 as usize].group_properties.result_kind
                == OperatorResultKind::Relation =>
        {
            SummaryInputExpr::Column(counted_column(schema, filter)?)
        }
        // A top-k ranks its child's aggregate output, not a column of its own.
        AggIntent::TopK { .. } => {
            SummaryInputExpr::Column(ranked_value(memo, input).map_err(|_| {
                PerComputationCandidateError::Unsupported("Top-K has no ranked column")
            })?)
        }
        _ => summarised_input(intent, schema).ok_or(PerComputationCandidateError::Unsupported(
            "input column outside child schema",
        ))?,
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
/// summary type reads the measure's input.
fn summary_update(
    intent: &AggIntent,
    summary_type: &FieldDataType,
    reduction: &Reduction,
    child: &Schema,
    weight: SummaryInputExpr,
) -> Result<SummaryUpdate, PerComputationCandidateError> {
    let algorithm = match summary_type {
        FieldDataType::Sketch(kind) => Some(kind),
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
        (_, Some(SketchAlgorithm::UnivMon)) | (AggIntent::Count, Some(SketchAlgorithm::Cms)) => {
            SummaryUpdate {
                item: Some(weight),
                weight: SummaryInputExpr::Constant(1.0),
                weight_domain: WeightDomain::NonNegative {
                    proof: NonNegativeWeightProof::UnitCount,
                },
            }
        }
        _ => SummaryUpdate {
            item: None,
            weight,
            weight_domain: WeightDomain::UnknownOrSigned,
        },
    })
}

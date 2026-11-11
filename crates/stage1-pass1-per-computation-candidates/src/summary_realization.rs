//! How a summary realizes an aggregate intent in Pass 1: the sketch
//! algorithms that serve the intent, what a measure's state reads from each
//! row and the update each row contributes, the column a top-k ranks, and
//! the keyed-additive rule for a heap sketch that reads a larger
//! sub-expression.

use asap_types::ir::operator::agg_intent::AggIntent;
use asap_types::ir::operator::non_asap::any_measure_filtered;
use asap_types::ir::operator::Reduction;
use asap_types::ir::scalar::column_resolution::{resolve_column_ref, ResolveError};
use asap_types::ir::scalar::expr_ir::ColumnRef;
use asap_types::ir::schema::{DataType, Field, FieldDataType, Schema};
use asap_types::ir::{NonASAPOp, Operator, OperatorResultKind, Predicate, ScalarExpr};
use asap_types::stages::{
    ranked_column_of_group, raw_data_samples_of_metric_of_group, Memo, MemoGroupId,
};
use asap_types::summary_type::{
    EntityIdentity, NonNegativeWeightProof, SketchAlgorithm, SketchStatistic, SummaryInputExpr,
    SummaryTypeCapability, SummaryUpdate, WeightDomain,
};

use crate::computation_targets::nested_aggregates;
use crate::{expression, schema, PerComputationCandidateError};

/// The sketch algorithms the planner sizes and offers, in the order Pass 1
/// offers those that answer an intent's statistic: the specialized algorithms
/// before the universal UnivMon.
const ALGORITHM_PREFERENCE: &[SketchAlgorithm] = &[
    SketchAlgorithm::Kll,
    SketchAlgorithm::DDSketch,
    SketchAlgorithm::Hll,
    SketchAlgorithm::Kmv,
    SketchAlgorithm::Cms,
    SketchAlgorithm::CountSketch,
    SketchAlgorithm::CmsWithHeap,
    SketchAlgorithm::CountSketchWithHeap,
    SketchAlgorithm::UnivMon,
];

/// The sketch algorithms that can serve an intent, most-preferred first:
/// those whose capability table row answers the intent's statistic, in
/// preference order, less UnivMon for a distinct-tuple
/// count. Empty for an intent no sketch
/// statistic serves.
pub(crate) fn summary_candidates(intent: &AggIntent) -> Vec<SketchAlgorithm> {
    let Some(statistic) = SketchStatistic::for_intent(intent) else {
        return Vec::new();
    };
    // A distinct-tuple count hashes the whole tuple as one item
    // (`SummaryInputExpr::Tuple`), which the distinct-count sketches take
    // unchanged. UnivMon is dropped there: it estimates frequency moments
    // over a single value stream, and would be fed one column of the tuple.
    let tuple = matches!(intent, AggIntent::Cardinality { cols, .. } if cols.len() > 1);
    ALGORITHM_PREFERENCE
        .iter()
        .filter(|algorithm| {
            SummaryTypeCapability::of(&FieldDataType::Sketch(**algorithm)).answers(&statistic)
        })
        .filter(|algorithm| !(tuple && **algorithm == SketchAlgorithm::UnivMon))
        .cloned()
        .collect()
}

/// The column a top-k ranks group `child` by ([`ranked_column_of_group`]),
/// as a reference: the sample value when it is that column, else the column
/// by name.
pub(crate) fn ranked_value(memo: &Memo, child: MemoGroupId) -> Result<ColumnRef, ResolveError> {
    let ranked = ranked_column_of_group(memo, child)?;
    Ok(
        match resolve_column_ref(&ColumnRef::SampleValue, schema(memo, child)) {
            Ok(value) if value == ranked => ColumnRef::SampleValue,
            _ => schema(memo, child).fields[ranked].column_ref(),
        },
    )
}

/// What the summary consumes per input row. An intent that reads one column
/// feeds that column by name, or the sample value when it reads none or its
/// column is outside the child schema; `COUNT(DISTINCT a, b)` feeds the whole
/// tuple as one item, so the distinct-count sketch hashes `(a, b)` rather
/// than `a` — the difference between tuple cardinality and single-column
/// cardinality.
///
/// A tuple leg outside the child schema gives `None` rather than the
/// sample-value fallback: a leg has no sample-value reading, and silently
/// dropping one would under-count.
pub(crate) fn summarised_input(
    intent: &AggIntent,
    child_schema: &Schema,
) -> Option<SummaryInputExpr> {
    let cols = intent.input_cols();
    if cols.len() < 2 {
        let column = cols
            .first()
            .and_then(|id| child_schema.fields.get(*id))
            .map_or(ColumnRef::SampleValue, Field::column_ref);
        return Some(SummaryInputExpr::Column(column));
    }
    let legs = cols
        .iter()
        .map(|id| child_schema.fields.get(*id).map(Field::column_ref))
        .collect::<Option<Vec<_>>>()?;
    Some(SummaryInputExpr::Tuple(
        legs.into_iter().map(SummaryInputExpr::Column).collect(),
    ))
}

/// The keyed-additive rule for a heap top-k over aggregate group `child`
/// (`TopK(Count GROUP BY key)`, `TopK(Sum GROUP BY key)`): the group the heap
/// reads and its update, one keyed heap over the raw rows rather than over
/// an independently materialized per-item result. The update serves both
/// heap algorithms; whether Count-Min is admissible over its weights is plan
/// selection's question. `None` when the rule does not apply or the shape is
/// unsupported (each reason is noted where it returns).
pub(crate) fn realize_keyed_additive_summary_input(
    memo: &Memo,
    output_reduction: &Reduction,
    child: MemoGroupId,
) -> Option<(MemoGroupId, SummaryUpdate)> {
    let Operator::NonASAP(NonASAPOp::Aggregate {
        reduction,
        measures,
        filters,
        having: None,
        child: raw_child,
        ..
    }) = expression(memo, child)
    else {
        return None;
    };
    let raw_child = *raw_child;
    // The heap sketch consumes every raw row; a filtered measure counts only
    // some of them.
    if any_measure_filtered(filters) {
        return None;
    }
    // A grouped sum of per-series `rate`/`increase`/`irate` values: the
    // metric they derive from, whose declared type decides their sign.
    let derivative_of = match (measures.as_slice(), expression(memo, raw_child)) {
        (
            [AggIntent::Sum { .. }],
            Operator::NonASAP(NonASAPOp::Aggregate {
                measures: derivative,
                child: samples,
                ..
            }),
        ) if matches!(
            derivative.as_slice(),
            [AggIntent::Rate | AggIntent::Increase | AggIntent::IRate]
        ) =>
        {
            Some(raw_data_samples_of_metric_of_group(memo, *samples))
        }
        _ => None,
    };
    let counter_input = derivative_of.is_some();
    // `sum by (G) (sum_over_time(m[W]))` or `... count_over_time ...`: the
    // grouped sum of per-series sums (counts) of raw data samples is their sum
    // (count) by `G`, so the heap reads those samples.
    let samples = summed_per_series_samples(memo, child);
    // A raw data sample's count or a count's row weighs one.
    let counted = match &samples {
        Some((_, inner)) => matches!(inner, AggIntent::Count),
        None => matches!(measures.as_slice(), [AggIntent::Count]),
    };
    let weight = match measures.as_slice() {
        _ if counted => SummaryInputExpr::Constant(1.0),
        [AggIntent::Sum { .. }] if counter_input || samples.is_some() => {
            SummaryInputExpr::Column(ColumnRef::SampleValue)
        }
        [AggIntent::Sum { col }] => SummaryInputExpr::Column(match col {
            None => ColumnRef::SampleValue,
            Some(index) => match schema(memo, raw_child)
                .fields
                .get(*index)
                .map(Field::column_ref)
            {
                Some(column) => column,
                None => {
                    // Unsupported: sum-ranked Top-K value column is outside the raw input schema.
                    return None;
                }
            },
        }),
        _ => return None,
    };
    let weight_domain = match derivative_of {
        _ if counted => WeightDomain::NonNegative {
            proof: NonNegativeWeightProof::UnitCount,
        },
        Some(Some(metric)) => WeightDomain::CounterDerivativeOfMetric { metric },
        _ => WeightDomain::UnknownOrSigned,
    };
    let subpopulation_columns = match output_reduction {
        Reduction::PerEntity => vec![],
        Reduction::Reduce(keys) => keys
            .iter()
            .filter_map(|index| {
                schema(memo, child)
                    .fields
                    .get(*index)
                    .map(Field::column_ref)
            })
            .collect(),
    };
    let item = match reduction {
        Reduction::PerEntity => SummaryInputExpr::EntityIdentity(EntityIdentity::PromqlLabelSet {
            excluding: subpopulation_columns,
        }),
        Reduction::Reduce(keys) if !keys.is_without() && !keys.is_empty() => {
            let Some(columns) = keys
                .iter()
                .map(|index| {
                    schema(memo, raw_child)
                        .fields
                        .get(*index)
                        .map(Field::column_ref)
                })
                .collect::<Option<Vec<_>>>()
            else {
                // Unsupported: ranked item column is outside the raw input schema.
                return None;
            };
            let item_columns: Vec<_> = columns
                .into_iter()
                .filter(|column| !subpopulation_columns.contains(column))
                .collect();
            match item_columns.as_slice() {
                [] => {
                    // Unsupported: subpopulation columns consume the complete ranked item identity.
                    return None;
                }
                [column] => SummaryInputExpr::Column(column.clone()),
                _ => SummaryInputExpr::Tuple(
                    item_columns
                        .into_iter()
                        .map(SummaryInputExpr::Column)
                        .collect(),
                ),
            }
        }
        Reduction::Reduce(_) => {
            // Unsupported: an empty or without grouping does not identify ranked items.
            return None;
        }
    };
    Some((
        samples.map_or(raw_child, |(rows, _)| rows),
        SummaryUpdate {
            item: Some(item),
            weight,
            weight_domain,
        },
    ))
}

/// The raw data sample rows and the per-series `sum` or `count` over them when
/// `grouped` is a grouped `sum` of the value of a per-series `sum` or
/// `count` of raw data samples (`sum by (G) (sum_over_time(m[W]))`), read
/// directly ([`nested_aggregates`]); `None` otherwise. The grouped sum is
/// then the sum (count) of those samples by `G`.
fn summed_per_series_samples(
    memo: &Memo,
    grouped: MemoGroupId,
) -> Option<(MemoGroupId, AggIntent)> {
    let nested = nested_aggregates(memo, grouped)?;
    if nested.renaming.is_some()
        || !matches!(nested.outer_measure, AggIntent::Sum { .. })
        || !matches!(nested.inner_reduction, Reduction::PerEntity)
        || !matches!(
            nested.inner_measure,
            AggIntent::Sum { col: None } | AggIntent::Count
        )
        || matches!(
            expression(memo, nested.rows),
            Operator::NonASAP(NonASAPOp::Aggregate { .. })
        )
    {
        return None;
    }
    Some((nested.rows, nested.inner_measure.clone()))
}

/// `update` with the open requirement that its weights are samples of
/// `metric` ([`WeightDomain::RawDataSamplesOfMetric`]), when it is a keyed heap
/// update whose weights are not otherwise proven.
pub(crate) fn with_raw_data_samples_requirement(
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

/// What a measure's state reads from each row of `input`, resolvable there.
/// A SQL `count` reads [`counted_column`]; any other measure, a PromQL count
/// included, reads its input column (the sample value when it names none).
pub(crate) fn measure_input(
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
/// ([`measure_input`]): a heap sketch ranks one item per row (the series
/// identity, the columns outside the partition and the ranked value, or the
/// PromQL label set outside the partition), weighted by the ranked value;
/// UnivMon and a Count-Min count count each value once; every other summary
/// type reads the measure's input.
pub(crate) fn summary_update(
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
                // the ranked value.
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

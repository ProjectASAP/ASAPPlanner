//! How a summary realizes an aggregate intent in Pass 1: the sketch
//! algorithms that serve the intent, the column or tuple a summary
//! consumes, the column a top-k ranks, and the keyed-additive rule for a
//! heap sketch that reads a larger sub-expression.

use asap_types::ir::operator::agg_intent::AggIntent;
use asap_types::ir::operator::non_asap::any_measure_filtered;
use asap_types::ir::operator::Reduction;
use asap_types::ir::scalar::column_resolution::{resolve_column_ref, ResolveError};
use asap_types::ir::scalar::expr_ir::ColumnRef;
use asap_types::ir::schema::{Field, FieldDataType, Schema};
use asap_types::ir::{NonASAPOp, Operator};
use asap_types::stages::{
    ranked_column_of_group, raw_data_samples_of_metric_of_group, Memo, MemoGroupId,
};
use asap_types::summary_type::{
    EntityIdentity, NonNegativeWeightProof, SketchAlgorithm, SummaryInputExpr,
    SummaryTypeCapability, SummaryUpdate, WeightDomain,
};

use asap_accuracy::sizing::sketch_statistic;

use crate::{expression, schema};

/// The sketch algorithms the planner sizes and offers, in the order Pass 1
/// offers those that answer an intent's statistic: the specialized algorithms
/// before the universal UnivMon.
pub(crate) const ALGORITHM_PREFERENCE: &[SketchAlgorithm] = &[
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
    candidates_by(intent, |algorithm| {
        SummaryTypeCapability::of(&FieldDataType::Sketch(*algorithm))
    })
}

/// [`summary_candidates`] with each algorithm's table row read from `capability`.
pub(crate) fn candidates_by(
    intent: &AggIntent,
    capability: impl Fn(&SketchAlgorithm) -> SummaryTypeCapability,
) -> Vec<SketchAlgorithm> {
    let Some(statistic) = sketch_statistic(intent) else {
        return Vec::new();
    };
    // A distinct-tuple count hashes the whole tuple as one item
    // (`SummaryInputExpr::Tuple`), which the distinct-count sketches take
    // unchanged. UnivMon is dropped there: it estimates frequency moments
    // over a single value stream, and `realize_value_frequency_summary_input`
    // would feed it one column of the tuple.
    let tuple = matches!(intent, AggIntent::Cardinality { cols, .. } if cols.len() > 1);
    ALGORITHM_PREFERENCE
        .iter()
        .filter(|algorithm| capability(algorithm).answers(&statistic))
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
    // some of them (#466).
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
/// `grouped` is a grouped `sum` (not by `without`, nor by the metric name
/// the per-series result drops) of the value of a per-series `sum` or
/// `count` of raw data samples (`sum by (G) (sum_over_time(m[W]))`), neither
/// filtered nor with `HAVING`; `None` otherwise. The grouped sum is then the
/// sum (count) of those samples by `G`.
fn summed_per_series_samples(
    memo: &Memo,
    grouped: MemoGroupId,
) -> Option<(MemoGroupId, AggIntent)> {
    let Operator::NonASAP(NonASAPOp::Aggregate {
        reduction: Reduction::Reduce(by),
        measures,
        filters,
        having: None,
        child: per_series,
        ..
    }) = expression(memo, grouped)
    else {
        return None;
    };
    let [AggIntent::Sum { col }] = measures.as_slice() else {
        return None;
    };
    let Operator::NonASAP(NonASAPOp::Aggregate {
        reduction: Reduction::PerEntity,
        measures: inner,
        filters: inner_filters,
        having: None,
        child: rows,
        ..
    }) = expression(memo, *per_series)
    else {
        return None;
    };
    let [inner @ (AggIntent::Sum { col: None } | AggIntent::Count)] = inner.as_slice() else {
        return None;
    };
    if by.is_without()
        || any_measure_filtered(filters)
        || any_measure_filtered(inner_filters)
        || matches!(
            expression(memo, *rows),
            Operator::NonASAP(NonASAPOp::Aggregate { .. })
        )
    {
        return None;
    }
    // The grouped sum reads the per-series value.
    let value = resolve_column_ref(&ColumnRef::SampleValue, schema(memo, *per_series)).ok()?;
    if col.is_some_and(|col| col != value) || by.keys().contains(&value) {
        return None;
    }
    if by.keys().iter().any(|&key| {
        schema(memo, *per_series)
            .fields
            .get(key)
            .is_none_or(|field| field.name == "__name__")
    }) {
        return None;
    }
    Some((*rows, inner.clone()))
}

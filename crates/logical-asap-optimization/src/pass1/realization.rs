//! Summary realization helpers shared by Pass 1 (`computation_targets`)
//! and Pass 2 (`asap_aware_cse`): which sketch kinds can serve an intent,
//! the column/tuple a summary consumes, the column a top-k ranks, and the
//! keyed-additive rule for a heap sketch that reads a larger sub-DAG.

use asap_types::ir::operator::agg_intent::AggIntent;
use asap_types::ir::operator::Reduction;
use asap_types::ir::scalar::column_resolution::resolve_column_ref;
use asap_types::ir::scalar::expr_ir::ColumnRef;
use asap_types::ir::schema::{Field, FieldDataType};
use asap_types::ir::{NonASAPOp, OperatorNode};
#[cfg(test)]
use asap_types::ir::{Operator, ScalarExpr};
use asap_types::summary_type::{
    EntityIdentity, NonNegativeWeightProof, SketchAlgorithm, SummaryInputExpr,
    SummaryTypeCapability, SummaryUpdate, WeightDomain,
};
use std::rc::Rc;

use asap_accuracy::sizing::sketch_statistic;

/// The sketch families the planner sizes and offers, in the order Pass 1
/// offers those that answer an intent's statistic: the specialized families
/// before the universal UnivMon.
const FAMILY_PREFERENCE: &[SketchAlgorithm] = &[
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
fn candidates_by(
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
    FAMILY_PREFERENCE
        .iter()
        .filter(|family| capability(family).answers(&statistic))
        .filter(|family| !(tuple && **family == SketchAlgorithm::UnivMon))
        .cloned()
        .collect()
}

/// The physical input consumed by one summary realization. Most summaries
/// consume the logical aggregate's immediate child and summarize its declared
/// input value. Composite realizations can instead consume a larger
/// logical sub-DAG and bind a different key or value.
pub(crate) struct PhysicalSummaryInput {
    pub(crate) child: Rc<OperatorNode>,
    pub(crate) input: SummaryUpdate,
}

/// Realize the composite heavy-hitter realization for
/// `TopK(Count GROUP BY key)`. The heap sketch consumes the raw keyed stream;
/// it does not consume an independently materialized Count result. `None`
/// when the rule does not apply or the shape is unsupported (each reason is
/// noted where it returns).
pub(crate) fn realize_keyed_additive_summary_input(
    intent: &AggIntent,
    family: &FieldDataType,
    output_reduction: &Reduction,
    child: &Rc<OperatorNode>,
) -> Option<PhysicalSummaryInput> {
    if !matches!(intent, AggIntent::TopK { .. }) {
        return None;
    }
    let FieldDataType::Sketch(kind) = family else {
        return None;
    };
    let heap_algorithm = kind;
    if !matches!(
        heap_algorithm,
        SketchAlgorithm::CmsWithHeap | SketchAlgorithm::CountSketchWithHeap
    ) {
        return None;
    }
    let Some(NonASAPOp::Aggregate {
        reduction,
        measures,
        filters,
        having: None,
        child: raw_child,
        ..
    }) = child.non_asap()
    else {
        return None;
    };
    // The heap sketch consumes every raw row; a filtered measure counts only
    // some of them (#466).
    if asap_types::ir::operator::non_asap::any_measure_filtered(filters) {
        return None;
    }
    // A grouped sum of per-series `rate`/`increase`/`irate` values: the
    // metric they derive from, whose declared type decides their sign.
    let derivative_of = match (measures.as_slice(), raw_child.non_asap()) {
        (
            [AggIntent::Sum { .. }],
            Some(NonASAPOp::Aggregate {
                measures: derivative,
                child: samples,
                ..
            }),
        ) if matches!(
            derivative.as_slice(),
            [AggIntent::Rate | AggIntent::Increase | AggIntent::IRate]
        ) =>
        {
            Some(asap_accuracy::propagation::raw_data_samples_of_metric(
                samples,
            ))
        }
        _ => None,
    };
    let counter_input = derivative_of.is_some();
    // `sum by (G) (sum_over_time(m[W]))` or `... count_over_time ...`: the
    // grouped sum of per-series sums (counts) of raw data samples is their sum
    // (count) by `G`, so the heap reads those samples.
    let samples = summed_per_series_samples(child);
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
            Some(index) => match raw_child.schema.fields.get(*index).map(Field::column_ref) {
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
    if matches!(heap_algorithm, SketchAlgorithm::CmsWithHeap)
        && !matches!(
            weight_domain,
            WeightDomain::NonNegative { .. } | WeightDomain::CounterDerivativeOfMetric { .. }
        )
    {
        // Unsupported: value-weighted CMS requires non-negative update evidence; use CountSketch for arbitrary values.
        return None;
    }
    let subpopulation_columns = match output_reduction {
        Reduction::PerEntity => vec![],
        Reduction::Reduce(keys) => keys
            .iter()
            .filter_map(|index| child.schema.fields.get(*index).map(Field::column_ref))
            .collect(),
    };
    let item = match reduction {
        Reduction::PerEntity => SummaryInputExpr::EntityIdentity(EntityIdentity::PromqlLabelSet {
            excluding: subpopulation_columns,
        }),
        Reduction::Reduce(keys) if !keys.is_without() && !keys.is_empty() => {
            let Some(columns) = keys
                .iter()
                .map(|index| raw_child.schema.fields.get(*index).map(Field::column_ref))
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
    Some(PhysicalSummaryInput {
        child: samples.map_or_else(|| Rc::clone(raw_child), |(rows, _)| rows),
        input: SummaryUpdate {
            item: Some(item),
            weight,
            weight_domain,
        },
    })
}

/// The raw data sample rows and the per-series `sum` or `count` over them when
/// `grouped` is a grouped `sum` (not by `without`, nor by the metric name
/// the per-series result drops) of the value of a per-series `sum` or
/// `count` of raw data samples (`sum by (G) (sum_over_time(m[W]))`), neither
/// filtered nor with `HAVING`; `None` otherwise. The grouped sum is then the
/// sum (count) of those samples by `G`.
fn summed_per_series_samples(grouped: &OperatorNode) -> Option<(Rc<OperatorNode>, AggIntent)> {
    use asap_types::ir::operator::non_asap::any_measure_filtered;
    let Some(NonASAPOp::Aggregate {
        reduction: Reduction::Reduce(by),
        measures,
        filters,
        having: None,
        child: per_series,
        ..
    }) = grouped.non_asap()
    else {
        return None;
    };
    let [AggIntent::Sum { col }] = measures.as_slice() else {
        return None;
    };
    let Some(NonASAPOp::Aggregate {
        reduction: Reduction::PerEntity,
        measures: inner,
        filters: inner_filters,
        having: None,
        child: rows,
        ..
    }) = per_series.non_asap()
    else {
        return None;
    };
    let [inner @ (AggIntent::Sum { col: None } | AggIntent::Count)] = inner.as_slice() else {
        return None;
    };
    if by.is_without()
        || any_measure_filtered(filters)
        || any_measure_filtered(inner_filters)
        || matches!(rows.non_asap(), Some(NonASAPOp::Aggregate { .. }))
    {
        return None;
    }
    // The grouped sum reads the per-series value.
    let value = resolve_column_ref(&ColumnRef::SampleValue, &per_series.schema).ok()?;
    if col.is_some_and(|col| col != value) || by.keys().contains(&value) {
        return None;
    }
    if by.keys().iter().any(|&key| {
        per_series
            .schema
            .fields
            .get(key)
            .is_none_or(|field| field.name == "__name__")
    }) {
        return None;
    }
    Some((Rc::clone(rows), inner.clone()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::metric_scan;
    use asap_types::ir::scalar::{CompareOpKind, ScalarValue};
    use asap_types::ir::{ExprSemantics, Predicate};

    /// A heap sketch consumes every raw row, so the keyed input rule refuses
    /// a TopK over a filtered count.
    #[test]
    fn keyed_heap_input_refuses_a_filtered_count() {
        let count_by_job = |filter: Option<Predicate>| {
            OperatorNode::new_shared(Operator::NonASAP(NonASAPOp::Aggregate {
                reduction: Reduction::by(vec![2]),
                measures: vec![AggIntent::Count],
                output_names: vec![],
                filters: vec![filter],
                having: None,
                child: metric_scan(&["job"]),
            }))
            .unwrap()
        };
        let intent = AggIntent::TopK { k: 5 };
        let family = FieldDataType::Sketch(SketchAlgorithm::CmsWithHeap);
        let output = Reduction::Reduce(asap_types::ir::operator::arguments::GroupKeys::none());
        assert!(realize_keyed_additive_summary_input(
            &intent,
            &family,
            &output,
            &count_by_job(None)
        )
        .is_some());
        let value_above_one = Predicate(ScalarExpr::Compare {
            left: Box::new(ScalarExpr::Column(1)),
            op: CompareOpKind::Gt,
            right: Box::new(ScalarExpr::Literal(ScalarValue::Float64(1.0))),
            semantics: ExprSemantics::Promql,
        });
        assert!(realize_keyed_additive_summary_input(
            &intent,
            &family,
            &output,
            &count_by_job(Some(value_above_one))
        )
        .is_none());
    }

    /// Each approximable intent is offered the families it was offered when
    /// the list was written by hand, in the same order; a non-sketch intent
    /// is offered none.
    #[test]
    fn summary_candidates_keep_the_hand_written_lists() {
        use SketchAlgorithm as S;
        let cardinality = |cols: Vec<usize>| AggIntent::Cardinality { cols };
        let cases = [
            (
                AggIntent::Quantile { col: None, q: 0.99 },
                vec![S::Kll, S::DDSketch],
            ),
            (cardinality(vec![]), vec![S::Hll, S::Kmv, S::UnivMon]),
            (cardinality(vec![1]), vec![S::Hll, S::Kmv, S::UnivMon]),
            (cardinality(vec![1, 2]), vec![S::Hll, S::Kmv]),
            (AggIntent::FrequencyL2 { col: None }, vec![S::UnivMon]),
            (AggIntent::FrequencyEntropy { col: None }, vec![S::UnivMon]),
            (
                AggIntent::TopK { k: 10 },
                vec![S::CmsWithHeap, S::CountSketchWithHeap],
            ),
            (AggIntent::Count, vec![S::Cms, S::UnivMon]),
            (AggIntent::Sum { col: None }, vec![]),
        ];
        for (intent, want) in cases {
            assert_eq!(summary_candidates(&intent), want, "{intent:?}");
        }
    }

    /// Dropping a readout from a family's table row drops the family from
    /// the candidates of the intents that read it, with no other edit.
    #[test]
    fn a_dropped_readout_drops_the_family() {
        // Count-Min-with-heap answered from Count-Min's row: no top-k readout.
        let heap_without_top_k = |family: &SketchAlgorithm| {
            SummaryTypeCapability::of(&FieldDataType::Sketch(match family {
                SketchAlgorithm::CmsWithHeap => SketchAlgorithm::Cms,
                other => *other,
            }))
        };
        let top_k = AggIntent::TopK { k: 10 };
        assert_eq!(
            candidates_by(&top_k, heap_without_top_k),
            vec![SketchAlgorithm::CountSketchWithHeap]
        );
    }

    /// The preference order names each family it offers once.
    #[test]
    fn preference_order_names_each_family_once() {
        let mut seen = std::collections::HashSet::new();
        for family in FAMILY_PREFERENCE {
            assert!(seen.insert(family), "{family:?} twice");
        }
    }
}

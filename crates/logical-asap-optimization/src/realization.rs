//! Summary realization helpers shared by Pass 1 (`per_computation_candidates`)
//! and Pass 2 (`asap_aware_cse`): which sketch kinds can serve an intent,
//! the intent's accuracy target and `(ε, δ)` budget, default sketch sizing,
//! the column/tuple a summary consumes, the column a top-k ranks, and the
//! keyed-additive rule for a heap sketch that reads a larger sub-DAG.

use asap_types::ir::operator::agg_intent::AggIntent;
use asap_types::ir::operator::Reduction;
use asap_types::ir::properties::{AccuracyMetric, AccuracyTarget};
use asap_types::ir::scalar::column_resolution::{resolve_column_ref, ResolveError};
use asap_types::ir::scalar::expr_ir::ColumnRef;
use asap_types::ir::schema::ColumnId;
use asap_types::ir::schema::{
    EntityIdentity, Field, FieldDataType, NonNegativeWeightProof, Schema, SketchAlgorithm,
    SketchParams, SummaryInputExpr, SummaryUpdate, WeightDomain,
};
use asap_types::ir::{ASAPOp, NonASAPOp, Operator, OperatorNode, ScalarExpr};
use std::rc::Rc;

/// Confidence δ assumed when the target carries only an ε
/// (`AccuracyTarget::epsilon`): the (ε, δ)-parameterised sketches (CMS) need
/// one. `ln(1/0.01) → depth 5`, matching the conventional CMS sizing.
pub const DEFAULT_DELTA: f64 = 0.01;

/// The sketch kinds that can serve an intent, most-preferred first.
/// This is the `AggIntent → SketchAlgorithm` map of issue #98, read by
/// Pass 1's summary choices. Listed here so the candidate set has one home.
pub fn summary_candidates(intent: &AggIntent) -> &'static [SketchAlgorithm] {
    match intent {
        AggIntent::Quantile { .. } => &[SketchAlgorithm::Kll, SketchAlgorithm::DDSketch],
        // A distinct-tuple count hashes the whole tuple as one item
        // (`SummaryInputExpr::Tuple`), which the distinct-count sketches take
        // unchanged. UnivMon is dropped there: it estimates frequency moments
        // over a single value stream, and `realize_value_frequency_summary_input`
        // would feed it one column of the tuple. Theta is not offered:
        // `asap_sketchlib` has no Theta sketch to execute it; add it back
        // once the library has one.
        AggIntent::Cardinality { cols, .. } if cols.len() > 1 => {
            &[SketchAlgorithm::Hll, SketchAlgorithm::Kmv]
        }
        AggIntent::Cardinality { .. } => &[
            SketchAlgorithm::Hll,
            SketchAlgorithm::Kmv,
            SketchAlgorithm::UnivMon,
        ],
        AggIntent::FrequencyL2 { .. } | AggIntent::FrequencyEntropy { .. } => {
            &[SketchAlgorithm::UnivMon]
        }
        // Count-Sketch-with-heap is CMS-with-heap's balanced/zero-mean-error
        // alternative for the same heavy-hitter shape.
        AggIntent::TopK { .. } => &[
            SketchAlgorithm::CmsWithHeap,
            SketchAlgorithm::CountSketchWithHeap,
        ],
        AggIntent::Count { .. } => &[
            SketchAlgorithm::Cms,
            SketchAlgorithm::CountSketch,
            SketchAlgorithm::UnivMon,
        ],
        _ => &[],
    }
}

/// The [`AccuracyTarget`] an approximate-capable intent's summary is sized
/// and checked against, or `None` for every other intent (no sketch
/// candidate applies): the query's target restated in the intent's own metric
/// (#690 §4): an L1-scaled absolute error for counts and top-k frequencies,
/// a relative error for distinct counts and L2 frequency moments, an
/// absolute error for entropy, and for a quantile the query's metric or a
/// rank error when none is set. The query's own metric, set or not, is
/// checked at its root in Stage 3.
pub fn sizing_target(intent: &AggIntent) -> Option<AccuracyTarget> {
    let (target, metric) = match intent {
        AggIntent::Quantile { accuracy, .. } => (
            accuracy,
            accuracy.accuracy_metric.unwrap_or(AccuracyMetric::Rank),
        ),
        AggIntent::Cardinality { accuracy, .. } | AggIntent::FrequencyL2 { accuracy, .. } => {
            (accuracy, AccuracyMetric::Relative)
        }
        AggIntent::FrequencyEntropy { accuracy, .. } => (accuracy, AccuracyMetric::ABSOLUTE),
        AggIntent::Count { accuracy } | AggIntent::TopK { accuracy, .. } => {
            (accuracy, AccuracyMetric::ABSOLUTE_STREAM_L1)
        }
        _ => return None,
    };
    Some(target_in(target, metric))
}

/// `target`'s ε, δ and scope stated in `metric`. An exact target stays
/// exact with no metric: exact is exact in every metric.
pub(crate) fn target_in(target: &AccuracyTarget, metric: AccuracyMetric) -> AccuracyTarget {
    if target.is_exact() {
        return AccuracyTarget::exact().accuracy_scope(target.accuracy_scope);
    }
    target.clone().accuracy_metric(metric)
}

/// Resolve an [`AccuracyTarget`] into the `(eps, delta)` budget
/// [`default_size_params`] needs — one place this resolution happens, so
/// nothing can drift apart on it.
///
/// An exact target degrades to the tightest parameters for a caller that
/// resolves it anyway.
pub fn accuracy_budget(accuracy: &AccuracyTarget) -> (f64, f64) {
    if accuracy.is_exact() {
        return (f64::MIN_POSITIVE, DEFAULT_DELTA);
    }
    (
        accuracy.max_error,
        accuracy.failure_probability.unwrap_or(DEFAULT_DELTA),
    )
}

/// `asap-plan`'s built-in `SketchParams` sizing, keyed off the resolved
/// `(eps, delta)` accuracy budget.
///
/// Each formula inverts the sketch family's standard error bound to the
/// smallest parameter satisfying the target, clamped to the family's sane
/// range. A non-positive ε saturates to the clamp maximum (tightest
/// allowed).
pub fn default_size_params(
    kind: SketchAlgorithm,
    intent: &AggIntent,
    eps: f64,
    delta: f64,
) -> SketchParams {
    crate::accuracy::estimators::size_params(kind, intent, eps, delta)
}

/// The column fed into a *single-column* summary: the intent's leading
/// positional input resolved to a name against the child schema, or the PromQL
/// sample value when it reads none. Callers are responsible for only reaching
/// here with a one-column intent — [`summarised_input`] is the general form.
pub(crate) fn summarised_column(intent: &AggIntent, child_schema: &Schema) -> ColumnRef {
    match intent
        .input_cols()
        .first()
        .and_then(|id| child_schema.fields.get(*id))
    {
        Some(c) => column_ref(c),
        None => ColumnRef::SampleValue,
    }
}

/// The column a top-k ranks `child` by. Over a one-measure grouped
/// aggregate (the SQL heavy-hitter shape and PromQL `topk(k, sum by ..)`) it
/// is that measure, the last column after the group keys, whichever Stage 1
/// candidate realizes it: the aggregate itself, or a `SummaryAgg` read by
/// `FinalizeExactAccumulator` or `SummaryEstimate`, under the `Project`
/// restoring its names (a renaming) and a value `HAVING`'s `Filter`. Over
/// per-series rows (PromQL `topk by (..) (k, rows)`) it is the sample value.
pub fn ranked_column(child: &OperatorNode) -> Result<ColumnId, ResolveError> {
    let mut node = child;
    let reduction = loop {
        match &node.operator {
            // Only a renaming keeps the measure last.
            Operator::NonASAP(NonASAPOp::Project { cols, child, .. })
                if cols.len() == child.schema.fields.len()
                    && cols
                        .iter()
                        .enumerate()
                        .all(|(i, item)| item.expr == ScalarExpr::Column(i)) =>
            {
                node = child
            }
            Operator::NonASAP(NonASAPOp::Filter { child, .. })
            | Operator::ASAP(
                ASAPOp::FinalizeExactAccumulator { child }
                | ASAPOp::SummaryEstimate {
                    summary_input: child,
                    ..
                },
            ) => node = child,
            Operator::ASAP(ASAPOp::SummaryAgg { reduction, .. }) => break Some(reduction),
            Operator::NonASAP(NonASAPOp::Aggregate {
                reduction,
                measures,
                ..
            }) if measures.len() == 1 => break Some(reduction),
            _ => break None,
        }
    };
    match reduction {
        Some(Reduction::Reduce(_)) => Ok(child.schema.fields.len() - 1),
        _ => resolve_column_ref(&ColumnRef::SampleValue, &child.schema),
    }
}

/// [`ranked_column`] as a reference: the sample value when it is that
/// column, else the column by name.
pub(crate) fn ranked_value(child: &OperatorNode) -> Result<(ColumnId, ColumnRef), ResolveError> {
    let ranked = ranked_column(child)?;
    let column = match resolve_column_ref(&ColumnRef::SampleValue, &child.schema) {
        Ok(value) if value == ranked => ColumnRef::SampleValue,
        _ => column_ref(&child.schema.fields[ranked]),
    };
    Ok((ranked, column))
}

pub(crate) fn column_ref(column: &Field) -> ColumnRef {
    match &column.table {
        Some(t) => ColumnRef::Qualified {
            table: t.clone(),
            name: column.name.clone(),
        },
        None => ColumnRef::Named(column.name.clone()),
    }
}

/// What the summary consumes per input row. An intent that reads one column (or
/// none) feeds that column; `COUNT(DISTINCT a, b)` feeds the whole tuple as one
/// item, so the distinct-count sketch hashes `(a, b)` rather than `a` — the
/// difference between tuple cardinality and single-column cardinality.
///
/// A tuple leg outside the child schema is an error rather than
/// [`summarised_column`]'s sample-value fallback: a leg has no sample-value
/// reading, and silently dropping one would under-count: `None` then.
pub(crate) fn summarised_input(
    intent: &AggIntent,
    child_schema: &Schema,
) -> Option<SummaryInputExpr> {
    let cols = intent.input_cols();
    if cols.len() < 2 {
        return Some(SummaryInputExpr::Column(summarised_column(
            intent,
            child_schema,
        )));
    }
    let legs = cols
        .iter()
        .map(|id| child_schema.fields.get(*id).map(column_ref))
        .collect::<Option<Vec<_>>>()?;
    Some(SummaryInputExpr::Tuple(
        legs.into_iter().map(SummaryInputExpr::Column).collect(),
    ))
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
    let FieldDataType::Sketch(kind, _) = family else {
        return None;
    };
    let heap_algorithm = kind.algorithm();
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
            Some(crate::per_computation_candidates::raw_data_samples_of_metric(samples))
        }
        _ => None,
    };
    let counter_input = derivative_of.is_some();
    let weight = match measures.as_slice() {
        [AggIntent::Count { .. }] => SummaryInputExpr::Constant(1.0),
        [AggIntent::Sum { .. }] if counter_input => {
            SummaryInputExpr::Column(ColumnRef::SampleValue)
        }
        [AggIntent::Sum { col }] => SummaryInputExpr::Column(match col {
            None => ColumnRef::SampleValue,
            Some(index) => match schema_column_ref(raw_child, *index) {
                Some(column) => column,
                None => {
                    // Unsupported: sum-ranked Top-K value column is outside the raw input schema.
                    return None;
                }
            },
        }),
        _ => return None,
    };
    let weight_domain = match measures.as_slice() {
        [AggIntent::Count { .. }] => WeightDomain::NonNegative {
            proof: NonNegativeWeightProof::UnitCount,
        },
        _ => match derivative_of {
            Some(Some(metric)) => WeightDomain::CounterDerivativeOfMetric { metric },
            _ => WeightDomain::UnknownOrSigned,
        },
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
            .filter_map(|index| schema_column_ref(child, *index))
            .collect(),
    };
    let item = match reduction {
        Reduction::PerEntity => SummaryInputExpr::EntityIdentity(EntityIdentity::PromqlLabelSet {
            excluding: subpopulation_columns,
        }),
        Reduction::Reduce(keys) if !keys.is_without() && !keys.is_empty() => {
            let Some(columns) = keys
                .iter()
                .map(|index| schema_column_ref(raw_child, *index))
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
        child: Rc::clone(raw_child),
        input: SummaryUpdate {
            item: Some(item),
            weight,
            weight_domain,
        },
    })
}

pub(crate) fn schema_column_ref(child: &OperatorNode, index: usize) -> Option<ColumnRef> {
    let column = child.schema.fields.get(index)?;
    Some(match &column.table {
        Some(table) => ColumnRef::Qualified {
            table: table.clone(),
            name: column.name.clone(),
        },
        None => ColumnRef::Named(column.name.clone()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::metric_scan;
    use asap_types::ir::scalar::{CompareOpKind, ScalarValue};
    use asap_types::ir::schema::{GroupingStrategy, SketchKind};
    use asap_types::ir::{ExprSemantics, Predicate};

    /// A heap sketch consumes every raw row, so the keyed input rule refuses
    /// a TopK over a filtered count.
    #[test]
    fn keyed_heap_input_refuses_a_filtered_count() {
        let count_by_job = |filter: Option<Predicate>| {
            OperatorNode::new_shared(Operator::NonASAP(NonASAPOp::Aggregate {
                reduction: Reduction::by(vec![2]),
                measures: vec![AggIntent::Count {
                    accuracy: AccuracyTarget::exact(),
                }],
                output_names: vec![],
                filters: vec![filter],
                having: None,
                child: metric_scan(&["job"]),
            }))
            .unwrap()
        };
        let intent = AggIntent::TopK {
            k: 5,
            accuracy: AccuracyTarget::max_error(0.01),
        };
        let family = FieldDataType::Sketch(
            SketchKind::new(
                SketchAlgorithm::CmsWithHeap,
                SketchParams::CmsWithHeap {
                    width: 64,
                    depth: 4,
                    heap_size: 5,
                },
            ),
            GroupingStrategy::default(),
        );
        let output =
            Reduction::Reduce(asap_types::ir::operator::operator_properties::GroupKeys::none());
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

    /// A summary reads the intent's own input columns: one column by name,
    /// none as the sample value, several as one hashed tuple.
    #[test]
    fn summarised_input_reads_the_intents_columns() {
        // `metric_scan(&["job"])` → columns [ts=0, value=1, job=2].
        let scan = metric_scan(&["job"]);
        let column = |name: &str| SummaryInputExpr::Column(ColumnRef::Named(name.into()));
        let cases = [
            (vec![2], column("job")),
            (vec![1], column("value")),
            (vec![], SummaryInputExpr::Column(ColumnRef::SampleValue)),
            (
                vec![1, 2],
                SummaryInputExpr::Tuple(vec![column("value"), column("job")]),
            ),
        ];
        for (cols, want) in cases {
            let intent = AggIntent::Cardinality {
                cols: cols.clone(),
                accuracy: AccuracyTarget::max_error(0.01),
            };
            assert_eq!(
                summarised_input(&intent, &scan.schema),
                Some(want),
                "wrong summarised input for cols={cols:?}"
            );
        }
    }
}

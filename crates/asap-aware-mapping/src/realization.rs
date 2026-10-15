//! Summary realization helpers shared by Pass 1 (`per_computation_candidates`)
//! and the legacy replacement search: which sketch kinds can serve an intent,
//! the intent's accuracy target and `(ε, δ)` budget, default sketch sizing,
//! the column/tuple a summary consumes, the column a top-k ranks, and the
//! keyed-additive rule for a heap sketch that reads a larger sub-DAG.

use asap_types::ir::{
    ASAPOp, NonASAPOp, Operator, OperatorNode, ScalarExpr, SchemaDerivationError,
};
use asap_types::post_asap::{AccuracyError, ExactOperationSchemaError, ExecutionDataStateError};
use asap_types::post_asap::{
    EntityIdentity, Field, FieldDataType, NonNegativeWeightProof, Schema, SketchAlgorithm,
    SketchParams, SummaryInputExpr, SummaryUpdate, WeightDomain,
};
use asap_types::pre_asap::agg_intent::AggIntent;
use asap_types::pre_asap::column_resolution::{resolve_column_ref, ResolveError};
use asap_types::pre_asap::expr_ir::ColumnRef;
use asap_types::pre_asap::schema::ColumnId;
use asap_types::pre_asap::Reduction;
use asap_types::types::{AccuracyMetric, AccuracyTarget};
use std::rc::Rc;
use thiserror::Error;

/// Confidence δ assumed when the target carries only an ε
/// (`AccuracyTarget::epsilon`): the (ε, δ)-parameterised sketches (CMS) need
/// one. `ln(1/0.01) → depth 5`, matching the conventional CMS sizing.
pub const DEFAULT_DELTA: f64 = 0.01;

/// The sketch kinds that can serve an intent, most-preferred first.
/// This is the `AggIntent → SketchAlgorithm` map of issue #98;
/// [`realizations_for_intent`] sizes and ranks every entry via `cost_model`.
/// Listed here so the candidate set has one home.
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
/// candidate applies — [`realizations_for_intent`]'s own match routes those
/// elsewhere). It is the query's target restated in the intent's own metric
/// (#690 §4): an L1-scaled absolute error for counts and top-k frequencies,
/// a relative error for distinct counts and L2 frequency moments, an
/// absolute error for entropy, and for a quantile the query's metric or a
/// rank error when none is set. The query's own metric, set or not, is
/// checked at its root in Stage 3.
pub fn accuracy_target(intent: &AggIntent) -> Option<AccuracyTarget> {
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
/// [`CostModel::size_params`] needs. Shared by [`sketch_realizations`] and
/// this crate's own sizing — one place this resolution happens, so nothing
/// can drift apart on it.
///
/// `Exact` is unreachable via [`realizations_for_intent`] (which routes
/// `Exact` to [`exact_realization`] instead); degrades to the tightest
/// parameters for a caller that resolves it directly anyway.
pub fn accuracy_budget(accuracy: &AccuracyTarget) -> (f64, f64) {
    if accuracy.is_exact() {
        return (f64::MIN_POSITIVE, DEFAULT_DELTA);
    }
    (
        accuracy.error,
        accuracy.failure_probability.unwrap_or(DEFAULT_DELTA),
    )
}

/// `asap-plan`'s built-in `SketchParams` sizing, keyed off the resolved
/// `(eps, delta)` accuracy budget. [`CostModel::size_params`]'s default
/// body — factored out to a free function so a deployment's own
/// `CostModel` impl can still delegate to it for the candidates it
/// doesn't want to resize itself.
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
pub(crate) fn ranked_column(child: &OperatorNode) -> Result<ColumnId, ResolveError> {
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
/// reading, and silently dropping one would under-count.
pub(crate) fn summarised_input(
    intent: &AggIntent,
    child_schema: &Schema,
) -> Result<SummaryInputExpr, RealizationError> {
    let cols = intent.input_cols();
    if cols.len() < 2 {
        return Ok(SummaryInputExpr::Column(summarised_column(
            intent,
            child_schema,
        )));
    }
    let legs = cols
        .iter()
        .map(|id| child_schema.fields.get(*id).map(column_ref))
        .collect::<Option<Vec<_>>>()
        .ok_or(RealizationError::PhysicalRealization(
            "a tuple column is outside the input schema",
        ))?;
    Ok(SummaryInputExpr::Tuple(
        legs.into_iter().map(SummaryInputExpr::Column).collect(),
    ))
}

/// Errors from the pre-ASAP → post-ASAP replacement/construction path
/// (`realize_child` and `retain_exact`). Moved here from the former
/// `bind.rs` (issue #251): this is what a `ReplacementStrategy`
/// implementor's own construction path can realistically fail with —
/// schema derivation over a pre-ASAP [`OperatorNode`] sub-DAG — not
/// something specific to workload-wide orchestration.
#[derive(Debug, Error)]
pub enum RealizationError {
    /// Schema derivation failed while lifting an edge to `Schema`.
    #[error("schema derivation failed during pre-ASAP → post-ASAP binding: {0}")]
    Schema(#[from] SchemaDerivationError),
    /// The candidate is accuracy-illegal (issue #172): its composed
    /// guarantee has no sound propagation rule, or misses the applicable
    /// `AccuracyTarget`. Fail-closed — the candidate is never constructed
    /// with the child "treated as exact". [`ASAPStrategies::propose`]
    /// records it as a [`RejectedCandidate`] instead of a candidate.
    #[error("accuracy-illegal candidate: {0}")]
    Accuracy(#[from] AccuracyError),
    /// The selected summary family has a physical realization rule for this
    /// logical shape, but the rule cannot represent the complete input. The
    /// candidate must not fall back to ordinary one-node binding because that
    /// would change its semantics.
    #[error("unsupported physical summary realization: {0}")]
    PhysicalRealization(&'static str),
    /// A constructed plan violates the update/evaluation phase contract
    /// (issue #171) — e.g. a summary evaluation placed beneath a maintained
    /// `SummaryAgg`. Detected at construction, never at runtime.
    #[error("execution-data_state violation in post-ASAP plan: {0}")]
    ExecutionDataState(#[from] ExecutionDataStateError),
    /// An `ExactOperator`'s output schema could not be derived over its
    /// child — the child carries summary state the operator can't read.
    #[error("exact operator schema derivation failed: {0}")]
    ExactOperationSchema(#[from] ExactOperationSchemaError),
}

/// The physical input consumed by one summary realization. Most summaries
/// consume the logical aggregate's immediate child and summarize its declared
/// input value. Composite realizations can instead consume a larger
/// logical sub-DAG and bind a different key or value.
pub(crate) struct PhysicalSummaryInput {
    pub(crate) child: Rc<OperatorNode>,
    pub(crate) input: SummaryUpdate,
}

pub(crate) enum PhysicalSummaryInputRuleResult {
    NotApplicable,
    Realized(PhysicalSummaryInput),
    Unsupported(&'static str),
}

/// Realize the composite heavy-hitter realization for
/// `TopK(Count GROUP BY key)`. The heap sketch consumes the raw keyed stream;
/// it does not consume an independently materialized Count result.
pub(crate) fn realize_keyed_additive_summary_input(
    intent: &AggIntent,
    family: &FieldDataType,
    output_reduction: &Reduction,
    child: &Rc<OperatorNode>,
) -> PhysicalSummaryInputRuleResult {
    if !matches!(intent, AggIntent::TopK { .. }) {
        return PhysicalSummaryInputRuleResult::NotApplicable;
    }
    let FieldDataType::Sketch(kind, _) = family else {
        return PhysicalSummaryInputRuleResult::NotApplicable;
    };
    let heap_algorithm = kind.algorithm();
    if !matches!(
        heap_algorithm,
        SketchAlgorithm::CmsWithHeap | SketchAlgorithm::CountSketchWithHeap
    ) {
        return PhysicalSummaryInputRuleResult::NotApplicable;
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
        return PhysicalSummaryInputRuleResult::NotApplicable;
    };
    // The heap sketch consumes every raw row; a filtered measure counts only
    // some of them (#466).
    if asap_types::ir::non_asap::any_measure_filtered(filters) {
        return PhysicalSummaryInputRuleResult::NotApplicable;
    }
    let counter_input = matches!(measures.as_slice(), [AggIntent::Sum { .. }])
        && matches!(raw_child.non_asap(), Some(NonASAPOp::Aggregate { measures, .. })
            if matches!(measures.as_slice(), [AggIntent::Rate | AggIntent::Increase]));
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
                    return PhysicalSummaryInputRuleResult::Unsupported(
                        "sum-ranked Top-K value column is outside the raw input schema",
                    )
                }
            },
        }),
        _ => return PhysicalSummaryInputRuleResult::NotApplicable,
    };
    let weight_domain = match measures.as_slice() {
        [AggIntent::Count { .. }] => WeightDomain::NonNegative {
            proof: NonNegativeWeightProof::UnitCount,
        },
        [AggIntent::Sum { .. }] if counter_input => WeightDomain::NonNegative {
            proof: NonNegativeWeightProof::ResetAwareCounterDerivative,
        },
        _ => WeightDomain::UnknownOrSigned,
    };
    if matches!(heap_algorithm, SketchAlgorithm::CmsWithHeap)
        && !matches!(weight_domain, WeightDomain::NonNegative { .. })
    {
        return PhysicalSummaryInputRuleResult::Unsupported(
            "value-weighted CMS requires non-negative update evidence; use CountSketch for arbitrary values",
        );
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
                return PhysicalSummaryInputRuleResult::Unsupported(
                    "ranked item column is outside the raw input schema",
                );
            };
            let item_columns: Vec<_> = columns
                .into_iter()
                .filter(|column| !subpopulation_columns.contains(column))
                .collect();
            match item_columns.as_slice() {
                [] => {
                    return PhysicalSummaryInputRuleResult::Unsupported(
                        "subpopulation columns consume the complete ranked item identity",
                    )
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
            return PhysicalSummaryInputRuleResult::Unsupported(
                "an empty or without grouping does not identify ranked items",
            )
        }
    };
    PhysicalSummaryInputRuleResult::Realized(PhysicalSummaryInput {
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

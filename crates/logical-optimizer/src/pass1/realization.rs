//! How one aggregate intent may be realized, and the rules Stage 1 uses to
//! size a summary and bind its input: the summary families per intent, the
//! accuracy budget a target resolves to, and the update a summary reads.

use std::rc::Rc;

use asap_types::ir::operator::{AggIntent, Reduction};
use asap_types::ir::scalar::ColumnRef;
use asap_types::ir::schema::{
    EntityIdentity, ExactKind, ExactParams, Field, FieldDataType, NonNegativeWeightProof,
    SamplingKind, SamplingParams, Schema, SketchAlgorithm, SketchKind, SketchParams, StatModelKind,
    StatModelParams, SummaryInputExpr, SummaryUpdate, WaveletKind, WaveletParams, WeightDomain,
};
use asap_types::ir::{NonASAPOp, OperatorNode};
use asap_types::types::AccuracyTarget;

/// How an [`AggIntent`] may be realised at post-ASAP binding time (issue
/// #98): by an approximate summary (sketch, sample, wavelet, statistical
/// model, …), by an exact mergeable accumulator, or by an ordinary exact
/// operator (pass-through). This is a post-ASAP concern — the pre-ASAP IR
/// carries only the intent + accuracy target, never the realization — and
/// it's a per-node decision, made once per `AggIntent`, not a plan-wide one.
///
/// Stage 1 lists every realization of a target
/// ([`local_realizations_for_intent`](crate::pass1::logical_candidates::local_realizations_for_intent)).
#[derive(Debug, Clone, PartialEq)]
pub enum Realization {
    /// An exact **mergeable** accumulator (partial state ≡ the value
    /// itself: `Sum` / `Count` / `Min` / `Max` / `Rate` / `Increase`). The
    /// built state *is* the answer already — no `SummaryEstimate` evaluation
    /// step.
    ExactAggregate {
        kind: ExactKind,
        params: ExactParams,
    },
    /// An approximate sketch sized to the intent's [`AccuracyTarget`].
    /// Needs a `SummaryEstimate` evaluation to recover a value. Already
    /// classified into its [`SketchKind`] category (`SketchKind::new`
    /// having been called) — construction always goes through that
    /// classifier, never this variant directly.
    Sketch(SketchKind),
    /// A sampling-based summary (a retained row subset). Needs a
    /// `SummaryEstimate` evaluation. Not chosen by any core `AggIntent`
    /// dispatch today — see the module docs.
    Sample {
        kind: SamplingKind,
        params: SamplingParams,
    },
    /// A wavelet-transform summary. Needs a `SummaryEstimate` evaluation. Not
    /// chosen by any core `AggIntent` dispatch today — see the module docs.
    Wavelet {
        kind: WaveletKind,
        params: WaveletParams,
    },
    /// A fitted statistical/parametric-model summary. Needs a
    /// `SummaryEstimate` evaluation. Not chosen by any core `AggIntent`
    /// dispatch today — see the module docs.
    StatModel {
        kind: StatModelKind,
        params: StatModelParams,
    },
    /// No summary form — the node stays a logical pre-ASAP operator and is
    /// executed exactly (per-series transforms, non-mergeable reducers, exact
    /// quantile/top-k/cardinality, classic-bucket `HistogramQuantile`, …).
    PassThrough,
}

/// Confidence δ assumed when the target carries only an ε
/// (`AccuracyTarget::Epsilon`): the (ε, δ)-parameterised sketches (CMS) need
/// one. `ln(1/0.01) → depth 5`, matching the conventional CMS sizing.
pub const DEFAULT_DELTA: f64 = 0.01;

/// The sketch kinds that can serve an intent, most-preferred first.
/// This is the `AggIntent → SketchAlgorithm` map of issue #98;
/// Stage 1 sizes every entry to the target's accuracy. Listed here so the
/// candidate set has one home.
pub fn summary_candidates(intent: &AggIntent) -> &'static [SketchAlgorithm] {
    match intent {
        AggIntent::Quantile { .. } => &[SketchAlgorithm::Kll, SketchAlgorithm::DDSketch],
        // A distinct-tuple count hashes the whole tuple as one item
        // (`SummaryInputExpr::Tuple`), which the distinct-count sketches take
        // unchanged. UnivMon is dropped there: it estimates frequency moments
        // over a single value stream, and `realize_value_frequency_summary_input`
        // would feed it one column of the tuple.
        AggIntent::Cardinality { cols, .. } if cols.len() > 1 => &[
            SketchAlgorithm::Hll,
            SketchAlgorithm::Theta,
            SketchAlgorithm::Kmv,
        ],
        AggIntent::Cardinality { .. } => &[
            SketchAlgorithm::Hll,
            SketchAlgorithm::Theta,
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

/// The [`AccuracyTarget`] threaded onto an approximate-capable intent
/// (`Quantile`/`Cardinality`/`Count`/`TopK`), or `None` for every other
/// intent (no sketch candidate applies).
pub fn accuracy_target(intent: &AggIntent) -> Option<&AccuracyTarget> {
    match intent {
        AggIntent::Quantile { accuracy, .. }
        | AggIntent::Cardinality { accuracy, .. }
        | AggIntent::FrequencyL2 { accuracy, .. }
        | AggIntent::FrequencyEntropy { accuracy, .. }
        | AggIntent::Count { accuracy }
        | AggIntent::TopK { accuracy, .. } => Some(accuracy),
        _ => None,
    }
}

/// Resolve an [`AccuracyTarget`] into the `(eps, delta)` budget sketch
/// sizing needs — one place this resolution happens, so nothing can drift
/// apart on it.
///
/// `Exact` has no sketch realization; it degrades to the tightest parameters
/// for a caller that resolves it directly anyway.
pub fn accuracy_budget(accuracy: &AccuracyTarget) -> (f64, f64) {
    match accuracy {
        AccuracyTarget::Exact => (f64::MIN_POSITIVE, DEFAULT_DELTA),
        AccuracyTarget::Epsilon(e) => (*e, DEFAULT_DELTA),
        AccuracyTarget::EpsilonDelta { epsilon, delta } => (*epsilon, *delta),
    }
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
        having: None,
        child: raw_child,
        ..
    }) = child.non_asap()
    else {
        return PhysicalSummaryInputRuleResult::NotApplicable;
    };
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
) -> Result<SummaryInputExpr, &'static str> {
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
        .ok_or("a tuple column is outside the input schema")?;
    Ok(SummaryInputExpr::Tuple(
        legs.into_iter().map(SummaryInputExpr::Column).collect(),
    ))
}

/// Whether `reduction` has a genuine subpopulation concept for
/// `GroupingStrategy::SharedMultiSubpopulation` to multiplex across — the
/// non-empty-`by` legality condition issue #256 requires.
///
/// - [`Reduction::PerEntity`]: no grouping concept at all (never merges
///   across entities) — `false`.
/// - [`Reduction::Reduce`] with an empty, non-`without` `by`: a genuine full
///   reduction, one output row, no subpopulations — `false`.
/// - [`Reduction::Reduce`] with a non-empty `by`, or any `without(...)`
///   exclusion grouping (which groups by whatever labels remain, even
///   `without([])` — "group by every label"): a real subpopulation concept
///   — `true`.
pub fn has_subpopulations(reduction: &Reduction) -> bool {
    match reduction.group_keys() {
        None => false,
        Some(keys) => keys.is_without() || !keys.is_empty(),
    }
}

/// Would a build sized to `tighter`'s accuracy requirement also satisfy
/// `looser`'s? A tighter `(eps, delta)` bound implies the looser one, so
/// this is a Pareto check: both
/// sides resolve through [`accuracy_budget`] to concrete `(eps, delta)`
/// numbers, and `tighter` dominates `looser` iff neither of its two numbers
/// is larger.
///
/// `AccuracyTarget::Exact` on either side always returns `false` — never a
/// dominator, never dominated. Numerically, `accuracy_budget(Exact)`
/// resolves to a budget that would Pareto-dominate everything (zero error),
/// but `Exact` is realized exactly, not by a sketch — a different
/// `Realization` family, not a point on the same sizing curve.
pub(crate) fn dominates(tighter: &AccuracyTarget, looser: &AccuracyTarget) -> bool {
    if matches!(tighter, AccuracyTarget::Exact) || matches!(looser, AccuracyTarget::Exact) {
        return false;
    }
    let (tighter_eps, tighter_delta) = accuracy_budget(tighter);
    let (looser_eps, looser_delta) = accuracy_budget(looser);
    tighter_eps <= looser_eps && tighter_delta <= looser_delta
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn per_entity_has_no_subpopulation_concept() {
        assert!(!has_subpopulations(&Reduction::PerEntity));
    }

    #[test]
    fn empty_by_reduction_has_no_subpopulation_concept() {
        assert!(!has_subpopulations(&Reduction::by(vec![])));
    }

    #[test]
    fn non_empty_by_reduction_has_a_subpopulation_concept() {
        assert!(has_subpopulations(&Reduction::by(vec![2])));
    }

    #[test]
    fn without_grouping_has_a_subpopulation_concept_even_when_empty() {
        use asap_types::ir::operator::operator_properties::GroupKeys;
        // `without([])` groups by every remaining label — a real
        // subpopulation concept, unlike `by([])`'s genuine full reduction.
        assert!(has_subpopulations(&Reduction::Reduce(GroupKeys::without(
            vec![]
        ))));
    }
}

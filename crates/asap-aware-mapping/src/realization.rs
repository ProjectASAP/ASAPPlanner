//! Stage 1, Pass 1 (per-computation candidate generation): how an aggregate
//! intent may be realized by a summary. These are the rules Pass 1 uses to
//! list a target's summary families, resolve the accuracy budget a target
//! asks for, size a summary, and bind the input it reads.

use asap_types::post_asap::{Field, Schema, SketchAlgorithm, SketchParams, SummaryInputExpr};
use asap_types::pre_asap::agg_intent::AggIntent;
use asap_types::pre_asap::expr_ir::ColumnRef;
use asap_types::types::AccuracyTarget;

/// Confidence δ assumed when the target carries only an ε
/// (`AccuracyTarget::Epsilon`): the (ε, δ)-parameterised sketches (CMS) need
/// one. `ln(1/0.01) → depth 5`, matching the conventional CMS sizing.
pub const DEFAULT_DELTA: f64 = 0.01;

/// The sketch kinds that can serve an intent, most-preferred first.
/// This is the `AggIntent → SketchAlgorithm` map of issue #98, listed here
/// so the candidate set has one home.
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

/// Resolve an [`AccuracyTarget`] into the `(eps, delta)` budget summary
/// sizing needs — one place this resolution happens, so nothing can drift
/// apart on it. `Exact` degrades to the tightest parameters.
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

/// The column fed into a *single-column* summary: the intent's leading
/// positional input resolved to a name against the child schema, or the PromQL
/// sample value when it reads none. Callers are responsible for only reaching
/// here with a one-column intent — [`summarised_input`] is the general form.
fn summarised_column(intent: &AggIntent, child_schema: &Schema) -> ColumnRef {
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

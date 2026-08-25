use super::sketch::{
    ExactKind, ExactParams, GroupingStrategy, SamplingKind, SamplingParams, SketchKind,
    StatModelKind, StatModelParams, WaveletKind, WaveletParams,
};
use crate::pre_asap::DataType;

// ── Post-ASAP data types ────────────────────────────────────────────────────

/// Column types that may appear on a post-ASAP DAG edge. A strict superset of
/// the pre-ASAP [`DataType`]: adds one variant per summary *family* for
/// edges that carry partial summary state between a `SummaryAgg` and a
/// downstream `SummaryEstimate` or `SummaryMerge`.
///
/// Every non-`Plain` variant carries the physical state identity required by
/// that family (`Sketch` additionally carries its grouping layout), so the
/// type system can reject merges of incompatible
/// summaries at plan construction time — a `SummaryMerge` over
/// `Sketch(Kll, …)` and `Sketch(Cms, …)` inputs is a plan-time error, and a
/// `Sketch(…)` can never be confused for a `Sample(…)` even though both are
/// "opaque summary state" at a glance.
#[derive(Debug, Clone, PartialEq)]
pub enum SummaryFamilyType {
    /// An ordinary, readable value — the same closed vocabulary as the
    /// pre-ASAP `DataType` (`Int64`/`Float64`/`Utf8`/`Bool`/`Timestamp`),
    /// passed through unchanged from a pre-ASAP edge.
    Plain(DataType),
    /// Exact, mergeable accumulator state (`Sum`/`Count`/`MinMax`/`Rate`/
    /// `Increase`) — the partial state *is* the value; no readout needed.
    ExactAggregate(ExactKind, ExactParams),
    /// Approximate sketch state (KLL/CMS/HLL/…), read out via a
    /// `SummaryEstimate`. A [`SketchKind`] already carries the concrete
    /// algorithm, params, and grouping layout committed to, not just its
    /// category — a bound node needs to know it's specifically independent
    /// KLL or shared Hydra-backed CMS, not merely "some sketch".
    Sketch(SketchKind, GroupingStrategy),
    /// Sampling-based summary state (a retained row subset).
    Sample(SamplingKind, SamplingParams),
    /// Wavelet-transform summary state (a coefficient vector).
    Wavelet(WaveletKind, WaveletParams),
    /// Fitted statistical/parametric-model summary state.
    StatModel(StatModelKind, StatModelParams),
}

// ── Post-ASAP schema ─────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct SummaryField {
    pub name: String,
    pub dtype: SummaryFamilyType,
    pub nullable: bool,
}

/// Schema carried on every edge of the post-ASAP DAG. Extends the pre-ASAP
/// `Schema` with the ability to express summary-state columns. The two are
/// separate types so a pre-ASAP node structurally cannot carry a
/// summary-state-typed column — any attempt to do so is a compile-time type
/// error.
#[derive(Debug, Clone)]
pub struct SummarySchema {
    pub fields: Vec<SummaryField>,
    /// Index into `fields` for the time axis, if any (same semantics as the
    /// pre-ASAP `Schema::time_index`).
    pub time_index: Option<usize>,
}

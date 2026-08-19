use super::sketch::{SummaryKind, SummaryParams};
use crate::pre_asap::DataType;

// ── Post-ASAP data types ────────────────────────────────────────────────────

/// Column types that may appear on a post-ASAP DAG edge. A strict superset of
/// the pre-ASAP [`DataType`]: adds `Sketch` for edges that carry partial
/// sketch state between a `SummaryAgg` and a downstream `SummaryEstimate` or
/// `SummaryMerge`.
///
/// The `Sketch` variant carries `(kind, params)` so the type system can reject
/// merges of incompatible sketches at plan construction time — a `SummaryMerge`
/// over `Sketch(Kll, …)` and `Sketch(Cms, …)` inputs is a plan-time error.
#[derive(Debug, Clone, PartialEq)]
pub enum SummaryDataType {
    /// Any base pre-ASAP column type — passed through unchanged from
    /// pre-ASAP edges.
    Primitive(DataType),
    /// Opaque summary state (exact accumulator or approximate sketch).
    /// The `(kind, params)` pair is the type identity: two summary columns
    /// are compatible only if both match exactly.
    Sketch(SummaryKind, SummaryParams),
}

// ── Post-ASAP schema ─────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct SummaryField {
    pub name: String,
    pub dtype: SummaryDataType,
    pub nullable: bool,
}

/// Schema carried on every edge of the post-ASAP DAG. Extends the pre-ASAP
/// `Schema` with the ability to express sketch-state columns. The two are
/// separate types so a pre-ASAP node structurally cannot carry a
/// `Sketch`-typed column — any attempt to do so is a compile-time type error.
#[derive(Debug, Clone)]
pub struct SummarySchema {
    pub fields: Vec<SummaryField>,
    /// Index into `fields` for the time axis, if any (same semantics as the
    /// pre-ASAP `Schema::time_index`).
    pub time_index: Option<usize>,
}

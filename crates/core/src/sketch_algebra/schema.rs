use super::sketch::{SummaryKind, SummaryParams};
use crate::intent_algebra::L3DataType;

// ── L4 data types ─────────────────────────────────────────────────────────────

/// Column types that may appear on an L4 DAG edge. A strict superset of
/// `L3DataType`: L4 adds `Sketch` for edges that carry partial sketch state
/// between a `SummaryAgg` and a downstream `SummaryEstimate` or `SummaryMerge`.
///
/// The `Sketch` variant carries `(kind, params)` so the type system can reject
/// merges of incompatible sketches at plan construction time — a `SummaryMerge`
/// over `Sketch(Kll, …)` and `Sketch(Cms, …)` inputs is a plan-time error.
#[derive(Debug, Clone, PartialEq)]
pub enum L4DataType {
    /// Any base L3 column type — passed through unchanged from L3 edges.
    Primitive(L3DataType),
    /// Opaque summary state (exact accumulator or approximate sketch).
    /// The `(kind, params)` pair is the type identity: two summary columns
    /// are compatible only if both match exactly.
    Sketch(SummaryKind, SummaryParams),
}

// ── L4 schema ─────────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct L4Field {
    pub name: String,
    pub dtype: L4DataType,
    pub nullable: bool,
}

/// Schema carried on every edge of the L4 DAG. Extends `L3Schema` with the
/// ability to express sketch-state columns. L3 and L4 schemas are separate
/// types so L3 nodes structurally cannot carry `Sketch`-typed columns — any
/// attempt to do so is a compile-time type error.
#[derive(Debug, Clone)]
pub struct L4Schema {
    pub fields: Vec<L4Field>,
    /// Index into `fields` for the time axis, if any (same semantics as
    /// `L3Schema::time_index`).
    pub time_index: Option<usize>,
}

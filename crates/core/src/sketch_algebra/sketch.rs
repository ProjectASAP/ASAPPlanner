use crate::intent_algebra::ColumnRef;

// ── Sketch algorithm identifiers ──────────────────────────────────────────────

/// Identifies a sketch algorithm family. Used as a type tag in `L4DataType`
/// and as the binding choice recorded in `SummaryExpr` nodes.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum SketchKind {
    /// KLL quantile sketch (mergeable, ε-accurate rank queries).
    Kll,
    /// Count-Min Sketch (mergeable, (ε,δ)-accurate frequency queries).
    Cms,
    /// HyperLogLog (mergeable, (ε,δ)-accurate cardinality).
    Hll,
    /// DDSketch (mergeable, relative-error quantile queries).
    DDSketch,
    /// CMS augmented with a min-heap for top-k / heavy-hitter queries.
    CmsWithHeap,
    /// K-Minimum Values sketch (mergeable, join-cardinality estimation).
    Kmv,
    /// Theta sketch (mergeable, set operations + cardinality).
    Theta,
}

// ── Sketch parameters ─────────────────────────────────────────────────────────

/// Concrete, catalog-validated parameters for a specific sketch instance.
/// The variant must correspond to the associated `SketchKind`; mismatches
/// are caught at L4 bind time before L5 ever sees the plan.
#[derive(Debug, Clone, PartialEq)]
pub enum SketchParams {
    Kll { k: u32 },
    Cms { width: u32, depth: u32 },
    Hll { precision: u8 },
    DDSketch { alpha: f64 },
    CmsWithHeap { width: u32, depth: u32, heap_size: u32 },
    Kmv { k: u32 },
    Theta { k: u32 },
}

// ── Sketch read-out queries ───────────────────────────────────────────────────

/// What to extract from a built sketch. Carried by `SummaryEstimate`.
#[derive(Debug, Clone)]
pub enum SketchQuery {
    /// Extract the value at quantile rank `q` ∈ (0, 1].
    Quantile { q: f64 },
    /// Estimated count / frequency for a specific key.
    PointCount { key: ColumnRef },
    /// Estimated number of distinct elements.
    Cardinality,
    /// Top-k most frequent (key, count) pairs.
    TopK { k: usize },
}

use asap_ir::intent_algebra::ColumnRef;

// ── Summary kind identifiers ───────────────────────────────────────────────────

/// Identifies a precomputed aggregation family — either an exact accumulator
/// or an approximate sketch. Used as a type tag in `L4DataType` and as the
/// binding choice recorded in `SummaryExpr` nodes.
///
/// `PartialOrd`/`Ord` (derived, by variant declaration order below): a
/// downstream deployment (ASAPQuery-backend's `control_plane`) needs a
/// deterministic set of families per metric — e.g. `BTreeSet<SummaryKind>`
/// — to emit reproducible YAML/JSON config. This crate has no ordering
/// opinion of its own; the derive just makes one available.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum SummaryKind {
    // ── Exact accumulators (no approximation error) ──────────────────────────
    /// Exact sum accumulator (mergeable by addition).
    Sum,
    /// Exact count accumulator (mergeable by addition).
    Count,
    /// Exact min/max accumulator (mergeable by comparison).
    MinMax,
    /// Exact increase accumulator (counter-reset-aware delta).
    Increase,
    /// Rate accumulator (increase / time window duration).
    Rate,
    // ── Approximate sketches ─────────────────────────────────────────────────
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
    /// Count-Sketch (mergeable, balanced/zero-mean-error frequency
    /// queries — an alternative to CMS's one-sided bias).
    CountSketch,
    /// Count-Sketch augmented with a min-heap for top-k / heavy-hitter
    /// queries — an alternative to `CmsWithHeap` on the Count-Sketch
    /// substrate.
    CountSketchWithHeap,
}

// ── Sketch parameters ─────────────────────────────────────────────────────────

/// Concrete, catalog-validated parameters for a specific summary instance.
/// The variant must correspond to the associated `SummaryKind`; mismatches
/// are caught at L4 bind time before L5 ever sees the plan.
/// Exact-accumulator variants carry no parameters (their semantics are fixed).
#[derive(Debug, Clone, PartialEq)]
pub enum SummaryParams {
    // Exact accumulators — no tuning parameters
    Sum,
    Count,
    MinMax,
    Increase,
    Rate,
    // Approximate sketches — algorithm-specific tuning parameters
    Kll {
        k: u32,
    },
    Cms {
        width: u32,
        depth: u32,
    },
    Hll {
        precision: u8,
    },
    DDSketch {
        alpha: f64,
    },
    CmsWithHeap {
        width: u32,
        depth: u32,
        heap_size: u32,
    },
    Kmv {
        k: u32,
    },
    Theta {
        k: u32,
    },
    CountSketch {
        width: u32,
        depth: u32,
    },
    CountSketchWithHeap {
        width: u32,
        depth: u32,
        heap_size: u32,
    },
}

// ── Sketch read-out queries ───────────────────────────────────────────────────

/// What to extract from a built sketch. Carried by `SummaryEstimate`.
#[derive(Debug, Clone)]
pub enum SketchQuery {
    /// Extract the value at quantile rank `q` ∈ (0, 1].
    Quantile { q: f64 },
    /// Estimated count / frequency. `key` names which column is being
    /// queried (`ColumnRef::SampleValue` for the bare bucket total, with
    /// `value: None`); a `Named`/`Qualified` `key` paired with
    /// `value: Some(v)` is a per-item point lookup (e.g.
    /// `count(cms_metric{item="checkout"})` — `key` is `item`, `value` is
    /// `"checkout"`). `value` is carried here rather than resolved by the
    /// `SummaryExecutor` from a `Filter` predicate because `readout`'s
    /// trait signature has no tree access — see `CostModel::readout_extension`.
    PointCount {
        key: ColumnRef,
        value: Option<String>,
    },
    /// Estimated number of distinct elements.
    Cardinality,
    /// Top-k most frequent (key, count) pairs.
    TopK { k: usize },
}

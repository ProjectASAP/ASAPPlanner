use crate::pre_asap::ColumnRef;

// ── Exact accumulators ──────────────────────────────────────────────────────

/// An exact, mergeable accumulator family — zero approximation error. The
/// partial state built for one of these *is* the answer; no
/// `SummaryEstimate` readout is needed to get a value out of it.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum ExactKind {
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
}

/// Parameters for an [`ExactKind`] accumulator. All exact accumulators have
/// fixed semantics — no tuning parameters — so each variant carries none;
/// kept as a per-kind enum (mirroring [`SketchParams`]) so a mismatched
/// `(kind, params)` pair is still a type error, not a runtime check.
#[derive(Debug, Clone, PartialEq)]
pub enum ExactParams {
    Sum,
    Count,
    MinMax,
    Increase,
    Rate,
}

// ── Approximate sketches ─────────────────────────────────────────────────────

/// A specific sketch algorithm — bounded error, sized by its
/// [`SketchParams`]. Each algorithm belongs to exactly one [`SketchKind`]
/// category (e.g. `Kll` and `DDSketch` both realize `SketchKind::Quantile`);
/// [`SketchKind::new`] is where that classification is made.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum SketchAlgorithm {
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

/// Concrete, catalog-validated parameters for a specific [`SketchAlgorithm`]
/// instance. The variant must correspond to the associated `SketchAlgorithm`;
/// mismatches are caught at post-ASAP bind time, before any later,
/// deployment-specific stage ever sees the plan.
#[derive(Debug, Clone, PartialEq)]
pub enum SketchParams {
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

/// A committed sketch choice: which *category* of query shape it answers —
/// quantile-style, cardinality-style, frequency-style, or heavy-hitter/
/// top-k-style estimation — together with the concrete [`SketchAlgorithm`]
/// and [`SketchParams`] realizing it. Sits between
/// [`SummaryFamilyType::Sketch`](super::schema::SummaryFamilyType::Sketch)
/// (the `Sketch` family as a whole, sibling to `Sample`/`Wavelet`/
/// `StatModel`) and the bare algorithm: `Kll` vs. `DDSketch` is a choice
/// *within* `Quantile`, not a choice *of* `SketchKind` — every `Quantile`
/// value already carries which of the two (and its params) was picked.
///
/// [`SketchKind::new`] is the one place `(SketchAlgorithm, SketchParams)`
/// pairs get classified into a category; construct through it rather than
/// naming a variant directly, so a new algorithm can't drift out of sync
/// with its category. See `implementation::summary_candidates` for the
/// `AggIntent -> [SketchAlgorithm]` candidate list this ultimately groups.
#[derive(Debug, Clone, PartialEq)]
pub enum SketchKind {
    /// Approximate rank/percentile queries (e.g. p99 latency).
    Quantile(SketchAlgorithm, SketchParams),
    /// Approximate distinct-element counting.
    Cardinality(SketchAlgorithm, SketchParams),
    /// Approximate point/frequency counting (e.g. per-key event counts).
    Frequency(SketchAlgorithm, SketchParams),
    /// Approximate heavy-hitter / top-k queries.
    TopK(SketchAlgorithm, SketchParams),
}

impl SketchKind {
    /// Classify `(algorithm, params)` into its `SketchKind` category. The
    /// one place that mapping is made — every other piece of this crate
    /// that needs to know an algorithm's category goes through this rather
    /// than re-deriving it.
    pub fn new(algorithm: SketchAlgorithm, params: SketchParams) -> Self {
        match algorithm {
            SketchAlgorithm::Kll | SketchAlgorithm::DDSketch => {
                SketchKind::Quantile(algorithm, params)
            }
            SketchAlgorithm::Hll | SketchAlgorithm::Theta | SketchAlgorithm::Kmv => {
                SketchKind::Cardinality(algorithm, params)
            }
            SketchAlgorithm::Cms | SketchAlgorithm::CountSketch => {
                SketchKind::Frequency(algorithm, params)
            }
            SketchAlgorithm::CmsWithHeap | SketchAlgorithm::CountSketchWithHeap => {
                SketchKind::TopK(algorithm, params)
            }
        }
    }

    /// The algorithm this kind committed to, regardless of category.
    pub fn algorithm(&self) -> &SketchAlgorithm {
        match self {
            SketchKind::Quantile(a, _)
            | SketchKind::Cardinality(a, _)
            | SketchKind::Frequency(a, _)
            | SketchKind::TopK(a, _) => a,
        }
    }

    /// The parameters this kind committed to, regardless of category.
    pub fn params(&self) -> &SketchParams {
        match self {
            SketchKind::Quantile(_, p)
            | SketchKind::Cardinality(_, p)
            | SketchKind::Frequency(_, p)
            | SketchKind::TopK(_, p) => p,
        }
    }
}

// ── Sampling summaries ───────────────────────────────────────────────────────

/// A sampling-based summary family — retains an actual (weighted) subset of
/// rows rather than a compressed sketch. Minimal starting vocabulary: one
/// family, extend as a deployment needs another (stratified, weighted, …).
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum SamplingKind {
    /// Reservoir sampling (mergeable via weighted reservoir merge;
    /// uniform-random retained subset of a fixed size).
    Reservoir,
}

/// Parameters for a [`SamplingKind`] instance.
#[derive(Debug, Clone, PartialEq)]
pub enum SamplingParams {
    Reservoir {
        /// Reservoir capacity — the number of retained rows.
        size: u32,
    },
}

// ── Wavelet summaries ────────────────────────────────────────────────────────

/// A wavelet-transform-based summary family — a compressed coefficient
/// vector supporting approximate range-sum / histogram queries. Minimal
/// starting vocabulary: one basis, extend as a deployment needs another.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum WaveletKind {
    /// Haar wavelet synopsis — the simplest, most common streaming basis.
    Haar,
}

/// Parameters for a [`WaveletKind`] instance.
#[derive(Debug, Clone, PartialEq)]
pub enum WaveletParams {
    Haar {
        /// Number of retained coefficients — the compression / accuracy knob.
        coefficients: u32,
    },
}

// ── Statistical-model summaries ──────────────────────────────────────────────

/// A fitted statistical/parametric-model summary family — e.g. a distribution
/// fit used to answer approximate quantile/density queries without
/// retaining raw samples. Deliberately open-ended: `family` names the model
/// family and is interpreted by whatever deployment builds/reads it, the
/// same "core doesn't enumerate every deployment shape" stance
/// `AggIntent::Extension` already takes for pre-ASAP intents.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum StatModelKind {
    /// A parametric model fit to the data (e.g. a fitted distribution or
    /// regression). `family` (in [`StatModelParams::Parametric`]) names
    /// which one.
    Parametric,
}

/// Parameters for a [`StatModelKind`] instance.
#[derive(Debug, Clone, PartialEq)]
pub enum StatModelParams {
    Parametric {
        /// Deployment-interpreted model family name (e.g.
        /// `"gaussian_mixture"`). Core has no fixed catalog of families —
        /// see [`StatModelKind::Parametric`].
        family: String,
    },
}

// ── Sketch read-out queries ───────────────────────────────────────────────────

/// What to extract from a built summary. Carried by `SummaryEstimate`.
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

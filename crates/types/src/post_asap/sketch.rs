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
/// category (e.g. `Kll` and `DDSketch` both realize quantile sketches);
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

/// The query category served by a committed sketch.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SketchCategory {
    Quantile,
    Cardinality,
    Frequency,
    TopK,
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
/// with its category. See `asap_aware_mapping::summary_candidates` for the
/// `AggIntent -> [SketchAlgorithm]` candidate list this ultimately groups.
#[derive(Debug, Clone, PartialEq)]
pub struct SketchKind {
    category: SketchCategory,
    algorithm: SketchAlgorithm,
    params: SketchParams,
}

impl SketchKind {
    /// Classify `(algorithm, params)` into its `SketchKind` category. The
    /// one place that mapping is made — every other piece of this crate
    /// that needs to know an algorithm's category goes through this rather
    /// than re-deriving it. Panics when `params` belongs to a different
    /// algorithm; invalid committed sketch states cannot be constructed.
    pub fn new(algorithm: SketchAlgorithm, params: SketchParams) -> Self {
        let valid_params = matches!(
            (&algorithm, &params),
            (SketchAlgorithm::Kll, SketchParams::Kll { .. })
                | (SketchAlgorithm::DDSketch, SketchParams::DDSketch { .. })
                | (SketchAlgorithm::Hll, SketchParams::Hll { .. })
                | (SketchAlgorithm::Theta, SketchParams::Theta { .. })
                | (SketchAlgorithm::Kmv, SketchParams::Kmv { .. })
                | (SketchAlgorithm::Cms, SketchParams::Cms { .. })
                | (
                    SketchAlgorithm::CountSketch,
                    SketchParams::CountSketch { .. }
                )
                | (
                    SketchAlgorithm::CmsWithHeap,
                    SketchParams::CmsWithHeap { .. }
                )
                | (
                    SketchAlgorithm::CountSketchWithHeap,
                    SketchParams::CountSketchWithHeap { .. }
                )
        );
        assert!(
            valid_params,
            "SketchKind parameter mismatch: algorithm={algorithm:?}, params={params:?}"
        );
        let category = match algorithm {
            SketchAlgorithm::Kll | SketchAlgorithm::DDSketch => SketchCategory::Quantile,
            SketchAlgorithm::Hll | SketchAlgorithm::Theta | SketchAlgorithm::Kmv => {
                SketchCategory::Cardinality
            }
            SketchAlgorithm::Cms | SketchAlgorithm::CountSketch => SketchCategory::Frequency,
            SketchAlgorithm::CmsWithHeap | SketchAlgorithm::CountSketchWithHeap => {
                SketchCategory::TopK
            }
        };
        Self {
            category,
            algorithm,
            params,
        }
    }

    pub fn category(&self) -> SketchCategory {
        self.category
    }

    /// The algorithm this kind committed to, regardless of category.
    pub fn algorithm(&self) -> &SketchAlgorithm {
        &self.algorithm
    }

    /// The parameters this kind committed to, regardless of category.
    pub fn params(&self) -> &SketchParams {
        &self.params
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

// ── Grouping strategy: per-subpopulation vs. shared multi-subpopulation
//    (Hydra) — issue #256 ──────────────────────────────────────────────────
//
// A grouped aggregate (`GROUP BY city, quantile(...)`) has always implicitly
// built one independent summary instance per distinct `by` key — there was
// no type anywhere expressing that as a *choice* rather than a foregone
// conclusion. Hydra (see e.g. this org's `sketch-bench`/`sketch-core`
// `hydra_kll`/`hydra_cms`/`hydra_hll`/`hydra_univmon`/`hydra_cs` wrappers —
// this crate doesn't depend on those repos, it just knows the concept is
// real) is the alternative: one shared structure serving every subpopulation
// instead of N independent instances, trading memory/build cost against
// per-subpopulation isolation.
//
// This axis is deliberately modeled here, alongside `SketchKind`/
// `SamplingKind`/`WaveletKind`/`StatModelKind`, rather than as a new
// `SketchKind` (or `SamplingKind`, etc.) entry: it is orthogonal to *which*
// family/kind answers an intent — any family could in principle grow its own
// per-subpopulation vs. shared-multi-subpopulation variant, so it is a
// second, independent axis, not a member of any one family's own kind
// vocabulary. See `asap_aware_mapping::grouping`'s module docs for where this
// axis actually plugs into the post-ASAP IR and the legality rules gating
// when `SharedMultiSubpopulation` is offered as a candidate at all.

/// A shared-multi-subpopulation summary family — one physical structure
/// serving every subpopulation of a grouped aggregate instead of one
/// independent instance per distinct `by` key. Named after Hydra (see the
/// module docs above).
///
/// Orthogonal to [`SketchKind`]/[`SamplingKind`]/[`WaveletKind`]/
/// [`StatModelKind`] the same way [`GroupingStrategy`] as a whole is
/// orthogonal to them. Minimal starting vocabulary: one variant (mirroring
/// how [`SamplingKind`]/[`WaveletKind`] each start with exactly one and a
/// comment explaining why) — extend as needed (`HydraCms`, `HydraHll`,
/// `HydraUnivMon`, `HydraCs` — the other wrappers this org's sketch-bench
/// names).
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum HydraKind {
    /// Hydra over a KLL-family quantile sketch: one shared structure
    /// answering every subpopulation's quantile query, instead of one KLL
    /// instance per distinct `by` key.
    HydraKll,
}

/// Parameters for a [`HydraKind`] instance. Unlike a plain [`SketchParams`]
/// (sized purely for one instance's own accuracy), a Hydra structure needs
/// two knobs: the per-subpopulation accuracy target it emulates, and how big
/// the one shared structure itself is — the latter is the whole
/// memory/accuracy trade Hydra makes, and has no equivalent at all for a
/// [`SketchParams::Kll`] instance built independently per subpopulation.
#[derive(Debug, Clone, PartialEq)]
pub enum HydraParams {
    HydraKll {
        /// Per-subpopulation accuracy knob — same meaning as
        /// `SketchParams::Kll`'s own `k`, for the per-subpopulation logical
        /// view this shared structure emulates.
        k: u32,
        /// Sizing knob for the one physical structure shared across every
        /// subpopulation. Correctly sizing this against an estimated
        /// subpopulation cardinality is a cost-model concern — out of scope
        /// for the legality axis this type lives on (see
        /// `asap_aware_mapping::grouping`'s module docs) — so this is
        /// deliberately not derived from any cardinality estimate here.
        shared_buckets: u32,
    },
}

/// Which [`HydraKind`] (if any) provides a shared-multi-subpopulation
/// variant of a plain per-subpopulation [`SketchAlgorithm`]. `None` means
/// this axis's scope stops at legality: not every `SketchAlgorithm` has a
/// Hydra wrapper modeled yet (only KLL's, mirroring [`HydraKind`]'s own
/// "start with one variant" stance) — extend alongside `HydraKind` as more
/// are added.
pub fn hydra_kind_for(algorithm: &SketchAlgorithm) -> Option<HydraKind> {
    match algorithm {
        SketchAlgorithm::Kll => Some(HydraKind::HydraKll),
        _ => None,
    }
}

/// Default [`HydraParams`] for `kind`, carrying over `per_subpopulation_k`
/// (the accuracy knob a per-subpopulation [`SketchParams::Kll`] instance
/// would have used) unchanged. `shared_buckets` is sized to the same value
/// as a placeholder pending real cost-model-driven sizing — see
/// [`HydraParams::HydraKll`]'s own doc on `shared_buckets` for why that's
/// deliberately out of scope for this issue.
pub fn default_hydra_params(kind: HydraKind, per_subpopulation_k: u32) -> HydraParams {
    match kind {
        HydraKind::HydraKll => HydraParams::HydraKll {
            k: per_subpopulation_k,
            shared_buckets: per_subpopulation_k,
        },
    }
}

/// How a grouped aggregate's summary state is physically instantiated
/// across its `by` subpopulations — orthogonal to *which*
/// `SketchKind`/`SamplingKind`/`WaveletKind`/`StatModelKind` answers the
/// intent (that choice lives on `SummaryFamilyType`, unchanged). Lives here,
/// alongside `SketchKind`/`SketchParams` etc., rather than on any of those
/// enums themselves, for exactly the reason explained in this section's
/// module docs above.
///
/// Carried on `SummaryExpr::SummaryAgg` (not on `SummaryFamilyType` /
/// `Implementation`) — see `asap_aware_mapping::grouping`'s module docs for
/// why, and for the legality rules gating when `SharedMultiSubpopulation` is
/// even offered as a candidate.
#[derive(Debug, Clone, PartialEq)]
pub enum GroupingStrategy {
    /// One independent summary instance per distinct `by` key — today's
    /// only (implicit) behavior, and this type's `Default` (below), so
    /// every existing caller that never chose this axis explicitly keeps
    /// observing exactly the same behavior it always has.
    PerSubpopulationInstance,
    /// One shared structure serving every subpopulation (Hydra and its
    /// per-family variants — see [`HydraKind`]), trading per-subpopulation
    /// isolation for shared memory/build cost.
    ///
    /// Named `SharedMultiSubpopulation`, not `SharedMultiTenant`: this
    /// shares across a *query's own* subpopulations/group-by keys, not
    /// across tenants in a deployment-isolation sense — a different, and
    /// unrelated, kind of "sharing".
    SharedMultiSubpopulation {
        kind: HydraKind,
        params: HydraParams,
    },
}

impl Default for GroupingStrategy {
    /// [`GroupingStrategy::PerSubpopulationInstance`] — see that variant's
    /// own doc for why this is the only sound default.
    fn default() -> Self {
        Self::PerSubpopulationInstance
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sketch_kind_classifies_a_valid_algorithm_and_params_pair() {
        let kind = SketchKind::new(SketchAlgorithm::Kll, SketchParams::Kll { k: 200 });
        assert_eq!(kind.category(), SketchCategory::Quantile);
        assert_eq!(kind.algorithm(), &SketchAlgorithm::Kll);
    }

    #[test]
    #[should_panic(expected = "SketchKind parameter mismatch")]
    fn sketch_kind_rejects_params_from_another_algorithm() {
        SketchKind::new(SketchAlgorithm::Kll, SketchParams::Hll { precision: 14 });
    }

    #[test]
    fn grouping_strategy_default_is_per_subpopulation_instance() {
        // Existing/default behavior must stay `PerSubpopulationInstance` —
        // this must not change any existing test's observed behavior.
        assert_eq!(
            GroupingStrategy::default(),
            GroupingStrategy::PerSubpopulationInstance
        );
    }

    #[test]
    fn hydra_kind_for_kll_is_the_only_mapped_kind_so_far() {
        assert_eq!(
            hydra_kind_for(&SketchAlgorithm::Kll),
            Some(HydraKind::HydraKll)
        );
        // Every other `SketchAlgorithm` has no Hydra variant modeled yet —
        // a deliberate, documented scope limit, not an oversight.
        for algorithm in [
            SketchAlgorithm::Cms,
            SketchAlgorithm::Hll,
            SketchAlgorithm::DDSketch,
            SketchAlgorithm::CmsWithHeap,
            SketchAlgorithm::Kmv,
            SketchAlgorithm::Theta,
            SketchAlgorithm::CountSketch,
            SketchAlgorithm::CountSketchWithHeap,
        ] {
            assert_eq!(hydra_kind_for(&algorithm), None, "{algorithm:?}");
        }
    }

    #[test]
    fn default_hydra_params_carries_over_the_per_subpopulation_k() {
        assert_eq!(
            default_hydra_params(HydraKind::HydraKll, 200),
            HydraParams::HydraKll {
                k: 200,
                shared_buckets: 200,
            }
        );
    }
}

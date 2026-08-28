use serde::{Deserialize, Serialize};

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
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
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
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
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
// conclusion. Hydra (Manousis et al., VLDB 2022 — see `HydraKind`'s own doc
// for the full citation and which variants are its actual proven
// construction) is the alternative: one shared structure serving every
// subpopulation instead of N independent instances, trading memory/build
// cost against per-subpopulation isolation.
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
/// independent instance per distinct `by` key. Named after Hydra (Manousis,
/// Cheng, Ben Basat, Liu, Sekar. "Enabling Efficient and General
/// Subpopulation Analytics in Multidimensional Data Streams." VLDB 2022).
///
/// Orthogonal to [`SketchKind`]/[`SamplingKind`]/[`WaveletKind`]/
/// [`StatModelKind`] the same way [`GroupingStrategy`] as a whole is
/// orthogonal to them.
///
/// ## Which variants are the paper's own proven construction, and which aren't
///
/// Hydra's accuracy proof (paper §4.5, Theorem 2) is over one specific
/// construction: hash each subpopulation into one of a shared w×r grid of
/// **linear, mergeable frequency-vector sketches** — the paper's own
/// heavy-hitter substrate is Count-Sketch (§4.3); Count-Min Sketch is the
/// same collision algebra — and bound the noise a colliding subpopulation's
/// estimate picks up from the others sharing its cell.
/// [`HydraKind::HydraCms`]/[`HydraKind::HydraCountSketch`] are exactly that
/// construction over this crate's existing `SketchAlgorithm::Cms`/
/// `CountSketch`, so the paper's bound applies to them directly.
///
/// [`HydraKind::HydraKll`] is **not** an instance of that proven
/// construction: KLL is an order-statistics sketch, not a linear frequency
/// vector, and has no analogous "sum the colliding contributions, bound the
/// noise" algebra. The paper is explicit that its own construction cannot
/// serve quantiles at all (§4.3: "A statistic that cannot directly be
/// estimated by Hydra-sketch is quantiles."). `HydraKll` remains available
/// as an explicit experimental IR value, but [`hydra_kind_for`] does not
/// expose it to semantics-preserving replacement search: no error bound is
/// modeled for it (see [`HydraParams::HydraKll`]'s own doc). Enable automatic
/// selection only alongside an actual proof and error model.
///
/// Not yet modeled: `HydraUnivMon`. The paper's own named "Hydra-sketch" is
/// really the universal-sketch composition (L layers of Count-Sketch plus a
/// heavy-hitter heap, Theorems 1+2 combined) estimating entropy/L1-norm/
/// L2-norm/cardinality/frequency-moments as one instance. That needs new
/// `AggIntent`/category vocabulary this crate doesn't have yet (no
/// `Entropy`/`L1Norm`/`L2Norm` intents) and is deliberately out of scope
/// here — see issue #256's follow-up.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum HydraKind {
    /// Hydra over a KLL-family quantile sketch. See this type's own doc:
    /// **not** an instance of the paper's proven construction — the paper
    /// excludes quantiles from Hydra-sketch entirely. Kept only as an
    /// explicit experimental representation; automatic replacement search
    /// must not treat it as equivalent to independent instances.
    HydraKll,
    /// Hydra over Count-Min Sketch: a direct instance of the paper's proven
    /// w×r shared-grid construction (§4.2/§4.5) — CMS is exactly the
    /// linear frequency-vector substrate Theorem 2 is proved over.
    HydraCms,
    /// Hydra over Count-Sketch — the paper's own heavy-hitter substrate
    /// (§4.3). A direct instance of the same proven construction as
    /// `HydraCms`, with Count-Sketch's balanced/zero-mean-error trade
    /// instead of CMS's one-sided bias.
    HydraCountSketch,
}

/// Parameters for a [`HydraKind`] instance. Unlike a plain [`SketchParams`]
/// (sized purely for one instance's own accuracy), a Hydra structure needs
/// both the per-subpopulation accuracy target it emulates *and* how big the
/// one shared structure itself is — the latter is the whole memory/accuracy
/// trade Hydra makes, and has no equivalent for an instance built
/// independently per subpopulation. One variant per [`HydraKind`] because
/// the knobs a "sketch of sketches" needs are specific to the inner
/// sketch's own parameter shape — a `HydraCms` instance is sized in
/// (`width`, `depth`), a `HydraKll` instance in `k`; there is no single knob
/// set general enough to cover every inner sketch type.
#[derive(Debug, Clone, PartialEq)]
pub enum HydraParams {
    /// See [`HydraKind::HydraKll`]: kept for legality, **no accuracy bound
    /// is modeled for this variant**. `k`/`shared_buckets` give a bound
    /// sketch a concrete size to build against; unlike `HydraCms`/
    /// `HydraCountSketch`'s fields, they are not backed by the paper's
    /// Theorem 2.
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
    /// Hydra over Count-Min Sketch — the paper's proven w×r
    /// shared-grid construction (§4.2, Theorem 2). `width`/`depth` are the
    /// per-subpopulation CMS knobs (mirroring `SketchParams::Cms`'s own
    /// fields); `shared_rows`/`shared_columns` are the paper's `r`/`w` — the
    /// redundant hash rows and shared-bucket width of the one physical grid
    /// every subpopulation is hashed into.
    HydraCms {
        width: u32,
        depth: u32,
        /// The paper's `r`: redundant, pairwise-independent hash rows —
        /// query time takes the median across rows to tighten the failure
        /// probability (Theorem 2's `δ` term).
        shared_rows: u32,
        /// The paper's `w`: shared buckets per row that colliding
        /// subpopulations share — the memory/accuracy knob Theorem 2's `ε`
        /// term depends on. Sizing this against an estimated subpopulation
        /// cardinality is a cost-model concern, deliberately out of scope
        /// for the legality axis this type lives on.
        shared_columns: u32,
    },
    /// Hydra over Count-Sketch — same shape, and the same Theorem 2, as
    /// `HydraCms`, over `SketchParams::CountSketch`'s knobs instead.
    HydraCountSketch {
        width: u32,
        depth: u32,
        shared_rows: u32,
        shared_columns: u32,
    },
}

/// Which [`HydraKind`] (if any) provides a shared-multi-subpopulation
/// variant of a plain per-subpopulation [`SketchAlgorithm`]. `None` means
/// this axis's scope stops at legality: not every `SketchAlgorithm` has a
/// Hydra wrapper modeled yet — extend alongside `HydraKind` as more are
/// added. See [`HydraKind`]'s own doc for which of the mapped kinds are the
/// paper's own proven construction. Unproven extensions such as `HydraKll`
/// deliberately return `None`: replacement search may only expose variants
/// with a modeled error guarantee.
pub fn hydra_kind_for(algorithm: &SketchAlgorithm) -> Option<HydraKind> {
    match algorithm {
        SketchAlgorithm::Cms => Some(HydraKind::HydraCms),
        SketchAlgorithm::CountSketch => Some(HydraKind::HydraCountSketch),
        _ => None,
    }
}

/// Default [`HydraParams`] for `kind`, carrying over `per_subpopulation_params`
/// — the [`SketchParams`] a plain, independent-per-subpopulation instance of
/// the same algorithm would have used — unchanged into the corresponding
/// `HydraParams` fields. `None` when `per_subpopulation_params` doesn't
/// belong to the [`SketchAlgorithm`] `kind` wraps: a caller bug, since
/// [`hydra_kind_for`] and the algorithm a `SketchParams` came from must
/// agree; callers that got both from the same already-ranked
/// `Implementation` (as `asap_aware_mapping::grouping` does) cannot hit
/// this.
///
/// This function is generic over which inner sketch type `kind` wraps
/// precisely because [`SketchParams`] already is: it destructures whichever
/// variant matches `kind` rather than assuming a single scalar knob (e.g. a
/// bare `k: u32`) that only KLL happens to have — a Hydra "sketch of
/// sketches" is a framework over *any* mergeable inner sketch, and `HydraCms`/
/// `HydraCountSketch`'s (`width`, `depth`) pairs are just as much a
/// per-subpopulation accuracy knob as `HydraKll`'s `k`.
///
/// `shared_buckets`/`shared_rows`/`shared_columns` are all sized to the same
/// per-subpopulation value as a placeholder pending real cost-model-driven
/// sizing (the paper's own `r`/`w`, §4.6) — see each field's own doc for why
/// that's deliberately out of scope here.
pub fn default_hydra_params(
    kind: HydraKind,
    per_subpopulation_params: &SketchParams,
) -> Option<HydraParams> {
    match (kind, per_subpopulation_params) {
        (HydraKind::HydraKll, SketchParams::Kll { k }) => Some(HydraParams::HydraKll {
            k: *k,
            shared_buckets: *k,
        }),
        (HydraKind::HydraCms, SketchParams::Cms { width, depth }) => Some(HydraParams::HydraCms {
            width: *width,
            depth: *depth,
            shared_rows: *depth,
            shared_columns: *width,
        }),
        (HydraKind::HydraCountSketch, SketchParams::CountSketch { width, depth }) => {
            Some(HydraParams::HydraCountSketch {
                width: *width,
                depth: *depth,
                shared_rows: *depth,
                shared_columns: *width,
            })
        }
        _ => None,
    }
}

/// How a grouped aggregate's summary state is physically instantiated
/// across its `by` subpopulations — orthogonal to *which*
/// `SketchKind`/`SamplingKind`/`WaveletKind`/`StatModelKind` answers the
/// intent (that choice lives alongside it on `SummaryFamilyType`). Lives here,
/// alongside `SketchKind`/`SketchParams` etc., rather than on any of those
/// enums themselves, for exactly the reason explained in this section's
/// module docs above.
///
/// Carried both on `SummaryExpr::SummaryAgg` (where planning consults it)
/// and on sketch-valued `SummaryFamilyType` edges (where it prevents
/// incompatible shared and independent physical states from type-checking
/// as merge-compatible).
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
    fn hydra_kind_for_only_maps_algorithms_with_modeled_error_bounds() {
        assert_eq!(hydra_kind_for(&SketchAlgorithm::Kll), None);
        assert_eq!(
            hydra_kind_for(&SketchAlgorithm::Cms),
            Some(HydraKind::HydraCms)
        );
        assert_eq!(
            hydra_kind_for(&SketchAlgorithm::CountSketch),
            Some(HydraKind::HydraCountSketch)
        );
        // Every other `SketchAlgorithm` has no Hydra variant modeled yet —
        // a deliberate, documented scope limit, not an oversight.
        for algorithm in [
            SketchAlgorithm::Hll,
            SketchAlgorithm::DDSketch,
            SketchAlgorithm::CmsWithHeap,
            SketchAlgorithm::Kmv,
            SketchAlgorithm::Theta,
            SketchAlgorithm::CountSketchWithHeap,
        ] {
            assert_eq!(hydra_kind_for(&algorithm), None, "{algorithm:?}");
        }
    }

    #[test]
    fn default_hydra_params_carries_over_per_subpopulation_params_by_kind() {
        assert_eq!(
            default_hydra_params(HydraKind::HydraKll, &SketchParams::Kll { k: 200 }),
            Some(HydraParams::HydraKll {
                k: 200,
                shared_buckets: 200,
            })
        );
        assert_eq!(
            default_hydra_params(
                HydraKind::HydraCms,
                &SketchParams::Cms {
                    width: 2048,
                    depth: 4,
                }
            ),
            Some(HydraParams::HydraCms {
                width: 2048,
                depth: 4,
                shared_rows: 4,
                shared_columns: 2048,
            })
        );
        assert_eq!(
            default_hydra_params(
                HydraKind::HydraCountSketch,
                &SketchParams::CountSketch {
                    width: 2048,
                    depth: 4,
                }
            ),
            Some(HydraParams::HydraCountSketch {
                width: 2048,
                depth: 4,
                shared_rows: 4,
                shared_columns: 2048,
            })
        );
    }

    #[test]
    fn default_hydra_params_rejects_a_mismatched_kind_and_params_pair() {
        // A `HydraCms` kind paired with KLL params (or vice versa) is a
        // caller bug — `hydra_kind_for` and the algorithm a `SketchParams`
        // came from must agree. Degrades to `None` rather than panicking,
        // matching this module's conservative stance elsewhere.
        assert_eq!(
            default_hydra_params(HydraKind::HydraCms, &SketchParams::Kll { k: 200 }),
            None
        );
    }
}

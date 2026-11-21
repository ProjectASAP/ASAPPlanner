//! Each summary type's parameters $\boldsymbol\theta$: structures only,
//! for exact accumulators, sketches, samples, wavelets, statistical models
//! and Hydra.

use serde::{Deserialize, Serialize};

/// An exact, mergeable accumulator family — zero approximation error. The
/// partial state built for one of these *is* the answer; no
/// `SummaryEstimate` evaluation is needed to get a value out of it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub enum ExactKind {
    /// Exact sum accumulator (mergeable by addition).
    Sum,
    /// Exact count accumulator (mergeable by addition).
    Count,
    /// Exact minimum accumulator (mergeable by comparison).
    Min,
    /// Exact maximum accumulator (mergeable by comparison).
    Max,
    /// A counter's window: its first and last samples, its sample count
    /// and the increase between them with counter resets handled. It
    /// answers both PromQL `rate` and `increase` (a [`CounterReadout`]),
    /// so `rate(x[1h])` and `increase(x[1h])` share one state.
    Counter,
    /// Instant-rate accumulator retaining the final two timestamped samples.
    IRate,
}

impl ExactKind {
    /// Bytes the executor keeps per group for a sealed state of this kind:
    /// its record in the column of the kind's records, whole bytes (a
    /// validity bit counted as one byte). Plan selection prices exact state
    /// with it; an executor test asserts each kind's sealed column holds it
    /// per group. Sum: the compensated sum and whether it saw a sample (24);
    /// count (8); min and max: the value and a validity bit (9); a counter:
    /// its two samples, increase and sample count, and a validity bit (49);
    /// irate: the last two samples (48).
    pub fn sealed_state_bytes(self) -> u64 {
        match self {
            ExactKind::Sum => 24,
            ExactKind::Count => 8,
            ExactKind::Min | ExactKind::Max => 9,
            ExactKind::Counter => 49,
            ExactKind::IRate => 48,
        }
    }
}

/// What a finalize reads from a [`ExactKind::Counter`] state: PromQL
/// `increase`, the increase extrapolated to the evaluation range, or
/// `rate`, that increase per second of the range. Both follow Prometheus'
/// `extrapolatedRate`. Example: samples 0 at 0 s and 60 at 60 s in
/// `(0 s, 60 s]` finalize to increase 60 and rate 1.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub enum CounterReadout {
    Rate,
    Increase,
}

impl CounterReadout {
    /// The readout that answers `intent`: `Rate` for `AggIntent::Rate`,
    /// `Increase` for `AggIntent::Increase`, `None` for any other.
    pub fn for_intent(intent: &crate::ir::operator::AggIntent) -> Option<Self> {
        use crate::ir::operator::AggIntent;
        match intent {
            AggIntent::Rate => Some(Self::Rate),
            AggIntent::Increase => Some(Self::Increase),
            _ => None,
        }
    }

    /// The aggregate this readout computes: `AggIntent::Rate` or
    /// `AggIntent::Increase`.
    pub(crate) fn measure(self) -> crate::ir::operator::AggIntent {
        use crate::ir::operator::AggIntent;
        match self {
            Self::Rate => AggIntent::Rate,
            Self::Increase => AggIntent::Increase,
        }
    }
}

impl ExactKind {
    /// The aggregate an exact accumulator of this kind finalizes to, of
    /// input column `column` (`None`: the sample value), read with
    /// `counter_readout`: `ExactKind::Counter` with `CounterReadout::Rate`
    /// finalizes to what `AggIntent::Rate` computes, and
    /// `ExactKind::Sum.measure(Some(2), None)` to `AggIntent::Sum { col:
    /// Some(2) }`. `None` when the readout does not fit the kind: a
    /// `Counter` needs one, any other kind takes none.
    pub(crate) fn measure(
        self,
        column: Option<crate::ir::schema::ColumnId>,
        counter_readout: Option<CounterReadout>,
    ) -> Option<crate::ir::operator::AggIntent> {
        use crate::ir::operator::AggIntent;
        match (self, counter_readout) {
            (ExactKind::Counter, Some(readout)) => Some(readout.measure()),
            (ExactKind::Counter, None) | (_, Some(_)) => None,
            (ExactKind::Sum, None) => Some(AggIntent::Sum { col: column }),
            (ExactKind::Min, None) => Some(AggIntent::Min { col: column }),
            (ExactKind::Max, None) => Some(AggIntent::Max { col: column }),
            (ExactKind::Count, None) => Some(AggIntent::Count),
            (ExactKind::IRate, None) => Some(AggIntent::IRate),
        }
    }

    /// Whether this accumulator tracks a counter's increase (`Counter`,
    /// `IRate`): besides its value it keeps the window's first
    /// and last samples and the resets between them, so it needs its rows
    /// in time order and does more work per row than a `Sum`. Example:
    /// `rate(requests_total[5m])`'s accumulator does; `sum_over_time`'s
    /// does not.
    pub fn tracks_counter_increase(&self) -> bool {
        match self {
            Self::Counter | Self::IRate => true,
            Self::Sum | Self::Min | Self::Max | Self::Count => false,
        }
    }
}

/// Parameters for an [`ExactKind`] accumulator. All exact accumulators have
/// fixed semantics — no tuning parameters — so each variant carries none;
/// kept as a per-kind enum (mirroring [`SketchParams`]) so a mismatched
/// `(kind, params)` pair is still a type error, not a runtime check.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum ExactParams {
    Sum,
    Count,
    Min,
    Max,
    Counter,
    IRate,
}

/// The largest KLL `k` `asap_sketchlib` builds: it silently caps a larger
/// `k` at this (`MAX_CACHEABLE_K` in its `sketches::kll`).
pub const KLL_MAX_K: u32 = 26_602;

/// The structure of a summary of one [`SketchAlgorithm`](crate::summary_type::SketchAlgorithm) (θ): its sizes,
/// from the algorithm's predefined list (`parameter_list`). No field records
/// the error or failure probability a configuration was sized for, so
/// equality compares every field.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum SketchParams {
    UnivMon {
        heap_size: u32,
        sketch_rows: u32,
        sketch_cols: u32,
        layers: u8,
    },
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
    CountSketch {
        width: u32,
        depth: u32,
    },
    CountSketchWithHeap {
        width: u32,
        depth: u32,
        heap_size: u32,
    },
    /// The sizes of a Hydra structure shared across subpopulations, for the
    /// `Hydra*` algorithms.
    Hydra(HydraParams),
}

/// A sampling summary family — retains an actual (weighted) subset of input
/// rows rather than a compressed sketch. The retained rows are its summary
/// sample ([`FieldDataType::SummarySample`](crate::ir::schema::FieldDataType::SummarySample)),
/// not the raw data samples a metric arrives as. Minimal
/// starting vocabulary: one family, extend as a deployment needs another
/// (stratified, weighted, …).
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub enum SamplingKind {
    /// Reservoir sampling (mergeable via weighted reservoir merge;
    /// uniform-random retained subset of a fixed size).
    Reservoir,
}

/// Parameters for a [`SamplingKind`] instance.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum SamplingParams {
    Reservoir {
        /// Reservoir capacity — the number of retained rows.
        size: u32,
    },
}

/// A wavelet-transform-based summary family — a compressed coefficient
/// vector supporting approximate range-sum / histogram queries. Minimal
/// starting vocabulary: one basis, extend as a deployment needs another.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub enum WaveletKind {
    /// Haar wavelet synopsis — the simplest, most common streaming basis.
    Haar,
}

/// Parameters for a [`WaveletKind`] instance.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum WaveletParams {
    Haar {
        /// Number of retained coefficients — the compression / accuracy knob.
        coefficients: u32,
    },
}

/// A fitted statistical/parametric-model summary family — e.g. a distribution
/// fit used to answer approximate quantile/density queries without
/// retaining raw data samples. Deliberately open-ended: `family` names the model
/// family and is interpreted by whatever deployment builds/reads it, the
/// same "core doesn't enumerate every deployment shape" stance
/// `AggIntent::Extension` already takes for pre-ASAP intents.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub enum StatModelKind {
    /// A parametric model fit to the data (e.g. a fitted distribution or
    /// regression). `family` (in [`StatModelParams::Parametric`]) names
    /// which one.
    Parametric,
}

/// Parameters for a [`StatModelKind`] instance.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum StatModelParams {
    Parametric {
        /// Deployment-interpreted model family name (e.g.
        /// `"gaussian_mixture"`). Core has no fixed catalog of families —
        /// see [`StatModelKind::Parametric`].
        family: String,
    },
}

/// θ of the `Hydra*` algorithms ([`SketchAlgorithm::HydraKll`](crate::summary_type::SketchAlgorithm::HydraKll),
/// [`SketchAlgorithm::HydraCms`](crate::summary_type::SketchAlgorithm::HydraCms), [`SketchAlgorithm::HydraCountSketch`](crate::summary_type::SketchAlgorithm::HydraCountSketch)): one
/// structure shared by every subpopulation of a grouped aggregate (Manousis
/// et al., "Enabling Efficient and General Subpopulation Analytics in
/// Multidimensional Data Streams", VLDB 2022). It holds both the
/// per-subpopulation sketch's sizes and the shared grid's. HydraCms and
/// HydraCountSketch are the paper's proven construction (§4.5, Theorem 2);
/// HydraKll is not (the paper excludes quantiles, §4.3), so no error model
/// covers it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum HydraParams {
    /// **No accuracy bound is modeled for this variant**. `k`/`shared_buckets` give a bound
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
        /// for the legality axis this type lives on — so this is
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

impl HydraParams {
    /// The parameters of the per-subpopulation sketch this shared structure
    /// emulates, which its error bound and per-group state follow.
    pub fn per_subpopulation(&self) -> SketchParams {
        match *self {
            HydraParams::HydraKll { k, .. } => SketchParams::Kll { k },
            HydraParams::HydraCms { width, depth, .. } => SketchParams::Cms { width, depth },
            HydraParams::HydraCountSketch { width, depth, .. } => {
                SketchParams::CountSketch { width, depth }
            }
        }
    }
}

/// An inclusive range of one sketch parameter; an unset bound is open.
/// JSON: `{"kll_k": {"at_most": 1403}}`,
/// `{"ddsketch_alpha": {"at_least": 0.01}}`.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum SketchParameterRange {
    KllK {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        at_least: Option<u32>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        at_most: Option<u32>,
    },
    KmvK {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        at_least: Option<u32>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        at_most: Option<u32>,
    },
    #[serde(rename = "ddsketch_alpha")]
    DDSketchAlpha {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        at_least: Option<f64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        at_most: Option<f64>,
    },
}

impl SketchParameterRange {
    /// The algorithm whose parameter it ranges over (`Kll`, `Kmv`,
    /// `DDSketch`).
    pub fn algorithm(&self) -> super::SketchAlgorithm {
        use super::SketchAlgorithm;
        match self {
            Self::KllK { .. } => SketchAlgorithm::Kll,
            Self::KmvK { .. } => SketchAlgorithm::Kmv,
            Self::DDSketchAlpha { .. } => SketchAlgorithm::DDSketch,
        }
    }

    /// Whether `parameters` are that algorithm's and the parameter is in
    /// range, both ends included: `KllK { at_most: Some(547) }` contains
    /// `Kll { k: 547 }`, not `Kll { k: 1404 }` nor any `Kmv`.
    pub fn contains(&self, parameters: &SketchParams) -> bool {
        fn within<T: PartialOrd>(value: T, at_least: Option<T>, at_most: Option<T>) -> bool {
            at_least.is_none_or(|low| value >= low) && at_most.is_none_or(|high| value <= high)
        }
        match (*self, parameters) {
            (Self::KllK { at_least, at_most }, SketchParams::Kll { k })
            | (Self::KmvK { at_least, at_most }, SketchParams::Kmv { k }) => {
                within(*k, at_least, at_most)
            }
            (Self::DDSketchAlpha { at_least, at_most }, SketchParams::DDSketch { alpha }) => {
                within(*alpha, at_least, at_most)
            }
            _ => false,
        }
    }

    /// Why the range is bad input, or `None` when it is valid: `at_least`
    /// is above `at_most`, so it contains no value (`KllK { at_least: 1404,
    /// at_most: 547 }`), or a DDSketch α bound is not a number in (0, 1).
    pub fn invalid_reason(&self) -> Option<String> {
        let (at_least, at_most) = match *self {
            Self::KllK { at_least, at_most } | Self::KmvK { at_least, at_most } => {
                (at_least.map(f64::from), at_most.map(f64::from))
            }
            Self::DDSketchAlpha { at_least, at_most } => {
                if let Some(alpha) = [at_least, at_most]
                    .into_iter()
                    .flatten()
                    .find(|alpha| !(*alpha > 0.0 && *alpha < 1.0))
                {
                    return Some(format!("a DDSketch alpha of {alpha} is not in (0, 1)"));
                }
                (at_least, at_most)
            }
        };
        match (at_least, at_most) {
            (Some(low), Some(high)) if low > high => Some(format!(
                "the range is empty: at_least {low} is above at_most {high}"
            )),
            _ => None,
        }
    }
}

#[cfg(test)]
mod sketch_parameter_range_tests {
    use super::*;
    use crate::summary_type::SketchAlgorithm;

    /// Both ends of a `k` range are included, an unset end is open, and a
    /// range over KLL's `k` never contains another algorithm's parameters.
    #[test]
    fn kll_k_range_bounds_are_inclusive() {
        let k547 = SketchParams::Kll { k: 547 };
        let k1404 = SketchParams::Kll { k: 1404 };
        let at_most_547 = SketchParameterRange::KllK {
            at_least: None,
            at_most: Some(547),
        };
        assert!(at_most_547.contains(&k547));
        assert!(!at_most_547.contains(&k1404));
        let at_least_1404 = SketchParameterRange::KllK {
            at_least: Some(1404),
            at_most: None,
        };
        assert!(at_least_1404.contains(&k1404));
        assert!(!at_least_1404.contains(&k547));
        let at_most_2863 = SketchParameterRange::KllK {
            at_least: None,
            at_most: Some(2863),
        };
        assert_eq!(at_most_2863.algorithm(), SketchAlgorithm::Kll);
        assert!(!at_most_2863.contains(&SketchParams::Kmv { k: 547 }));
        assert!(!at_most_2863.contains(&SketchParams::DDSketch { alpha: 0.01 }));
    }

    /// A KMV `k` range and a DDSketch α range each select their own
    /// algorithm's listed sizes (`parameter_list`), both ends included.
    #[test]
    fn kmv_k_and_ddsketch_alpha_ranges_select_their_own_parameter() {
        let kmv = SketchParameterRange::KmvK {
            at_least: Some(4323),
            at_most: None,
        };
        assert_eq!(kmv.algorithm(), SketchAlgorithm::Kmv);
        let kmv_contained: Vec<bool> = [1_118, 4_323, 26_656, 106_263]
            .into_iter()
            .map(|k| kmv.contains(&SketchParams::Kmv { k }))
            .collect();
        assert_eq!(kmv_contained, [false, true, true, true]);
        let ddsketch = SketchParameterRange::DDSketchAlpha {
            at_least: Some(0.01),
            at_most: Some(0.05),
        };
        assert_eq!(ddsketch.algorithm(), SketchAlgorithm::DDSketch);
        let ddsketch_contained: Vec<bool> = [0.1, 0.05, 0.01, 0.005]
            .into_iter()
            .map(|alpha| ddsketch.contains(&SketchParams::DDSketch { alpha }))
            .collect();
        assert_eq!(ddsketch_contained, [false, true, true, false]);
    }
}

use serde::{Deserialize, Serialize};

use crate::ir::scalar::ColumnRef;

// ── Exact accumulators ──────────────────────────────────────────────────────

/// Bytes the executor holds per group for an exact accumulator of any
/// [`ExactKind`]: one element of its column of fixed-size states (the
/// executor's `ScalarState`, whose largest variant, a counter's, is 48
/// bytes). Plan selection prices exact state with it; an executor test
/// asserts it equals that state's size.
pub const EXACT_ACCUMULATOR_STATE_BYTES: u64 = 56;

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
    /// Exact increase accumulator (counter-reset-aware delta).
    Increase,
    /// Rate accumulator (increase / time window duration).
    Rate,
    /// Instant-rate accumulator retaining the final two timestamped samples.
    IRate,
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
    Increase,
    Rate,
    IRate,
}

// ── Approximate sketches ─────────────────────────────────────────────────────

/// A specific sketch algorithm — bounded error, sized by its
/// [`SketchParams`]. It is `asap_sketchlib`'s family enum; the statistics
/// each answers are its row of the family-capability table
/// ([`SummaryTypeCapability`](crate::ir::schema::SummaryTypeCapability)). The planner
/// sizes and offers only the families [`SketchParams`] has a variant for.
pub use asap_sketchlib::SketchAlgorithm;

/// The largest KLL `k` `asap_sketchlib` builds: it silently caps a larger
/// `k` at this (`MAX_CACHEABLE_K` in its `sketches::kll`).
pub const KLL_MAX_K: u32 = 26_602;

/// The structure of a summary of one [`SketchAlgorithm`] (θ): its sizes,
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

// ── Sampling summaries ───────────────────────────────────────────────────────

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

// ── Wavelet summaries ────────────────────────────────────────────────────────

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

// ── Statistical-model summaries ──────────────────────────────────────────────

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

// ── Hydra: one structure shared by every subpopulation ─────────────────────

/// θ of the `Hydra*` algorithms ([`SketchAlgorithm::HydraKll`],
/// [`SketchAlgorithm::HydraCms`], [`SketchAlgorithm::HydraCountSketch`]): one
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

// ── Sketch evaluation queries ─────────────────────────────────────────────────

/// One summary-state update. `item` identifies a keyed item when the family
/// is keyed; `weight` is the observation/update expression applied to state.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SummaryUpdate {
    pub item: Option<SummaryInputExpr>,
    pub weight: SummaryInputExpr,
    /// Planner proof for algorithms whose error/update semantics require a
    /// non-negative stream. Absence of proof is never interpreted as zero or
    /// non-negative.
    #[serde(default)]
    pub weight_domain: WeightDomain,
}

impl SummaryUpdate {
    pub fn column(column: ColumnRef) -> Self {
        Self {
            item: None,
            weight: SummaryInputExpr::Column(column),
            weight_domain: WeightDomain::UnknownOrSigned,
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum WeightDomain {
    #[default]
    UnknownOrSigned,
    NonNegative {
        proof: NonNegativeWeightProof,
    },
    /// An open requirement, not a proof: the weights are raw data samples of
    /// `metric`, or plain sums of them (no rate, no other arithmetic), so they
    /// are non-negative when every raw data sample of `metric` is: `metric` is
    /// declared a counter, or declared non-negative. Logical optimization
    /// records it without reading the data workload; plan selection confirms
    /// it from the declared source facts
    /// ([`SourceFacts::metric_type`](crate::workload::SourceFacts::metric_type),
    /// [`SourceFacts::column_signs`](crate::workload::SourceFacts::column_signs)).
    /// Unconfirmed, it counts as not proven. Serialized as
    /// `samples_of_metric`.
    #[serde(rename = "samples_of_metric")]
    RawDataSamplesOfMetric {
        metric: String,
    },
    /// An open requirement, not a proof: the weights are reset-corrected
    /// `rate`, `increase` or `irate` values of `metric`'s raw data samples, or plain
    /// sums of them. Reset correction keeps them non-negative exactly when
    /// the samples are, that is when `metric` is a counter; over a gauge a
    /// negative sample can give a negative value. Plan selection confirms
    /// it from the declared metric types, as
    /// [`RawDataSamplesOfMetric`](Self::RawDataSamplesOfMetric). Unconfirmed, it
    /// counts as not proven.
    CounterDerivativeOfMetric {
        metric: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NonNegativeWeightProof {
    UnitCount,
    /// `rate`, `increase` or `irate` of a counter's samples, or plain sums
    /// of them: reset correction over non-negative samples gives a
    /// non-negative increase, and a rate divides it by a positive duration.
    /// Plan selection gives it on confirming a
    /// [`WeightDomain::CounterDerivativeOfMetric`] requirement.
    ResetAwareCounterDerivative,
    /// Raw data samples of a metric declared a counter
    /// ([`PrometheusMetricType::Counter`](crate::workload::PrometheusMetricType::Counter)), or
    /// plain sums of them: a counter sample is never negative. Plan selection
    /// gives it on confirming a
    /// [`WeightDomain::RawDataSamplesOfMetric`] requirement. Serialized as
    /// `counter_samples`.
    #[serde(rename = "counter_samples")]
    CounterRawDataSamples,
    /// Raw data samples of a metric declared non-negative
    /// ([`SourceFacts::column_signs`](crate::workload::SourceFacts::column_signs)),
    /// such as a gauge, or plain sums of them. Plan selection gives it on
    /// confirming a [`WeightDomain::RawDataSamplesOfMetric`] requirement for a
    /// metric not declared a counter. Serialized as
    /// `declared_non_negative_samples`.
    #[serde(rename = "declared_non_negative_samples")]
    DeclaredNonNegativeRawDataSamples,
    /// A constant weight greater than 0, such as `Constant(2.0)`: every row
    /// adds the same positive amount, so no weight is negative.
    PositiveConstant,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum EntityIdentity {
    PromqlLabelSet { excluding: Vec<ColumnRef> },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum SummaryInputExpr {
    Constant(f64),
    Column(ColumnRef),
    Tuple(Vec<SummaryInputExpr>),
    EntityIdentity(EntityIdentity),
}

/// The statistic computed from summary state by `SummaryEstimate`, for example
/// `Quantile { q: 0.99 }`. This is a result operation, not a workload query.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum SketchStatistic {
    /// sqrt(sum_v frequency(v)^2), not the norm of numeric input values.
    FrequencyL2,
    /// Shannon entropy of the value-frequency distribution, in bits.
    FrequencyEntropy,
    /// Extract the value at quantile rank `q` ∈ (0, 1].
    Quantile { q: f64 },
    /// Estimated count / frequency. `key` names which column is being
    /// queried (`ColumnRef::SampleValue` for the bare bucket total, with
    /// `value: None`); a `Named`/`Qualified` `key` paired with
    /// `value: Some(v)` is a per-item point lookup (e.g.
    /// `count(cms_metric{item="checkout"})` — `key` is `item`, `value` is
    /// `"checkout"`). `value` is carried here rather than resolved by the
    /// `SummaryExecutor` from a `Filter` predicate because a summary
    /// evaluation is read without access to the rest of the DAG.
    PointCount {
        key: ColumnRef,
        value: Option<String>,
    },
    /// Estimated number of distinct elements.
    Cardinality,
    /// Read the top-k entries from a heap-backed summary.
    TopK { k: usize },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keyed_summary_input_and_topk_evaluation_round_trip() {
        for input in [
            SummaryUpdate {
                item: Some(SummaryInputExpr::EntityIdentity(
                    EntityIdentity::PromqlLabelSet { excluding: vec![] },
                )),
                weight: SummaryInputExpr::Constant(1.0),
                weight_domain: WeightDomain::NonNegative {
                    proof: NonNegativeWeightProof::UnitCount,
                },
            },
            SummaryUpdate {
                item: Some(SummaryInputExpr::Tuple(vec![SummaryInputExpr::Column(
                    ColumnRef::Named("service".into()),
                )])),
                weight: SummaryInputExpr::Column(ColumnRef::SampleValue),
                weight_domain: WeightDomain::UnknownOrSigned,
            },
        ] {
            let input_json = serde_json::to_string(&input).unwrap();
            assert_eq!(
                serde_json::from_str::<SummaryUpdate>(&input_json).unwrap(),
                input
            );
            let query = SketchStatistic::TopK { k: 10 };
            let json = serde_json::to_string(&query).unwrap();
            assert_eq!(
                serde_json::from_str::<SketchStatistic>(&json).unwrap(),
                query
            );
        }
    }

    /// The raw-data-sample and summary-sample renames keep their JSON names,
    /// so committed plans and workloads still read.
    #[test]
    fn renamed_sample_terms_keep_their_wire_names() {
        use crate::ir::schema::FieldDataType;
        let json = |value: serde_json::Value| value.to_string();
        let raw = WeightDomain::RawDataSamplesOfMetric {
            metric: "m".to_string(),
        };
        assert_eq!(
            serde_json::to_string(&raw).unwrap(),
            json(serde_json::json!({"kind": "samples_of_metric", "metric": "m"}))
        );
        for (proof, name) in [
            (
                NonNegativeWeightProof::CounterRawDataSamples,
                "counter_samples",
            ),
            (
                NonNegativeWeightProof::DeclaredNonNegativeRawDataSamples,
                "declared_non_negative_samples",
            ),
        ] {
            assert_eq!(
                serde_json::to_string(&proof).unwrap(),
                format!("\"{name}\"")
            );
        }
        let summary_sample = FieldDataType::SummarySample(
            SamplingKind::Reservoir,
            SamplingParams::Reservoir { size: 64 },
        );
        assert_eq!(
            serde_json::to_string(&summary_sample).unwrap(),
            json(serde_json::json!({"Sample": ["Reservoir", {"Reservoir": {"size": 64}}]}))
        );
    }
}

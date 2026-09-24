//! DDSketch accumulator — wraps `asap_sketchlib::DdSketch`.
//!
//! Concrete accumulator reached from the modified-OTLP
//! `Metric.data = DDSketch{…}` hot path (PR C-CountSketch follow-up).
//! Merge via bucket-index alignment on the inner sketch, serialize as
//! MessagePack for the sink, and decode from the sketchlib
//! `DDSketchState` proto.
//!
//! Query semantics follow the STRICT policy after the DataPoint-level
//! METRIC scalars were dropped from the wire format
//! (ProjectASAP/sketchlib-go#243 / asap_sketchlib#57): the sketch serves
//! Quantile (log-bucket estimation) and Count (sum of bucket counts).
//! Sum/Min/Max are no longer derivable from the wire bytes and are
//! served by controller-provisioned exact aggregations — `query_statistic`
//! returns the unavailable-statistic error for them.

use crate::{AggregateCore, AggregationType, KeyByLabelValues, SerializableToSink};
use asap_sketchlib::{DdSketch, DdSketchDelta, MessagePackCodec};
use serde_json::Value;
use std::collections::HashMap;

/// DDSketch accumulator — inner log-bucketed sketch.
#[derive(Debug, Clone)]
pub struct DDSketchAccumulator {
    pub inner: DdSketch,
    /// Edge sampling probability `p ∈ (0,1]` carried on the producer's
    /// `SketchEnvelope.sample_p`. The edge admits each value with probability
    /// `p` (NitroSketch geometric skip), so `inner.total_count()` is ~`p`× the
    /// true count and a `Count` query must rescale by `1/p`. Quantiles are
    /// rank-preserving and need NO rescale. `1.0` (and the proto3 default `0.0`,
    /// dual-read as `1.0`) means no sampling, so the rescale is a no-op and the
    /// behaviour is identical to before. The factor is a per-series config
    /// constant: it is set from the first (always-full, otel.rs ingest
    /// contract) frame and preserved across delta applies, window-boundary
    /// `reset_to_empty`, and `merge_with`.
    pub sample_p: f64,
}

/// Normalize a wire `sample_p` to a usable rescale denominator. `0.0` (proto3
/// default), `>= 1.0`, and non-finite all collapse to `1.0` (no sampling), so a
/// `Count` rescale by `1/p` is a no-op on unsampled / legacy frames.
pub(crate) fn normalize_sample_p(p: f64) -> f64 {
    if p.is_finite() && p > 0.0 && p < 1.0 {
        p
    } else {
        1.0
    }
}

impl DDSketchAccumulator {
    pub fn new(alpha: f64) -> Self {
        Self {
            inner: DdSketch::new(alpha),
            sample_p: 1.0,
        }
    }

    /// Read the normalized edge sampling probability from a full-frame
    /// `SketchEnvelope`'s `sample_p`. Returns `1.0` (no sampling) for bare
    /// `DdSketchState` bytes or any decode failure — the primary production
    /// decode path (`reconstruct_via_runtime`) discards the envelope's
    /// `sample_p`, so the ingest call site re-reads it from the same bytes.
    pub fn sample_p_from_envelope_bytes(buffer: &[u8]) -> f64 {
        use asap_sketchlib::proto::sketchlib::SketchEnvelope;
        use prost::Message;
        SketchEnvelope::decode(buffer)
            .map(|env| normalize_sample_p(env.sample_p))
            .unwrap_or(1.0)
    }

    /// Decode from the modified OTLP wire format's
    /// `DDSketchDataPoint.sketch` bytes when
    /// `encoding = DDSKETCH_ENCODING_MSGPACK`. The bytes are the
    /// MessagePack serialization of the cross-language sketch-core
    /// `DdSketch` struct — PR I parity entrypoint.
    pub fn from_msgpack_bytes(buffer: &[u8]) -> Result<Self, Box<dyn std::error::Error>> {
        Ok(Self {
            inner: DdSketch::from_msgpack(buffer)
                .map_err(|e| format!("deserialize DdSketch msgpack: {e}"))?,
            // The msgpack DdSketch struct carries no envelope/sample_p; the
            // msgpack path is parity/test-only and is never edge-sampled.
            sample_p: 1.0,
        })
    }

    /// Decode from the modified OTLP wire format's
    /// `DDSketchDataPoint.sketch` bytes — the protobuf-encoded
    /// `asap_sketchlib::proto::sketchlib::DDSketchState` message that
    /// DataCollector's `ddsketchprocessor` emits when
    /// `encoding = DD_SKETCH_ENCODING_PROTO`.
    pub fn from_sketchlib_proto_bytes(buffer: &[u8]) -> Result<Self, Box<dyn std::error::Error>> {
        let (state, sample_p) = asap_sketch_codec::ddsketch_state(buffer)?;
        if !(state.alpha > 0.0 && state.alpha < 1.0) {
            return Err(format!(
                "DDSketchState alpha {} out of range (expected 0 < alpha < 1)",
                state.alpha
            )
            .into());
        }
        // The DataPoint-level METRIC scalars (count/sum/min/max) were
        // dropped from `DDSketchState` (ProjectASAP/sketchlib-go#243 /
        // asap_sketchlib#57). Reconstruct from the bucket store only:
        // `DdSketch::from_raw` now takes just (alpha, store_counts,
        // store_offset) and recovers `count` by summing the bucket
        // counts via `total_count()`.
        let inner = DdSketch::from_raw(state.alpha, state.store_counts.clone(), state.store_offset);
        Ok(Self {
            inner,
            sample_p: normalize_sample_p(sample_p),
        })
    }

    /// Apply a proto-encoded `DDSketchDelta` frame to this
    /// accumulator's inner sketch — the decode path for
    /// `DD_SKETCH_ENCODING_PROTO_DELTA` (paper §6.2 B3 / B4).
    ///
    /// Called against an accumulator that already carries the base
    /// sketch state; the caller is the per-series snapshot cache in
    /// the ingest path. Bytes are the
    /// `asap_sketchlib::proto::sketchlib::DdSketchDelta` message.
    pub fn apply_proto_delta_bytes(
        &mut self,
        buffer: &[u8],
    ) -> Result<(), Box<dyn std::error::Error>> {
        use asap_sketchlib::proto::sketchlib::DdSketchDelta as PbDelta;
        use prost::Message;

        let pb = PbDelta::decode(buffer).map_err(|e| format!("decode DDSketchDelta: {e}"))?;

        // The delta no longer carries d_count/d_sum/min/max
        // (ProjectASAP/sketchlib-go#243 / asap_sketchlib#57). Apply the
        // bucket deltas only; `DdSketch` recomputes its total count from
        // the merged bucket counts (`total_count()`).
        let buckets = pb
            .buckets
            .into_iter()
            .map(|b| (b.index, b.d_count))
            .collect();
        let delta = DdSketchDelta {
            buckets,
            ..Default::default()
        };
        self.inner
            .apply_delta(&delta)
            .map_err(|error| format!("apply DDSketchDelta: {error}"))?;
        Ok(())
    }
}

impl SerializableToSink for DDSketchAccumulator {
    fn serialize_to_json(&self) -> Value {
        // The DataPoint-level scalars (sum/min/max) are no longer carried
        // by `DdSketch` (ProjectASAP/sketchlib-go#243 / asap_sketchlib#57).
        // `count` is the bucket-derived total via `total_count()`.
        serde_json::json!({
            "alpha": self.inner.alpha,
            "store_offset": self.inner.store_offset,
            "bucket_count": self.inner.store_counts.len(),
            // Raw bucket-derived count (admitted samples). `sample_p` is the
            // scale factor a consumer applies (count / sample_p) to estimate
            // the true count; `query_statistic(Count)` already does this.
            "count": self.inner.total_count(),
            "sample_p": self.sample_p,
        })
    }

    fn serialize_to_bytes(&self) -> Vec<u8> {
        self.inner.to_msgpack().unwrap_or_default()
    }
}

impl AggregateCore for DDSketchAccumulator {
    fn clone_boxed_core(&self) -> Box<dyn AggregateCore> {
        Box::new(self.clone())
    }

    fn type_name(&self) -> &'static str {
        "DDSketchAccumulator"
    }

    /// Per-window base rotation: drop all bucket counts but keep the
    /// relative-accuracy parameter so the next window's bucket deltas
    /// index into the same log-bucket layout. `sample_p` is a per-series
    /// config constant (not per-window data), so it is intentionally
    /// preserved across the rotation — the next window's deltas are sampled
    /// at the same rate and must rescale identically.
    fn reset_to_empty(&mut self) {
        self.inner = DdSketch::new(self.inner.alpha);
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }

    fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
        self
    }

    fn merge_with(
        &self,
        other: &dyn AggregateCore,
    ) -> Result<Box<dyn AggregateCore>, Box<dyn std::error::Error + Send + Sync>> {
        if other.get_accumulator_type() != self.get_accumulator_type() {
            return Err(format!(
                "Cannot merge DDSketchAccumulator with {}",
                other.get_accumulator_type()
            )
            .into());
        }
        let other_dd = other
            .as_any()
            .downcast_ref::<DDSketchAccumulator>()
            .ok_or("Failed to downcast to DDSketchAccumulator")?;
        let merged_inner = DdSketch::merge_refs(&[&self.inner, &other_dd.inner])?;
        // sample_p is a per-series config constant, so both operands carry the
        // same value in practice. Prefer a sampled factor over the no-sampling
        // default so a merge with a freshly-reset (1.0) base keeps the series'
        // sampling rate.
        let sample_p = if self.sample_p < 1.0 {
            self.sample_p
        } else {
            other_dd.sample_p
        };
        Ok(Box::new(Self {
            inner: merged_inner,
            sample_p,
        }))
    }

    fn get_accumulator_type(&self) -> AggregationType {
        AggregationType::DDSketch
    }

    fn get_keys(&self) -> Option<Vec<KeyByLabelValues>> {
        None
    }

    fn query_statistic(
        &self,
        statistic: crate::Statistic,
        _key: &Option<KeyByLabelValues>,
        query_kwargs: &HashMap<String, String>,
    ) -> Result<f64, Box<dyn std::error::Error + Send + Sync>> {
        use crate::Statistic;

        match statistic {
            Statistic::Quantile => {
                // PromQL `histogram_quantile(q, …)` and
                // `quantile_over_time(q, …)` both land here with
                // `q` in `query_kwargs["quantile"]`. Default to
                // 0.99 when the caller didn't provide one
                // (defensive — pattern-matched queries in
                // `inference_config.yaml` always populate it).
                let q: f64 = query_kwargs
                    .get("quantile")
                    .and_then(|s| s.parse().ok())
                    .unwrap_or(0.99);
                if !(0.0..=1.0).contains(&q) {
                    return Err(format!("DDSketchAccumulator: quantile {q} out of [0,1]").into());
                }
                self.inner.quantile(q).ok_or_else(|| {
                    "DDSketchAccumulator: quantile() returned None (sketch empty?)".into()
                })
            }
            // Count is derived by summing the bucket store counts — the only
            // DataPoint-level scalar that survives the wire-format trim
            // (ProjectASAP/sketchlib-go#243 / asap_sketchlib#57). When the edge
            // sampled this series (sample_p < 1.0), the stored count is ~p× the
            // true count, so rescale by 1/sample_p to recover an unbiased
            // estimate. sample_p == 1.0 (unsampled / legacy) makes this a no-op.
            Statistic::Count => Ok(self.inner.total_count() as f64 / self.sample_p),
            // STRICT policy: the Sum/Min/Max scalars were removed from
            // the DDSketch wire format. They are now served by the
            // controller-provisioned exact aggregations (an exact `Sum`
            // and an exact `MinMax`), NOT estimated from the buckets.
            // Surface the unavailable-statistic error so the query path
            // routes to those aggregations instead of returning a wrong
            // (0 / panicked) value.
            Statistic::Sum => Err(
                "DDSketchAccumulator: Sum not available from DDSketch wire format \
                 (ProjectASAP/sketchlib-go#243); use an exact Sum aggregation"
                    .into(),
            ),
            Statistic::Min | Statistic::Max => Err(format!(
                "DDSketchAccumulator: {statistic:?} not available from DDSketch wire format \
                 (ProjectASAP/sketchlib-go#243); use an exact MinMax aggregation",
            )
            .into()),
            other => Err(format!(
                "DDSketchAccumulator: statistic {other:?} not supported (only Quantile / Count)",
            )
            .into()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // The DataPoint-level METRIC scalars (count/sum/min/max) were dropped
    // from `DdSketchState` (ProjectASAP/sketchlib-go#243 /
    // asap_sketchlib#57); the proto now carries only
    // `alpha`/`store_counts`/`store_offset`.
    fn encode_state(alpha: f64, store_counts: Vec<u64>, store_offset: i32) -> Vec<u8> {
        use asap_sketchlib::proto::sketchlib::{sketch_envelope, DdSketchState, SketchEnvelope};
        use prost::Message;
        let state = DdSketchState {
            alpha,
            store_counts,
            store_offset,
        };
        SketchEnvelope {
            sketch_state: Some(sketch_envelope::SketchState::Ddsketch(state)),
            ..Default::default()
        }
        .encode_to_vec()
    }

    #[test]
    fn test_from_sketchlib_proto_bytes_round_trip() {
        let bytes = encode_state(0.01, vec![1, 2, 3, 4], -2);
        let acc = DDSketchAccumulator::from_sketchlib_proto_bytes(&bytes).expect("decode ok");
        assert_eq!(acc.inner.alpha, 0.01);
        assert_eq!(acc.inner.store_counts, vec![1, 2, 3, 4]);
        assert_eq!(acc.inner.store_offset, -2);
        // `count` is recovered by summing the bucket store counts.
        assert_eq!(acc.inner.total_count(), 10);
    }

    #[test]
    fn test_from_sketchlib_proto_bytes_rejects_invalid_alpha() {
        let bytes = encode_state(0.0, vec![1], 0);
        let result = DDSketchAccumulator::from_sketchlib_proto_bytes(&bytes);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("alpha"));
    }

    #[test]
    fn test_from_sketchlib_proto_bytes_envelope_wrapped() {
        // Mirrors what DataCollector's ddsketchprocessor emits: the
        // state wrapped in a `SketchEnvelope{ddsketch: ...}` via
        // sketchlib-go's `SerializePortableFO` + `proto.Marshal`.
        use asap_sketchlib::proto::sketchlib::{sketch_envelope, DdSketchState, SketchEnvelope};
        use prost::Message;

        let state = DdSketchState {
            alpha: 0.01,
            store_counts: vec![1, 2, 3, 4],
            store_offset: -2,
        };
        let env = SketchEnvelope {
            sketch_state: Some(sketch_envelope::SketchState::Ddsketch(state)),
            ..Default::default()
        };
        let bytes = env.encode_to_vec();

        let acc = DDSketchAccumulator::from_sketchlib_proto_bytes(&bytes)
            .expect("envelope-wrapped decode should succeed");
        assert_eq!(acc.inner.alpha, 0.01);
        assert_eq!(acc.inner.total_count(), 10);
    }

    #[test]
    fn test_from_sketchlib_proto_bytes_envelope_wrong_sketch_type() {
        use asap_sketchlib::proto::sketchlib::{sketch_envelope, KllState, SketchEnvelope};
        use prost::Message;

        let env = SketchEnvelope {
            sketch_state: Some(sketch_envelope::SketchState::Kll(KllState::default())),
            ..Default::default()
        };
        let bytes = env.encode_to_vec();

        let result = DDSketchAccumulator::from_sketchlib_proto_bytes(&bytes);
        assert!(result.is_err(), "wrong-sketch envelope should error");
    }

    #[test]
    fn test_aggregate_core_merge_aligns_buckets() {
        let a = DDSketchAccumulator {
            inner: DdSketch::from_raw(0.01, vec![1, 1, 1], -1),
            sample_p: 1.0,
        };
        let b = DDSketchAccumulator {
            inner: DdSketch::from_raw(0.01, vec![10, 10, 10], 0),
            sample_p: 1.0,
        };
        let merged_box = a.merge_with(&b).expect("merge ok");
        let merged = merged_box
            .as_any()
            .downcast_ref::<DDSketchAccumulator>()
            .expect("downcast ok");
        assert_eq!(merged.inner.store_counts, vec![1, 11, 11, 10]);
        assert_eq!(merged.inner.store_offset, -1);
        assert_eq!(merged.inner.total_count(), 33);
    }

    #[test]
    fn test_aggregate_core_merge_wrong_type_rejects() {
        use crate::summary_operators::count_sketch::CountSketchAccumulator;
        let dd = DDSketchAccumulator::new(0.01);
        let cs = CountSketchAccumulator::new(2, 3);
        assert!(dd.merge_with(&cs).is_err());
    }

    #[test]
    fn test_from_msgpack_bytes_round_trip() {
        let original = DdSketch::from_raw(0.01, vec![5, 10, 15, 20], -2);
        let bytes = original.to_msgpack().unwrap();
        let acc = DDSketchAccumulator::from_msgpack_bytes(&bytes).expect("decode ok");
        assert_eq!(acc.inner.alpha, 0.01);
        assert_eq!(acc.inner.store_counts, vec![5, 10, 15, 20]);
        assert_eq!(acc.inner.store_offset, -2);
        // `count` is recovered by summing the bucket store counts.
        assert_eq!(acc.inner.total_count(), 50);
    }

    #[test]
    fn test_from_msgpack_bytes_rejects_garbage() {
        let result = DDSketchAccumulator::from_msgpack_bytes(b"not valid msgpack");
        assert!(result.is_err());
    }

    #[test]
    fn test_apply_proto_delta_bytes_round_trip() {
        use asap_sketchlib::proto::sketchlib::{DdSketchBucketDelta, DdSketchDelta as PbDelta};
        use prost::Message;

        let mut acc = DDSketchAccumulator::new(0.01);
        acc.inner = DdSketch::from_raw(0.01, vec![1, 2, 3], 0);

        // The wire delta now carries only bucket deltas (tags 2-7
        // reserved); `DdSketchBucketDelta` has just `index` + `d_count`.
        let bytes = PbDelta {
            buckets: vec![
                DdSketchBucketDelta {
                    index: 0,
                    d_count: 10,
                },
                DdSketchBucketDelta {
                    index: 2,
                    d_count: 20,
                },
            ],
        }
        .encode_to_vec();

        acc.apply_proto_delta_bytes(&bytes).expect("apply ok");
        assert_eq!(acc.inner.store_counts, vec![11, 2, 23]);
        // `count` recomputed from the merged buckets: 11 + 2 + 23 = 36.
        assert_eq!(acc.inner.total_count(), 36);
    }

    /// A valid protobuf with an inadmissible span must not acknowledge a dropped update.
    #[test]
    fn test_apply_proto_delta_rejects_span_without_mutating_state() {
        use asap_sketchlib::proto::sketchlib::{DdSketchBucketDelta, DdSketchDelta as PbDelta};
        use prost::Message;
        let mut acc = DDSketchAccumulator::new(0.01);
        acc.inner = DdSketch::from_raw(0.01, vec![1, 2, 3], 0);
        let bytes = PbDelta {
            buckets: vec![DdSketchBucketDelta {
                index: i32::MAX,
                d_count: 1,
            }],
        }
        .encode_to_vec();
        assert!(acc.apply_proto_delta_bytes(&bytes).is_err());
        assert_eq!(acc.inner.store_counts, vec![1, 2, 3]);
        assert_eq!(acc.inner.store_offset, 0);
    }

    #[test]
    fn test_apply_proto_delta_bytes_rejects_garbage() {
        let mut acc = DDSketchAccumulator::new(0.01);
        assert!(acc.apply_proto_delta_bytes(b"not valid proto").is_err());
    }

    // ----- query_statistic STRICT policy -----
    //
    // After the DataPoint-level METRIC scalars were dropped from the
    // DDSketch wire format (ProjectASAP/sketchlib-go#243 /
    // asap_sketchlib#57), DDSketch serves only quantiles and Count.
    // Sum/Min/Max move to controller-provisioned exact aggregations and
    // MUST surface the unavailable-statistic error (never a panic / 0).

    fn sample_accumulator() -> DDSketchAccumulator {
        // Build the in-memory sketch from bucket counts only — no scalars.
        DDSketchAccumulator {
            inner: DdSketch::from_raw(0.01, vec![1, 2, 3, 4], -2),
            sample_p: 1.0,
        }
    }

    #[test]
    fn test_query_statistic_quantile_is_sketch_derived() {
        use crate::Statistic;
        let acc = sample_accumulator();
        let mut kwargs = HashMap::new();
        kwargs.insert("quantile".to_string(), "0.5".to_string());
        let v = acc
            .query_statistic(Statistic::Quantile, &None, &kwargs)
            .expect("quantile should be served from the sketch buckets");
        assert!(
            v.is_finite() && v > 0.0,
            "quantile estimate should be positive finite, got {v}"
        );
    }

    #[test]
    fn test_query_statistic_count_is_bucket_derived() {
        use crate::Statistic;
        let acc = sample_accumulator();
        let v = acc
            .query_statistic(Statistic::Count, &None, &HashMap::new())
            .expect("count should be derivable from the bucket store");
        // 1 + 2 + 3 + 4 = 10.
        assert_eq!(v, 10.0);
    }

    #[test]
    fn test_query_statistic_sum_min_max_return_unavailable_error() {
        use crate::Statistic;
        let acc = sample_accumulator();
        for stat in [Statistic::Sum, Statistic::Min, Statistic::Max] {
            let result = acc.query_statistic(stat, &None, &HashMap::new());
            assert!(
                result.is_err(),
                "{stat:?} must return the unavailable-statistic error (not a panic / 0)"
            );
            let msg = result.unwrap_err().to_string();
            assert!(
                msg.contains("not available"),
                "{stat:?} error should explain the statistic is unavailable, got: {msg}"
            );
        }
    }

    // ----- sample_p count rescale -----
    //
    // When the edge sampled a DDSketch (sample_p < 1.0), the stored count is
    // ~p× the true count, so Count rescales by 1/p. Quantiles are
    // rank-preserving and must NOT be rescaled.

    #[test]
    fn test_count_is_rescaled_by_sample_p() {
        use crate::Statistic;
        let acc = DDSketchAccumulator {
            inner: DdSketch::from_raw(0.01, vec![1, 2, 3, 4], -2),
            sample_p: 0.1,
        };
        let c = acc
            .query_statistic(Statistic::Count, &None, &HashMap::new())
            .expect("count ok");
        // Raw bucket sum 10, rescaled by 1/0.1 = 100.
        assert!((c - 100.0).abs() < 1e-9, "expected rescaled 100, got {c}");
    }

    #[test]
    fn test_quantile_ignores_sample_p() {
        use crate::Statistic;
        let mut kwargs = HashMap::new();
        kwargs.insert("quantile".to_string(), "0.5".to_string());
        let unsampled = DDSketchAccumulator {
            inner: DdSketch::from_raw(0.01, vec![1, 2, 3, 4], -2),
            sample_p: 1.0,
        };
        let sampled = DDSketchAccumulator {
            inner: DdSketch::from_raw(0.01, vec![1, 2, 3, 4], -2),
            sample_p: 0.1,
        };
        let qu = unsampled
            .query_statistic(Statistic::Quantile, &None, &kwargs)
            .expect("q ok");
        let qs = sampled
            .query_statistic(Statistic::Quantile, &None, &kwargs)
            .expect("q ok");
        assert_eq!(qu, qs, "quantile must be sample_p-invariant");
    }

    #[test]
    fn test_from_sketchlib_proto_bytes_reads_envelope_sample_p() {
        use crate::Statistic;
        use asap_sketchlib::proto::sketchlib::{sketch_envelope, DdSketchState, SketchEnvelope};
        use prost::Message;

        let env = SketchEnvelope {
            sample_p: 0.25,
            sketch_state: Some(sketch_envelope::SketchState::Ddsketch(DdSketchState {
                alpha: 0.01,
                store_counts: vec![2, 4, 6, 8],
                store_offset: -2,
            })),
            ..Default::default()
        };
        let bytes = env.encode_to_vec();
        let acc = DDSketchAccumulator::from_sketchlib_proto_bytes(&bytes).expect("decode ok");
        assert_eq!(acc.sample_p, 0.25);
        // Raw 20, rescaled 20 / 0.25 = 80.
        let c = acc
            .query_statistic(Statistic::Count, &None, &HashMap::new())
            .expect("count ok");
        assert!((c - 80.0).abs() < 1e-9, "expected rescaled 80, got {c}");
    }

    #[test]
    fn test_sample_p_normalization() {
        // proto3 default (0.0), >=1.0, and non-finite all mean no sampling.
        assert_eq!(normalize_sample_p(0.0), 1.0);
        assert_eq!(normalize_sample_p(1.0), 1.0);
        assert_eq!(normalize_sample_p(1.5), 1.0);
        assert_eq!(normalize_sample_p(f64::NAN), 1.0);
        assert_eq!(normalize_sample_p(-0.1), 1.0);
        assert_eq!(normalize_sample_p(0.5), 0.5);
    }

    #[test]
    fn test_sample_p_from_envelope_bytes_defaults_to_one() {
        use asap_sketchlib::proto::sketchlib::DdSketchState;
        use prost::Message;
        // Bare DdSketchState bytes (no envelope) → no sampling info → 1.0.
        let bare = DdSketchState {
            alpha: 0.01,
            store_counts: vec![1, 2, 3],
            store_offset: 0,
        }
        .encode_to_vec();
        assert_eq!(
            DDSketchAccumulator::sample_p_from_envelope_bytes(&bare),
            1.0
        );
    }

    #[test]
    fn test_reset_to_empty_preserves_sample_p() {
        let mut acc = DDSketchAccumulator {
            inner: DdSketch::from_raw(0.01, vec![1, 2, 3], 0),
            sample_p: 0.2,
        };
        acc.reset_to_empty();
        assert_eq!(acc.sample_p, 0.2, "window rotation must keep sample_p");
        assert_eq!(acc.inner.total_count(), 0, "buckets cleared");
    }

    #[test]
    fn test_merge_prefers_sampled_factor() {
        // A sampled base merged with a freshly-reset (1.0) operand keeps the
        // series' sampling rate.
        let a = DDSketchAccumulator {
            inner: DdSketch::from_raw(0.01, vec![1, 1, 1], 0),
            sample_p: 0.1,
        };
        let b = DDSketchAccumulator {
            inner: DdSketch::from_raw(0.01, vec![1, 1, 1], 0),
            sample_p: 1.0,
        };
        let merged = a.merge_with(&b).expect("merge ok");
        let merged = merged
            .as_any()
            .downcast_ref::<DDSketchAccumulator>()
            .expect("downcast ok");
        assert_eq!(merged.sample_p, 0.1);
    }
}

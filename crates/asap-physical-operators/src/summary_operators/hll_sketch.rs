//! HLL accumulator — wraps `asap_sketchlib::HllSketch`.
//!
//! Concrete accumulator reached from the modified-OTLP
//! `Metric.data = HLLSketch{…}` hot path (PR C-CountSketch follow-up).
//! Mirrors the CountSketch accumulator's shape: merge via register-wise
//! max on the inner sketch, serialize as MessagePack for the sink, and
//! decode from the sketchlib `HyperLogLogState` proto.
//!
//! Query semantics (cardinality estimation via the three HLL variants'
//! estimators) are intentionally deferred — the wire format carries the
//! registers + variant + HIP accumulators losslessly, so the merge +
//! store round-trip works end-to-end without that richer query surface.

use crate::summary_operators::dd_sketch::normalize_sample_p;
use crate::{AggregateCore, AggregationType, KeyByLabelValues, SerializableToSink};
use asap_sketchlib::{HllSketch, HllVariant, MessagePackCodec};
use serde_json::Value;
use std::collections::HashMap;

/// Decode one protobuf base-128 varint (LEB128) from the front of `buf`.
/// Returns `(value, bytes_consumed)`, or `None` if the buffer is truncated
/// or the varint overflows u64.
pub(crate) fn read_uvarint(buf: &[u8]) -> Option<(u64, usize)> {
    let mut result: u64 = 0;
    let mut shift: u32 = 0;
    for (i, &b) in buf.iter().enumerate() {
        if shift >= 64 {
            return None;
        }
        result |= u64::from(b & 0x7f) << shift;
        if b & 0x80 == 0 {
            return Some((result, i + 1));
        }
        shift += 7;
    }
    None
}

/// Expand sketchlib-go's sparse HLL register encoding
/// (`HLLSparseRegisters.packed`) into the dense `num_registers`-byte array.
///
/// Layout (sketchlib-go `proto/hll/hll.proto`): varint-packed
/// `(index_delta, value)` pairs in ascending index order; `prev_index`
/// starts at 0, so each register's absolute index is the running sum of the
/// deltas. Mirrors the Go encoder in `sketches/HLL/sparse.go`
/// (`encodeSparseRegisters`). The reconstructed array is byte-identical to
/// the dense `registers` field a high-cardinality producer would have sent.
pub(crate) fn expand_sparse_hll_registers(
    packed: &[u8],
    num_registers: usize,
) -> Result<Vec<u8>, Box<dyn std::error::Error>> {
    let mut regs = vec![0u8; num_registers];
    let mut prev: u64 = 0;
    let mut pos = 0usize;
    while pos < packed.len() {
        let (delta, n1) = read_uvarint(&packed[pos..])
            .ok_or("HLLSparseRegisters.packed: truncated index_delta varint")?;
        pos += n1;
        let (value, n2) = read_uvarint(&packed[pos..])
            .ok_or("HLLSparseRegisters.packed: truncated value varint")?;
        pos += n2;
        let idx = prev + delta;
        let i = usize::try_from(idx)
            .map_err(|_| format!("HLLSparseRegisters: index {idx} overflows usize"))?;
        if i >= num_registers {
            return Err(format!(
                "HLLSparseRegisters: register index {i} >= num_registers {num_registers}"
            )
            .into());
        }
        regs[i] = u8::try_from(value)
            .map_err(|_| format!("HLLSparseRegisters: register value {value} > 255"))?;
        prev = idx;
    }
    Ok(regs)
}

/// HLL accumulator — inner register array + variant metadata.
#[derive(Debug, Clone)]
pub struct HllSketchAccumulator {
    pub inner: HllSketch,
    /// Edge sampling probability `p ∈ (0,1]` carried on the producer's
    /// `SketchEnvelope.sample_p`. HLL uses HASH-THRESHOLD sampling — each
    /// DISTINCT key is admitted into the sketch with probability `p`, so the
    /// register-derived distinct-count estimate is ~`p`× the true
    /// cardinality and a `Cardinality`/`Count` query must rescale by `1/p`.
    /// `1.0` (and the proto3 default `0.0`, dual-read as `1.0`) means no
    /// sampling, so the rescale is a no-op and the behaviour is identical to
    /// before. Mirrors `DDSketchAccumulator::sample_p`; set from the envelope
    /// at the `from_sketchlib_proto_bytes` decode site and preserved across
    /// `reset_to_empty` and `merge_with`.
    ///
    /// NOTE: HLL edge sampling is currently force-disabled in the edge
    /// (`warm_sketch.go` HLL case always emits `sample_p = 1.0`), so in
    /// practice `p = 1.0` today and this is a latent-correctness fix that
    /// activates if HLL sampling is ever enabled.
    pub sample_p: f64,
}

impl HllSketchAccumulator {
    pub fn new(variant: HllVariant, precision: u32) -> Self {
        Self {
            inner: HllSketch::new(variant, precision),
            sample_p: 1.0,
        }
    }

    /// Decode from the modified OTLP wire format's
    /// `HLLSketchDataPoint.sketch` bytes when
    /// `encoding = HLL_SKETCH_ENCODING_MSGPACK`. The bytes are the
    /// MessagePack serialization of the cross-language sketch-core
    /// `HllSketch` struct — PR I parity entrypoint.
    pub fn from_msgpack_bytes(buffer: &[u8]) -> Result<Self, Box<dyn std::error::Error>> {
        Ok(Self {
            inner: HllSketch::from_msgpack(buffer)
                .map_err(|e| format!("deserialize HllSketch msgpack: {e}"))?,
            // The msgpack HllSketch struct carries no envelope/sample_p; the
            // msgpack path is parity/test-only and is never edge-sampled.
            sample_p: 1.0,
        })
    }

    /// Decode from the modified OTLP wire format's
    /// `HLLSketchDataPoint.sketch` bytes — the protobuf-encoded
    /// `asap_sketchlib::proto::sketchlib::HyperLogLogState` message
    /// that DataCollector's `hllprocessor` emits when
    /// `encoding = HLL_SKETCH_ENCODING_PROTO`.
    pub fn from_sketchlib_proto_bytes(buffer: &[u8]) -> Result<Self, Box<dyn std::error::Error>> {
        use asap_sketchlib::proto::sketchlib::{
            sketch_envelope, HllVariant as ProtoVariant, HyperLogLogState, SketchEnvelope,
        };
        use prost::Message;

        // DataCollector's hllprocessor wraps the state in a
        // `SketchEnvelope{hll: HyperLogLogState}` via sketchlib-go's
        // `SerializePortableFO` + `proto.Marshal`. Try envelope first,
        // fall back to bare `HyperLogLogState` for callers (e.g. unit
        // tests) that encode the state directly. Mirrors the PR #14
        // fix on `CountMinSketchAccumulator::from_sketchlib_proto_bytes`.
        // Capture the envelope's `sample_p` alongside the state so a
        // Cardinality query can rescale the distinct-count estimate by
        // `1/p`. Bare `HyperLogLogState` bytes (no envelope) carry no
        // sampling info → `sample_p` 1.0 (no rescale). Mirrors
        // `DDSketchAccumulator`.
        let (state, sample_p) = match SketchEnvelope::decode(buffer) {
            Ok(env) => {
                let sp = env.sample_p;
                match env.sketch_state {
                    Some(sketch_envelope::SketchState::Hll(st)) => (st, sp),
                    Some(other) => {
                        return Err(format!(
                            "SketchEnvelope contains non-HLL sketch: {:?}",
                            std::mem::discriminant(&other)
                        )
                        .into());
                    }
                    None => (
                        HyperLogLogState::decode(buffer)
                            .map_err(|e| format!("decode HyperLogLogState: {e}"))?,
                        1.0,
                    ),
                }
            }
            Err(_) => (
                HyperLogLogState::decode(buffer)
                    .map_err(|e| format!("decode HyperLogLogState: {e}"))?,
                1.0,
            ),
        };
        if state.precision == 0 || state.precision > 20 {
            return Err(format!(
                "HyperLogLogState precision {} out of range (expected 1..=20)",
                state.precision
            )
            .into());
        }
        let expected_len = 1usize << state.precision;
        // Register resolution. sketchlib-go emits the SPARSE
        // `registers_sparse` (proto tag 7) form below its dense/sparse
        // crossover (~6000 non-zero registers — see
        // sketchlib-go/sketches/HLL/sparse.go); low-cardinality producers
        // (the common case) therefore leave the dense `registers` (tag 3)
        // field empty. The proto contract (hll.proto) is: read whichever of
        // `registers` / `registers_sparse` is present; if both are empty the
        // sketch is all-zero. Reconstruct the dense 2^precision array in all
        // three cases so the inner `HllSketch` always gets a full register
        // vector.
        let dense_registers: Vec<u8> = if state.registers.len() == expected_len {
            state.registers.clone()
        } else if !state.registers.is_empty() {
            // A non-empty dense field of the wrong length is a malformed frame.
            return Err(format!(
                "HyperLogLogState registers has {} bytes, expected 2^precision = {}",
                state.registers.len(),
                expected_len
            )
            .into());
        } else if let Some(sparse) = state.registers_sparse.as_ref() {
            expand_sparse_hll_registers(&sparse.packed, expected_len)?
        } else {
            // Neither representation populated → all-zero register array.
            vec![0u8; expected_len]
        };
        let proto_variant = ProtoVariant::try_from(state.variant)
            .map_err(|_| format!("HyperLogLogState has unknown variant tag {}", state.variant))?;
        let variant = match proto_variant {
            ProtoVariant::Unspecified => HllVariant::Unspecified,
            ProtoVariant::Regular => HllVariant::Regular,
            ProtoVariant::ErtlMle => HllVariant::Datafusion,
            ProtoVariant::Hip => HllVariant::Hip,
        };
        let inner = HllSketch::from_raw(
            variant,
            state.precision,
            dense_registers,
            state.hip_kxq0,
            state.hip_kxq1,
            state.hip_est,
        );
        Ok(Self {
            inner,
            sample_p: normalize_sample_p(sample_p),
        })
    }

    /// Apply a proto-encoded `HLLDelta` frame to this accumulator's
    /// inner sketch — the decode path for
    /// `HLL_SKETCH_ENCODING_PROTO_DELTA` (paper §6.2 B3 / B4).
    ///
    /// Called against an accumulator that already carries the base
    /// sketch state; the caller is the per-series snapshot cache in
    /// the ingest path. Bytes are the
    /// `asap_sketchlib::proto::sketchlib::HllDelta` message.
    pub fn apply_proto_delta_bytes(
        &mut self,
        buffer: &[u8],
    ) -> Result<(), Box<dyn std::error::Error>> {
        // The HLLDelta wire format is a varint-packed (index_delta, value) blob;
        // decode + apply (register-wise max) via the shared sketch library so
        // the unpacking stays a single source of truth.
        self.inner
            .apply_delta_bytes(buffer)
            .map_err(|e| format!("apply HLLDelta: {e}"))?;
        Ok(())
    }
}

impl SerializableToSink for HllSketchAccumulator {
    fn serialize_to_json(&self) -> Value {
        serde_json::json!({
            "variant": format!("{:?}", self.inner.variant),
            "precision": self.inner.precision,
            "register_bytes": self.inner.registers.len(),
            "hip_kxq0": self.inner.hip_kxq0,
            "hip_kxq1": self.inner.hip_kxq1,
            "hip_est": self.inner.hip_est,
        })
    }

    fn serialize_to_bytes(&self) -> Vec<u8> {
        self.inner.to_msgpack().unwrap_or_default()
    }
}

impl AggregateCore for HllSketchAccumulator {
    fn approx_memory_bytes(&self) -> usize {
        std::mem::size_of::<Self>().saturating_add(self.inner.registers.capacity())
    }
    fn clone_boxed_core(&self) -> Box<dyn AggregateCore> {
        Box::new(self.clone())
    }

    fn type_name(&self) -> &'static str {
        "HllSketchAccumulator"
    }

    /// Per-window base rotation: zero the registers but keep the variant
    /// and precision. Critical for HLL — its register-wise `max` merge
    /// has no inverse, so a never-reset base accumulates the all-time-max
    /// across windows (`docs/delta-baseline-contract.md` §1.5); rotating
    /// to an empty register array makes per-window cardinality correct.
    /// `sample_p` is a per-series config constant (not per-window data), so
    /// it is intentionally preserved across the rotation — mirrors
    /// `DDSketchAccumulator`.
    fn reset_to_empty(&mut self) {
        self.inner = HllSketch::new(self.inner.variant, self.inner.precision);
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
                "Cannot merge HllSketchAccumulator with {}",
                other.get_accumulator_type()
            )
            .into());
        }
        let other_hll = other
            .as_any()
            .downcast_ref::<HllSketchAccumulator>()
            .ok_or("Failed to downcast to HllSketchAccumulator")?;
        let merged_inner = HllSketch::merge_refs(&[&self.inner, &other_hll.inner])?;
        // Mirror DDSketchAccumulator's merge policy exactly: sample_p is a
        // per-series config constant, so both operands carry the same value
        // in practice. Prefer a sampled factor over the no-sampling default
        // so a merge with a freshly-reset (1.0) base keeps the series'
        // sampling rate.
        let sample_p = if self.sample_p < 1.0 {
            self.sample_p
        } else {
            other_hll.sample_p
        };
        Ok(Box::new(Self {
            inner: merged_inner,
            sample_p,
        }))
    }

    fn get_accumulator_type(&self) -> AggregationType {
        AggregationType::HLL
    }

    fn get_keys(&self) -> Option<Vec<KeyByLabelValues>> {
        None
    }

    fn query_statistic(
        &self,
        statistic: crate::Statistic,
        _key: &Option<KeyByLabelValues>,
        _query_kwargs: &HashMap<String, String>,
    ) -> Result<f64, Box<dyn std::error::Error + Send + Sync>> {
        use crate::Statistic;
        match statistic {
            // HLL's natural answer is unique-cardinality. PromQL's
            // `count_over_time(...)` and `count(...)` both surface
            // as `Statistic::Count` after pattern matching but
            // semantically they mean "how many distinct values
            // were observed in this window" when the underlying
            // aggregator is HLL — that's the cardinality estimate,
            // not a sample-count. Accept both.
            Statistic::Cardinality | Statistic::Count => {
                // HLL uses hash-threshold sampling — each distinct key is
                // admitted with probability `sample_p`, so the register-
                // derived distinct-count estimate is ~`p`× the true
                // cardinality. Rescale by `1/sample_p` for an unbiased
                // estimate. `sample_p == 1.0` (unsampled / legacy / edge
                // HLL sampling currently force-disabled) makes this a no-op.
                Ok(hll_cardinality_estimate(&self.inner.registers) / self.sample_p)
            }
            other => Err(format!(
                "HllSketchAccumulator: statistic {:?} not supported (only Cardinality / Count)",
                other,
            )
            .into()),
        }
    }
}

/// Standard HyperLogLog cardinality estimate with the canonical
/// `α_m × m² / Σ 2^(-register[i])` formula plus the small-range
/// (linear-counting) and large-range (32-bit space) corrections
/// from the original Flajolet et al. paper.
///
/// Inlined here rather than added as a method on `asap_sketchlib::HllSketch`
/// because the existing `asap_sketchlib::asap` types only expose merge /
/// serialize today; adding a query method there would force a
/// cross-crate change.
fn hll_cardinality_estimate(registers: &[u8]) -> f64 {
    let m = registers.len() as f64;
    if m == 0.0 {
        return 0.0;
    }
    let alpha = match registers.len() {
        16 => 0.673,
        32 => 0.697,
        64 => 0.709,
        _ => 0.7213 / (1.0 + 1.079 / m),
    };

    let mut sum = 0.0f64;
    let mut zero_registers = 0usize;
    for &r in registers {
        sum += 2f64.powi(-(r as i32));
        if r == 0 {
            zero_registers += 1;
        }
    }
    let raw = alpha * m * m / sum;

    // Small-range (linear-counting) correction.
    if raw <= 2.5 * m && zero_registers > 0 {
        return m * (m / zero_registers as f64).ln();
    }

    // Large-range correction (only meaningful with 32-bit register
    // spaces; sketch-core uses up to 64-bit hashes so this branch
    // rarely fires in practice — kept for completeness).
    let two_pow_32 = 4_294_967_296f64;
    if raw > two_pow_32 / 30.0 {
        return -two_pow_32 * (1.0 - raw / two_pow_32).ln();
    }
    raw
}

#[cfg(test)]
mod tests {
    use super::*;

    fn encode_state(
        variant: i32,
        precision: u32,
        registers: Vec<u8>,
        hip_kxq0: f64,
        hip_kxq1: f64,
        hip_est: f64,
    ) -> Vec<u8> {
        use asap_sketchlib::proto::sketchlib::HyperLogLogState;
        use prost::Message;
        let state = HyperLogLogState {
            variant,
            precision,
            registers,
            hip_kxq0,
            hip_kxq1,
            hip_est,
            registers_sparse: None,
        };
        state.encode_to_vec()
    }

    #[test]
    fn test_from_sketchlib_proto_bytes_regular() {
        use asap_sketchlib::proto::sketchlib::HllVariant as ProtoVariant;
        let bytes = encode_state(
            ProtoVariant::Regular as i32,
            2,
            vec![1, 2, 3, 4],
            0.0,
            0.0,
            0.0,
        );
        let acc = HllSketchAccumulator::from_sketchlib_proto_bytes(&bytes).expect("decode ok");
        assert_eq!(acc.inner.variant, HllVariant::Regular);
        assert_eq!(acc.inner.precision, 2);
        assert_eq!(acc.inner.registers, vec![1, 2, 3, 4]);
    }

    #[test]
    fn test_from_sketchlib_proto_bytes_hip_preserves_accumulators() {
        use asap_sketchlib::proto::sketchlib::HllVariant as ProtoVariant;
        let bytes = encode_state(
            ProtoVariant::Hip as i32,
            2,
            vec![0, 0, 0, 0],
            1.5,
            2.5,
            42.0,
        );
        let acc = HllSketchAccumulator::from_sketchlib_proto_bytes(&bytes).expect("decode ok");
        assert_eq!(acc.inner.variant, HllVariant::Hip);
        assert_eq!(acc.inner.hip_kxq0, 1.5);
        assert_eq!(acc.inner.hip_kxq1, 2.5);
        assert_eq!(acc.inner.hip_est, 42.0);
    }

    #[test]
    fn test_from_sketchlib_proto_bytes_envelope_wrapped() {
        // Mirrors what DataCollector's hllprocessor emits: the state
        // wrapped in a `SketchEnvelope{hll: ...}` via sketchlib-go's
        // `SerializePortableFO` + `proto.Marshal`.
        use asap_sketchlib::proto::sketchlib::{
            sketch_envelope, HllVariant as ProtoVariant, HyperLogLogState, SketchEnvelope,
        };
        use prost::Message;

        let state = HyperLogLogState {
            variant: ProtoVariant::Regular as i32,
            precision: 2,
            registers: vec![1, 2, 3, 4],
            hip_kxq0: 0.0,
            hip_kxq1: 0.0,
            hip_est: 0.0,
            registers_sparse: None,
        };
        let env = SketchEnvelope {
            sketch_state: Some(sketch_envelope::SketchState::Hll(state)),
            ..Default::default()
        };
        let bytes = env.encode_to_vec();

        let acc = HllSketchAccumulator::from_sketchlib_proto_bytes(&bytes)
            .expect("envelope-wrapped decode should succeed");
        assert_eq!(acc.inner.variant, HllVariant::Regular);
        assert_eq!(acc.inner.registers, vec![1, 2, 3, 4]);
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

        let result = HllSketchAccumulator::from_sketchlib_proto_bytes(&bytes);
        assert!(result.is_err(), "wrong-sketch envelope should error");
    }

    #[test]
    fn test_from_sketchlib_proto_bytes_register_length_mismatch() {
        use asap_sketchlib::proto::sketchlib::HllVariant as ProtoVariant;
        // precision=2 → expected 4 registers; supply only 3
        let bytes = encode_state(
            ProtoVariant::Regular as i32,
            2,
            vec![1, 2, 3],
            0.0,
            0.0,
            0.0,
        );
        let result = HllSketchAccumulator::from_sketchlib_proto_bytes(&bytes);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("registers"));
    }

    #[test]
    fn test_from_sketchlib_proto_bytes_zero_precision_rejected() {
        use asap_sketchlib::proto::sketchlib::HyperLogLogState;
        use prost::Message;
        let state = HyperLogLogState::default();
        let bytes = state.encode_to_vec();
        let result = HllSketchAccumulator::from_sketchlib_proto_bytes(&bytes);
        assert!(result.is_err());
    }

    #[test]
    fn test_aggregate_core_merge_matches_register_max() {
        let a = HllSketchAccumulator {
            inner: HllSketch::from_raw(HllVariant::Regular, 2, vec![1, 5, 3, 7], 0.0, 0.0, 0.0),
            sample_p: 1.0,
        };
        let b = HllSketchAccumulator {
            inner: HllSketch::from_raw(HllVariant::Regular, 2, vec![4, 2, 6, 0], 0.0, 0.0, 0.0),
            sample_p: 1.0,
        };
        let merged_box = a.merge_with(&b).expect("merge ok");
        let merged = merged_box
            .as_any()
            .downcast_ref::<HllSketchAccumulator>()
            .expect("downcast ok");
        assert_eq!(merged.inner.registers, vec![4, 5, 6, 7]);
    }

    #[test]
    fn test_aggregate_core_merge_wrong_type_rejects() {
        use crate::summary_operators::count_sketch::CountSketchAccumulator;
        let hll = HllSketchAccumulator::new(HllVariant::Regular, 2);
        let cs = CountSketchAccumulator::new(2, 3);
        assert!(hll.merge_with(&cs).is_err());
    }

    #[test]
    fn test_from_msgpack_bytes_round_trip() {
        let original = HllSketch::from_raw(
            HllVariant::Hip,
            3,
            vec![0, 1, 2, 3, 4, 5, 6, 7],
            1.5,
            2.5,
            42.0,
        );
        let bytes = original.to_msgpack().unwrap();
        let acc = HllSketchAccumulator::from_msgpack_bytes(&bytes).expect("decode ok");
        assert_eq!(acc.inner.variant, HllVariant::Hip);
        assert_eq!(acc.inner.precision, 3);
        assert_eq!(acc.inner.registers, vec![0, 1, 2, 3, 4, 5, 6, 7]);
        assert_eq!(acc.inner.hip_kxq0, 1.5);
    }

    #[test]
    fn test_from_msgpack_bytes_rejects_garbage() {
        let result = HllSketchAccumulator::from_msgpack_bytes(b"not valid msgpack");
        assert!(result.is_err());
    }

    #[test]
    fn test_apply_proto_delta_bytes_round_trip() {
        use asap_sketchlib::proto::sketchlib::HllDelta as PbDelta;
        use prost::Message;

        let mut acc = HllSketchAccumulator::new(HllVariant::Regular, 2);
        acc.inner.registers = vec![1, 5, 3, 7];

        // Packed (index_delta, value) blob for updates {0:4, 2:6}:
        // varint(0),varint(4),varint(2),varint(6).
        let delta_bytes = PbDelta {
            packed_updates: vec![0, 4, 2, 6],
        }
        .encode_to_vec();

        acc.apply_proto_delta_bytes(&delta_bytes).expect("apply ok");
        // Max semantics: reg[0]=max(1,4)=4, reg[2]=max(3,6)=6; others unchanged.
        assert_eq!(acc.inner.registers, vec![4, 5, 6, 7]);
    }

    #[test]
    fn test_apply_proto_delta_bytes_rejects_garbage() {
        let mut acc = HllSketchAccumulator::new(HllVariant::Regular, 2);
        assert!(acc.apply_proto_delta_bytes(b"not valid proto").is_err());
    }

    // ----- sample_p cardinality rescale -----
    //
    // HLL uses hash-threshold sampling: each distinct key is admitted into
    // the sketch with probability `p`, so the register-derived cardinality
    // estimate is ~p× the true distinct count and must be rescaled by 1/p.

    #[test]
    fn test_cardinality_is_rescaled_by_sample_p() {
        use crate::Statistic;
        // Build two accumulators with identical registers but different
        // sample_p. The sampled one (p=0.25) must report ~4× the unsampled
        // estimate. Use precision 8 (256 registers) with a spread of
        // register values so the estimate is a non-trivial positive number.
        let mut registers = vec![0u8; 256];
        for (i, r) in registers.iter_mut().enumerate() {
            *r = ((i % 7) + 1) as u8;
        }
        let unsampled = HllSketchAccumulator {
            inner: HllSketch::from_raw(HllVariant::Regular, 8, registers.clone(), 0.0, 0.0, 0.0),
            sample_p: 1.0,
        };
        let sampled = HllSketchAccumulator {
            inner: HllSketch::from_raw(HllVariant::Regular, 8, registers, 0.0, 0.0, 0.0),
            sample_p: 0.25,
        };
        let raw = unsampled
            .query_statistic(Statistic::Cardinality, &None, &HashMap::new())
            .expect("cardinality ok");
        let rescaled = sampled
            .query_statistic(Statistic::Cardinality, &None, &HashMap::new())
            .expect("cardinality ok");
        assert!(raw > 0.0, "raw estimate should be positive, got {raw}");
        // Exact algebraic relationship: rescaled == raw / 0.25 == raw * 4.
        assert!(
            (rescaled - raw * 4.0).abs() < 1e-9,
            "expected rescaled ≈ 4×raw ({}), got {rescaled}",
            raw * 4.0
        );
    }

    #[test]
    fn test_count_statistic_also_rescaled_by_sample_p() {
        use crate::Statistic;
        // Count maps to the same cardinality estimate for HLL, so it must
        // rescale identically.
        let registers = vec![3u8; 16];
        let unsampled = HllSketchAccumulator {
            inner: HllSketch::from_raw(HllVariant::Regular, 4, registers.clone(), 0.0, 0.0, 0.0),
            sample_p: 1.0,
        };
        let sampled = HllSketchAccumulator {
            inner: HllSketch::from_raw(HllVariant::Regular, 4, registers, 0.0, 0.0, 0.0),
            sample_p: 0.25,
        };
        let raw = unsampled
            .query_statistic(Statistic::Count, &None, &HashMap::new())
            .expect("count ok");
        let rescaled = sampled
            .query_statistic(Statistic::Count, &None, &HashMap::new())
            .expect("count ok");
        assert!((rescaled - raw * 4.0).abs() < 1e-9);
    }

    #[test]
    fn test_sample_p_unset_behaves_as_one() {
        use asap_sketchlib::proto::sketchlib::{
            sketch_envelope, HllVariant as ProtoVariant, HyperLogLogState, SketchEnvelope,
        };
        use prost::Message;
        // An envelope with no sample_p set (proto3 default 0.0) must
        // normalize to 1.0 (no rescale) — byte-compatible with legacy frames.
        let state = HyperLogLogState {
            variant: ProtoVariant::Regular as i32,
            precision: 4,
            registers: vec![2u8; 16],
            hip_kxq0: 0.0,
            hip_kxq1: 0.0,
            hip_est: 0.0,
            registers_sparse: None,
        };
        let env = SketchEnvelope {
            // sample_p left at proto3 default 0.0.
            sketch_state: Some(sketch_envelope::SketchState::Hll(state)),
            ..Default::default()
        };
        let bytes = env.encode_to_vec();
        let acc = HllSketchAccumulator::from_sketchlib_proto_bytes(&bytes).expect("decode ok");
        assert_eq!(acc.sample_p, 1.0, "unset sample_p must normalize to 1.0");
    }

    #[test]
    fn test_from_sketchlib_proto_bytes_reads_envelope_sample_p() {
        use crate::Statistic;
        use asap_sketchlib::proto::sketchlib::{
            sketch_envelope, HllVariant as ProtoVariant, HyperLogLogState, SketchEnvelope,
        };
        use prost::Message;

        let registers = vec![3u8; 16];
        let state = HyperLogLogState {
            variant: ProtoVariant::Regular as i32,
            precision: 4,
            registers: registers.clone(),
            hip_kxq0: 0.0,
            hip_kxq1: 0.0,
            hip_est: 0.0,
            registers_sparse: None,
        };
        let env = SketchEnvelope {
            sample_p: 0.25,
            sketch_state: Some(sketch_envelope::SketchState::Hll(state)),
            ..Default::default()
        };
        let bytes = env.encode_to_vec();
        let acc = HllSketchAccumulator::from_sketchlib_proto_bytes(&bytes).expect("decode ok");
        assert_eq!(acc.sample_p, 0.25);

        // Compare against the unsampled estimate over the same registers.
        let unsampled = HllSketchAccumulator {
            inner: HllSketch::from_raw(HllVariant::Regular, 4, registers, 0.0, 0.0, 0.0),
            sample_p: 1.0,
        };
        let raw = unsampled
            .query_statistic(Statistic::Cardinality, &None, &HashMap::new())
            .expect("cardinality ok");
        let rescaled = acc
            .query_statistic(Statistic::Cardinality, &None, &HashMap::new())
            .expect("cardinality ok");
        assert!(
            (rescaled - raw * 4.0).abs() < 1e-9,
            "expected 4×raw rescale"
        );
    }

    #[test]
    fn test_reset_to_empty_preserves_sample_p() {
        let mut acc = HllSketchAccumulator {
            inner: HllSketch::from_raw(HllVariant::Regular, 4, vec![3u8; 16], 0.0, 0.0, 0.0),
            sample_p: 0.25,
        };
        acc.reset_to_empty();
        assert_eq!(acc.sample_p, 0.25, "window rotation must keep sample_p");
        assert_eq!(acc.inner.registers, vec![0u8; 16], "registers cleared");
    }

    #[test]
    fn test_merge_prefers_sampled_factor() {
        let a = HllSketchAccumulator {
            inner: HllSketch::from_raw(HllVariant::Regular, 2, vec![1, 1, 1, 1], 0.0, 0.0, 0.0),
            sample_p: 0.25,
        };
        let b = HllSketchAccumulator {
            inner: HllSketch::from_raw(HllVariant::Regular, 2, vec![1, 1, 1, 1], 0.0, 0.0, 0.0),
            sample_p: 1.0,
        };
        let merged = a.merge_with(&b).expect("merge ok");
        let merged = merged
            .as_any()
            .downcast_ref::<HllSketchAccumulator>()
            .expect("downcast ok");
        assert_eq!(merged.sample_p, 0.25);
    }
}

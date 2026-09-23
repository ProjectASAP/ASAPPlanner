//! CountSketch accumulator backed by `asap_sketchlib::CountSketch`.
//!
//! Supports worker merge, persistence serialization, and modified-OTLP proto
//! decoding. Per-key queries delegate to sketchlib's median-of-signed-rows
//! estimator so query and ingest use the same hash specification. Top-k
//! requires the separate heap-bearing accumulator.

use crate::{
    AggregateCore, AggregationType, KeyByLabelValues, MergeableAccumulator,
    MultipleSubpopulationAggregate, SerializableToSink,
};
use asap_sketchlib::{CountSketch, CountSketchDelta, MessagePackCodec};
use serde_json::Value;
use std::collections::HashMap;

use crate::Statistic;

/// Count Sketch accumulator — inner matrix of signed counts.
#[derive(Debug, Clone)]
pub struct CountSketchAccumulator {
    pub inner: CountSketch,
}

impl CountSketchAccumulator {
    pub fn new(row_num: usize, col_num: usize) -> Self {
        Self {
            inner: CountSketch::new(row_num, col_num),
        }
    }

    /// Median-of-signed-rows point estimate for `key`, via the real
    /// `asap_sketchlib::CountSketch::estimate` — the canonical, hash-spec-
    /// compatible estimator (see `AggregateCore::query_statistic`'s doc for
    /// why this replaced a hand-rolled, non-compatible hash).
    pub fn query_key(&self, key: &KeyByLabelValues) -> f64 {
        self.inner.estimate(&key.to_semicolon_str())
    }

    /// Decode from the modified OTLP wire format's
    /// `CountSketchDataPoint.sketch` bytes when
    /// `encoding = COUNT_SKETCH_ENCODING_MSGPACK`. The bytes are the
    /// MessagePack serialization of the cross-language sketch-core
    /// `CountSketch` struct — PR I parity entrypoint.
    pub fn from_msgpack_bytes(buffer: &[u8]) -> Result<Self, Box<dyn std::error::Error>> {
        Ok(Self {
            inner: CountSketch::from_msgpack(buffer)
                .map_err(|e| format!("deserialize CountSketch msgpack: {e}"))?,
        })
    }

    /// Decode from the modified OTLP wire format's
    /// `CountSketchDataPoint.sketch` bytes — the protobuf-encoded
    /// `asap_sketchlib::proto::sketchlib::CountSketchState` message
    /// that DataCollector's `countsketchprocessor` emits when
    /// `encoding = COUNT_SKETCH_ENCODING_PROTO`.
    ///
    /// Mirrors `CountMinSketchAccumulator::from_sketchlib_proto_bytes`
    /// but on the signed-counter `CountSketchState`. The resulting
    /// accumulator is constructed via
    /// `CountSketch::from_legacy_matrix` after reshaping the flat
    /// `counts_int` / `counts_float` field into a `Vec<Vec<f64>>`.
    pub fn from_sketchlib_proto_bytes(buffer: &[u8]) -> Result<Self, Box<dyn std::error::Error>> {
        use asap_sketchlib::proto::sketchlib::{
            sketch_envelope, CountSketchState, CounterType, SketchEnvelope,
        };
        use prost::Message;

        // DataCollector's countsketchprocessor wraps the state in a
        // `SketchEnvelope{count_sketch: CountSketchState}` via
        // sketchlib-go's `SerializePortableFO` + `proto.Marshal`. Try
        // decoding as envelope first, fall back to bare
        // `CountSketchState` for callers (e.g. unit tests) that
        // encode the state directly. Mirrors the PR #14 fix on
        // `CountMinSketchAccumulator::from_sketchlib_proto_bytes`.
        let state = match SketchEnvelope::decode(buffer) {
            Ok(env) => match env.sketch_state {
                Some(sketch_envelope::SketchState::CountSketch(st)) => st,
                Some(other) => {
                    return Err(format!(
                        "SketchEnvelope contains non-CountSketch sketch: {:?}",
                        std::mem::discriminant(&other)
                    )
                    .into());
                }
                None => CountSketchState::decode(buffer)
                    .map_err(|e| format!("decode CountSketchState: {e}"))?,
            },
            Err(_) => CountSketchState::decode(buffer)
                .map_err(|e| format!("decode CountSketchState: {e}"))?,
        };
        let rows = state.rows as usize;
        let cols = state.cols as usize;
        // Defensive dim validation BEFORE reconstructing the matrix:
        // reject degenerate / narrow-hash-budget-violating / absurdly
        // oversized dims so a malformed payload fails gracefully (the
        // ingest caller skips the data point) instead of building a
        // degenerate or huge matrix. Shares the CMS validator since the
        // CountSketch matrix uses the same packed-hash column layout.
        crate::accumulators::count_min_sketch_accumulator::validate_sketch_dims(
            "CountSketchState",
            rows,
            cols,
        )?;
        let expected_len = rows * cols;
        let counter_type = CounterType::try_from(state.counter_type).map_err(|_| {
            format!(
                "CountSketchState has unknown counter_type tag {}",
                state.counter_type
            )
        })?;
        let flat: Vec<f64> = match counter_type {
            CounterType::Int32 | CounterType::Int64 => {
                if state.counts_int.len() != expected_len {
                    return Err(format!(
                        "CountSketchState counts_int has {} entries, expected rows*cols = {}",
                        state.counts_int.len(),
                        expected_len
                    )
                    .into());
                }
                state.counts_int.iter().map(|&v| v as f64).collect()
            }
            CounterType::Float64 => {
                if state.counts_float.len() != expected_len {
                    return Err(format!(
                        "CountSketchState counts_float has {} entries, expected rows*cols = {}",
                        state.counts_float.len(),
                        expected_len
                    )
                    .into());
                }
                state.counts_float.clone()
            }
            other => {
                return Err(format!(
                    "CountSketchState counter_type {other:?} not yet supported \
                     (INT128 stores interleaved hi/lo pairs; will be added when needed)"
                )
                .into());
            }
        };
        let mut matrix = Vec::with_capacity(rows);
        for r in 0..rows {
            let start = r * cols;
            matrix.push(flat[start..start + cols].to_vec());
        }
        Ok(Self {
            inner: CountSketch::from_legacy_matrix(matrix, rows, cols),
        })
    }

    /// Apply a proto-encoded `CountSketchDelta` frame to this
    /// accumulator's inner sketch — the decode path for
    /// `COUNT_SKETCH_ENCODING_PROTO_DELTA` (paper §6.2 B3 / B4).
    ///
    /// Cells apply additively: `matrix[cell_rows[i]][cell_cols[i]]
    /// += d_counts[i]`. Per-row L2 is parsed off the wire but
    /// ignored at application time — it's a downstream error-
    /// accounting signal, not a merge input.
    pub fn apply_proto_delta_bytes(
        &mut self,
        buffer: &[u8],
    ) -> Result<(), Box<dyn std::error::Error>> {
        use asap_sketchlib::proto::sketchlib::CountSketchDelta as PbDelta;
        use prost::Message;

        let pb = PbDelta::decode(buffer).map_err(|e| format!("decode CountSketchDelta: {e}"))?;

        if pb.cell_rows.len() != pb.cell_cols.len() || pb.cell_rows.len() != pb.d_counts.len() {
            return Err(format!(
                "CountSketchDelta packed-array length mismatch: \
                 cell_rows={}, cell_cols={}, d_counts={}",
                pb.cell_rows.len(),
                pb.cell_cols.len(),
                pb.d_counts.len()
            )
            .into());
        }
        let cells = pb
            .cell_rows
            .iter()
            .zip(pb.cell_cols.iter())
            .zip(pb.d_counts.iter())
            .map(|((r, c), dc)| (*r, *c, *dc))
            .collect();
        // This is the heap-less matrix kernel; ranked membership is handled
        // by the explicit heap-bearing operator, not inferred from delta keys.
        let delta = CountSketchDelta {
            rows: pb.rows,
            cols: pb.cols,
            cells,
            l2: pb.l2,
            hh_keys: Vec::new(),
        };
        self.inner
            .apply_delta(&delta)
            .map_err(|e| format!("apply CountSketchDelta: {e}"))?;
        Ok(())
    }
}

impl SerializableToSink for CountSketchAccumulator {
    fn serialize_to_json(&self) -> Value {
        serde_json::json!({
            "row_num": self.inner.rows,
            "col_num": self.inner.cols,
            "sketch": self.inner.sketch(),
        })
    }

    fn serialize_to_bytes(&self) -> Vec<u8> {
        self.inner.to_msgpack().unwrap_or_default()
    }
}

impl AggregateCore for CountSketchAccumulator {
    fn clone_boxed_core(&self) -> Box<dyn AggregateCore> {
        Box::new(self.clone())
    }

    fn type_name(&self) -> &'static str {
        "CountSketchAccumulator"
    }

    /// Per-window base rotation: rebuild an empty signed-counter matrix
    /// with the same (rows, cols) so the next window's additive cell
    /// deltas align to the identical hash geometry.
    fn reset_to_empty(&mut self) {
        self.inner = CountSketch::new(self.inner.rows, self.inner.cols);
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
                "Cannot merge CountSketchAccumulator with {}",
                other.get_accumulator_type()
            )
            .into());
        }
        let other_cs = other
            .as_any()
            .downcast_ref::<CountSketchAccumulator>()
            .ok_or("Failed to downcast to CountSketchAccumulator")?;

        let merged_inner = CountSketch::merge_refs(&[&self.inner, &other_cs.inner])?;
        Ok(Box::new(Self {
            inner: merged_inner,
        }))
    }

    fn get_accumulator_type(&self) -> AggregationType {
        AggregationType::CountSketch
    }

    fn get_keys(&self) -> Option<Vec<KeyByLabelValues>> {
        None
    }

    fn query_statistic(
        &self,
        statistic: crate::Statistic,
        key: &Option<KeyByLabelValues>,
        query_kwargs: &HashMap<String, String>,
    ) -> Result<f64, Box<dyn std::error::Error + Send + Sync>> {
        use crate::Statistic;
        // Key-provided path: route to MultipleSubpopulationAggregate::query
        // (the canonical "what's the count of this key?" lookup), same
        // pattern as CountMinSketchAccumulator. Fixed from a hand-rolled
        // `DefaultHasher`-based estimator that did NOT use the sketchlib
        // hash spec (its own doc admitted this — "not the sketchlib hash
        // spec... the canonical compatibility path requires plumbing the
        // sketchlib seeds through") — `asap_sketchlib::CountSketch::estimate`
        // already hashes against the correct portable spec, so this is a
        // genuine correctness fix, not just a refactor.
        if let Some(key_val) = key.as_ref() {
            return self.query(statistic, key_val, Some(query_kwargs));
        }
        if let Some(k) = query_kwargs.get("key") {
            let key_val = KeyByLabelValues::new_with_labels(vec![k.clone()]);
            return self.query(statistic, &key_val, Some(query_kwargs));
        }
        // No-key path: unchanged from before this fix -- CountSketch's
        // signed rows have no CMS-style "min-row-sum = true total"
        // property, so these are documented approximations, not a
        // heavy-hitter answer. Not touched by this fix (only the
        // key-provided path above had the hash-compatibility bug).
        match statistic {
            Statistic::Topk | Statistic::Count => {
                let matrix = self.inner.sketch();
                let total: f64 = matrix.iter().flatten().map(|v| v.abs()).sum();
                let rows = matrix.len() as f64;
                Ok(if rows > 0.0 { total / rows } else { 0.0 })
            }
            Statistic::Sum => {
                let matrix = self.inner.sketch();
                let total: f64 = matrix.iter().flatten().sum();
                let rows = matrix.len() as f64;
                Ok(if rows > 0.0 { total / rows } else { 0.0 })
            }
            other => Err(format!(
                "CountSketchAccumulator: statistic {:?} not supported (only Topk / Count / Sum, with optional `key` in query_kwargs)",
                other,
            )
            .into()),
        }
    }
}

impl MultipleSubpopulationAggregate for CountSketchAccumulator {
    fn query(
        &self,
        _statistic: Statistic,
        key: &KeyByLabelValues,
        _query_kwargs: Option<&HashMap<String, String>>,
    ) -> Result<f64, Box<dyn std::error::Error + Send + Sync>> {
        Ok(self.query_key(key))
    }

    fn clone_boxed(&self) -> Box<dyn MultipleSubpopulationAggregate> {
        Box::new(self.clone())
    }
}

impl MergeableAccumulator<CountSketchAccumulator> for CountSketchAccumulator {
    fn merge_accumulators(
        accumulators: Vec<CountSketchAccumulator>,
    ) -> Result<CountSketchAccumulator, Box<dyn std::error::Error + Send + Sync>> {
        if accumulators.is_empty() {
            return Err("No accumulators to merge".into());
        }
        let mut iter = accumulators.into_iter();
        let mut merged = iter.next().unwrap();
        for acc in iter {
            merged.inner.merge(&acc.inner)?;
        }
        Ok(merged)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_query_key_uses_real_sketchlib_estimator() {
        // `query_key` must match sketchlib's estimator and hash specification.
        let mut cs = CountSketchAccumulator::new(4, 1000);
        let key = KeyByLabelValues::new_with_labels(vec!["web".to_string()]);
        cs.inner.update(&key.to_semicolon_str(), 10.0);
        assert_eq!(
            cs.query_key(&key),
            cs.inner.estimate(&key.to_semicolon_str())
        );
    }

    #[test]
    fn test_multiple_subpopulation_aggregate_query() {
        let mut cs = CountSketchAccumulator::new(4, 1000);
        let key = KeyByLabelValues::new_with_labels(vec!["checkout".to_string()]);
        cs.inner.update(&key.to_semicolon_str(), 25.0);

        let multi_trait: &dyn MultipleSubpopulationAggregate = &cs;
        let result = multi_trait.query(Statistic::Sum, &key, None).unwrap();
        assert_eq!(result, cs.query_key(&key));

        // query_statistic (the AggregateCore entry point) must route a
        // provided key through the same path.
        let core: &dyn AggregateCore = &cs;
        let via_core = core
            .query_statistic(Statistic::Sum, &Some(key.clone()), &HashMap::new())
            .unwrap();
        assert_eq!(via_core, cs.query_key(&key));
    }

    #[test]
    fn test_mergeable_accumulator_merge_accumulators() {
        let cs1 = CountSketchAccumulator {
            inner: CountSketch::from_legacy_matrix(vec![vec![1.0, -2.0], vec![3.0, -4.0]], 2, 2),
        };
        let cs2 = CountSketchAccumulator {
            inner: CountSketch::from_legacy_matrix(vec![vec![-1.0, 2.0], vec![-3.0, 4.0]], 2, 2),
        };
        let merged = CountSketchAccumulator::merge_accumulators(vec![cs1, cs2]).unwrap();
        assert_eq!(merged.inner.sketch(), &vec![vec![0.0, 0.0], vec![0.0, 0.0]]);
    }

    #[test]
    fn test_mergeable_accumulator_rejects_empty() {
        let result = CountSketchAccumulator::merge_accumulators(vec![]);
        assert!(result.is_err());
    }

    fn encode_state(
        rows: u32,
        cols: u32,
        counter_type: i32,
        counts_int: Vec<i64>,
        counts_float: Vec<f64>,
    ) -> Vec<u8> {
        use asap_sketchlib::proto::sketchlib::CountSketchState;
        use prost::Message;
        let state = CountSketchState {
            rows,
            cols,
            counter_type,
            counts_int,
            counts_float,
            l2: Vec::new(),
            topk: None,
        };
        state.encode_to_vec()
    }

    #[test]
    fn test_from_sketchlib_proto_bytes_int64() {
        use asap_sketchlib::proto::sketchlib::CounterType;
        // Signed 2x3 matrix: row 0 = [1,-2,3], row 1 = [-4,5,-6]
        let bytes = encode_state(
            2,
            3,
            CounterType::Int64 as i32,
            vec![1, -2, 3, -4, 5, -6],
            Vec::new(),
        );
        let acc = CountSketchAccumulator::from_sketchlib_proto_bytes(&bytes).expect("decode ok");
        let matrix = acc.inner.sketch();
        assert_eq!(matrix[0], vec![1.0, -2.0, 3.0]);
        assert_eq!(matrix[1], vec![-4.0, 5.0, -6.0]);
    }

    #[test]
    fn test_from_sketchlib_proto_bytes_envelope_wrapped() {
        // Mirrors what DataCollector's countsketchprocessor emits:
        // the state wrapped in a `SketchEnvelope{count_sketch: ...}`
        // via sketchlib-go's `SerializePortableFO` + `proto.Marshal`.
        use asap_sketchlib::proto::sketchlib::{
            sketch_envelope, CountSketchState, CounterType, SketchEnvelope,
        };
        use prost::Message;

        let state = CountSketchState {
            rows: 2,
            cols: 3,
            counter_type: CounterType::Int64 as i32,
            counts_int: vec![1, -2, 3, -4, 5, -6],
            counts_float: Vec::new(),
            ..Default::default()
        };
        let env = SketchEnvelope {
            sketch_state: Some(sketch_envelope::SketchState::CountSketch(state)),
            ..Default::default()
        };
        let bytes = env.encode_to_vec();

        let acc = CountSketchAccumulator::from_sketchlib_proto_bytes(&bytes)
            .expect("envelope-wrapped decode should succeed");
        let matrix = acc.inner.sketch();
        assert_eq!(matrix[0], vec![1.0, -2.0, 3.0]);
        assert_eq!(matrix[1], vec![-4.0, 5.0, -6.0]);
    }

    #[test]
    fn test_from_sketchlib_proto_bytes_envelope_wrong_sketch_type() {
        // An envelope carrying a non-CountSketch sketch should be
        // rejected with a clear error rather than silently producing
        // garbage.
        use asap_sketchlib::proto::sketchlib::{sketch_envelope, KllState, SketchEnvelope};
        use prost::Message;

        let env = SketchEnvelope {
            sketch_state: Some(sketch_envelope::SketchState::Kll(KllState::default())),
            ..Default::default()
        };
        let bytes = env.encode_to_vec();

        let result = CountSketchAccumulator::from_sketchlib_proto_bytes(&bytes);
        assert!(result.is_err(), "wrong-sketch envelope should error");
    }

    #[test]
    fn test_from_sketchlib_proto_bytes_float64() {
        use asap_sketchlib::proto::sketchlib::CounterType;
        let bytes = encode_state(
            2,
            2,
            CounterType::Float64 as i32,
            Vec::new(),
            vec![1.5, -2.5, 3.5, -4.5],
        );
        let acc = CountSketchAccumulator::from_sketchlib_proto_bytes(&bytes).expect("decode ok");
        let matrix = acc.inner.sketch();
        assert_eq!(matrix[0], vec![1.5, -2.5]);
        assert_eq!(matrix[1], vec![3.5, -4.5]);
    }

    #[test]
    fn test_from_sketchlib_proto_bytes_dimension_mismatch() {
        use asap_sketchlib::proto::sketchlib::CounterType;
        // 2x3 declared but only 5 int entries
        let bytes = encode_state(
            2,
            3,
            CounterType::Int64 as i32,
            vec![1, 2, 3, 4, 5],
            Vec::new(),
        );
        let result = CountSketchAccumulator::from_sketchlib_proto_bytes(&bytes);
        assert!(result.is_err());
        assert!(
            result.unwrap_err().to_string().contains("counts_int"),
            "error should mention counts_int dim mismatch"
        );
    }

    #[test]
    fn test_from_sketchlib_proto_bytes_zero_dims_rejected() {
        use asap_sketchlib::proto::sketchlib::CountSketchState;
        use prost::Message;
        let state = CountSketchState::default();
        let bytes = state.encode_to_vec();
        let result = CountSketchAccumulator::from_sketchlib_proto_bytes(&bytes);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("degenerate dims"));
    }

    #[test]
    fn test_aggregate_core_merge_matches_matrix_add() {
        let a = CountSketchAccumulator {
            inner: CountSketch::from_legacy_matrix(vec![vec![1.0, -2.0], vec![3.0, -4.0]], 2, 2),
        };
        let b = CountSketchAccumulator {
            inner: CountSketch::from_legacy_matrix(vec![vec![-1.0, 2.0], vec![-3.0, 4.0]], 2, 2),
        };
        let merged_box = a.merge_with(&b).expect("merge ok");
        let merged = merged_box
            .as_any()
            .downcast_ref::<CountSketchAccumulator>()
            .expect("downcast ok");
        let m = merged.inner.sketch();
        assert_eq!(m[0], vec![0.0, 0.0]);
        assert_eq!(m[1], vec![0.0, 0.0]);
    }

    #[test]
    fn test_aggregate_core_merge_wrong_type_rejects() {
        use crate::accumulators::count_min_sketch_accumulator::CountMinSketchAccumulator;
        let cs = CountSketchAccumulator::new(2, 3);
        let cms = CountMinSketchAccumulator::new(2, 3);
        let result = cs.merge_with(&cms);
        assert!(result.is_err());
    }

    #[test]
    fn test_from_msgpack_bytes_round_trip() {
        let original = CountSketch::from_legacy_matrix(
            vec![vec![1.0, -2.0, 3.0], vec![-4.0, 5.0, -6.0]],
            2,
            3,
        );
        let bytes = original.to_msgpack().unwrap();
        let acc = CountSketchAccumulator::from_msgpack_bytes(&bytes).expect("decode ok");
        assert_eq!(acc.inner.rows, 2);
        assert_eq!(acc.inner.cols, 3);
        assert_eq!(acc.inner.sketch(), original.sketch());
    }

    #[test]
    fn test_from_msgpack_bytes_rejects_garbage() {
        let result = CountSketchAccumulator::from_msgpack_bytes(b"not valid msgpack");
        assert!(result.is_err());
    }

    #[test]
    fn test_apply_proto_delta_bytes_round_trip() {
        use asap_sketchlib::proto::sketchlib::CountSketchDelta as PbDelta;
        use prost::Message;

        let mut acc = CountSketchAccumulator {
            inner: CountSketch::from_legacy_matrix(
                vec![vec![1.0, 2.0, 3.0], vec![4.0, 5.0, 6.0]],
                2,
                3,
            ),
        };
        let bytes = PbDelta {
            rows: 2,
            cols: 3,
            cell_rows: vec![0, 1],
            cell_cols: vec![0, 2],
            d_counts: vec![10, -6],
            l2: vec![],
            ..Default::default()
        }
        .encode_to_vec();

        acc.apply_proto_delta_bytes(&bytes).expect("apply ok");
        assert_eq!(
            acc.inner.sketch(),
            &vec![vec![11.0, 2.0, 3.0], vec![4.0, 5.0, 0.0]]
        );
    }

    #[test]
    fn test_apply_proto_delta_bytes_rejects_garbage() {
        let mut acc = CountSketchAccumulator::new(2, 3);
        assert!(acc.apply_proto_delta_bytes(b"not valid proto").is_err());
    }

    // ----------------------------------------------------------------
    // Defensive inbound-dimension validation (harden/sketch-dim-validation).
    // Malformed / narrow-hash-budget-violating CountSketch dims must be
    // rejected gracefully (Err, never a panic); valid configs the backend
    // actually uses (5x2048, 5x4096, 5x2000) must still decode.
    // ----------------------------------------------------------------

    #[test]
    fn test_from_sketchlib_proto_bytes_rejects_bad_dims_no_panic() {
        use asap_sketchlib::proto::sketchlib::CounterType;
        // 5 * ceil(log2(8192))=5*13=65 > 64 — narrow-hash-budget violation.
        // counts sized to rows*cols so rejection is on dims, not length.
        let n = 5usize * 8192usize;
        let bytes = encode_state(
            5,
            8192,
            CounterType::Int64 as i32,
            vec![0i64; n],
            Vec::new(),
        );
        let result = CountSketchAccumulator::from_sketchlib_proto_bytes(&bytes);
        assert!(result.is_err(), "budget-violating dims should be rejected");
        assert!(result.unwrap_err().to_string().contains("rejecting"));

        // A valid neighbour (5x4096) on the same path still decodes fine.
        let n_ok = 5usize * 4096usize;
        let ok_bytes = encode_state(
            5,
            4096,
            CounterType::Int64 as i32,
            vec![0i64; n_ok],
            Vec::new(),
        );
        let acc = CountSketchAccumulator::from_sketchlib_proto_bytes(&ok_bytes)
            .expect("valid 5x4096 CountSketch should still decode");
        assert_eq!(acc.inner.rows, 5);
        assert_eq!(acc.inner.cols, 4096);
    }

    #[test]
    fn test_from_sketchlib_proto_bytes_rejects_oversized_dims() {
        use asap_sketchlib::proto::sketchlib::CounterType;
        // Declare 1 x 16,777,216 = 16M cells (> 8M cap) but send an empty
        // counts vector: validation must reject on the dim cap BEFORE the
        // decoder tries to allocate/reshape a 16M-entry matrix. (1 row keeps
        // the hash budget tiny so the cap check, not the budget check, fires.)
        let bytes = encode_state(
            1,
            16_777_216,
            CounterType::Int64 as i32,
            Vec::new(),
            Vec::new(),
        );
        let result = CountSketchAccumulator::from_sketchlib_proto_bytes(&bytes);
        assert!(result.is_err(), "oversized dims should be rejected");
        let msg = result.unwrap_err().to_string();
        assert!(msg.contains("cap"), "expected cell-cap error, got: {msg}");
    }
}

use crate::summary_operators::dd_sketch_accumulator::normalize_sample_p;
use crate::{
    AggregateCore, AggregationType, KeyByLabelValues, MergeableAccumulator,
    MultipleSubpopulationAggregate, SerializableToSink,
};
use asap_sketchlib::{CountMinSketch, CountMinSketchDelta, MessagePackCodec};
use serde_json::Value;
use std::collections::HashMap;

use crate::Statistic;

/// Count-Min Sketch accumulator — wraps asap_sketchlib::CountMinSketch.
/// Core struct, update/merge/serde logic live in `asap_sketchlib::sketches`.
/// This file retains QE-specific trait impls, legacy deserializers, and JSON output.
#[derive(Debug, Clone)]
pub struct CountMinSketchAccumulator {
    pub inner: CountMinSketch,
    /// Edge sampling probability `p ∈ (0,1]` carried on the producer's
    /// `SketchEnvelope.sample_p`. The edge admits each insert with
    /// probability `p`, so every stored cell count is ~`p`× the true count.
    /// CMS is L1/additive and linear, so the unbiased rescale of BOTH a
    /// point-frequency estimate (`query_key`) and the aggregate
    /// total-event statistics (`Count`/`Sum`/`Increase`/`Rate`) is `×1/p`.
    /// `1.0` (and the proto3 default `0.0`, dual-read as `1.0`) means no
    /// sampling, so the rescale is a no-op and the behaviour is identical
    /// to before. Mirrors `DDSketchAccumulator::sample_p`; set from the
    /// envelope at the `from_sketchlib_proto_bytes` decode site and
    /// preserved across `reset_to_empty` and `merge_with`.
    pub sample_p: f64,
}

impl CountMinSketchAccumulator {
    pub fn new(row_num: usize, col_num: usize) -> Self {
        Self {
            inner: CountMinSketch::new(row_num, col_num),
            sample_p: 1.0,
        }
    }

    // Marked as _update and kept private; only called internally.
    fn _update(&mut self, key: &KeyByLabelValues, value: f64) {
        self.inner.update(&key.to_semicolon_str(), value);
    }

    pub fn query_key(&self, key: &KeyByLabelValues) -> f64 {
        // The edge sampled inserts with probability `sample_p`, so the
        // stored point-frequency estimate is ~`p`× the true frequency.
        // CMS is linear/additive, so `×1/p` is the unbiased rescale.
        // `sample_p == 1.0` (unsampled / legacy) makes this a no-op.
        self.inner.estimate(&key.to_semicolon_str()) / self.sample_p
    }

    pub fn deserialize_from_json(data: &Value) -> Result<Self, Box<dyn std::error::Error>> {
        let row_num = data["row_num"]
            .as_f64()
            .ok_or("Missing or invalid 'row_num' field")? as usize;
        let col_num = data["col_num"]
            .as_f64()
            .ok_or("Missing or invalid 'col_num' field")? as usize;

        let sketch_data = data["sketch"]
            .as_array()
            .ok_or("Missing or invalid 'sketch' field")?;

        let mut sketch = Vec::new();
        for row in sketch_data {
            let row_array = row.as_array().ok_or("Invalid row in sketch data")?;
            let mut sketch_row = Vec::new();
            for cell in row_array {
                let value = cell.as_f64().ok_or("Invalid cell value in sketch data")?;
                sketch_row.push(value);
            }
            sketch.push(sketch_row);
        }

        Ok(Self {
            inner: CountMinSketch::from_legacy_matrix(sketch, row_num, col_num),
            sample_p: 1.0,
        })
    }

    /// Decode from the modified OTLP wire format's
    /// `CountMinSketchDataPoint.sketch` bytes when
    /// `encoding = COUNT_MIN_SKETCH_ENCODING_MSGPACK`. The bytes are the
    /// MessagePack serialization of the cross-language sketch-core
    /// `CountMinSketch` wire struct (same format the legacy Arroyo path
    /// uses — this method is the modified-OTLP entrypoint for PR I).
    pub fn from_msgpack_bytes(buffer: &[u8]) -> Result<Self, Box<dyn std::error::Error>> {
        Ok(Self {
            inner: CountMinSketch::from_msgpack(buffer)
                .map_err(|e| -> Box<dyn std::error::Error> { e.to_string().into() })?,
            // The msgpack CountMinSketch struct carries no envelope/sample_p;
            // the msgpack path is parity/test-only and is never edge-sampled.
            sample_p: 1.0,
        })
    }

    /// Decode from the modified OTLP wire format's
    /// `CountMinSketchDataPoint.sketch` bytes — i.e. the protobuf-encoded
    /// `asap_sketchlib::proto::sketchlib::CountMinState` message used by
    /// DataCollector's `countminsketchprocessor` when emitting via
    /// `Metric.data = CountMinSketch{…}` with
    /// `encoding = COUNT_MIN_SKETCH_ENCODING_PROTO`.
    ///
    /// The resulting accumulator is constructed via
    /// `CountMinSketch::from_legacy_matrix` after reshaping the flat
    /// `counts_int` / `counts_float` field into a `Vec<Vec<f64>>`.
    pub fn from_sketchlib_proto_bytes(buffer: &[u8]) -> Result<Self, Box<dyn std::error::Error>> {
        use asap_sketchlib::proto::sketchlib::{
            sketch_envelope, CountMinState, CounterType, SketchEnvelope,
        };
        use prost::Message;

        // DataCollector's countminsketchprocessor wraps the state in a
        // `SketchEnvelope{count_min: CountMinState}` via
        // `SerializePortableFO` + `proto.Marshal`. Try decoding as envelope
        // first, fall back to bare `CountMinState` for callers (e.g. unit
        // tests) that encode the state directly. Capture the envelope's
        // `sample_p` alongside the state so the point-frequency
        // (`query_key`) and aggregate statistics rescale by `1/p`. Bare
        // `CountMinState` bytes (no envelope) carry no sampling info →
        // `sample_p` 1.0 (no rescale). Mirrors `DDSketchAccumulator`.
        let (state, sample_p) = match SketchEnvelope::decode(buffer) {
            Ok(env) => {
                let sp = env.sample_p;
                match env.sketch_state {
                    Some(sketch_envelope::SketchState::CountMin(st)) => (st, sp),
                    Some(other) => {
                        return Err(format!(
                            "SketchEnvelope contains non-CountMin sketch: {:?}",
                            std::mem::discriminant(&other)
                        )
                        .into());
                    }
                    // Envelope decoded but was empty (e.g. the buffer is a
                    // bare CountMinState that happened to parse as a default
                    // envelope). Fall through to bare decode.
                    None => (
                        CountMinState::decode(buffer)
                            .map_err(|e| format!("decode CountMinState: {e}"))?,
                        1.0,
                    ),
                }
            }
            Err(_) => (
                CountMinState::decode(buffer).map_err(|e| format!("decode CountMinState: {e}"))?,
                1.0,
            ),
        };
        let rows = state.rows as usize;
        let cols = state.cols as usize;
        // Defensive dim validation BEFORE reconstructing the matrix:
        // reject degenerate / narrow-hash-budget-violating / absurdly
        // oversized dims so a malformed payload fails gracefully (the
        // ingest caller skips the data point) instead of building a
        // degenerate or huge matrix.
        validate_sketch_dims("CountMinState", rows, cols)?;
        let expected_len = rows * cols;
        let counter_type = CounterType::try_from(state.counter_type).map_err(|_| {
            format!(
                "CountMinState has unknown counter_type tag {}",
                state.counter_type
            )
        })?;
        let flat: Vec<f64> = match counter_type {
            CounterType::Int32 | CounterType::Int64 => {
                if state.counts_int.len() != expected_len {
                    return Err(format!(
                        "CountMinState counts_int has {} entries, expected rows*cols = {}",
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
                        "CountMinState counts_float has {} entries, expected rows*cols = {}",
                        state.counts_float.len(),
                        expected_len
                    )
                    .into());
                }
                state.counts_float.clone()
            }
            // INT128 stores (hi, lo) pairs and would have 2 * rows * cols
            // entries in counts_int; defer to PR C if a producer ever uses it.
            other => {
                return Err(format!(
                    "CountMinState counter_type {other:?} not yet supported \
                     (PR C will extend coverage)"
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
            inner: CountMinSketch::from_legacy_matrix(matrix, rows, cols),
            sample_p: normalize_sample_p(sample_p),
        })
    }

    /// Apply a proto-encoded `CountMinDelta` frame to this
    /// accumulator's inner sketch — the decode path for
    /// `COUNT_MIN_SKETCH_ENCODING_PROTO_DELTA` (paper §6.2 B3 / B4).
    pub fn apply_proto_delta_bytes(
        &mut self,
        buffer: &[u8],
    ) -> Result<(), Box<dyn std::error::Error>> {
        use asap_sketchlib::proto::sketchlib::CountMinDelta as PbDelta;
        use prost::Message;

        let pb = PbDelta::decode(buffer).map_err(|e| format!("decode CountMinDelta: {e}"))?;

        if pb.cell_rows.len() != pb.cell_cols.len() || pb.cell_rows.len() != pb.d_counts.len() {
            return Err(format!(
                "CountMinDelta packed-array length mismatch: \
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
        let delta = CountMinSketchDelta {
            rows: pb.rows,
            cols: pb.cols,
            cells,
            l1: pb.l1,
            l2: pb.l2,
            // The Go-side CountMinDelta proto now carries an hh_keys field
            // (heavy-hitter candidates), mirrored on asap_sketchlib's
            // CountMinSketchDelta. The vendored Rust proto bindings here don't
            // decode it yet, and CountMin has no TopK to rebuild, so pass an
            // empty set — same handling as CountSketch's hh_keys.
            hh_keys: Vec::new(),
        };
        self.inner
            .apply_delta(&delta)
            .map_err(|e| format!("apply CountMinDelta: {e}"))?;
        Ok(())
    }

    pub fn deserialize_from_bytes(buffer: &[u8]) -> Result<Self, Box<dyn std::error::Error>> {
        if buffer.len() < 8 {
            return Err("Buffer too short for row_num and col_num".into());
        }

        // TODO: this logic will need to be checked for i32 -> f64
        // Github Issue #11

        let row_num = u32::from_le_bytes([buffer[0], buffer[1], buffer[2], buffer[3]]) as usize;
        let col_num = u32::from_le_bytes([buffer[4], buffer[5], buffer[6], buffer[7]]) as usize;

        let expected_size = 8 + (row_num * col_num * 4);
        if buffer.len() < expected_size {
            return Err("Buffer too short for sketch data".into());
        }

        let mut sketch = Vec::new();
        let mut offset = 8;

        for _ in 0..row_num {
            let mut row = Vec::new();
            for _ in 0..col_num {
                let value = f64::from_le_bytes([
                    buffer[offset],
                    buffer[offset + 1],
                    buffer[offset + 2],
                    buffer[offset + 3],
                    buffer[offset + 4],
                    buffer[offset + 5],
                    buffer[offset + 6],
                    buffer[offset + 7],
                ]);
                row.push(value);
                offset += 8;
            }
            sketch.push(row);
        }

        Ok(Self {
            inner: CountMinSketch::from_legacy_matrix(sketch, row_num, col_num),
            sample_p: 1.0,
        })
    }

    /// Merge multiple accumulators efficiently without cloning all of them.
    pub fn merge_multiple(
        accumulators: &[Box<dyn crate::AggregateCore>],
    ) -> Result<Self, Box<dyn std::error::Error + Send + Sync>> {
        if accumulators.is_empty() {
            return Err("No accumulators to merge".into());
        }

        let mut cms_accumulators = Vec::with_capacity(accumulators.len());
        for acc in accumulators {
            if acc.get_accumulator_type() != AggregationType::CountMinSketch {
                return Err(format!(
                    "Cannot merge CountMinSketchAccumulator with {:?}",
                    acc.get_accumulator_type()
                )
                .into());
            }
            let cms_acc = acc
                .as_any()
                .downcast_ref::<CountMinSketchAccumulator>()
                .ok_or("Failed to downcast to CountMinSketchAccumulator")?;
            cms_accumulators.push(cms_acc);
        }

        // Check dimensions are consistent
        let rows = cms_accumulators[0].inner.rows();
        let cols = cms_accumulators[0].inner.cols();
        for acc in &cms_accumulators {
            if acc.inner.rows() != rows || acc.inner.cols() != cols {
                return Err(
                    "Cannot merge CountMinSketch accumulators with different dimensions".into(),
                );
            }
        }

        let inner_refs: Vec<&CountMinSketch> =
            cms_accumulators.iter().map(|acc| &acc.inner).collect();
        let merged_inner = CountMinSketch::merge_refs(&inner_refs)?;
        // sample_p is a per-series config constant, so all operands carry the
        // same value in practice. Mirror DDSketch's merge policy: prefer a
        // sampled factor (< 1.0) over the no-sampling default so a merge with
        // a freshly-reset (1.0) base keeps the series' sampling rate.
        let sample_p = cms_accumulators
            .iter()
            .map(|acc| acc.sample_p)
            .find(|&p| p < 1.0)
            .unwrap_or(cms_accumulators[0].sample_p);
        Ok(Self {
            inner: merged_inner,
            sample_p,
        })
    }
}

/// Defensive upper bound on the number of matrix cells (`rows * cols`)
/// we'll reconstruct from an inbound wire-declared CMS / CountSketch
/// dimension pair. A malformed / hostile payload could declare absurd
/// dims (e.g. `rows = cols = u32::MAX`) and trick the decoder into a
/// huge `Vec` allocation before the `counts_*.len() != rows*cols`
/// check ever runs. Realistic sketches are at most a few hundred rows
/// by tens-of-thousands of columns, so 8M cells (~64 MiB of f64) is a
/// generous ceiling that no legitimate producer reaches.
pub(crate) const MAX_SKETCH_CELLS: usize = 8 * 1024 * 1024;

/// Validate an inbound, wire-declared `(rows, cols)` pair for a
/// matrix-backed frequency sketch (CMS / CountSketch) BEFORE any matrix
/// is reconstructed from it. Returns `Ok(())` for dimensions a
/// legitimate producer could have emitted, and an `Err` (never a panic)
/// for malformed / degenerate ones so the ingest path can skip the data
/// point and fall through to its existing decode-failure accounting.
///
/// Rejections:
/// 1. `rows < 1` or `cols < 1` — a zero-dim matrix has no cells.
/// 2. Narrow-hash-budget violation. The cross-language wire hasher
///    (`sketchlib`'s `MatrixHashType::Packed64`) derives every row's
///    column index from disjoint bit-fields of a single 64-bit hash
///    word: row `r` reads `mask_bits = ceil(log2(cols))` bits at offset
///    `r * mask_bits`. Once `rows * mask_bits > 64` the per-row column
///    slices overflow / alias the 64-bit word and the matrix-cell
///    layout is no longer the one the producer hashed into — the sketch
///    is internally degenerate. This mirrors sketchlib's own
///    `MatrixFastHash::assert_compatible` budget (`rows * (mask_bits + 1) <= 64`); we check the column-index bits alone so realistic
///    configs (5x2048, 5x4096, 5x2000) — for which the sign bits share
///    the top of the word without affecting the cell layout — still
///    pass.
/// 3. Obviously-oversized dims: `rows * cols > MAX_SKETCH_CELLS`,
///    guarding against a huge allocation from a malformed payload.
///
/// `what` names the wire struct for the error message (e.g.
/// `"CountMinState"`).
pub(crate) fn validate_sketch_dims(what: &str, rows: usize, cols: usize) -> Result<(), String> {
    if rows < 1 || cols < 1 {
        return Err(format!(
            "{what} has degenerate dims (rows={rows}, cols={cols}); rejecting"
        ));
    }
    // mask_bits = ceil(log2(cols)); cols >= 1 here. ilog2 is floor(log2).
    let mask_bits = if cols.is_power_of_two() {
        cols.ilog2() as usize
    } else {
        cols.ilog2() as usize + 1
    };
    if rows.saturating_mul(mask_bits) > 64 {
        return Err(format!(
            "{what} dims (rows={rows}, cols={cols}) exceed the 64-bit \
             packed-hash column budget (rows * ceil(log2(cols)) = {} > 64); \
             the sketch's matrix-cell layout is degenerate, rejecting",
            rows.saturating_mul(mask_bits)
        ));
    }
    if rows.saturating_mul(cols) > MAX_SKETCH_CELLS {
        return Err(format!(
            "{what} dims (rows={rows}, cols={cols}) declare {} cells, \
             exceeding the {MAX_SKETCH_CELLS}-cell ingest cap; rejecting to \
             avoid a huge allocation from a malformed payload",
            rows.saturating_mul(cols)
        ));
    }
    Ok(())
}

impl SerializableToSink for CountMinSketchAccumulator {
    fn serialize_to_json(&self) -> Value {
        serde_json::json!({
            "row_num": self.inner.rows(),
            "col_num": self.inner.cols(),
            "sketch": self.inner.sketch()
        })
    }

    fn serialize_to_bytes(&self) -> Vec<u8> {
        self.inner.to_msgpack().unwrap_or_default()
    }
}

impl AggregateCore for CountMinSketchAccumulator {
    fn clone_boxed_core(&self) -> Box<dyn AggregateCore> {
        Box::new(self.clone())
    }

    fn type_name(&self) -> &'static str {
        "CountMinSketchAccumulator"
    }

    /// Per-window base rotation: rebuild an empty counter matrix with
    /// the same (rows, cols) so the next window's additive cell deltas
    /// align to the identical hash geometry. `sample_p` is a per-series
    /// config constant (not per-window data), so it is intentionally
    /// preserved across the rotation — mirrors `DDSketchAccumulator`.
    fn reset_to_empty(&mut self) {
        self.inner = CountMinSketch::new(self.inner.rows(), self.inner.cols());
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
                "Cannot merge CountMinSketchAccumulator with {}",
                other.get_accumulator_type()
            )
            .into());
        }

        let other_cms = other
            .as_any()
            .downcast_ref::<CountMinSketchAccumulator>()
            .ok_or("Failed to downcast to CountMinSketchAccumulator")?;

        let merged_inner = CountMinSketch::merge_refs(&[&self.inner, &other_cms.inner])?;
        // Mirror DDSketchAccumulator's merge policy exactly: sample_p is a
        // per-series config constant, so both operands carry the same value
        // in practice. Prefer a sampled factor over the no-sampling default
        // so a merge with a freshly-reset (1.0) base keeps the series'
        // sampling rate.
        let sample_p = if self.sample_p < 1.0 {
            self.sample_p
        } else {
            other_cms.sample_p
        };
        Ok(Box::new(Self {
            inner: merged_inner,
            sample_p,
        }))
    }

    fn get_accumulator_type(&self) -> AggregationType {
        AggregationType::CountMinSketch
    }

    fn approx_memory_bytes(&self) -> usize {
        // Conservative constant for the CountMinSketch counter matrix.
        // Real per-instance sizing would require exposing rows/cols on
        // the inner sketch; 16 KiB is a reasonable v1 default.
        16 * 1024
    }

    fn get_keys(&self) -> Option<Vec<crate::KeyByLabelValues>> {
        None
    }

    fn query_statistic(
        &self,
        statistic: crate::Statistic,
        key: &Option<crate::KeyByLabelValues>,
        query_kwargs: &std::collections::HashMap<String, String>,
    ) -> Result<f64, Box<dyn std::error::Error + Send + Sync>> {
        use crate::MultipleSubpopulationAggregate;
        use crate::Statistic;

        // Key-provided path: route to MultipleSubpopulationAggregate::query
        // (the canonical "what's the count of this key?" lookup).
        if let Some(key_val) = key.as_ref() {
            return self.query(statistic, key_val, Some(query_kwargs));
        }
        if let Some(k) = query_kwargs.get("key") {
            let key_val = crate::KeyByLabelValues::new_with_labels(vec![k.clone()]);
            return self.query(statistic, &key_val, Some(query_kwargs));
        }

        // No-key path: return total event volume. The min-row-sum is the
        // canonical CMS estimator for "how many inserts were observed" —
        // each insert increments exactly one cell per row, so every row
        // sums to the true insert count (modulo collisions, which CMS
        // never *underestimates*; min is the tightest upper bound).
        //
        // When the edge sampled this series (sample_p < 1.0), each insert
        // was admitted w.p. `p`, so the stored min-row-sum is ~`p`× the
        // true event count. CMS is L1/additive and linear, so rescale by
        // `1/sample_p` for an unbiased estimate. `sample_p == 1.0`
        // (unsampled / legacy) makes this a no-op. This rescales BOTH the
        // Count/Sum/Increase statistics and (via the same closure) the
        // Rate per-second readout.
        let total_events = || -> f64 {
            let matrix = self.inner.sketch();
            if matrix.is_empty() || matrix[0].is_empty() {
                return 0.0;
            }
            let row_totals = matrix.iter().map(|r| r.iter().sum::<f64>());
            let min_total = row_totals.fold(f64::INFINITY, f64::min);
            if min_total.is_finite() {
                min_total / self.sample_p
            } else {
                0.0
            }
        };
        match statistic {
            Statistic::Count | Statistic::Sum => Ok(total_events()),
            // PR #111 honest-gap closure (in-the-bag for ASAP tier).
            // CMS records insert counts but not timestamps, so per-second
            // `rate(metric[range])` requires the engine to push the
            // range duration via `query_kwargs["range_ms"]`. When
            // present, divide the min-row-sum by `range_ms / 1000`. When
            // absent (the engine has not been wired to inject range_ms
            // for this query, e.g. instant `rate` calls outside the
            // PromQL range-vector pattern), fall back to the raw event
            // count so the answer is at least non-empty — the caller's
            // caveat is that the units are events/window rather than
            // events/second. Increase carries the same caveat.
            Statistic::Rate => {
                let total = total_events();
                let range_ms_str = query_kwargs.get("range_ms").map(String::as_str);
                let Some(s) = range_ms_str else {
                    return Ok(total);
                };
                let range_ms: f64 = s
                    .parse()
                    .map_err(|e| format!("CountMinSketchAccumulator: bad range_ms='{s}': {e}"))?;
                if range_ms <= 0.0 {
                    return Err("CountMinSketchAccumulator: range_ms must be positive".into());
                }
                Ok(total * 1000.0 / range_ms)
            }
            Statistic::Increase => Ok(total_events()),
            other => Err(format!(
                "CountMinSketchAccumulator: statistic {:?} not supported \
                 without a key (only Count / Sum / Rate / Increase aggregate \
                 over the whole sketch)",
                other,
            )
            .into()),
        }
    }
}

impl MultipleSubpopulationAggregate for CountMinSketchAccumulator {
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

impl MergeableAccumulator<CountMinSketchAccumulator> for CountMinSketchAccumulator {
    fn merge_accumulators(
        accumulators: Vec<CountMinSketchAccumulator>,
    ) -> Result<CountMinSketchAccumulator, Box<dyn std::error::Error + Send + Sync>> {
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
    fn test_count_min_sketch_creation() {
        let cms = CountMinSketchAccumulator::new(4, 1000);
        assert_eq!(cms.inner.rows(), 4);
        assert_eq!(cms.inner.cols(), 1000);
        let sketch = cms.inner.sketch();
        assert_eq!(sketch.len(), 4);
        assert_eq!(sketch[0].len(), 1000);

        for row in &sketch {
            for &value in row {
                assert_eq!(value, 0.0);
            }
        }
    }

    #[test]
    fn test_count_min_sketch_update() {
        let mut cms = CountMinSketchAccumulator::new(2, 10);
        let key = KeyByLabelValues::new();
        cms._update(&key, 1.0);
        let result = cms.query_key(&key);
        assert!(result >= 1.0);
    }

    #[test]
    fn test_count_min_sketch_query() {
        let cms = CountMinSketchAccumulator::new(2, 10);
        let key = KeyByLabelValues::new();
        assert_eq!(cms.query_key(&key), 0.0);

        let multi_trait: &dyn MultipleSubpopulationAggregate = &cms;
        assert_eq!(multi_trait.query(Statistic::Sum, &key, None).unwrap(), 0.0);
    }

    #[test]
    fn test_count_min_sketch_merge() {
        // Build controlled state via from_legacy_matrix (works for both Legacy and Sketchlib backends).
        let cms1 = CountMinSketchAccumulator {
            inner: CountMinSketch::from_legacy_matrix(
                vec![vec![5.0, 0.0, 0.0], vec![0.0, 0.0, 10.0]],
                2,
                3,
            ),
            sample_p: 1.0,
        };
        let cms2 = CountMinSketchAccumulator {
            inner: CountMinSketch::from_legacy_matrix(
                vec![vec![3.0, 7.0, 0.0], vec![0.0, 0.0, 0.0]],
                2,
                3,
            ),
            sample_p: 1.0,
        };

        let merged = CountMinSketchAccumulator::merge_accumulators(vec![cms1, cms2]).unwrap();

        let merged_sketch = merged.inner.sketch();
        assert_eq!(merged_sketch[0][0], 8.0);
        assert_eq!(merged_sketch[0][1], 7.0);
        assert_eq!(merged_sketch[1][2], 10.0);
    }

    #[test]
    fn test_count_min_sketch_merge_dimension_mismatch() {
        let cms1 = CountMinSketchAccumulator::new(2, 3);
        let cms2 = CountMinSketchAccumulator::new(3, 3);
        let result = CountMinSketchAccumulator::merge_accumulators(vec![cms1, cms2]);
        assert!(result.is_err());
    }

    #[test]
    fn test_count_min_sketch_as_aggregate_core() {
        let cms = CountMinSketchAccumulator::new(2, 3);
        assert_eq!(cms.type_name(), "CountMinSketchAccumulator");
    }

    #[test]
    fn test_trait_object() {
        let cms = CountMinSketchAccumulator::new(2, 3);
        let trait_obj: Box<dyn AggregateCore> = Box::new(cms);
        assert_eq!(trait_obj.type_name(), "CountMinSketchAccumulator");
    }

    #[test]
    fn test_count_min_sketch_key_query() {
        let mut cms = CountMinSketchAccumulator::new(4, 100);
        let key = KeyByLabelValues::new();
        assert_eq!(cms.query_key(&key), 0.0);
        cms._update(&key, 5.0);
        let result = cms.query_key(&key);
        assert!(result >= 5.0);
    }

    #[test]
    fn test_update_and_query_use_same_key_encoding() {
        // Regression test: _update and query_key must hash the same key string.
        // Previously _update went through serialize_to_json (which returns a JSON
        // array, so as_object() is always None) and always stored under key "".
        // query_key correctly used key.labels.join(";"), so they never matched.
        let mut cms = CountMinSketchAccumulator::new(4, 1000);
        let key = KeyByLabelValues::new_with_labels(vec!["web".to_string(), "prod".to_string()]);
        cms._update(&key, 5.0);
        let result = cms.query_key(&key);
        assert!(
            result >= 5.0,
            "_update and query_key used different key encodings: got {result}"
        );

        // Also verify a different key does not interfere.
        let other_key = KeyByLabelValues::new_with_labels(vec!["api".to_string()]);
        // other_key was never updated; its estimate should be lower than key's.
        let other_result = cms.query_key(&other_key);
        // In a sketch this large there should be no collision, so other_result == 0.
        assert_eq!(
            other_result, 0.0,
            "unrelated key returned non-zero: {other_result}"
        );
    }

    #[test]
    fn test_multiple_subpopulation_aggregate() {
        let mut cms = CountMinSketchAccumulator::new(3, 50);
        let key = KeyByLabelValues::new();
        cms._update(&key, 10.0);

        let multi_trait: &dyn MultipleSubpopulationAggregate = &cms;
        let result = multi_trait.query(Statistic::Sum, &key, None).unwrap();
        assert!(result >= 10.0);

        let keys = multi_trait.get_keys();
        assert!(keys.is_none());
    }

    #[test]
    fn test_count_min_sketch_merge_multiple() {
        // Build controlled state via from_legacy_matrix (works for both Legacy and Sketchlib backends).
        let cms1 = CountMinSketchAccumulator {
            inner: CountMinSketch::from_legacy_matrix(
                vec![vec![5.0, 0.0, 0.0], vec![0.0, 0.0, 10.0]],
                2,
                3,
            ),
            sample_p: 1.0,
        };
        let cms2 = CountMinSketchAccumulator {
            inner: CountMinSketch::from_legacy_matrix(
                vec![vec![3.0, 7.0, 0.0], vec![0.0, 0.0, 0.0]],
                2,
                3,
            ),
            sample_p: 1.0,
        };
        let cms3 = CountMinSketchAccumulator {
            inner: CountMinSketch::from_legacy_matrix(
                vec![vec![2.0, 0.0, 0.0], vec![0.0, 0.0, 5.0]],
                2,
                3,
            ),
            sample_p: 1.0,
        };

        let boxed_accs: Vec<Box<dyn AggregateCore>> =
            vec![Box::new(cms1), Box::new(cms2), Box::new(cms3)];

        let merged = CountMinSketchAccumulator::merge_multiple(&boxed_accs).unwrap();

        let merged_sketch = merged.inner.sketch();
        assert_eq!(merged_sketch[0][0], 10.0);
        assert_eq!(merged_sketch[0][1], 7.0);
        assert_eq!(merged_sketch[1][2], 15.0);
    }

    #[test]
    fn test_count_min_sketch_merge_multiple_error_cases() {
        let empty: Vec<Box<dyn AggregateCore>> = vec![];
        assert!(CountMinSketchAccumulator::merge_multiple(&empty).is_err());

        let cms1 = CountMinSketchAccumulator::new(2, 3);
        let cms2 = CountMinSketchAccumulator::new(3, 3);
        let boxed_accs: Vec<Box<dyn AggregateCore>> = vec![Box::new(cms1), Box::new(cms2)];
        assert!(CountMinSketchAccumulator::merge_multiple(&boxed_accs).is_err());

        use crate::summary_operators::sum_accumulator::SumAccumulator;
        let cms = CountMinSketchAccumulator::new(2, 3);
        let sum = SumAccumulator::new();
        let mixed_accs: Vec<Box<dyn AggregateCore>> = vec![Box::new(cms), Box::new(sum)];
        assert!(CountMinSketchAccumulator::merge_multiple(&mixed_accs).is_err());
    }

    #[test]
    fn test_from_sketchlib_proto_bytes_int64() {
        // Hand-build a CountMinState proto with INT64 counters and verify
        // round-tripping through from_sketchlib_proto_bytes yields the same
        // matrix that the modified-OTLP wire format would carry.
        use asap_sketchlib::proto::sketchlib::{CountMinState, CounterType};
        use prost::Message;

        let rows = 2u32;
        let cols = 3u32;
        // Row-major: row 0 = [1,2,3], row 1 = [4,5,6]
        let counts_int: Vec<i64> = vec![1, 2, 3, 4, 5, 6];
        let state = CountMinState {
            rows,
            cols,
            counter_type: CounterType::Int64 as i32,
            counts_int: counts_int.clone(),
            counts_float: Vec::new(),
            sum_counts: Vec::new(),
            sum2_counts: Vec::new(),
            l1: Vec::new(),
            l2: Vec::new(),
        };
        let bytes = state.encode_to_vec();

        let acc = CountMinSketchAccumulator::from_sketchlib_proto_bytes(&bytes).expect("decode ok");
        let matrix = acc.inner.sketch();
        assert_eq!(matrix.len(), rows as usize);
        assert_eq!(matrix[0], vec![1.0, 2.0, 3.0]);
        assert_eq!(matrix[1], vec![4.0, 5.0, 6.0]);
    }

    #[test]
    fn test_from_sketchlib_proto_bytes_envelope_wrapped() {
        // Mirrors what DataCollector's countminsketchprocessor emits:
        // the state is wrapped in a `SketchEnvelope{count_min: ...}`
        // via sketchlib-go's `SerializePortableFO` + `proto.Marshal`.
        // Before the fix, the Rust decoder decoded the envelope bytes as
        // a bare CountMinState, which produced "invalid wire type"
        // errors on field `cols` and silently fell through to §5.2.
        use asap_sketchlib::proto::sketchlib::{
            sketch_envelope, CountMinState, CounterType, SketchEnvelope,
        };
        use prost::Message;

        let state = CountMinState {
            rows: 2,
            cols: 3,
            counter_type: CounterType::Int64 as i32,
            counts_int: vec![7, 8, 9, 10, 11, 12],
            counts_float: Vec::new(),
            sum_counts: Vec::new(),
            sum2_counts: Vec::new(),
            l1: Vec::new(),
            l2: Vec::new(),
        };
        let env = SketchEnvelope {
            sketch_state: Some(sketch_envelope::SketchState::CountMin(state)),
            ..Default::default()
        };
        let bytes = env.encode_to_vec();

        let acc = CountMinSketchAccumulator::from_sketchlib_proto_bytes(&bytes)
            .expect("envelope-wrapped decode should succeed");
        let matrix = acc.inner.sketch();
        assert_eq!(matrix[0], vec![7.0, 8.0, 9.0]);
        assert_eq!(matrix[1], vec![10.0, 11.0, 12.0]);
    }

    #[test]
    fn test_from_sketchlib_proto_bytes_envelope_wrong_sketch_type() {
        // An envelope carrying a non-CountMin sketch should be rejected
        // with a clear error rather than silently producing garbage.
        use asap_sketchlib::proto::sketchlib::{sketch_envelope, KllState, SketchEnvelope};
        use prost::Message;

        let kll = KllState::default();
        let env = SketchEnvelope {
            sketch_state: Some(sketch_envelope::SketchState::Kll(kll)),
            ..Default::default()
        };
        let bytes = env.encode_to_vec();

        let result = CountMinSketchAccumulator::from_sketchlib_proto_bytes(&bytes);
        assert!(result.is_err(), "wrong-sketch envelope should error");
    }

    #[test]
    fn test_from_sketchlib_proto_bytes_float64() {
        use asap_sketchlib::proto::sketchlib::{CountMinState, CounterType};
        use prost::Message;

        let state = CountMinState {
            rows: 2,
            cols: 2,
            counter_type: CounterType::Float64 as i32,
            counts_int: Vec::new(),
            counts_float: vec![1.5, 2.5, 3.5, 4.5],
            sum_counts: Vec::new(),
            sum2_counts: Vec::new(),
            l1: Vec::new(),
            l2: Vec::new(),
        };
        let bytes = state.encode_to_vec();

        let acc = CountMinSketchAccumulator::from_sketchlib_proto_bytes(&bytes).expect("decode ok");
        let matrix = acc.inner.sketch();
        assert_eq!(matrix[0], vec![1.5, 2.5]);
        assert_eq!(matrix[1], vec![3.5, 4.5]);
    }

    #[test]
    fn test_from_sketchlib_proto_bytes_dimension_mismatch() {
        // counts_int has 5 entries but rows*cols = 6 → expect error
        use asap_sketchlib::proto::sketchlib::{CountMinState, CounterType};
        use prost::Message;

        let state = CountMinState {
            rows: 2,
            cols: 3,
            counter_type: CounterType::Int64 as i32,
            counts_int: vec![1, 2, 3, 4, 5],
            counts_float: Vec::new(),
            sum_counts: Vec::new(),
            sum2_counts: Vec::new(),
            l1: Vec::new(),
            l2: Vec::new(),
        };
        let bytes = state.encode_to_vec();

        let result = CountMinSketchAccumulator::from_sketchlib_proto_bytes(&bytes);
        assert!(result.is_err());
        assert!(
            result.unwrap_err().to_string().contains("counts_int"),
            "error should mention counts_int dim mismatch"
        );
    }

    #[test]
    fn test_from_sketchlib_proto_bytes_zero_dims_rejected() {
        use asap_sketchlib::proto::sketchlib::CountMinState;
        use prost::Message;

        let state = CountMinState::default();
        let bytes = state.encode_to_vec();

        let result = CountMinSketchAccumulator::from_sketchlib_proto_bytes(&bytes);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("degenerate dims"));
    }

    #[test]
    fn test_apply_proto_delta_bytes_round_trip() {
        use asap_sketchlib::proto::sketchlib::CountMinDelta as PbDelta;
        use prost::Message;

        let mut acc = CountMinSketchAccumulator {
            inner: CountMinSketch::from_legacy_matrix(
                vec![vec![1.0, 2.0, 3.0], vec![4.0, 5.0, 6.0]],
                2,
                3,
            ),
            sample_p: 1.0,
        };
        let bytes = PbDelta {
            rows: 2,
            cols: 3,
            cell_rows: vec![0, 1],
            cell_cols: vec![0, 2],
            d_counts: vec![10, 100],
            l1: vec![],
            l2: vec![],
            ..Default::default()
        }
        .encode_to_vec();

        acc.apply_proto_delta_bytes(&bytes).expect("apply ok");
        assert_eq!(
            acc.inner.sketch(),
            vec![vec![11.0, 2.0, 3.0], vec![4.0, 5.0, 106.0]]
        );
    }

    #[test]
    fn test_apply_proto_delta_bytes_rejects_garbage() {
        let mut acc = CountMinSketchAccumulator::new(2, 3);
        assert!(acc.apply_proto_delta_bytes(b"not valid proto").is_err());
    }

    // ----------------------------------------------------------------
    // Statistic::Rate / Statistic::Increase — PR #111 honest-gap closure.
    // CMS records insert counts but not timestamps. The Rate readout
    // requires the engine to push `range_ms` via query_kwargs; without
    // it the accumulator falls back to the raw event count (units of
    // events/window) so the answer is at least non-empty.
    // ----------------------------------------------------------------

    #[test]
    fn test_query_statistic_rate_with_range_ms() {
        // Build a CMS whose min-row-sum is 100 events. With a 5-minute
        // (300_000 ms) range, the per-second rate is 100 / 300 ≈ 0.333.
        let cms = CountMinSketchAccumulator {
            inner: CountMinSketch::from_legacy_matrix(
                vec![vec![100.0, 0.0], vec![100.0, 0.0]],
                2,
                2,
            ),
            sample_p: 1.0,
        };
        let mut kwargs = HashMap::new();
        kwargs.insert("range_ms".to_string(), "300000".to_string());
        let trait_obj: &dyn AggregateCore = &cms;
        let v = trait_obj
            .query_statistic(Statistic::Rate, &None, &kwargs)
            .expect("Rate with range_ms is supported");
        assert!(
            (v - (100.0 / 300.0)).abs() < 1e-9,
            "expected 100/300 = {}, got {v}",
            100.0 / 300.0,
        );
    }

    #[test]
    fn test_query_statistic_rate_without_range_ms_falls_back_to_count() {
        // Without `range_ms` in kwargs the accumulator returns the raw
        // event volume (events/window units). Caller is responsible for
        // surfacing that caveat to the user; this avoids `status=error`
        // for instant rate-shape queries that bypass the matrix-selector
        // code path.
        let cms = CountMinSketchAccumulator {
            inner: CountMinSketch::from_legacy_matrix(vec![vec![42.0, 0.0], vec![42.0, 0.0]], 2, 2),
            sample_p: 1.0,
        };
        let trait_obj: &dyn AggregateCore = &cms;
        let v = trait_obj
            .query_statistic(Statistic::Rate, &None, &HashMap::new())
            .expect("Rate without range_ms still answers (fallback)");
        assert_eq!(v, 42.0);
    }

    #[test]
    fn test_query_statistic_increase_returns_total_count() {
        // Increase semantics on CMS: total events in the window — the
        // same min-row-sum as Sum / Count. Differs from Rate only in
        // that it never divides by range.
        let cms = CountMinSketchAccumulator {
            inner: CountMinSketch::from_legacy_matrix(vec![vec![5.0, 7.0], vec![3.0, 9.0]], 2, 2),
            sample_p: 1.0,
        };
        let trait_obj: &dyn AggregateCore = &cms;
        let v = trait_obj
            .query_statistic(Statistic::Increase, &None, &HashMap::new())
            .expect("Increase is supported");
        // min-row-sum: row0 = 12, row1 = 12, min = 12.
        assert_eq!(v, 12.0);
    }

    // ----------------------------------------------------------------
    // Defensive inbound-dimension validation (harden/sketch-dim-validation).
    // Malformed / degenerate / narrow-hash-budget-violating CMS dims must
    // be rejected gracefully (Err, never a panic); valid configs the
    // backend actually uses (5x2048, 5x4096, 5x2000) must still decode.
    // ----------------------------------------------------------------

    /// Build a bare `CountMinState` proto carrying the given dims and a
    /// row-major INT64 counts vector sized to `rows*cols` so that, IF the
    /// dims pass validation, the reshape also succeeds. Used to prove a
    /// malformed-dim payload is rejected at the dim gate, not later.
    fn cms_state_bytes(rows: u32, cols: u32) -> Vec<u8> {
        use asap_sketchlib::proto::sketchlib::{CountMinState, CounterType};
        use prost::Message;
        let n = (rows as usize).saturating_mul(cols as usize);
        let state = CountMinState {
            rows,
            cols,
            counter_type: CounterType::Int64 as i32,
            counts_int: vec![0i64; n],
            counts_float: Vec::new(),
            sum_counts: Vec::new(),
            sum2_counts: Vec::new(),
            l1: Vec::new(),
            l2: Vec::new(),
        };
        state.encode_to_vec()
    }

    #[test]
    fn test_validate_sketch_dims_accepts_valid_configs() {
        // The realistic configs the backend uses must pass unchanged.
        for (r, c) in [(5usize, 2048usize), (5, 4096), (5, 2000), (4, 1000), (2, 3)] {
            assert!(
                validate_sketch_dims("CountMinState", r, c).is_ok(),
                "valid config {r}x{c} was wrongly rejected"
            );
        }
    }

    #[test]
    fn test_validate_sketch_dims_rejects_malformed() {
        // Zero dims.
        assert!(validate_sketch_dims("CountMinState", 0, 2048).is_err());
        assert!(validate_sketch_dims("CountMinState", 5, 0).is_err());
        // Narrow-hash-budget violation: 5 * ceil(log2(8192))=5*13=65 > 64.
        let err = validate_sketch_dims("CountMinState", 5, 8192).unwrap_err();
        assert!(err.contains("budget"), "expected budget error, got: {err}");
        // Absurdly oversized: 1 x 16,777,216 = 16M cells > 8M cap. (1 row
        // keeps the hash budget tiny — 1*24=24 — so the cap check, not the
        // budget check, is what fires here.)
        let err = validate_sketch_dims("CountMinState", 1, 16_777_216).unwrap_err();
        assert!(err.contains("cap"), "expected cell-cap error, got: {err}");
        // No panic on extreme dims (saturating_mul guards the products).
        assert!(validate_sketch_dims("CountMinState", usize::MAX, usize::MAX).is_err());
    }

    #[test]
    fn test_from_sketchlib_proto_bytes_rejects_bad_dims_no_panic() {
        // A data point declaring narrow-hash-budget-violating dims must be
        // skipped (Err returned, NOT a panic). The ingest caller turns
        // this Err into a dropped data point + WARN log.
        let bytes = cms_state_bytes(5, 8192);
        let result = CountMinSketchAccumulator::from_sketchlib_proto_bytes(&bytes);
        assert!(result.is_err(), "budget-violating dims should be rejected");
        assert!(result.unwrap_err().to_string().contains("rejecting"));

        // A valid neighbour (5x4096) on the same path still decodes fine.
        let ok_bytes = cms_state_bytes(5, 4096);
        let acc = CountMinSketchAccumulator::from_sketchlib_proto_bytes(&ok_bytes)
            .expect("valid 5x4096 CMS should still decode");
        assert_eq!(acc.inner.rows(), 5);
        assert_eq!(acc.inner.cols(), 4096);
    }

    #[test]
    fn test_query_statistic_rate_rejects_invalid_range_ms() {
        let cms = CountMinSketchAccumulator::new(2, 2);
        let mut kwargs = HashMap::new();
        kwargs.insert("range_ms".to_string(), "0".to_string());
        let trait_obj: &dyn AggregateCore = &cms;
        let err = trait_obj
            .query_statistic(Statistic::Rate, &None, &kwargs)
            .expect_err("range_ms=0 should error");
        assert!(err.to_string().contains("positive"));

        let mut kwargs = HashMap::new();
        kwargs.insert("range_ms".to_string(), "not-a-number".to_string());
        let err = trait_obj
            .query_statistic(Statistic::Rate, &None, &kwargs)
            .expect_err("non-numeric range_ms should error");
        assert!(err.to_string().contains("bad range_ms"));
    }

    // ----------------------------------------------------------------
    // sample_p rescale. The edge admits each insert with probability `p`,
    // so every stored cell is ~p× the true count. CMS is L1/additive and
    // linear, so BOTH the point-frequency (query_key) and the aggregate
    // total-event statistics (Count/Sum/Increase/Rate) rescale by 1/p.
    // ----------------------------------------------------------------

    #[test]
    fn test_query_key_rescaled_by_sample_p() {
        // Same stored cell counts, two sample_p values: the p=0.25 sketch
        // must report 4× the point-frequency of the unsampled one.
        let key = KeyByLabelValues::new_with_labels(vec!["web".to_string()]);
        let mut unsampled = CountMinSketchAccumulator::new(4, 1000);
        unsampled._update(&key, 10.0);
        let mut sampled = CountMinSketchAccumulator::new(4, 1000);
        sampled._update(&key, 10.0);
        sampled.sample_p = 0.25;

        let raw = unsampled.query_key(&key);
        let rescaled = sampled.query_key(&key);
        assert!(
            raw >= 10.0,
            "raw estimate should be >= inserted 10, got {raw}"
        );
        assert!(
            (rescaled - raw * 4.0).abs() < 1e-9,
            "expected point-frequency rescaled ≈ 4×raw ({}), got {rescaled}",
            raw * 4.0
        );
    }

    #[test]
    fn test_aggregate_statistics_rescaled_by_sample_p() {
        use crate::Statistic;
        // Build a CMS with a known min-row-sum of 12 events, sampled at
        // p=0.25 → every aggregate statistic should report 12 / 0.25 = 48.
        let cms = CountMinSketchAccumulator {
            inner: CountMinSketch::from_legacy_matrix(vec![vec![5.0, 7.0], vec![3.0, 9.0]], 2, 2),
            sample_p: 0.25,
        };
        let trait_obj: &dyn AggregateCore = &cms;
        for stat in [Statistic::Count, Statistic::Sum, Statistic::Increase] {
            let v = trait_obj
                .query_statistic(stat, &None, &HashMap::new())
                .unwrap_or_else(|e| panic!("{stat:?} should be supported: {e}"));
            // min-row-sum = 12, rescaled by 1/0.25 = 48.
            assert!(
                (v - 48.0).abs() < 1e-9,
                "{stat:?}: expected rescaled 48, got {v}"
            );
        }
        // Rate also divides through the rescaled total: 48 events over a
        // 6-second (6000 ms) range = 8 events/s.
        let mut kwargs = HashMap::new();
        kwargs.insert("range_ms".to_string(), "6000".to_string());
        let r = trait_obj
            .query_statistic(Statistic::Rate, &None, &kwargs)
            .expect("rate ok");
        assert!((r - 8.0).abs() < 1e-9, "expected rate 8.0, got {r}");
    }

    #[test]
    fn test_sample_p_unset_behaves_as_one() {
        use asap_sketchlib::proto::sketchlib::{
            sketch_envelope, CountMinState, CounterType, SketchEnvelope,
        };
        use prost::Message;
        // An envelope with no sample_p (proto3 default 0.0) must normalize
        // to 1.0 (no rescale) — byte-compatible with legacy frames.
        let state = CountMinState {
            rows: 2,
            cols: 2,
            counter_type: CounterType::Int64 as i32,
            counts_int: vec![1, 2, 3, 4],
            counts_float: Vec::new(),
            sum_counts: Vec::new(),
            sum2_counts: Vec::new(),
            l1: Vec::new(),
            l2: Vec::new(),
        };
        let env = SketchEnvelope {
            // sample_p left at proto3 default 0.0.
            sketch_state: Some(sketch_envelope::SketchState::CountMin(state)),
            ..Default::default()
        };
        let bytes = env.encode_to_vec();
        let acc = CountMinSketchAccumulator::from_sketchlib_proto_bytes(&bytes).expect("decode ok");
        assert_eq!(acc.sample_p, 1.0, "unset sample_p must normalize to 1.0");
    }

    #[test]
    fn test_from_sketchlib_proto_bytes_reads_envelope_sample_p() {
        use crate::Statistic;
        use asap_sketchlib::proto::sketchlib::{
            sketch_envelope, CountMinState, CounterType, SketchEnvelope,
        };
        use prost::Message;
        // min-row-sum = 12 raw; sample_p 0.25 → Count = 48.
        let state = CountMinState {
            rows: 2,
            cols: 2,
            counter_type: CounterType::Float64 as i32,
            counts_int: Vec::new(),
            counts_float: vec![5.0, 7.0, 3.0, 9.0],
            sum_counts: Vec::new(),
            sum2_counts: Vec::new(),
            l1: Vec::new(),
            l2: Vec::new(),
        };
        let env = SketchEnvelope {
            sample_p: 0.25,
            sketch_state: Some(sketch_envelope::SketchState::CountMin(state)),
            ..Default::default()
        };
        let bytes = env.encode_to_vec();
        let acc = CountMinSketchAccumulator::from_sketchlib_proto_bytes(&bytes).expect("decode ok");
        assert_eq!(acc.sample_p, 0.25);
        let trait_obj: &dyn AggregateCore = &acc;
        let v = trait_obj
            .query_statistic(Statistic::Count, &None, &HashMap::new())
            .expect("count ok");
        assert!((v - 48.0).abs() < 1e-9, "expected rescaled 48, got {v}");
    }

    #[test]
    fn test_reset_to_empty_preserves_sample_p() {
        let mut acc = CountMinSketchAccumulator::new(2, 3);
        acc.sample_p = 0.25;
        acc.reset_to_empty();
        assert_eq!(acc.sample_p, 0.25, "window rotation must keep sample_p");
    }

    #[test]
    fn test_merge_prefers_sampled_factor() {
        let mut a = CountMinSketchAccumulator::new(2, 3);
        a.sample_p = 0.25;
        let b = CountMinSketchAccumulator::new(2, 3); // sample_p 1.0
        let merged = a.merge_with(&b).expect("merge ok");
        let merged = merged
            .as_any()
            .downcast_ref::<CountMinSketchAccumulator>()
            .expect("downcast ok");
        assert_eq!(merged.sample_p, 0.25);

        // merge_multiple mirrors the same policy.
        let mut c = CountMinSketchAccumulator::new(2, 3);
        c.sample_p = 0.25;
        let d = CountMinSketchAccumulator::new(2, 3);
        let boxed: Vec<Box<dyn AggregateCore>> = vec![Box::new(d), Box::new(c)];
        let merged = CountMinSketchAccumulator::merge_multiple(&boxed).expect("merge ok");
        assert_eq!(merged.sample_p, 0.25);
    }
}

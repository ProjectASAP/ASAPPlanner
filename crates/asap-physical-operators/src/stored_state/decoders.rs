//! Shared sketch state reconstruction and decoding.
use asap_sketchlib::CountMinSketch;
use asap_sketchlib::CountMinSketchDelta;
use asap_sketchlib::CountMinSketchWithHeap;
use asap_sketchlib::CountSketch;
use asap_sketchlib::CountSketchDelta;
use asap_sketchlib::CountSketchWithHeap;
use asap_sketchlib::CsHeapItem;
use asap_sketchlib::MessagePackCodec;

use crate::accumulators::count_min_sketch_with_heap_accumulator::CountMinSketchWithHeapAccumulator;

/// Decode a `CountMinSketch` from the modified-OTLP wire bytes.
/// MSGPACK path round-trips `CountMinSketch::deserialize_msgpack`;
/// PROTO path decodes a `SketchEnvelope{count_min: CountMinState}`
/// (or bare `CountMinState`) and re-projects to a flat matrix. Mirrors
/// `precompute_operators::count_min_sketch_accumulator::from_sketchlib_proto_bytes`.
pub fn decode_cms_from_proto(buffer: &[u8]) -> Result<CountMinSketch, String> {
    use asap_sketchlib::proto::sketchlib::{
        sketch_envelope, CountMinState, CounterType, SketchEnvelope,
    };
    use prost::Message;

    let state = match SketchEnvelope::decode(buffer) {
        Ok(env) => match env.sketch_state {
            Some(sketch_envelope::SketchState::CountMin(st)) => st,
            Some(_) => return Err("SketchEnvelope contains non-CountMin sketch".to_string()),
            None => {
                CountMinState::decode(buffer).map_err(|e| format!("decode CountMinState: {e}"))?
            }
        },
        Err(_) => {
            CountMinState::decode(buffer).map_err(|e| format!("decode CountMinState: {e}"))?
        }
    };
    let rows = state.rows as usize;
    let cols = state.cols as usize;
    if rows == 0 || cols == 0 {
        return Err(format!(
            "CountMinState has zero dims (rows={rows}, cols={cols})"
        ));
    }
    let expected_len = rows * cols;
    let counter_type = CounterType::try_from(state.counter_type)
        .map_err(|_| format!("CountMinState unknown counter_type {}", state.counter_type))?;
    let flat: Vec<f64> = match counter_type {
        CounterType::Int32 | CounterType::Int64 => {
            if state.counts_int.len() != expected_len {
                return Err(format!(
                    "CountMinState counts_int has {} entries, expected {}",
                    state.counts_int.len(),
                    expected_len
                ));
            }
            state.counts_int.iter().map(|&v| v as f64).collect()
        }
        CounterType::Float64 => {
            if state.counts_float.len() != expected_len {
                return Err(format!(
                    "CountMinState counts_float has {} entries, expected {}",
                    state.counts_float.len(),
                    expected_len
                ));
            }
            state.counts_float.clone()
        }
        other => {
            return Err(format!(
                "CountMinState counter_type {other:?} not yet supported in reducer"
            ));
        }
    };
    let mut matrix = Vec::with_capacity(rows);
    for r in 0..rows {
        let start = r * cols;
        matrix.push(flat[start..start + cols].to_vec());
    }
    Ok(CountMinSketch::from_legacy_matrix(matrix, rows, cols))
}

/// Decode a `CountMinSketch` from msgpack bytes (sketch-core wire
/// format). Mirrors
/// `CountMinSketchAccumulator::from_msgpack_bytes`.
pub fn decode_cms_from_msgpack(buffer: &[u8]) -> Result<CountMinSketch, String> {
    CountMinSketch::from_msgpack(buffer)
        .map_err(|e| format!("deserialize CountMinSketch msgpack: {e}"))
}

/// Decode a `CountSketch` from the modified-OTLP proto wire bytes.
/// Mirrors
/// `precompute_operators::count_sketch_accumulator::from_sketchlib_proto_bytes`.
pub fn decode_cs_from_proto(buffer: &[u8]) -> Result<CountSketch, String> {
    use asap_sketchlib::proto::sketchlib::{
        sketch_envelope, CountSketchState, CounterType, SketchEnvelope,
    };
    use prost::Message;

    let state = match SketchEnvelope::decode(buffer) {
        Ok(env) => match env.sketch_state {
            Some(sketch_envelope::SketchState::CountSketch(st)) => st,
            Some(_) => return Err("SketchEnvelope contains non-CountSketch sketch".to_string()),
            None => CountSketchState::decode(buffer)
                .map_err(|e| format!("decode CountSketchState: {e}"))?,
        },
        Err(_) => {
            CountSketchState::decode(buffer).map_err(|e| format!("decode CountSketchState: {e}"))?
        }
    };
    let rows = state.rows as usize;
    let cols = state.cols as usize;
    if rows == 0 || cols == 0 {
        return Err(format!(
            "CountSketchState has zero dims (rows={rows}, cols={cols})"
        ));
    }
    let expected_len = rows * cols;
    let counter_type = CounterType::try_from(state.counter_type).map_err(|_| {
        format!(
            "CountSketchState unknown counter_type {}",
            state.counter_type
        )
    })?;
    let flat: Vec<f64> = match counter_type {
        CounterType::Int32 | CounterType::Int64 => {
            if state.counts_int.len() != expected_len {
                return Err(format!(
                    "CountSketchState counts_int has {} entries, expected {}",
                    state.counts_int.len(),
                    expected_len
                ));
            }
            state.counts_int.iter().map(|&v| v as f64).collect()
        }
        CounterType::Float64 => {
            if state.counts_float.len() != expected_len {
                return Err(format!(
                    "CountSketchState counts_float has {} entries, expected {}",
                    state.counts_float.len(),
                    expected_len
                ));
            }
            state.counts_float.clone()
        }
        other => {
            return Err(format!(
                "CountSketchState counter_type {other:?} not yet supported in reducer"
            ));
        }
    };
    let mut matrix = Vec::with_capacity(rows);
    for r in 0..rows {
        let start = r * cols;
        matrix.push(flat[start..start + cols].to_vec());
    }
    Ok(CountSketch::from_legacy_matrix(matrix, rows, cols))
}

/// Decode a `CountSketch` from msgpack bytes (sketch-core wire format).
pub fn decode_cs_from_msgpack(buffer: &[u8]) -> Result<CountSketch, String> {
    CountSketch::from_msgpack(buffer).map_err(|e| format!("deserialize CountSketch msgpack: {e}"))
}

/// Decode a `CountMinSketchWithHeap` from msgpack bytes — the OTLP
/// `CountMinSketch` wire bytes when the gateway/precompute layer
/// marked the sid as CmsWithHeap (heap embedded in the
/// `CountMinSketchWithHeapSerialized` outer wrapper). Delegates to
/// `asap_sketchlib::CountMinSketchWithHeap::deserialize_msgpack`.
pub fn decode_cms_with_heap_from_msgpack(buffer: &[u8]) -> Result<CountMinSketchWithHeap, String> {
    CountMinSketchWithHeap::from_msgpack(buffer)
        .map_err(|e| format!("deserialize CountMinSketchWithHeap msgpack: {e}"))
}

/// Decode a `CountSketchWithHeap` (median-estimator, Count Sketch family)
/// from msgpack bytes. Distinct wire type from `CountMinSketchWithHeap`
/// (min-estimator, Count-Min family) even though both are heap-bearing
/// frequency sketches — see `asap_sketchlib::CountSketchWithHeap`.
/// Delegates to `asap_sketchlib::CountSketchWithHeap::from_msgpack`.
pub fn decode_cs_with_heap_from_msgpack(buffer: &[u8]) -> Result<CountSketchWithHeap, String> {
    CountSketchWithHeap::from_msgpack(buffer)
        .map_err(|e| format!("deserialize CountSketchWithHeap msgpack: {e}"))
}

// ---------------------------------------------------------------------------
// Delta decoders. Under the per-window-reset (PWR) contract
// (`asap-precompute-go/window.go`: a delta is that window's own state
// applied onto a freshly-reset per-series sketch), each stored *Delta
// frame reconstructs into the FULL window state when applied onto an
// EMPTY base of the frame's declared dimensions. The reducer's
// `FrequencyEstimate` / `FrequencyTopk` paths are per-window evaluations,
// so "empty + apply(this window's delta)" yields exactly the window's
// matrix/heap — no cross-window stitching needed (mirrors how the ingest
// accumulators reset_to_empty per window before applying).
//
// The proto path reuses the PUBLIC `asap_sketchlib::{CountSketch,
// CountMinSketch}::apply_delta`; the proto `*Delta` message is decoded via
// `asap_sketchlib::proto::sketchlib::{CountSketchDelta, CountMinDelta}`,
// exactly as `precompute_operators::{count_sketch,
// count_min_sketch}_accumulator::apply_proto_delta_bytes` does.
// ---------------------------------------------------------------------------

/// Decode a `CountMinSketch` PROTO_DELTA frame into a FULL sketch by
/// applying the sparse cell delta onto an empty base of the frame's
/// declared dimensions. Mirrors
/// `precompute_operators::count_min_sketch_accumulator::apply_proto_delta_bytes`.
pub fn decode_cms_from_proto_delta(buffer: &[u8]) -> Result<CountMinSketch, String> {
    use asap_sketchlib::proto::sketchlib::CountMinDelta as PbDelta;
    use prost::Message;

    let pb = PbDelta::decode(buffer).map_err(|e| format!("decode CountMinDelta: {e}"))?;
    if pb.cell_rows.len() != pb.cell_cols.len() || pb.cell_rows.len() != pb.d_counts.len() {
        return Err(format!(
            "CountMinDelta packed-array length mismatch: cell_rows={}, cell_cols={}, d_counts={}",
            pb.cell_rows.len(),
            pb.cell_cols.len(),
            pb.d_counts.len()
        ));
    }
    let rows = pb.rows as usize;
    let cols = pb.cols as usize;
    if rows == 0 || cols == 0 {
        return Err(format!(
            "CountMinDelta has zero dims (rows={rows}, cols={cols})"
        ));
    }
    let cells = pb
        .cell_rows
        .iter()
        .zip(pb.cell_cols.iter())
        .zip(pb.d_counts.iter())
        .map(|((r, c), dc)| (*r, *c, *dc))
        .collect();
    // hh_keys is parsed off the wire by the precompute accumulator but
    // intentionally dropped (the vendored Go proto bindings don't yet
    // populate it); match that to keep behavior identical.
    let delta = CountMinSketchDelta {
        rows: pb.rows,
        cols: pb.cols,
        cells,
        l1: pb.l1,
        l2: pb.l2,
        hh_keys: Vec::new(),
    };
    let mut cms = CountMinSketch::from_legacy_matrix(vec![vec![0.0; cols]; rows], rows, cols);
    cms.apply_delta(&delta)
        .map_err(|e| format!("apply CountMinDelta onto empty base: {e}"))?;
    Ok(cms)
}

/// Decode a `CountSketch` PROTO_DELTA frame into a FULL sketch by applying
/// the sparse cell delta onto an empty base of the frame's declared
/// dimensions. Mirrors
/// `precompute_operators::count_sketch_accumulator::apply_proto_delta_bytes`.
pub fn decode_cs_from_proto_delta(buffer: &[u8]) -> Result<CountSketch, String> {
    use asap_sketchlib::proto::sketchlib::CountSketchDelta as PbDelta;
    use prost::Message;

    let pb = PbDelta::decode(buffer).map_err(|e| format!("decode CountSketchDelta: {e}"))?;
    if pb.cell_rows.len() != pb.cell_cols.len() || pb.cell_rows.len() != pb.d_counts.len() {
        return Err(format!(
            "CountSketchDelta packed-array length mismatch: cell_rows={}, cell_cols={}, d_counts={}",
            pb.cell_rows.len(),
            pb.cell_cols.len(),
            pb.d_counts.len()
        ));
    }
    let rows = pb.rows as usize;
    let cols = pb.cols as usize;
    if rows == 0 || cols == 0 {
        return Err(format!(
            "CountSketchDelta has zero dims (rows={rows}, cols={cols})"
        ));
    }
    let cells = pb
        .cell_rows
        .iter()
        .zip(pb.cell_cols.iter())
        .zip(pb.d_counts.iter())
        .map(|((r, c), dc)| (*r, *c, *dc))
        .collect();
    let delta = CountSketchDelta {
        rows: pb.rows,
        cols: pb.cols,
        cells,
        l2: pb.l2,
        hh_keys: Vec::new(),
    };
    let mut cs = CountSketch::from_legacy_matrix(vec![vec![0.0; cols]; rows], rows, cols);
    cs.apply_delta(&delta)
        .map_err(|e| format!("apply CountSketchDelta onto empty base: {e}"))?;
    Ok(cs)
}

/// Decode a heap-bearing CountSketch MSGPACK_DELTA frame into a FULL
/// `CountMinSketchWithHeap` by applying the sparse matrix delta + full
/// heap onto an empty base of the frame's declared dimensions. This
/// REUSES the ingest-side delta-heap apply logic
/// (`CountMinSketchWithHeapAccumulator::from_msgpack_heap_delta_bytes` →
/// `apply_msgpack_heap_delta_bytes`), which decodes the frame generically
/// with `rmp_serde` — no `asap_sketchlib` delta API is added.
pub fn decode_cms_with_heap_from_msgpack_delta(
    buffer: &[u8],
) -> Result<CountMinSketchWithHeap, String> {
    let acc = CountMinSketchWithHeapAccumulator::from_msgpack_heap_delta_bytes(buffer)
        .map_err(|e| format!("reconstruct CountMinSketchWithHeap from delta: {e}"))?;
    Ok(acc.inner)
}

/// Decode a heap-bearing CountSketch (median-estimator) MSGPACK_DELTA frame
/// into a FULL `asap_sketchlib::CountSketchWithHeap` by applying the sparse
/// matrix delta + full heap onto an empty base of the frame's declared
/// dimensions. Same DELTA-HEAP wire shape as the CmsWithHeap delta frame
/// (see `HeapDeltaWire`/`MatrixDeltaWire` in
/// `count_min_sketch_with_heap_accumulator.rs`), decoded here directly
/// with `rmp_serde` since there is no CountSketchWithHeap ingest
/// accumulator to delegate to. No `asap_sketchlib` delta API needed — the
/// public `from_legacy_matrix` rebuilds both the matrix and heap.
pub fn decode_cs_with_heap_from_msgpack_delta(
    buffer: &[u8],
) -> Result<CountSketchWithHeap, String> {
    #[derive(serde::Deserialize)]
    struct HeapDeltaWire {
        is_delta: bool,
        matrix_delta: MatrixDeltaWire,
        topk_heap: Vec<(String, f64)>,
        heap_size: u64,
    }
    #[derive(serde::Deserialize)]
    struct MatrixDeltaWire {
        rows: u32,
        cols: u32,
        cells: Vec<(u32, u32, i64)>,
    }

    let wire: HeapDeltaWire = rmp_serde::from_slice(buffer)
        .map_err(|e| format!("decode CountSketchWithHeap delta msgpack: {e}"))?;
    if !wire.is_delta {
        return Err("CountSketchWithHeap delta frame has is_delta=false".to_string());
    }
    let rows = wire.matrix_delta.rows as usize;
    let cols = wire.matrix_delta.cols as usize;
    if rows == 0 || cols == 0 {
        return Err(format!(
            "CountSketchWithHeap delta frame has zero dims (rows={rows}, cols={cols})"
        ));
    }
    let mut matrix = vec![vec![0.0; cols]; rows];
    for (r, c, dc) in &wire.matrix_delta.cells {
        let (r, c) = (*r as usize, *c as usize);
        if r >= rows || c >= cols {
            continue;
        }
        matrix[r][c] += *dc as f64;
    }
    let heap: Vec<CsHeapItem> = wire
        .topk_heap
        .into_iter()
        .map(|(key, value)| CsHeapItem { key, value })
        .collect();
    Ok(CountSketchWithHeap::from_legacy_matrix(
        matrix,
        heap,
        rows,
        cols,
        wire.heap_size as usize,
    ))
}

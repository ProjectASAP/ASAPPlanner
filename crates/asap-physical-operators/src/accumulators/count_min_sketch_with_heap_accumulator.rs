use crate::{
    AggregateCore, AggregationType, KeyByLabelValues, MergeableAccumulator,
    MultipleSubpopulationAggregate, SerializableToSink,
};
use asap_sketchlib::{CmsHeapItem, CountMinSketchWithHeap, MessagePackCodec};
use serde::Deserialize;
use serde_json::Value;
use std::collections::HashMap;

use crate::Statistic;

/// Local serde view of the DELTA-HEAP wire frame produced by sketchlib-go's
/// `CountSketch.SerializeMsgpackWithHeapDelta` (encoding `MSGPACK_DELTA`).
/// Decoded with `rmp_serde` directly in the backend so NO delta API needs to
/// be added to the public `asap_sketchlib`.
///
/// rmp_serde compact layout — a 4-element positional array:
///
///   [
///     is_delta: bool (always true),
///     matrix_delta: ( rows:u32, cols:u32, cells: Vec<(u32,u32,i64)> ),
///     topk_heap: Vec<(String, f64)>,   // FULL heap, [key, value] pairs
///     heap_size: u64,
///   ]
///
/// Tuple structs deserialize from msgpack fixed arrays positionally, so this
/// matches the Go encoder's byte layout exactly (no field names on the wire).
#[derive(Debug, Deserialize)]
struct HeapDeltaWire {
    is_delta: bool,
    matrix_delta: MatrixDeltaWire,
    topk_heap: Vec<(String, f64)>,
    #[allow(dead_code)]
    heap_size: u64,
}

#[derive(Debug, Deserialize)]
struct MatrixDeltaWire {
    rows: u32,
    cols: u32,
    cells: Vec<(u32, u32, i64)>,
}

/// Validated/flattened view of a decoded DELTA-HEAP frame.
struct HeapDeltaFrame {
    rows: u32,
    cols: u32,
    heap_size: u64,
    cells: Vec<(u32, u32, i64)>,
    heap: Vec<(String, f64)>,
}

impl HeapDeltaFrame {
    fn from_msgpack(buffer: &[u8]) -> Result<Self, Box<dyn std::error::Error>> {
        let wire: HeapDeltaWire = rmp_serde::from_slice(buffer)
            .map_err(|e| format!("decode CountSketchWithHeap delta msgpack: {e}"))?;
        if !wire.is_delta {
            return Err("CountSketchWithHeap delta frame has is_delta=false".into());
        }
        Ok(Self {
            rows: wire.matrix_delta.rows,
            cols: wire.matrix_delta.cols,
            heap_size: wire.heap_size,
            cells: wire.matrix_delta.cells,
            heap: wire.topk_heap,
        })
    }
}

/// Count-Min Sketch with Heap accumulator — wraps `asap_sketchlib::CountMinSketchWithHeap`.
/// Core struct, update/merge/serde logic live in `asap_sketchlib::message_pack_format::portable::countminsketch_topk`.
/// This file retains QE-specific trait impls, legacy deserializers, and JSON output.
#[derive(Debug, Clone)]
pub struct CountMinSketchWithHeapAccumulator {
    pub inner: CountMinSketchWithHeap,
}

// Re-export HeapItem so existing code using CountMinSketchWithHeapAccumulator::HeapItem still works.
pub use asap_sketchlib::CmsHeapItem as HeapItemReexport;

impl CountMinSketchWithHeapAccumulator {
    pub fn new(row_num: usize, col_num: usize, heap_size: usize) -> Self {
        Self {
            inner: CountMinSketchWithHeap::new(row_num, col_num, heap_size),
        }
    }

    pub fn query_key(&self, key: &KeyByLabelValues) -> f64 {
        let key_string = key.labels.join(";");
        self.inner.estimate(&key_string)
    }

    /// Decode a heap-bearing CountSketch FULL msgpack frame
    /// (`{sketch:[matrix,rows,cols], topk_heap, heap_size}`) into a heap
    /// accumulator. This is the window-1 / full-frame base for the
    /// DELTA-HEAP delta path: the backend caches THIS accumulator as the
    /// per-series base so a later `MSGPACK_DELTA` frame applies its sparse
    /// matrix delta onto a heap accumulator (not a plain CountSketch).
    ///
    /// Delegates to the PUBLIC `asap_sketchlib::CountMinSketchWithHeap::
    /// from_msgpack` (both heap-bearing frequency variants share the wire
    /// shape; the CountSketch-with-heap promotion is decided by the ingest
    /// router, not the bytes).
    pub fn from_msgpack_with_heap_bytes(buffer: &[u8]) -> Result<Self, Box<dyn std::error::Error>> {
        Ok(Self {
            inner: CountMinSketchWithHeap::from_msgpack(buffer)
                .map_err(|e| format!("deserialize CountMinSketchWithHeap msgpack: {e}"))?,
        })
    }

    /// Apply a DELTA-HEAP msgpack frame (encoding `MSGPACK_DELTA`) onto this
    /// accumulator IN PLACE, WITHOUT any change to the public
    /// `asap_sketchlib`: the frame is decoded generically with `rmp_serde`
    /// into local serde structs, the sparse signed cell deltas are added to
    /// the stored matrix (read back via the public `sketch_matrix()`), and
    /// the top-k heap is REPLACED with the frame's full heap. The rebuilt
    /// inner is produced via the public `from_legacy_matrix`, which rounds
    /// cells to the i64 storage and re-seeds the heap.
    ///
    /// Under the per-window-reset model (`docs/delta-baseline-contract.md`
    /// §3) the ingest caller resets this accumulator to empty at a window
    /// boundary before applying, so the delta — which is the window's own
    /// matrix against an empty base — reconstructs the window's state.
    pub fn apply_msgpack_heap_delta_bytes(
        &mut self,
        buffer: &[u8],
    ) -> Result<(), Box<dyn std::error::Error>> {
        let frame = HeapDeltaFrame::from_msgpack(buffer)?;

        let rows = self.inner.rows();
        let cols = self.inner.cols();
        let heap_size = self.inner.heap_size;

        // Read the current (post-reset, possibly empty) matrix and apply the
        // sparse signed deltas additively. Cells outside the stored
        // dimensions are skipped defensively (mirrors the plain-CountSketch
        // delta apply).
        let mut matrix = self.inner.sketch_matrix();
        for (r, c, dc) in &frame.cells {
            let (r, c) = (*r as usize, *c as usize);
            if r >= rows || c >= cols {
                continue;
            }
            matrix[r][c] += *dc as f64;
        }

        // Replace the heap with the frame's full heap. `from_legacy_matrix`
        // re-seeds both the matrix and the heap from these inputs.
        let heap: Vec<CmsHeapItem> = frame
            .heap
            .into_iter()
            .map(|(key, value)| CmsHeapItem { key, value })
            .collect();

        self.inner =
            CountMinSketchWithHeap::from_legacy_matrix(matrix, heap, rows, cols, heap_size);
        Ok(())
    }

    /// Reconstruct a heap accumulator STANDALONE from a single DELTA-HEAP
    /// msgpack frame (encoding `MSGPACK_DELTA`), with NO cached per-series
    /// base. Used by the read-side reducer's `FrequencyTopk` path, where —
    /// unlike the ingest accumulator — there is no rolling base to apply
    /// onto: under the per-window-reset contract
    /// (`docs/delta-baseline-contract.md` §3) each window's delta encodes
    /// that window's own state against an EMPTY base, so reconstruction is
    /// "empty(dims) + apply(delta)".
    ///
    /// Reuses the exact ingest-side apply logic: read the (rows, cols,
    /// heap_size) the frame declares, build an empty accumulator of those
    /// dims (equivalent to `reset_to_empty` on a same-shape base), then
    /// fold the frame in via `apply_msgpack_heap_delta_bytes`. No
    /// `asap_sketchlib` change — the frame is decoded generically with
    /// `rmp_serde`.
    pub fn from_msgpack_heap_delta_bytes(
        buffer: &[u8],
    ) -> Result<Self, Box<dyn std::error::Error>> {
        let frame = HeapDeltaFrame::from_msgpack(buffer)?;
        if frame.rows == 0 || frame.cols == 0 {
            return Err(format!(
                "CountSketchWithHeap delta frame has zero dims (rows={}, cols={})",
                frame.rows, frame.cols
            )
            .into());
        }
        let mut acc = Self::new(
            frame.rows as usize,
            frame.cols as usize,
            frame.heap_size as usize,
        );
        acc.apply_msgpack_heap_delta_bytes(buffer)?;
        Ok(acc)
    }

    /// This function seems will never be used anymore. Keep it for possible future use.
    pub fn deserialize_from_json(data: &Value) -> Result<Self, Box<dyn std::error::Error>> {
        let row_num = data["row_num"]
            .as_f64()
            .ok_or("Missing or invalid 'row_num' field")? as usize;
        let col_num = data["col_num"]
            .as_f64()
            .ok_or("Missing or invalid 'col_num' field")? as usize;
        let heap_size = data["heap_size"]
            .as_f64()
            .ok_or("Missing or invalid 'heap_size' field")? as usize;

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

        let topk_heap_data = data["topk_heap"]
            .as_array()
            .ok_or("Missing or invalid 'topk_heap' field")?;

        let mut topk_heap = Vec::new();
        for item in topk_heap_data {
            let key = item["key"]
                .as_str()
                .ok_or("Missing or invalid 'key' in heap item")?
                .to_string();
            let value = item["value"]
                .as_f64()
                .ok_or("Missing or invalid 'value' in heap item")?;
            topk_heap.push(CmsHeapItem { key, value });
        }

        Ok(Self {
            inner: CountMinSketchWithHeap::from_legacy_matrix(
                sketch, topk_heap, row_num, col_num, heap_size,
            ),
        })
    }

    pub fn deserialize_from_bytes(_buffer: &[u8]) -> Result<Self, Box<dyn std::error::Error>> {
        Err("deserialize_from_bytes for CountMinSketchWithHeapAccumulator not implemented".into())
    }

    /// VALUE-WEIGHTED heavy-hitter update (FIX: CountSketch/CMS topk
    /// recall-0). The default ingest path inserts `+1` per occurrence keyed
    /// by the raw `item`, so the heap ranks groups by OCCURRENCE COUNT — the
    /// wrong answer for `topk(k, sum by (label) (metric))`, which asks for
    /// the top groups by SUM OF VALUE. This update adds the sample `value`
    /// (not `+1`) into both the CMS matrix and the top-k heap, keyed by the
    /// GROUP LABEL (e.g. the `host` / `zone` value), so the heap's ranking is
    /// by summed value. Repeated calls for the same `group_label` accumulate,
    /// so after folding a window the heap holds Σvalue per group.
    ///
    /// Delegates to the library's value-weighted `CountMinSketchWithHeap::
    /// update(key, value)` (`sketchlib_cms_heap_update` → `insert_many(key,
    /// round(value))`), which is the "separate update path" the evaluation
    /// plan (Fig 3c) called for.
    pub fn insert_value(&mut self, group_label: &str, value: f64) {
        self.inner.update(group_label, value);
    }

    /// Read the top-`k` GROUPS ranked by summed VALUE (descending), keyed by
    /// the group label. Pairs with [`Self::insert_value`]: the heap built by
    /// value-weighted updates ranks by Σvalue, so this returns the
    /// value-weighted top-k (not the occurrence-count top-k the raw `item`
    /// heap would give). Sorted descending by value; ties broken by key for
    /// determinism; truncated to `k`.
    pub fn topk_by_value(&self, k: usize) -> Vec<(String, f64)> {
        let mut items: Vec<(String, f64)> = self
            .inner
            .topk_heap_items()
            .into_iter()
            .map(|it| (it.key, it.value))
            .collect();
        items.sort_by(|a, b| {
            b.1.partial_cmp(&a.1)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| a.0.cmp(&b.0))
        });
        items.truncate(k);
        items
    }

    /// Get all keys from the top-k heap.
    pub fn get_topk_keys(&self) -> Vec<KeyByLabelValues> {
        self.inner
            .topk_heap_items()
            .iter()
            .map(|item| {
                let labels: Vec<String> = item.key.split(';').map(|s| s.to_string()).collect();
                KeyByLabelValues { labels }
            })
            .collect()
    }
}

impl SerializableToSink for CountMinSketchWithHeapAccumulator {
    fn serialize_to_json(&self) -> Value {
        let heap_items: Vec<Value> = self
            .inner
            .topk_heap_items()
            .iter()
            .map(|item| {
                serde_json::json!({
                    "key": item.key,
                    "value": item.value
                })
            })
            .collect();

        serde_json::json!({
            "row_num": self.inner.rows(),
            "col_num": self.inner.cols(),
            "heap_size": self.inner.heap_size,
            "sketch": self.inner.sketch_matrix(),
            "topk_heap": heap_items
        })
    }

    fn serialize_to_bytes(&self) -> Vec<u8> {
        self.inner.to_msgpack().unwrap_or_default()
    }
}

impl AggregateCore for CountMinSketchWithHeapAccumulator {
    fn clone_boxed_core(&self) -> Box<dyn AggregateCore> {
        Box::new(self.clone())
    }

    fn type_name(&self) -> &'static str {
        "CountMinSketchWithHeapAccumulator"
    }

    /// Per-window base rotation (`docs/delta-baseline-contract.md` §3):
    /// rebuild an empty heap accumulator with the same (rows, cols,
    /// heap_size) so the next window's DELTA-HEAP frame applies onto a clean,
    /// same-shape base. Without this override the trait default is a no-op,
    /// which would let the additive matrix delta accumulate across windows
    /// (over-counting). Mirrors `CountSketchAccumulator::reset_to_empty`.
    fn reset_to_empty(&mut self) {
        self.inner =
            CountMinSketchWithHeap::new(self.inner.rows(), self.inner.cols(), self.inner.heap_size);
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
                "Cannot merge CountMinSketchWithHeapAccumulator with {}",
                other.get_accumulator_type()
            )
            .into());
        }

        let other_cms = other
            .as_any()
            .downcast_ref::<CountMinSketchWithHeapAccumulator>()
            .ok_or("Failed to downcast to CountMinSketchWithHeapAccumulator")?;

        let merged = Self::merge_accumulators(vec![self.clone(), other_cms.clone()])?;
        Ok(Box::new(merged))
    }

    fn get_accumulator_type(&self) -> AggregationType {
        AggregationType::CountMinSketchWithHeap
    }

    fn get_keys(&self) -> Option<Vec<crate::KeyByLabelValues>> {
        Some(self.get_topk_keys())
    }

    fn query_statistic(
        &self,
        statistic: crate::Statistic,
        key: &Option<crate::KeyByLabelValues>,
        query_kwargs: &std::collections::HashMap<String, String>,
    ) -> Result<f64, Box<dyn std::error::Error + Send + Sync>> {
        use crate::MultipleSubpopulationAggregate;
        let key_val = key
            .as_ref()
            .ok_or("Key required for CountMinSketchWithHeapAccumulator")?;
        self.query(statistic, key_val, Some(query_kwargs))
    }
}

impl MultipleSubpopulationAggregate for CountMinSketchWithHeapAccumulator {
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

impl MergeableAccumulator<CountMinSketchWithHeapAccumulator> for CountMinSketchWithHeapAccumulator {
    fn merge_accumulators(
        accumulators: Vec<CountMinSketchWithHeapAccumulator>,
    ) -> Result<CountMinSketchWithHeapAccumulator, Box<dyn std::error::Error + Send + Sync>> {
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
    fn test_count_min_sketch_with_heap_creation() {
        let cms = CountMinSketchWithHeapAccumulator::new(4, 1000, 20);
        assert_eq!(cms.inner.rows(), 4);
        assert_eq!(cms.inner.cols(), 1000);
        assert_eq!(cms.inner.heap_size, 20);
        assert_eq!(cms.inner.topk_heap_items().len(), 0);
    }

    #[test]
    fn test_count_min_sketch_with_heap_query() {
        let cms = CountMinSketchWithHeapAccumulator::new(2, 10, 5);
        let key = KeyByLabelValues::new();
        assert_eq!(cms.query_key(&key), 0.0);

        let multi_trait: &dyn MultipleSubpopulationAggregate = &cms;
        assert_eq!(multi_trait.query(Statistic::Sum, &key, None).unwrap(), 0.0);
    }

    #[test]
    fn test_count_min_sketch_with_heap_merge() {
        // Build controlled state via from_legacy_matrix (works regardless of backend config).
        let sketch1 = vec![
            vec![10.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0],
            vec![0.0, 20.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0],
        ];
        let heap1 = vec![
            CmsHeapItem {
                key: "key1".to_string(),
                value: 100.0,
            },
            CmsHeapItem {
                key: "key2".to_string(),
                value: 50.0,
            },
        ];
        let sketch2 = vec![
            vec![5.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0],
            vec![0.0, 15.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0],
        ];
        let heap2 = vec![
            CmsHeapItem {
                key: "key3".to_string(),
                value: 75.0,
            },
            CmsHeapItem {
                key: "key1".to_string(),
                value: 80.0,
            },
        ];

        let cms1 = CountMinSketchWithHeapAccumulator {
            inner: CountMinSketchWithHeap::from_legacy_matrix(sketch1, heap1, 2, 10, 5),
        };
        let cms2 = CountMinSketchWithHeapAccumulator {
            inner: CountMinSketchWithHeap::from_legacy_matrix(sketch2, heap2, 2, 10, 3),
        };

        let result = CountMinSketchWithHeapAccumulator::merge_accumulators(vec![cms1, cms2]);
        assert!(result.is_ok());
        let merged = result.unwrap();
        assert_eq!(merged.inner.sketch_matrix()[0][0], 15.0);
        assert_eq!(merged.inner.sketch_matrix()[1][1], 35.0);
        assert_eq!(merged.inner.heap_size, 3);
        assert!(merged.inner.topk_heap_items().len() <= 3);
    }

    #[test]
    fn test_count_min_sketch_with_heap_merge_single() {
        let cms = CountMinSketchWithHeapAccumulator::new(2, 3, 5);
        let result = CountMinSketchWithHeapAccumulator::merge_accumulators(vec![cms.clone()]);
        assert!(result.is_ok());
        let merged = result.unwrap();
        assert_eq!(merged.inner.rows(), cms.inner.rows());
        assert_eq!(merged.inner.cols(), cms.inner.cols());
        assert_eq!(merged.inner.heap_size, cms.inner.heap_size);
    }

    #[test]
    fn test_count_min_sketch_with_heap_merge_dimension_mismatch() {
        let cms1 = CountMinSketchWithHeapAccumulator::new(2, 10, 5);
        let cms2 = CountMinSketchWithHeapAccumulator::new(3, 10, 5);
        let result = CountMinSketchWithHeapAccumulator::merge_accumulators(vec![cms1, cms2]);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("dimension"));
    }

    #[test]
    fn test_count_min_sketch_with_heap_as_aggregate_core() {
        let cms = CountMinSketchWithHeapAccumulator::new(2, 3, 5);
        assert_eq!(cms.type_name(), "CountMinSketchWithHeapAccumulator");
    }

    #[test]
    fn test_get_topk_keys() {
        let mut cms = CountMinSketchWithHeapAccumulator::new(2, 3, 5);
        cms.inner.update("label1;label2", 100.0);
        cms.inner.update("label3;label4", 50.0);

        let keys = cms.get_topk_keys();
        assert_eq!(keys.len(), 2);
        // Top-k order can differ between Legacy and Sketchlib backends (heap ordering / estimates).
        let label_sets: std::collections::HashSet<_> =
            keys.iter().map(|k| k.labels.clone()).collect();
        assert!(label_sets.contains(&vec!["label1".to_string(), "label2".to_string()]));
        assert!(label_sets.contains(&vec!["label3".to_string(), "label4".to_string()]));
    }

    #[test]
    fn test_multiple_subpopulation_aggregate() {
        let cms = CountMinSketchWithHeapAccumulator::new(3, 50, 10);
        let key = KeyByLabelValues::new();

        let multi_trait: &dyn MultipleSubpopulationAggregate = &cms;
        let result = multi_trait.query(Statistic::Sum, &key, None).unwrap();
        assert_eq!(result, 0.0);

        let keys = multi_trait.get_keys();
        assert!(keys.is_some());
        assert_eq!(keys.unwrap().len(), 0);
    }

    // ----------------------------------------------------------------
    // DELTA-HEAP wire form (encoding MSGPACK_DELTA): apply a sparse matrix
    // delta + replace the heap, decoded generically (rmp_serde) WITHOUT any
    // asap_sketchlib delta API. The first test feeds a frame produced by the
    // Go encoder (sketchlib-go `MarshalCountSketchWithHeapDelta`) to prove
    // cross-language byte parity — mirrors how the full-heap parity is
    // proven. The second proves PWR full -> delta -> delta reconstruction.
    // ----------------------------------------------------------------

    /// Cross-language byte-parity: this hex is the exact output of
    /// sketchlib-go's `asapmsgpack.MarshalCountSketchWithHeapDelta(5, 1024,
    /// cells=[(0,1,50),(1,3,-4),(4,1023,1_000_000)],
    /// heap=[("/checkout",50),("/cart",20)], heap_size=20)` (captured via a
    /// throw-away Go print test, identical methodology to the full-heap
    /// golden in `sketchlib-go/.../count_sketch_with_heap_test.go`). If the
    /// Go encoder or the rmp_serde layout ever shifts, this decode fails
    /// loudly.
    const GO_DELTA_HEAP_GOLDEN_HEX: &str = "94c39305cd04009393000132930103fc9304cd03ffce000f42409292a92f636865636b6f7574cb404900000000000092a52f63617274cb403400000000000014";

    #[test]
    fn test_apply_go_produced_delta_heap_frame_matrix_and_heap() {
        let bytes = hex::decode(GO_DELTA_HEAP_GOLDEN_HEX).expect("hex");

        // Base = empty heap accumulator with the frame's dims (what the
        // ingest caller holds after the per-window base rotation).
        let mut acc = CountMinSketchWithHeapAccumulator::new(5, 1024, 20);
        acc.apply_msgpack_heap_delta_bytes(&bytes)
            .expect("apply Go delta-heap frame");

        // Matrix: the three sparse cells landed onto the empty base.
        let m = acc.inner.sketch_matrix();
        assert_eq!(m.len(), 5);
        assert_eq!(m[0].len(), 1024);
        assert_eq!(m[0][1], 50.0, "cell (0,1)");
        assert_eq!(m[1][3], -4.0, "cell (1,3)");
        assert_eq!(m[4][1023], 1_000_000.0, "cell (4,1023)");
        // Everything else stays zero.
        assert_eq!(m[2][2], 0.0);
        assert_eq!(m[0][0], 0.0);

        // Heap: the frame's full heap, with /checkout ranked above /cart.
        let mut items = acc.inner.topk_heap_items();
        items.sort_by(|a, b| b.value.partial_cmp(&a.value).unwrap());
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].key, "/checkout");
        assert_eq!(items[0].value, 50.0);
        assert_eq!(items[1].key, "/cart");
        assert_eq!(items[1].value, 20.0);
    }

    #[test]
    fn test_pwr_full_then_delta_then_delta_reconstructs_per_window() {
        use asap_sketchlib::MessagePackCodec;

        // Window 1 (full frame): build a heap-bearing CountSketch with mass
        // and serialize the FULL `{sketch,topk_heap,heap_size}` frame, then
        // decode it into a heap accumulator (the cached per-series base).
        let w1 = CountMinSketchWithHeap::from_legacy_matrix(
            vec![vec![300.0; 4]; 5],
            vec![CmsHeapItem {
                key: "k".into(),
                value: 300.0,
            }],
            5,
            4,
            20,
        );
        let w1_bytes = w1.to_msgpack().expect("w1 full msgpack");
        let mut base = CountMinSketchWithHeapAccumulator::from_msgpack_with_heap_bytes(&w1_bytes)
            .expect("decode w1 full frame as heap accumulator");
        assert_eq!(base.inner.sketch_matrix()[0][0], 300.0);

        // Window 2 delta: this window's own state is matrix cells of value 50
        // against an EMPTY base + heap {k:50}. The DELTA-HEAP frame is encoded
        // the same way the Go producer does (4-array, is_delta, sparse cells).
        let w2_frame = encode_delta_heap(5, 4, &[(0, 0, 50), (1, 1, 50)], &[("k", 50.0)], 20);
        // PWR: rotate base to empty at the window boundary, then apply.
        base.reset_to_empty();
        assert_eq!(
            base.inner.sketch_matrix()[0][0],
            0.0,
            "reset_to_empty cleared matrix"
        );
        base.apply_msgpack_heap_delta_bytes(&w2_frame)
            .expect("apply w2 delta");
        assert_eq!(base.inner.sketch_matrix()[0][0], 50.0, "window-2 cell");
        assert_eq!(base.inner.sketch_matrix()[1][1], 50.0);
        // No cross-window leakage from window 1's 300s.
        assert_eq!(base.inner.sketch_matrix()[2][2], 0.0);
        let h2: Vec<_> = base.inner.topk_heap_items();
        assert_eq!(h2.len(), 1);
        assert_eq!(h2[0].key, "k");
        assert_eq!(h2[0].value, 50.0);

        // Window 3 delta: 80s against empty + heap {k:80}.
        let w3_frame = encode_delta_heap(5, 4, &[(0, 0, 80)], &[("k", 80.0)], 20);
        base.reset_to_empty();
        base.apply_msgpack_heap_delta_bytes(&w3_frame)
            .expect("apply w3 delta");
        assert_eq!(base.inner.sketch_matrix()[0][0], 80.0, "window-3 cell");
        assert_eq!(base.inner.sketch_matrix()[1][1], 0.0, "no window-2 leakage");
        let h3 = base.inner.topk_heap_items();
        assert_eq!(h3.len(), 1);
        assert_eq!(h3[0].value, 80.0);
    }

    #[test]
    fn test_rmp_serde_layout_is_byte_identical_to_go_encoder() {
        // The rmp_serde positional encoding of the delta-heap frame must be
        // BYTE-IDENTICAL to sketchlib-go's hand-rolled
        // `MarshalCountSketchWithHeapDelta`. This hex is the Go encoder's
        // output for (5, 4, cells=[(0,0,50),(1,1,50)], heap=[("k",50)],
        // heap_size=20) — the same inputs `encode_delta_heap` uses below.
        // Equality here proves both encode AND decode are cross-language
        // byte-compatible (the decode path is exercised by the Go-golden
        // test above).
        const GO_PARITY_HEX: &str = "94c39305049293000032930101329192a16bcb404900000000000014";
        let rust_bytes = encode_delta_heap(5, 4, &[(0, 0, 50), (1, 1, 50)], &[("k", 50.0)], 20);
        assert_eq!(hex::encode(&rust_bytes), GO_PARITY_HEX);
    }

    #[test]
    fn test_apply_delta_rejects_full_frame_and_garbage() {
        use asap_sketchlib::MessagePackCodec;
        let mut acc = CountMinSketchWithHeapAccumulator::new(2, 4, 5);
        // A FULL frame (3-array, no is_delta marker) must NOT decode as a
        // delta — the routing relies on the two shapes being distinct.
        let full = CountMinSketchWithHeap::from_legacy_matrix(
            vec![vec![1.0; 4]; 2],
            vec![CmsHeapItem {
                key: "a".into(),
                value: 1.0,
            }],
            2,
            4,
            5,
        )
        .to_msgpack()
        .unwrap();
        assert!(acc.apply_msgpack_heap_delta_bytes(&full).is_err());
        assert!(acc.apply_msgpack_heap_delta_bytes(b"not msgpack").is_err());
    }

    /// Encode a DELTA-HEAP frame the same way sketchlib-go's
    /// `MarshalCountSketchWithHeapDelta` does (rmp_serde positional layout),
    /// so the test exercises the real decode path. Tuple structs serialize
    /// as msgpack fixed arrays — byte-identical to the Go hand-rolled writer.
    fn encode_delta_heap(
        rows: u32,
        cols: u32,
        cells: &[(u32, u32, i64)],
        heap: &[(&str, f64)],
        heap_size: u64,
    ) -> Vec<u8> {
        #[derive(serde::Serialize)]
        struct W<'a>(
            bool,
            (u32, u32, &'a [(u32, u32, i64)]),
            Vec<(String, f64)>,
            u64,
        );
        let heap_owned: Vec<(String, f64)> =
            heap.iter().map(|(k, v)| (k.to_string(), *v)).collect();
        let w = W(true, (rows, cols, cells), heap_owned, heap_size);
        rmp_serde::to_vec(&w).expect("encode delta-heap")
    }

    // ----------------------------------------------------------------
    // FIX 1 — VALUE-WEIGHTED top-k (recall 0 → correct).
    //
    // `topk(k, sum by (host) (cpu_load))` asks for the top-k hosts by
    // SUM OF VALUE. The heavy-hitter heap built by the default `+1`-per-
    // occurrence update ranks by COUNT keyed by `item`, so its recall
    // against the value-weighted ground truth is 0 when the busiest host
    // (most samples) is NOT the heaviest host (largest Σvalue).
    // `insert_value(group_label, value)` adds the sample VALUE keyed by the
    // GROUP LABEL, so `topk_by_value` ranks by Σvalue — correct recall.
    // ----------------------------------------------------------------

    /// Crafted adversarial dataset: the host with the MOST samples
    /// (`h_chatty`, 100 tiny samples) is NOT the host with the largest
    /// value-sum (`h_heavy`, a handful of huge samples). A COUNT-ranked
    /// heap would surface `h_chatty`; the value-weighted top-k must surface
    /// the true heavy hitters by Σvalue, giving recall 1.0 against the
    /// ground-truth top-k-by-value-sum.
    #[test]
    fn value_weighted_topk_has_full_recall_vs_count_topk() {
        // (host, per-sample value, sample count) → true Σvalue:
        //   h_heavy : 1000 × 3   = 3000   (few samples, huge value)
        //   h_mid   : 200  × 5   = 1000
        //   h_small : 50   × 6   = 300
        //   h_chatty: 1    × 100 = 100    (MOST samples, tiny value)
        let data: &[(&str, f64, usize)] = &[
            ("h_heavy", 1000.0, 3),
            ("h_mid", 200.0, 5),
            ("h_small", 50.0, 6),
            ("h_chatty", 1.0, 100),
        ];

        // Wide CMS + heap large enough to hold every group exactly (4 groups)
        // so the estimate equals the true Σvalue with no hash collisions.
        let mut acc = CountMinSketchWithHeapAccumulator::new(5, 4096, 16);
        let mut truth: std::collections::HashMap<&str, f64> = std::collections::HashMap::new();
        for (host, value, count) in data {
            for _ in 0..*count {
                acc.insert_value(host, *value);
            }
            *truth.entry(*host).or_insert(0.0) += value * (*count as f64);
        }

        // Ground-truth top-2 by value-sum: h_heavy (3000), h_mid (1000).
        let mut truth_ranked: Vec<(&str, f64)> = truth.into_iter().collect();
        truth_ranked.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap());
        let truth_top2: std::collections::HashSet<&str> =
            truth_ranked.iter().take(2).map(|(k, _)| *k).collect();
        assert!(
            truth_top2.contains("h_heavy") && truth_top2.contains("h_mid"),
            "ground-truth top-2 by value-sum should be h_heavy + h_mid"
        );

        // Value-weighted top-2 from the heap.
        let got = acc.topk_by_value(2);
        assert_eq!(got.len(), 2, "k=2 → two groups: {got:?}");
        let got_keys: std::collections::HashSet<&str> =
            got.iter().map(|(k, _)| k.as_str()).collect();

        // RECALL = |got ∩ truth| / |truth| must be 1.0.
        let hits = got_keys.intersection(&truth_top2).count();
        let recall = hits as f64 / truth_top2.len() as f64;
        assert_eq!(
            recall, 1.0,
            "value-weighted top-k recall must be 1.0 (count-ranked heap would \
             surface h_chatty and miss h_heavy → recall < 1): got={got:?}"
        );

        // The busiest-by-count host (h_chatty) must NOT be in the top-2,
        // proving we rank by value-sum, not occurrence count.
        assert!(
            !got_keys.contains("h_chatty"),
            "h_chatty (most samples, smallest value-sum) must be excluded: {got:?}"
        );

        // Estimates are exact here (no collisions, heap holds all groups):
        // top-1 must be h_heavy with Σvalue 3000.
        assert_eq!(got[0].0, "h_heavy");
        assert!(
            (got[0].1 - 3000.0).abs() < 1e-6,
            "h_heavy value-sum estimate ≈ 3000, got {}",
            got[0].1
        );
        assert_eq!(got[1].0, "h_mid");
        assert!(
            (got[1].1 - 1000.0).abs() < 1e-6,
            "h_mid value-sum estimate ≈ 1000, got {}",
            got[1].1
        );
    }

    /// A single value-weighted insert must put the full value (not +1) into
    /// the heap, and repeated inserts for the same group must accumulate.
    #[test]
    fn insert_value_accumulates_summed_value_in_heap() {
        let mut acc = CountMinSketchWithHeapAccumulator::new(4, 1024, 8);
        acc.insert_value("g", 10.0);
        acc.insert_value("g", 25.0);
        let top = acc.topk_by_value(1);
        assert_eq!(top.len(), 1);
        assert_eq!(top[0].0, "g");
        assert!(
            (top[0].1 - 35.0).abs() < 1e-6,
            "summed value should be 35 (10+25), got {}",
            top[0].1
        );
    }
}

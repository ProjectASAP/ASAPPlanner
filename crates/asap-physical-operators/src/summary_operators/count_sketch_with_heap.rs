//! Count Sketch with Heap accumulator — wraps
//! `asap_sketchlib::CountSketchWithHeap`.
//!
//! Port of `count_min_sketch_with_heap.rs` for the distinct
//! `CountSketchWithHeap` (median-of-signed-rows estimator) rather than
//! `CountMinSketchWithHeap` (min-over-rows estimator). The two are
//! different sketch algorithms that happen to share a storage shape and
//! wire layout -- see `asap_sketchlib::CountSketchWithHeap`'s own doc and
//! this session's `delta_apply.rs`/`decoders.rs` fix on the read side.
//! Before this file existed, `accumulator_factory.rs`'s raw-metric
//! ingest dispatch built a `CountMinSketchWithHeapAccumulator` (CMS math)
//! for `SketchAlgorithm::CountSketchWithHeap` sids -- the same conflation bug
//! already fixed on the read side, now closed on the write side too.

use crate::{
    AggregateCore, AggregationType, KeyByLabelValues, MergeableAccumulator,
    MultipleSubpopulationAggregate, SerializableToSink,
};
use asap_sketchlib::{CountSketchWithHeap, CsHeapItem, MessagePackCodec};
use serde::Deserialize;
use serde_json::Value;
use std::collections::HashMap;

use crate::Statistic;

/// Local serde view of the DELTA-HEAP wire frame (encoding `MSGPACK_DELTA`).
/// Identical shape to `count_min_sketch_with_heap.rs`'s
/// `HeapDeltaWire`/`MatrixDeltaWire` -- the wire frame is generic (sparse
/// cell deltas + a full heap), not CMS-specific. See that file's doc for
/// the exact rmp_serde positional layout.
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

/// Count Sketch with Heap accumulator — wraps `asap_sketchlib::CountSketchWithHeap`.
/// Core struct, update/merge/serde logic live in
/// `asap_sketchlib::message_pack_format::portable::countsketch_topk`. This
/// file retains QE-specific trait impls, legacy deserializers, and JSON
/// output -- same split as `CountMinSketchWithHeapAccumulator`.
#[derive(Debug, Clone)]
pub struct CountSketchWithHeapAccumulator {
    pub inner: CountSketchWithHeap,
}

impl CountSketchWithHeapAccumulator {
    pub fn new(row_num: usize, col_num: usize, heap_size: usize) -> Self {
        Self {
            inner: CountSketchWithHeap::new(row_num, col_num, heap_size),
        }
    }

    pub fn query_key(&self, key: &KeyByLabelValues) -> f64 {
        let key_string = key.labels.join(";");
        self.inner.estimate(&key_string)
    }

    /// Decode a heap-bearing CountSketch FULL msgpack frame into a heap
    /// accumulator -- the window-1 / full-frame base for the DELTA-HEAP
    /// delta path. Mirrors `CountMinSketchWithHeapAccumulator::from_msgpack_with_heap_bytes`.
    pub fn from_msgpack_with_heap_bytes(buffer: &[u8]) -> Result<Self, Box<dyn std::error::Error>> {
        Ok(Self {
            inner: CountSketchWithHeap::from_msgpack(buffer)
                .map_err(|e| format!("deserialize CountSketchWithHeap msgpack: {e}"))?,
        })
    }

    /// Apply a DELTA-HEAP msgpack frame (encoding `MSGPACK_DELTA`) onto this
    /// accumulator IN PLACE. Mirrors
    /// `CountMinSketchWithHeapAccumulator::apply_msgpack_heap_delta_bytes`
    /// exactly -- the frame decode/apply logic is generic, not tied to
    /// which estimator the rebuilt sketch uses.
    pub fn apply_msgpack_heap_delta_bytes(
        &mut self,
        buffer: &[u8],
    ) -> Result<(), Box<dyn std::error::Error>> {
        let frame = HeapDeltaFrame::from_msgpack(buffer)?;

        let rows = self.inner.rows();
        let cols = self.inner.cols();
        let heap_size = self.inner.heap_size;

        let mut matrix = self.inner.sketch_matrix();
        for (r, c, dc) in &frame.cells {
            let (r, c) = (*r as usize, *c as usize);
            if r >= rows || c >= cols {
                continue;
            }
            matrix[r][c] += *dc as f64;
        }

        let heap: Vec<CsHeapItem> = frame
            .heap
            .into_iter()
            .map(|(key, value)| CsHeapItem { key, value })
            .collect();

        self.inner = CountSketchWithHeap::from_legacy_matrix(matrix, heap, rows, cols, heap_size);
        Ok(())
    }

    /// Reconstruct a heap accumulator STANDALONE from a single DELTA-HEAP
    /// msgpack frame, with no cached per-series base. Mirrors
    /// `CountMinSketchWithHeapAccumulator::from_msgpack_heap_delta_bytes`.
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

    /// Value-weighted heavy-hitter update -- see
    /// `CountMinSketchWithHeapAccumulator::insert_value`'s doc for why
    /// this (not a `+1`-per-occurrence update) is the correct semantics
    /// for `topk(k, sum by (label) (metric))`-shaped queries.
    pub fn insert_value(&mut self, group_label: &str, value: f64) {
        self.inner.update(group_label, value);
    }

    /// Read the top-`k` groups ranked by summed value (descending, tie-broken
    /// by key for determinism). Mirrors `CountMinSketchWithHeapAccumulator::topk_by_value`.
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

impl SerializableToSink for CountSketchWithHeapAccumulator {
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

impl AggregateCore for CountSketchWithHeapAccumulator {
    fn clone_boxed_core(&self) -> Box<dyn AggregateCore> {
        Box::new(self.clone())
    }

    fn type_name(&self) -> &'static str {
        "CountSketchWithHeapAccumulator"
    }

    /// Per-window base rotation -- mirrors
    /// `CountMinSketchWithHeapAccumulator::reset_to_empty`.
    fn reset_to_empty(&mut self) {
        self.inner =
            CountSketchWithHeap::new(self.inner.rows(), self.inner.cols(), self.inner.heap_size);
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
                "Cannot merge CountSketchWithHeapAccumulator with {}",
                other.get_accumulator_type()
            )
            .into());
        }

        let other_cs = other
            .as_any()
            .downcast_ref::<CountSketchWithHeapAccumulator>()
            .ok_or("Failed to downcast to CountSketchWithHeapAccumulator")?;

        let merged = Self::merge_accumulators(vec![self.clone(), other_cs.clone()])?;
        Ok(Box::new(merged))
    }

    fn get_accumulator_type(&self) -> AggregationType {
        AggregationType::CountSketchWithHeap
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
            .ok_or("Key required for CountSketchWithHeapAccumulator")?;
        self.query(statistic, key_val, Some(query_kwargs))
    }
}

impl MultipleSubpopulationAggregate for CountSketchWithHeapAccumulator {
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

impl MergeableAccumulator<CountSketchWithHeapAccumulator> for CountSketchWithHeapAccumulator {
    fn merge_accumulators(
        accumulators: Vec<CountSketchWithHeapAccumulator>,
    ) -> Result<CountSketchWithHeapAccumulator, Box<dyn std::error::Error + Send + Sync>> {
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
    fn test_count_sketch_with_heap_creation() {
        let cs = CountSketchWithHeapAccumulator::new(4, 1000, 20);
        assert_eq!(cs.inner.rows(), 4);
        assert_eq!(cs.inner.cols(), 1000);
        assert_eq!(cs.inner.heap_size, 20);
        assert_eq!(cs.inner.topk_heap_items().len(), 0);
    }

    #[test]
    fn test_count_sketch_with_heap_query() {
        let cs = CountSketchWithHeapAccumulator::new(2, 10, 5);
        let key = KeyByLabelValues::new();
        assert_eq!(cs.query_key(&key), 0.0);

        let multi_trait: &dyn MultipleSubpopulationAggregate = &cs;
        assert_eq!(multi_trait.query(Statistic::Sum, &key, None).unwrap(), 0.0);
    }

    #[test]
    fn test_count_sketch_with_heap_merge() {
        let sketch1 = vec![
            vec![10.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0],
            vec![0.0, 20.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0],
        ];
        let heap1 = vec![
            CsHeapItem {
                key: "key1".to_string(),
                value: 100.0,
            },
            CsHeapItem {
                key: "key2".to_string(),
                value: 50.0,
            },
        ];
        let sketch2 = vec![
            vec![5.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0],
            vec![0.0, 15.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0],
        ];
        let heap2 = vec![
            CsHeapItem {
                key: "key3".to_string(),
                value: 75.0,
            },
            CsHeapItem {
                key: "key1".to_string(),
                value: 80.0,
            },
        ];

        let cs1 = CountSketchWithHeapAccumulator {
            inner: CountSketchWithHeap::from_legacy_matrix(sketch1, heap1, 2, 10, 5),
        };
        let cs2 = CountSketchWithHeapAccumulator {
            inner: CountSketchWithHeap::from_legacy_matrix(sketch2, heap2, 2, 10, 3),
        };

        let result = CountSketchWithHeapAccumulator::merge_accumulators(vec![cs1, cs2]);
        assert!(result.is_ok());
        let merged = result.unwrap();
        assert_eq!(merged.inner.sketch_matrix()[0][0], 15.0);
        assert_eq!(merged.inner.sketch_matrix()[1][1], 35.0);
        assert_eq!(merged.inner.heap_size, 3);
        assert!(merged.inner.topk_heap_items().len() <= 3);
    }

    #[test]
    fn test_count_sketch_with_heap_merge_single() {
        let cs = CountSketchWithHeapAccumulator::new(2, 3, 5);
        let result = CountSketchWithHeapAccumulator::merge_accumulators(vec![cs.clone()]);
        assert!(result.is_ok());
        let merged = result.unwrap();
        assert_eq!(merged.inner.rows(), cs.inner.rows());
        assert_eq!(merged.inner.cols(), cs.inner.cols());
        assert_eq!(merged.inner.heap_size, cs.inner.heap_size);
    }

    #[test]
    fn test_count_sketch_with_heap_merge_dimension_mismatch() {
        let cs1 = CountSketchWithHeapAccumulator::new(2, 10, 5);
        let cs2 = CountSketchWithHeapAccumulator::new(3, 10, 5);
        let result = CountSketchWithHeapAccumulator::merge_accumulators(vec![cs1, cs2]);
        assert!(result.is_err());
    }

    #[test]
    fn test_count_sketch_with_heap_as_aggregate_core() {
        let cs = CountSketchWithHeapAccumulator::new(2, 3, 5);
        assert_eq!(cs.type_name(), "CountSketchWithHeapAccumulator");
    }

    #[test]
    fn test_get_topk_keys() {
        let mut cs = CountSketchWithHeapAccumulator::new(2, 3, 5);
        cs.inner.update("label1;label2", 100.0);
        cs.inner.update("label3;label4", 50.0);

        let keys = cs.get_topk_keys();
        assert_eq!(keys.len(), 2);
        let label_sets: std::collections::HashSet<_> =
            keys.iter().map(|k| k.labels.clone()).collect();
        assert!(label_sets.contains(&vec!["label1".to_string(), "label2".to_string()]));
        assert!(label_sets.contains(&vec!["label3".to_string(), "label4".to_string()]));
    }

    #[test]
    fn test_multiple_subpopulation_aggregate() {
        let cs = CountSketchWithHeapAccumulator::new(3, 50, 10);
        let key = KeyByLabelValues::new();

        let multi_trait: &dyn MultipleSubpopulationAggregate = &cs;
        let result = multi_trait.query(Statistic::Sum, &key, None).unwrap();
        assert_eq!(result, 0.0);

        let keys = multi_trait.get_keys();
        assert!(keys.is_some());
        assert_eq!(keys.unwrap().len(), 0);
    }

    #[test]
    fn test_pwr_full_then_delta_then_delta_reconstructs_per_window() {
        use asap_sketchlib::MessagePackCodec;

        let w1 = CountSketchWithHeap::from_legacy_matrix(
            vec![vec![300.0; 4]; 5],
            vec![CsHeapItem {
                key: "k".into(),
                value: 300.0,
            }],
            5,
            4,
            20,
        );
        let w1_bytes = w1.to_msgpack().expect("w1 full msgpack");
        let mut base = CountSketchWithHeapAccumulator::from_msgpack_with_heap_bytes(&w1_bytes)
            .expect("decode w1 full frame as heap accumulator");
        assert_eq!(base.inner.sketch_matrix()[0][0], 300.0);

        let w2_frame = encode_delta_heap(5, 4, &[(0, 0, 50), (1, 1, 50)], &[("k", 50.0)], 20);
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
        assert_eq!(base.inner.sketch_matrix()[2][2], 0.0);
        let h2: Vec<_> = base.inner.topk_heap_items();
        assert_eq!(h2.len(), 1);
        assert_eq!(h2[0].key, "k");
        assert_eq!(h2[0].value, 50.0);

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
    fn test_apply_delta_rejects_full_frame_and_garbage() {
        use asap_sketchlib::MessagePackCodec;
        let mut acc = CountSketchWithHeapAccumulator::new(2, 4, 5);
        let full = CountSketchWithHeap::from_legacy_matrix(
            vec![vec![1.0; 4]; 2],
            vec![CsHeapItem {
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

    #[test]
    fn insert_value_accumulates_summed_value_in_heap() {
        let mut acc = CountSketchWithHeapAccumulator::new(4, 1024, 8);
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

    /// The core proof this file exists at all: `CountSketchWithHeapAccumulator`
    /// wraps the real, distinct `asap_sketchlib::CountSketchWithHeap` --
    /// not the CMS-family `CountMinSketchWithHeap` a collapsed dispatch
    /// used to substitute (the exact bug this file fixes on the ingest
    /// side, mirroring the already-fixed read side). Two different Rust
    /// types means `merge_with` rejects mixing them at the type-check
    /// level, same as any other mismatched-family merge attempt --
    /// verified directly rather than via a numeric estimate comparison
    /// (asap_sketchlib's own test suite already proves the median vs
    /// min-over-rows divergence at the sketch-math level).
    #[test]
    fn test_rejects_merge_with_cms_family_accumulator() {
        use crate::summary_operators::count_min_sketch_with_heap::CountMinSketchWithHeapAccumulator;

        let cs = CountSketchWithHeapAccumulator::new(4, 64, 10);
        let cms = CountMinSketchWithHeapAccumulator::new(4, 64, 10);
        let result = cs.merge_with(&cms);
        assert!(
            result.is_err(),
            "CountSketchWithHeapAccumulator must not merge with CountMinSketchWithHeapAccumulator \
             -- different algorithms sharing only a storage shape"
        );
    }
}

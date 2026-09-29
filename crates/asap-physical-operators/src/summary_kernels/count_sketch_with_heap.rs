//! CountSketch with a top-k heap over `asap_sketchlib::CountSketchWithHeap`
//! (median-of-signed-rows), distinct from the Count-Min heap variant.

use crate::{AggregateCore, KeyByLabelValues};
use asap_sketchlib::CountSketchWithHeap;

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

impl AggregateCore for CountSketchWithHeapAccumulator {
    fn clone_boxed_core(&self) -> Box<dyn AggregateCore> {
        Box::new(self.clone())
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }

    fn merge_with(
        &self,
        other: &dyn AggregateCore,
    ) -> Result<Box<dyn AggregateCore>, Box<dyn std::error::Error + Send + Sync>> {
        let other_cs = other
            .as_any()
            .downcast_ref::<CountSketchWithHeapAccumulator>()
            .ok_or("Failed to downcast to CountSketchWithHeapAccumulator")?;

        let mut merged = self.clone();
        merged.inner.merge(&other_cs.inner)?;
        Ok(Box::new(merged))
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

    /// CountSketch and Count-Min heap states are distinct families and never merge.
    #[test]
    fn test_rejects_merge_with_cms_family_accumulator() {
        use crate::summary_kernels::count_min_sketch_with_heap::CountMinSketchWithHeapAccumulator;

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

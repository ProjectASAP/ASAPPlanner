use crate::{AggregateCore, KeyByLabelValues};
use asap_sketchlib::CountMinSketchWithHeap;

/// Count-Min Sketch with a top-k heap over `asap_sketchlib::CountMinSketchWithHeap`.
#[derive(Debug, Clone)]
pub struct CountMinSketchWithHeapAccumulator {
    pub inner: CountMinSketchWithHeap,
}

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

impl AggregateCore for CountMinSketchWithHeapAccumulator {
    fn clone_boxed_core(&self) -> Box<dyn AggregateCore> {
        Box::new(self.clone())
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
        let other_cms = other
            .as_any()
            .downcast_ref::<CountMinSketchWithHeapAccumulator>()
            .ok_or("Failed to downcast to CountMinSketchWithHeapAccumulator")?;

        let mut merged = self.clone();
        merged.inner.merge(&other_cms.inner)?;
        Ok(Box::new(merged))
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
    fn test_get_topk_keys() {
        let mut cms = CountMinSketchWithHeapAccumulator::new(2, 3, 5);
        cms.inner.update("label1;label2", 100.0);
        cms.inner.update("label3;label4", 50.0);

        let keys = cms.get_topk_keys();
        assert_eq!(keys.len(), 2);
        // Heap order is not part of the contract; compare as a set.
        let label_sets: std::collections::HashSet<_> =
            keys.iter().map(|k| k.labels.clone()).collect();
        assert!(label_sets.contains(&vec!["label1".to_string(), "label2".to_string()]));
        assert!(label_sets.contains(&vec!["label3".to_string(), "label4".to_string()]));
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

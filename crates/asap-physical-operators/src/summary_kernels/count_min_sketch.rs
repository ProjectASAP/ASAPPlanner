//! Count-Min Sketch frequency summary over `asap_sketchlib::CountMinSketch`.
use crate::{AggregateCore, KernelError, KeyByLabelValues};
use asap_sketchlib::CountMinSketch;

#[derive(Debug, Clone)]
pub struct CountMinSketchAccumulator {
    pub inner: CountMinSketch,
}

impl CountMinSketchAccumulator {
    pub fn new(row_num: usize, col_num: usize) -> Self {
        Self {
            inner: CountMinSketch::new(row_num, col_num),
        }
    }

    /// Estimated frequency of one item.
    pub fn query_key(&self, key: &KeyByLabelValues) -> f64 {
        self.inner.estimate(&key.to_semicolon_str())
    }
}

impl AggregateCore for CountMinSketchAccumulator {
    fn clone_boxed_core(&self) -> Box<dyn AggregateCore> {
        Box::new(self.clone())
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }

    fn merge_with(&self, other: &dyn AggregateCore) -> Result<Box<dyn AggregateCore>, KernelError> {
        let other = other
            .as_any()
            .downcast_ref::<Self>()
            .ok_or("Count-Min Sketch merges only with Count-Min Sketch")?;
        Ok(Box::new(Self {
            inner: CountMinSketch::merge_refs(&[&self.inner, &other.inner])?,
        }))
    }

    fn approx_memory_bytes(&self) -> usize {
        16 * 1024
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Merged point counts add item frequencies and never underestimate.
    #[test]
    fn merged_point_counts_add() {
        let (mut a, mut b) = (
            CountMinSketchAccumulator::new(3, 128),
            CountMinSketchAccumulator::new(3, 128),
        );
        let key = KeyByLabelValues::new_with_labels(vec!["checkout".into()]);
        a.inner.update(&key.to_semicolon_str(), 2.0);
        b.inner.update(&key.to_semicolon_str(), 3.0);
        let merged = a.merge_with(&b).unwrap();
        let merged = merged
            .as_any()
            .downcast_ref::<CountMinSketchAccumulator>()
            .unwrap();
        assert!(merged.query_key(&key) >= 5.0);
    }

    // Merge rejects a different summary family.
    #[test]
    fn rejects_foreign_merge() {
        let cms = CountMinSketchAccumulator::new(3, 128);
        let kll = crate::summary_kernels::DatasketchesKLLAccumulator::new(200);
        assert!(cms.merge_with(&kll).is_err());
    }
}

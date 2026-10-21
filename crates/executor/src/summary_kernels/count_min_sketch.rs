//! Count-Min Sketch frequency summary over `asap_sketchlib::CountMinF64`.
use crate::{AggregateCore, KernelError, KeyByLabelValues};
use asap_sketchlib::{CountMinF64, CountMinSketch, DataInput};
use planner_types::ir::schema::SketchStatistic;

#[derive(Debug, Clone)]
pub struct CountMinSketchAccumulator {
    pub inner: CountMinF64,
}

impl CountMinSketchAccumulator {
    /// Fails for a shape `asap_sketchlib` refuses (a zero dimension).
    pub fn new(rows: usize, cols: usize) -> Result<Self, KernelError> {
        CountMinSketch::check_shape(rows, cols)?;
        Ok(Self {
            inner: CountMinF64::with_dimensions(rows, cols),
        })
    }

    /// Adds `weight` to `item`, whatever its sign.
    pub fn update(&mut self, item: &str, weight: f64) {
        self.inner.update_weighted(&DataInput::Str(item), weight);
    }

    /// Estimated frequency of one item.
    pub fn query_key(&self, key: &KeyByLabelValues) -> f64 {
        self.inner
            .estimate(&DataInput::Str(&key.to_semicolon_str()))
    }
}

impl AggregateCore for CountMinSketchAccumulator {
    fn clone_boxed_core(&self) -> Box<dyn AggregateCore> {
        Box::new(self.clone())
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
    fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
        self
    }

    fn merge_with(&self, other: &dyn AggregateCore) -> Result<Box<dyn AggregateCore>, KernelError> {
        let other = other
            .as_any()
            .downcast_ref::<Self>()
            .ok_or("Count-Min Sketch merges only with Count-Min Sketch")?;
        // sketchlib's merge panics on a shape mismatch.
        if (self.inner.rows(), self.inner.cols()) != (other.inner.rows(), other.inner.cols()) {
            return Err("Count-Min Sketch merges only with the same shape".into());
        }
        let mut merged = self.clone();
        merged.inner.merge(&other.inner);
        Ok(Box::new(merged))
    }

    /// A bare point count reads the total update weight: every Count-Min row
    /// receives each update exactly once, so one row's mass survives collisions.
    fn estimate(&self, query: &SketchStatistic) -> Result<f64, KernelError> {
        match query {
            SketchStatistic::PointCount { value: None, .. } => {
                Ok(self.inner.as_storage().row_slice(0).iter().sum())
            }
            _ => Err(format!("{query:?} is not supported by Count-Min Sketch").into()),
        }
    }

    fn approx_memory_bytes(&self) -> usize {
        self.inner.memory_bytes()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Merged point counts add item frequencies and never underestimate.
    #[test]
    fn merged_point_counts_add() {
        let (mut a, mut b) = (
            CountMinSketchAccumulator::new(3, 128).unwrap(),
            CountMinSketchAccumulator::new(3, 128).unwrap(),
        );
        let key = KeyByLabelValues::new_with_labels(vec!["checkout".into()]);
        a.update(&key.to_semicolon_str(), 2.0);
        b.update(&key.to_semicolon_str(), 3.0);
        let merged = a.merge_with(&b).unwrap();
        let merged = merged
            .as_any()
            .downcast_ref::<CountMinSketchAccumulator>()
            .unwrap();
        assert!(merged.query_key(&key) >= 5.0);
    }

    // The bare count keeps colliding items' weight, adds across merges, and is 0 when empty.
    #[test]
    fn bare_count_reads_total_weight() {
        let bare_count = SketchStatistic::PointCount {
            key: planner_types::ir::scalar::ColumnRef::SampleValue,
            value: None,
        };
        let mut state = CountMinSketchAccumulator::new(2, 1).unwrap();
        state.update("a", 3.0);
        state.update("b", 7.0);
        assert_eq!(state.estimate(&bare_count).unwrap(), 10.0);
        let merged = state.merge_with(&state).unwrap();
        assert_eq!(merged.estimate(&bare_count).unwrap(), 20.0);
        let empty = CountMinSketchAccumulator::new(2, 1).unwrap();
        assert_eq!(empty.estimate(&bare_count).unwrap(), 0.0);
        assert!(state.estimate(&SketchStatistic::Cardinality).is_err());
    }

    // Every weight reaches the counters: a zero adds nothing and a negative
    // one subtracts.
    #[test]
    fn non_positive_weights_are_added() {
        let mut state = CountMinSketchAccumulator::new(3, 128).unwrap();
        let key = KeyByLabelValues::new_with_labels(vec!["checkout".into()]);
        state.update(&key.to_semicolon_str(), 5.0);
        state.update(&key.to_semicolon_str(), 0.0);
        state.update(&key.to_semicolon_str(), -2.0);
        assert_eq!(state.query_key(&key), 3.0);
    }

    // Merge rejects a different summary family.
    #[test]
    fn rejects_foreign_merge() {
        let cms = CountMinSketchAccumulator::new(3, 128).unwrap();
        let kll = crate::summary_kernels::DatasketchesKLLAccumulator::new(200);
        assert!(cms.merge_with(&kll).is_err());
    }

    // States of different rows or columns do not merge.
    #[test]
    fn merge_rejects_a_different_shape() {
        let cms = CountMinSketchAccumulator::new(3, 128).unwrap();
        for (rows, cols) in [(3, 64), (4, 128)] {
            let other = CountMinSketchAccumulator::new(rows, cols).unwrap();
            assert!(cms.merge_with(&other).is_err());
        }
        assert!(cms.merge_with(&cms).is_ok());
    }
}

//! Count-Min Sketch frequency summary over `asap_sketchlib::CountMinSketch`.
use crate::{AggregateCore, KernelError, KeyByLabelValues};
use asap_sketchlib::{DataInput, FastPath, Vector2D};
use planner_types::ir::schema::SketchStatistic;

/// Float64 Count-Min counters, an item keyed by its text.
pub type CountMinSketchF64 = asap_sketchlib::CountMinSketch<Vector2D<f64>, FastPath>;

#[derive(Clone)]
pub struct CountMinSketchAccumulator {
    pub inner: CountMinSketchF64,
}

impl std::fmt::Debug for CountMinSketchAccumulator {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CountMinSketchAccumulator")
            .field("rows", &self.inner.rows())
            .field("cols", &self.inner.cols())
            .finish()
    }
}

impl CountMinSketchAccumulator {
    pub fn new(row_num: usize, col_num: usize) -> Self {
        Self {
            inner: CountMinSketchF64::with_dimensions(row_num, col_num),
        }
    }

    /// Adds `weight` to `item`.
    pub fn update(&mut self, item: &str, weight: f64) {
        self.inner.update_weighted(&DataInput::Str(item), weight);
    }

    /// Estimated frequency of one item.
    pub fn query_key(&self, key: &KeyByLabelValues) -> f64 {
        self.inner.estimate(&DataInput::Str(&key.to_semicolon_str()))
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
            SketchStatistic::PointCount { value: None, .. } => Ok(row_mass(&self.inner)),
            _ => Err(format!("{query:?} is not supported by Count-Min Sketch").into()),
        }
    }

    /// sketchlib's count of the struct and its counters.
    fn approx_memory_bytes(&self) -> usize {
        self.inner.memory_bytes()
    }
}

/// The sum of row 0's counters (rows are stored one after another).
pub(crate) fn row_mass(sketch: &CountMinSketchF64) -> f64 {
    sketch.as_storage().as_slice()[..sketch.cols()].iter().sum()
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
        let mut state = CountMinSketchAccumulator::new(2, 1);
        state.update("a", 3.0);
        state.update("b", 7.0);
        assert_eq!(state.estimate(&bare_count).unwrap(), 10.0);
        let merged = state.merge_with(&state).unwrap();
        assert_eq!(merged.estimate(&bare_count).unwrap(), 20.0);
        let empty = CountMinSketchAccumulator::new(2, 1);
        assert_eq!(empty.estimate(&bare_count).unwrap(), 0.0);
        assert!(state.estimate(&SketchStatistic::Cardinality).is_err());
    }

    // Merge rejects a different summary family.
    #[test]
    fn rejects_foreign_merge() {
        let cms = CountMinSketchAccumulator::new(3, 128);
        let kll = crate::summary_kernels::DatasketchesKLLAccumulator::new(200);
        assert!(cms.merge_with(&kll).is_err());
    }
}

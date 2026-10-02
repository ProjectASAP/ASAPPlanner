//! Count-Min Sketch frequency summary over `asap_sketchlib::CountMinSketch`.
use crate::{AggregateCore, KernelError, KeyByLabelValues};
use asap_sketchlib::CountMinSketch;
use planner_types::post_asap::SketchStatistic;

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
    fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
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

    /// A bare point count reads the total update weight: every Count-Min row
    /// receives each update exactly once, so one row's mass survives collisions.
    fn estimate(&self, query: &SketchStatistic) -> Result<f64, KernelError> {
        match query {
            SketchStatistic::PointCount { value: None, .. } => Ok(row_mass(&self.inner.sketch())),
            _ => Err(format!("{query:?} is not supported by Count-Min Sketch").into()),
        }
    }

    fn approx_memory_bytes(&self) -> usize {
        16 * 1024
    }
}

pub(crate) fn row_mass(matrix: &[Vec<f64>]) -> f64 {
    matrix.first().map_or(0.0, |row| row.iter().sum())
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

    // The bare count keeps colliding items' weight, adds across merges, and is 0 when empty.
    #[test]
    fn bare_count_reads_total_weight() {
        let bare_count = SketchStatistic::PointCount {
            key: planner_types::pre_asap::ColumnRef::SampleValue,
            value: None,
        };
        let mut state = CountMinSketchAccumulator::new(2, 1);
        state.inner.update("a", 3.0);
        state.inner.update("b", 7.0);
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

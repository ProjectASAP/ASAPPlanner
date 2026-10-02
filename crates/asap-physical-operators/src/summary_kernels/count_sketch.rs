//! CountSketch accumulator backed by `asap_sketchlib::CountSketch`.
//!
//! Per-key queries delegate to sketchlib's median-of-signed-rows estimator.
//! Top-k requires the separate heap-bearing accumulator.

use crate::{AggregateCore, KeyByLabelValues};
use asap_sketchlib::CountSketch;

/// Count Sketch accumulator — inner matrix of signed counts.
#[derive(Debug, Clone)]
pub struct CountSketchAccumulator {
    pub inner: CountSketch,
}

impl CountSketchAccumulator {
    pub fn new(row_num: usize, col_num: usize) -> Self {
        Self {
            inner: CountSketch::new(row_num, col_num),
        }
    }

    /// Median-of-signed-rows point estimate for `key`, via
    /// `asap_sketchlib::CountSketch::estimate`.
    pub fn query_key(&self, key: &KeyByLabelValues) -> f64 {
        self.inner.estimate(&key.to_semicolon_str())
    }
}

impl AggregateCore for CountSketchAccumulator {
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
        let other_cs = other
            .as_any()
            .downcast_ref::<CountSketchAccumulator>()
            .ok_or("Failed to downcast to CountSketchAccumulator")?;

        let merged_inner = CountSketch::merge_refs(&[&self.inner, &other_cs.inner])?;
        Ok(Box::new(Self {
            inner: merged_inner,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_query_key_uses_real_sketchlib_estimator() {
        // `query_key` must match sketchlib's estimator and hash specification.
        let mut cs = CountSketchAccumulator::new(4, 1000);
        let key = KeyByLabelValues::new_with_labels(vec!["web".to_string()]);
        cs.inner.update(&key.to_semicolon_str(), 10.0);
        assert_eq!(
            cs.query_key(&key),
            cs.inner.estimate(&key.to_semicolon_str())
        );
    }

    #[test]
    fn test_aggregate_core_merge_matches_matrix_add() {
        let a = CountSketchAccumulator {
            inner: CountSketch::from_legacy_matrix(vec![vec![1.0, -2.0], vec![3.0, -4.0]], 2, 2),
        };
        let b = CountSketchAccumulator {
            inner: CountSketch::from_legacy_matrix(vec![vec![-1.0, 2.0], vec![-3.0, 4.0]], 2, 2),
        };
        let merged_box = a.merge_with(&b).expect("merge ok");
        let merged = merged_box
            .as_any()
            .downcast_ref::<CountSketchAccumulator>()
            .expect("downcast ok");
        let m = merged.inner.sketch();
        assert_eq!(m[0], vec![0.0, 0.0]);
        assert_eq!(m[1], vec![0.0, 0.0]);
    }

    #[test]
    fn test_aggregate_core_merge_wrong_type_rejects() {
        use crate::summary_kernels::count_min_sketch::CountMinSketchAccumulator;
        let cs = CountSketchAccumulator::new(2, 3);
        let cms = CountMinSketchAccumulator::new(2, 3);
        let result = cs.merge_with(&cms);
        assert!(result.is_err());
    }
}

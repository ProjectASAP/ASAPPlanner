//! CountSketch accumulator backed by `asap_sketchlib::CountSketch`.
//!
//! Per-key queries delegate to sketchlib's median-of-signed-rows estimator.
//! Top-k requires the separate heap-bearing accumulator.

use crate::{AggregateCore, KernelError, KeyByLabelValues};
use asap_sketchlib::CountSketch;

/// Count Sketch accumulator — inner matrix of signed counts.
#[derive(Debug, Clone)]
pub struct CountSketchAccumulator {
    pub inner: CountSketch,
    /// Edge sampling probability; see [`super::sampling`]. Private so it stays in (0, 1].
    sample_p: f64,
}

impl CountSketchAccumulator {
    pub fn new(row_num: usize, col_num: usize) -> Self {
        Self {
            inner: CountSketch::new(row_num, col_num),
            sample_p: 1.0,
        }
    }

    /// Adopt a sketch decoded from an edge frame whose updates were sampled
    /// with probability `sample_p` in (0, 1]; `1` means unsampled.
    pub fn from_sketch(sketch: CountSketch, sample_p: f64) -> Result<Self, KernelError> {
        Ok(Self {
            inner: sketch,
            sample_p: super::sampling::checked(sample_p)?,
        })
    }

    /// Edge sampling probability, for deployments that persist this state.
    pub fn sample_p(&self) -> f64 {
        self.sample_p
    }

    /// Median-of-signed-rows point estimate for `key`, via
    /// `asap_sketchlib::CountSketch::estimate`, scaled by `1/p` for a sampled sketch.
    pub fn query_key(&self, key: &KeyByLabelValues) -> f64 {
        self.inner.estimate(&key.to_semicolon_str()) / self.sample_p
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
            sample_p: super::sampling::merged(self.sample_p, other_cs.sample_p)?,
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
            sample_p: 1.0,
        };
        let b = CountSketchAccumulator {
            inner: CountSketch::from_legacy_matrix(vec![vec![-1.0, 2.0], vec![-3.0, 4.0]], 2, 2),
            sample_p: 1.0,
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

    // Count Sketch is linear, so a sampled point frequency scales by 1/p like Count-Min.
    #[test]
    fn sampled_point_count_is_rescaled() {
        let key = KeyByLabelValues::new_with_labels(vec!["web".into()]);
        let mut sketch = CountSketch::new(4, 1000);
        sketch.update(&key.to_semicolon_str(), 10.0);
        let raw = CountSketchAccumulator::from_sketch(sketch.clone(), 1.0).unwrap();
        let sampled = CountSketchAccumulator::from_sketch(sketch.clone(), 0.25).unwrap();
        assert!((sampled.query_key(&key) - raw.query_key(&key) * 4.0).abs() < 1e-9);
        assert!(CountSketchAccumulator::from_sketch(sketch, -0.1).is_err());
    }

    // Merging with an unsampled base keeps p; two sampled probabilities do not merge.
    #[test]
    fn merge_carries_sample_p() {
        let sampled = CountSketchAccumulator::from_sketch(CountSketch::new(2, 3), 0.25).unwrap();
        let merged = CountSketchAccumulator::new(2, 3)
            .merge_with(&sampled)
            .unwrap();
        let merged = merged
            .as_any()
            .downcast_ref::<CountSketchAccumulator>()
            .unwrap();
        assert_eq!(merged.sample_p(), 0.25);
        let other = CountSketchAccumulator::from_sketch(CountSketch::new(2, 3), 0.5).unwrap();
        assert!(sampled.merge_with(&other).is_err());
    }
}

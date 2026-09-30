//! Count-Min Sketch frequency summary over `asap_sketchlib::CountMinSketch`.
use crate::{AggregateCore, KernelError, KeyByLabelValues};
use asap_sketchlib::CountMinSketch;

#[derive(Debug, Clone)]
pub struct CountMinSketchAccumulator {
    pub inner: CountMinSketch,
    /// Edge sampling probability; see [`super::sampling`]. Private so it stays in (0, 1].
    sample_p: f64,
}

impl CountMinSketchAccumulator {
    pub fn new(row_num: usize, col_num: usize) -> Self {
        Self {
            inner: CountMinSketch::new(row_num, col_num),
            sample_p: 1.0,
        }
    }

    /// Adopt a sketch decoded from an edge frame whose updates were sampled
    /// with probability `sample_p` in (0, 1]; `1` means unsampled. Wire
    /// formats that encode "unsampled" as `0` must map it to `1` first.
    pub fn from_sketch(sketch: CountMinSketch, sample_p: f64) -> Result<Self, KernelError> {
        Ok(Self {
            inner: sketch,
            sample_p: super::sampling::checked(sample_p)?,
        })
    }

    /// Edge sampling probability, for deployments that persist this state.
    pub fn sample_p(&self) -> f64 {
        self.sample_p
    }

    /// Record that updates sampled at `sample_p` were applied to `inner` in
    /// place (e.g. an ingest delta), under the same rule as `merge_with`.
    pub fn merge_sample_p(&mut self, sample_p: f64) -> Result<(), KernelError> {
        self.sample_p =
            super::sampling::merged(self.sample_p, super::sampling::checked(sample_p)?)?;
        Ok(())
    }

    /// Total sampled update weight scaled by `1/p`, for unkeyed count readouts.
    /// Each Count-Min row receives every update once, including collisions.
    pub fn total(&self) -> f64 {
        self.inner
            .sketch()
            .first()
            .map_or(0.0, |row| row.iter().sum::<f64>())
            / self.sample_p
    }

    /// Estimated frequency of one item, scaled by `1/p` for a sampled sketch.
    pub fn query_key(&self, key: &KeyByLabelValues) -> f64 {
        self.inner.estimate(&key.to_semicolon_str()) / self.sample_p
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
            sample_p: super::sampling::merged(self.sample_p, other.sample_p)?,
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

    // A sampled Count-Min point frequency scales by 1/p (old kernel: 4x raw at p=0.25).
    #[test]
    fn sampled_point_count_is_rescaled() {
        let key = KeyByLabelValues::new_with_labels(vec!["web".into()]);
        let mut sketch = CountMinSketch::new(4, 1000);
        sketch.update(&key.to_semicolon_str(), 10.0);
        let raw = CountMinSketchAccumulator::from_sketch(sketch.clone(), 1.0).unwrap();
        let sampled = CountMinSketchAccumulator::from_sketch(sketch.clone(), 0.25).unwrap();
        assert!(raw.query_key(&key) >= 10.0);
        assert!((sampled.query_key(&key) - raw.query_key(&key) * 4.0).abs() < 1e-9);
        assert!(CountMinSketchAccumulator::from_sketch(sketch, f64::NAN).is_err());
    }

    // Totals retain all update weight despite collisions and rescale edge sampling.
    #[test]
    fn total_weight_is_scaled_after_merge() {
        let mut sketch = CountMinSketch::new(2, 1);
        sketch.update("a", 3.0);
        sketch.update("b", 7.0);
        let sampled = CountMinSketchAccumulator::from_sketch(sketch, 0.25).unwrap();
        assert_eq!(sampled.total(), 40.0);
        let merged = sampled.merge_with(&sampled).unwrap();
        assert_eq!(
            merged
                .as_any()
                .downcast_ref::<CountMinSketchAccumulator>()
                .unwrap()
                .total(),
            80.0
        );
        assert_eq!(CountMinSketchAccumulator::new(2, 1).total(), 0.0);
    }

    // Merging with an unsampled base keeps p; two sampled probabilities do not merge.
    #[test]
    fn merge_carries_sample_p() {
        let sampled =
            CountMinSketchAccumulator::from_sketch(CountMinSketch::new(2, 3), 0.25).unwrap();
        let merged = sampled
            .merge_with(&CountMinSketchAccumulator::new(2, 3))
            .unwrap();
        let merged = merged
            .as_any()
            .downcast_ref::<CountMinSketchAccumulator>()
            .unwrap();
        assert_eq!(merged.sample_p(), 0.25);
        let other = CountMinSketchAccumulator::from_sketch(CountMinSketch::new(2, 3), 0.5).unwrap();
        assert!(sampled.merge_with(&other).is_err());
    }

    // A sampled merge then readout scales the combined frequency by 1/p.
    #[test]
    fn sampled_merge_then_readout() {
        let key = KeyByLabelValues::new_with_labels(vec!["web".into()]);
        let mut sketch = CountMinSketch::new(4, 1000);
        sketch.update(&key.to_semicolon_str(), 10.0);
        let sampled = CountMinSketchAccumulator::from_sketch(sketch, 0.25).unwrap();
        let mut base = CountMinSketchAccumulator::new(4, 1000);
        base.merge_sample_p(1.0).unwrap();
        let merged = base.merge_with(&sampled).unwrap();
        let merged = merged
            .as_any()
            .downcast_ref::<CountMinSketchAccumulator>()
            .unwrap();
        assert_eq!(merged.query_key(&key), sampled.query_key(&key));
        assert!(merged.query_key(&key) >= 40.0);
    }
}

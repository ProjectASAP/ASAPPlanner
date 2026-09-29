//! KLL quantile summary over `asap_sketchlib::KllSketch`.
use crate::{AggregateCore, KernelError};
use asap_sketchlib::KllSketch;
use planner_types::post_asap::SketchQuery;

#[derive(Clone)]
pub struct DatasketchesKLLAccumulator {
    pub inner: KllSketch,
}

impl DatasketchesKLLAccumulator {
    pub fn new(k: u16) -> Self {
        Self {
            inner: KllSketch::new(k),
        }
    }

    pub fn update(&mut self, value: f64) {
        self.inner.update(value);
    }

    pub fn get_quantile(&self, quantile: f64) -> f64 {
        self.inner.quantile(quantile)
    }
}

impl std::fmt::Debug for DatasketchesKLLAccumulator {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DatasketchesKLLAccumulator")
            .field("k", &self.inner.k)
            .field("sketch_n", &self.inner.count())
            .finish()
    }
}

// SAFETY: `KllSketch` owns its buffers and has no interior mutability; the
// accumulator is only mutated through `&mut self`.
unsafe impl Send for DatasketchesKLLAccumulator {}
unsafe impl Sync for DatasketchesKLLAccumulator {}

impl AggregateCore for DatasketchesKLLAccumulator {
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
            .ok_or("KLL merges only with KLL")?;
        Ok(Box::new(Self {
            inner: KllSketch::merge_refs(&[&self.inner, &other.inner])?,
        }))
    }

    fn estimate(&self, query: &SketchQuery) -> Result<f64, KernelError> {
        match query {
            SketchQuery::Quantile { q } if (0.0..=1.0).contains(q) => Ok(self.get_quantile(*q)),
            SketchQuery::Quantile { .. } => Err("quantile must be in [0, 1]".into()),
            other => Err(format!("KLL does not answer {other:?}").into()),
        }
    }

    fn approx_memory_bytes(&self) -> usize {
        // KLL with default k=200 holds ~2*k items (~3 KiB); round up for overhead.
        4 * 1024
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Merging two KLL states reads like one state built over both inputs.
    #[test]
    fn merged_quantile_matches_single_build() {
        let (mut a, mut b, mut all) = (
            DatasketchesKLLAccumulator::new(200),
            DatasketchesKLLAccumulator::new(200),
            DatasketchesKLLAccumulator::new(200),
        );
        for v in 0..100 {
            a.update(f64::from(v));
            all.update(f64::from(v));
        }
        for v in 100..200 {
            b.update(f64::from(v));
            all.update(f64::from(v));
        }
        let merged = a.merge_with(&b).unwrap();
        let q = SketchQuery::Quantile { q: 0.5 };
        assert_eq!(merged.estimate(&q).unwrap(), all.estimate(&q).unwrap());
    }

    // KLL answers only quantiles in [0, 1].
    #[test]
    fn rejects_unsupported_or_out_of_range_queries() {
        let kll = DatasketchesKLLAccumulator::new(200);
        assert!(kll.estimate(&SketchQuery::Quantile { q: 1.5 }).is_err());
        assert!(kll.estimate(&SketchQuery::Cardinality).is_err());
    }
}

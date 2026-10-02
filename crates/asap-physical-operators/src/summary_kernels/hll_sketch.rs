//! HyperLogLog distinct-count summary over `asap_sketchlib::HllSketch`.
use crate::{AggregateCore, KernelError};
use asap_sketchlib::{HllSketch, HllVariant};
use planner_types::post_asap::SketchStatistic;

#[derive(Debug, Clone)]
pub struct HllSketchAccumulator {
    pub inner: HllSketch,
}

impl HllSketchAccumulator {
    pub fn new(variant: HllVariant, precision: u32) -> Self {
        Self {
            inner: HllSketch::new(variant, precision),
        }
    }
}

impl AggregateCore for HllSketchAccumulator {
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
            .ok_or("HLL merges only with HLL")?;
        Ok(Box::new(Self {
            inner: HllSketch::merge_refs(&[&self.inner, &other.inner])?,
        }))
    }

    /// Distinct count. A bare `PointCount` over an HLL also reads the distinct count.
    fn estimate(&self, query: &SketchStatistic) -> Result<f64, KernelError> {
        match query {
            SketchStatistic::Cardinality | SketchStatistic::PointCount { value: None, .. } => {
                Ok(self.inner.estimate())
            }
            other => Err(format!("HLL does not answer {other:?}").into()),
        }
    }

    fn approx_memory_bytes(&self) -> usize {
        std::mem::size_of::<Self>().saturating_add(self.inner.registers.capacity())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Distinct count after merge counts overlapping items once.
    #[test]
    fn merged_cardinality_deduplicates_overlap() {
        let (mut a, mut b) = (
            HllSketchAccumulator::new(HllVariant::Regular, 12),
            HllSketchAccumulator::new(HllVariant::Regular, 12),
        );
        for v in 0..1000u32 {
            a.inner.update(&v.to_le_bytes());
            b.inner.update(&(v + 500).to_le_bytes());
        }
        let merged = a.merge_with(&b).unwrap();
        let estimate = merged.estimate(&SketchStatistic::Cardinality).unwrap();
        assert!((estimate - 1500.0).abs() / 1500.0 < 0.05, "{estimate}");
    }

    // HLL does not answer quantiles.
    #[test]
    fn rejects_quantile() {
        let hll = HllSketchAccumulator::new(HllVariant::Regular, 12);
        assert!(hll.estimate(&SketchStatistic::Quantile { q: 0.5 }).is_err());
    }
}

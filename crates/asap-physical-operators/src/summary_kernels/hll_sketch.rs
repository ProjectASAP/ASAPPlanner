//! HyperLogLog distinct-count summary over `asap_sketchlib::HllSketch`.
use crate::{AggregateCore, KernelError};
use asap_sketchlib::{HllSketch, HllVariant};
use planner_types::post_asap::SketchQuery;

#[derive(Debug, Clone)]
pub struct HllSketchAccumulator {
    pub inner: HllSketch,
    /// Edge sampling probability; see [`super::sampling`]. Private so it stays in (0, 1].
    sample_p: f64,
}

impl HllSketchAccumulator {
    pub fn new(variant: HllVariant, precision: u32) -> Self {
        Self {
            inner: HllSketch::new(variant, precision),
            sample_p: 1.0,
        }
    }

    /// Adopt a sketch decoded from an edge frame whose distinct keys were sampled
    /// with probability `sample_p` in (0, 1]; `1` means unsampled. Wire
    /// formats that encode "unsampled" as `0` must map it to `1` first.
    pub fn from_sketch(sketch: HllSketch, sample_p: f64) -> Result<Self, KernelError> {
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
            sample_p: super::sampling::merged(self.sample_p, other.sample_p)?,
        }))
    }

    /// Distinct count. A bare `PointCount` over an HLL also reads the distinct
    /// count. Both scale by `1/p`, since each distinct key was admitted with
    /// probability `p`.
    fn estimate(&self, query: &SketchQuery) -> Result<f64, KernelError> {
        match query {
            SketchQuery::Cardinality | SketchQuery::PointCount { value: None, .. } => {
                Ok(self.inner.estimate() / self.sample_p)
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
        let estimate = merged.estimate(&SketchQuery::Cardinality).unwrap();
        assert!((estimate - 1500.0).abs() / 1500.0 < 0.05, "{estimate}");
    }

    // HLL does not answer quantiles.
    #[test]
    fn rejects_quantile() {
        let hll = HllSketchAccumulator::new(HllVariant::Regular, 12);
        assert!(hll.estimate(&SketchQuery::Quantile { q: 0.5 }).is_err());
    }

    fn spread_registers() -> HllSketch {
        let registers = (0..256).map(|i| (i % 7 + 1) as u8).collect();
        HllSketch::from_raw(HllVariant::Regular, 8, registers, 0.0, 0.0, 0.0)
    }

    // Sampling admits each distinct key with probability p, so cardinality and a
    // bare point count scale by 1/p (old kernel: exactly 4x raw at p=0.25).
    #[test]
    fn sampled_cardinality_is_rescaled() {
        let raw = HllSketchAccumulator::from_sketch(spread_registers(), 1.0).unwrap();
        let sampled = HllSketchAccumulator::from_sketch(spread_registers(), 0.25).unwrap();
        let bare_count = SketchQuery::PointCount {
            key: planner_types::pre_asap::ColumnRef::SampleValue,
            value: None,
        };
        for query in [SketchQuery::Cardinality, bare_count] {
            let raw = raw.estimate(&query).unwrap();
            assert!(raw > 0.0);
            assert!((sampled.estimate(&query).unwrap() - raw * 4.0).abs() < 1e-9);
        }
        assert!(HllSketchAccumulator::from_sketch(spread_registers(), 1.5).is_err());
    }

    // Merging with an unsampled base keeps p; two sampled probabilities do not merge.
    #[test]
    fn merge_carries_sample_p() {
        let sampled = HllSketchAccumulator::from_sketch(spread_registers(), 0.25).unwrap();
        let empty = HllSketchAccumulator::new(HllVariant::Regular, 8);
        let merged = empty.merge_with(&sampled).unwrap();
        assert_eq!(
            merged.estimate(&SketchQuery::Cardinality).unwrap(),
            sampled.estimate(&SketchQuery::Cardinality).unwrap()
        );
        let other = HllSketchAccumulator::from_sketch(spread_registers(), 0.5).unwrap();
        assert!(sampled.merge_with(&other).is_err());
    }
}

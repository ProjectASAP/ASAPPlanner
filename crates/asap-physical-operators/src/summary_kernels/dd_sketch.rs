//! DDSketch quantile summary over `asap_sketchlib::DdSketch`.
use crate::{AggregateCore, KernelError};
use asap_sketchlib::DdSketch;
use planner_types::post_asap::SketchQuery;

#[derive(Debug, Clone)]
pub struct DDSketchAccumulator {
    pub inner: DdSketch,
    /// Edge sampling probability; see [`super::sampling`]. Private so it stays in (0, 1].
    sample_p: f64,
}

impl DDSketchAccumulator {
    pub fn new(alpha: f64) -> Self {
        Self {
            inner: DdSketch::new(alpha),
            sample_p: 1.0,
        }
    }

    /// Adopt a sketch decoded from an edge frame whose updates were sampled
    /// with probability `sample_p` in (0, 1]; `1` means unsampled. Wire
    /// formats that encode "unsampled" as `0` must map it to `1` first.
    pub fn from_sketch(sketch: DdSketch, sample_p: f64) -> Result<Self, KernelError> {
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

impl AggregateCore for DDSketchAccumulator {
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
            .ok_or("DDSketch merges only with DDSketch")?;
        Ok(Box::new(Self {
            inner: DdSketch::merge_refs(&[&self.inner, &other.inner])?,
            sample_p: super::sampling::merged(self.sample_p, other.sample_p)?,
        }))
    }

    /// Quantiles, and the total sample count as a bare `PointCount`. Only the
    /// count scales by `1/p`; quantiles are rank statistics.
    fn estimate(&self, query: &SketchQuery) -> Result<f64, KernelError> {
        match query {
            SketchQuery::Quantile { q } if (0.0..=1.0).contains(q) => self
                .inner
                .quantile(*q)
                .ok_or_else(|| "DDSketch quantile of an empty population".into()),
            SketchQuery::Quantile { .. } => Err("quantile must be in [0, 1]".into()),
            SketchQuery::PointCount { value: None, .. } => {
                Ok(self.inner.total_count() as f64 / self.sample_p)
            }
            other => Err(format!("DDSketch does not answer {other:?}").into()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use planner_types::pre_asap::ColumnRef;

    fn bare_count() -> SketchQuery {
        SketchQuery::PointCount {
            key: ColumnRef::SampleValue,
            value: None,
        }
    }

    // A bare point count reads the total sample count, and merge adds counts.
    #[test]
    fn count_and_quantile_survive_merge() {
        let (mut a, mut b) = (
            DDSketchAccumulator::new(0.01),
            DDSketchAccumulator::new(0.01),
        );
        for v in 1..=50 {
            a.inner.update(f64::from(v));
            b.inner.update(f64::from(v + 50));
        }
        let merged = a.merge_with(&b).unwrap();
        assert_eq!(merged.estimate(&bare_count()).unwrap(), 100.0);
        let median = merged.estimate(&SketchQuery::Quantile { q: 0.5 }).unwrap();
        assert!((median - 50.0).abs() <= 1.0, "{median}");
    }

    // An empty DDSketch has no quantile, and unsupported queries are errors.
    #[test]
    fn empty_quantile_and_unsupported_queries_fail() {
        let dd = DDSketchAccumulator::new(0.01);
        assert!(dd.estimate(&SketchQuery::Quantile { q: 0.5 }).is_err());
        assert!(dd.estimate(&SketchQuery::Cardinality).is_err());
    }

    // A sampled DDSketch scales its count by 1/p (old kernel: raw 10 at p=0.1 reads 100).
    #[test]
    fn sampled_count_is_rescaled() {
        let dd =
            DDSketchAccumulator::from_sketch(DdSketch::from_raw(0.01, vec![1, 2, 3, 4], -2), 0.1)
                .unwrap();
        assert_eq!(dd.sample_p(), 0.1);
        assert!((dd.estimate(&bare_count()).unwrap() - 100.0).abs() < 1e-9);
    }

    // Quantiles are rank statistics and do not depend on p.
    #[test]
    fn sampled_quantile_is_unscaled() {
        let raw = || DdSketch::from_raw(0.01, vec![1, 2, 3, 4], -2);
        let q = SketchQuery::Quantile { q: 0.5 };
        let unsampled = DDSketchAccumulator::from_sketch(raw(), 1.0).unwrap();
        let sampled = DDSketchAccumulator::from_sketch(raw(), 0.1).unwrap();
        assert_eq!(
            unsampled.estimate(&q).unwrap(),
            sampled.estimate(&q).unwrap()
        );
        assert!(DDSketchAccumulator::from_sketch(raw(), 0.0).is_err());
    }

    // Merging with an unsampled base keeps p; two sampled probabilities do not merge.
    #[test]
    fn merge_carries_sample_p() {
        let sampled =
            DDSketchAccumulator::from_sketch(DdSketch::from_raw(0.01, vec![1, 1], 0), 0.25)
                .unwrap();
        let merged = DDSketchAccumulator::new(0.01).merge_with(&sampled).unwrap();
        assert_eq!(merged.estimate(&bare_count()).unwrap(), 8.0);
        let other = DDSketchAccumulator::from_sketch(DdSketch::new(0.01), 0.5).unwrap();
        assert!(sampled.merge_with(&other).is_err());
    }

    // A sampled delta applied in place to an unsampled base scales its readout by 1/p.
    #[test]
    fn in_place_delta_records_sample_p() {
        let mut base = DDSketchAccumulator::new(0.01);
        base.inner.update(1.0);
        base.merge_sample_p(0.5).unwrap();
        assert_eq!(base.estimate(&bare_count()).unwrap(), 2.0);
        assert!(base.merge_sample_p(0.25).is_err());
        assert!(base.merge_sample_p(0.0).is_err());
    }
}

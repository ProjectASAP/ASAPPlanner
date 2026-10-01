//! DDSketch quantile summary over `asap_sketchlib::DdSketch`.
use crate::{AggregateCore, KernelError};
use asap_sketchlib::DdSketch;
use planner_types::post_asap::SketchQuery;

#[derive(Debug, Clone)]
pub struct DDSketchAccumulator {
    pub inner: DdSketch,
}

impl DDSketchAccumulator {
    pub fn new(alpha: f64) -> Self {
        Self {
            inner: DdSketch::new(alpha),
        }
    }
}

impl AggregateCore for DDSketchAccumulator {
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
            .ok_or("DDSketch merges only with DDSketch")?;
        Ok(Box::new(Self {
            inner: DdSketch::merge_refs(&[&self.inner, &other.inner])?,
        }))
    }

    /// Quantiles, and the total sample count as a bare `PointCount`.
    fn estimate(&self, query: &SketchQuery) -> Result<f64, KernelError> {
        match query {
            SketchQuery::Quantile { q } if (0.0..=1.0).contains(q) => self
                .inner
                .quantile(*q)
                .ok_or_else(|| "DDSketch quantile of an empty population".into()),
            SketchQuery::Quantile { .. } => Err("quantile must be in [0, 1]".into()),
            SketchQuery::PointCount { value: None, .. } => Ok(self.inner.total_count() as f64),
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
}

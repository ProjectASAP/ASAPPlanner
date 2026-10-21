//! DDSketch quantile summary over `asap_sketchlib::DdSketch`.
use crate::{AggregateCore, KernelError};
use asap_sketchlib::DdSketch;
use planner_types::ir::schema::SketchStatistic;

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
    fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
        self
    }

    fn merge_with(&self, other: &dyn AggregateCore) -> Result<Box<dyn AggregateCore>, KernelError> {
        let other = other
            .as_any()
            .downcast_ref::<Self>()
            .ok_or("DDSketch merges only with DDSketch")?;
        let mut merged = self.clone();
        merged.inner.merge(&other.inner)?;
        Ok(Box::new(merged))
    }

    fn is_empty(&self) -> bool {
        self.inner.get_count() == 0
    }

    /// Quantiles, and the total sample count as a bare `PointCount`.
    fn estimate(&self, query: &SketchStatistic) -> Result<f64, KernelError> {
        match query {
            SketchStatistic::Quantile { q } if (0.0..=1.0).contains(q) => self
                .inner
                .get_value_at_quantile(*q)
                .ok_or_else(|| "DDSketch quantile of an empty population".into()),
            SketchStatistic::Quantile { .. } => Err("quantile must be in [0, 1]".into()),
            SketchStatistic::PointCount { value: None, .. } => Ok(self.inner.get_count() as f64),
            other => Err(format!("DDSketch does not answer {other:?}").into()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use planner_types::ir::scalar::ColumnRef;

    fn bare_count() -> SketchStatistic {
        SketchStatistic::PointCount {
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
            a.inner.add(&f64::from(v));
            b.inner.add(&f64::from(v + 50));
        }
        let merged = a.merge_with(&b).unwrap();
        assert_eq!(merged.estimate(&bare_count()).unwrap(), 100.0);
        let median = merged
            .estimate(&SketchStatistic::Quantile { q: 0.5 })
            .unwrap();
        assert!((median - 50.0).abs() <= 1.0, "{median}");
    }

    // An empty DDSketch has no quantile, and unsupported queries are errors.
    #[test]
    fn empty_quantile_and_unsupported_queries_fail() {
        let dd = DDSketchAccumulator::new(0.01);
        assert!(dd.estimate(&SketchStatistic::Quantile { q: 0.5 }).is_err());
        assert!(dd.estimate(&SketchStatistic::Cardinality).is_err());
    }

    // The quantile at q reads the value of rank ⌈q·n⌉; q = 0 and q = 1 read
    // the exact minimum and maximum.
    #[test]
    fn quantile_reads_rank_ceil_q_n() {
        let mut dd = DDSketchAccumulator::new(0.01);
        for v in [1.0, 2.0, 3.0, 4.0] {
            dd.inner.add(&v);
        }
        let at = |q| dd.estimate(&SketchStatistic::Quantile { q }).unwrap();
        assert_eq!(at(0.0), 1.0);
        assert!((at(0.5) - 2.0).abs() <= 0.01 * 2.0, "{}", at(0.5));
        assert_eq!(at(0.9), 4.0);
        assert_eq!(at(1.0), 4.0);
    }
}

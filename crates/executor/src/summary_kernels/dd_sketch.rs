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

    /// Quantiles.
    fn estimate(&self, query: &SketchStatistic) -> Result<f64, KernelError> {
        match query {
            // PromQL `quantile_over_time` and SQL `percentile_cont`
            // interpolate between the order statistics at ranks ⌊q (n − 1)⌋
            // and ⌈q (n − 1)⌉. The Planner never sends a q outside [0, 1]
            // (Pass 1 prunes the sketch candidate,
            // `QuantileOutsideUnitInterval`; the exact plan answers ±Inf),
            // so sketchlib's error for one, NaN included, is passed on.
            SketchStatistic::Quantile { q } => self
                .inner
                .quantile_interpolated(*q)?
                .ok_or_else(|| "DDSketch quantile of an empty population".into()),
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

    // A merged state's quantile is the union's.
    #[test]
    fn quantile_survives_merge() {
        let (mut a, mut b) = (
            DDSketchAccumulator::new(0.01),
            DDSketchAccumulator::new(0.01),
        );
        for v in 1..=50 {
            a.inner.add(&f64::from(v));
            b.inner.add(&f64::from(v + 50));
        }
        let merged = a.merge_with(&b).unwrap();
        let median = merged
            .estimate(&SketchStatistic::Quantile { q: 0.5 })
            .unwrap();
        assert!((median - 50.0).abs() <= 1.0, "{median}");
    }

    // An empty DDSketch has no quantile, and unsupported queries (its
    // total among them) are errors.
    #[test]
    fn empty_quantile_and_unsupported_queries_fail() {
        let dd = DDSketchAccumulator::new(0.01);
        assert!(dd.estimate(&SketchStatistic::Quantile { q: 0.5 }).is_err());
        assert!(dd.estimate(&SketchStatistic::Cardinality).is_err());
        assert!(dd.estimate(&bare_count()).is_err());
    }

    // The quantile at q interpolates linearly between the order statistics
    // at ranks ⌊q (n − 1)⌋ and ⌈q (n − 1)⌉, as PromQL `quantile_over_time`
    // and SQL `percentile_cont` do: [20, 40] at q = 0.9 is 38 (nearest rank
    // read 40), within α of the interpolated value; q = 0 and q = 1 read the
    // exact minimum and maximum.
    #[test]
    fn quantile_interpolates_between_adjacent_order_statistics() {
        let alpha = 0.01;
        let mut dd = DDSketchAccumulator::new(alpha);
        for v in [20.0, 40.0] {
            dd.inner.add(&v);
        }
        let at = |q| dd.estimate(&SketchStatistic::Quantile { q }).unwrap();
        assert!((at(0.9) - 38.0).abs() <= alpha * 38.0, "{}", at(0.9));
        assert_eq!(at(0.0), 20.0);
        assert_eq!(at(1.0), 40.0);
        let mut four = DDSketchAccumulator::new(alpha);
        for v in [1.0, 2.0, 3.0, 4.0] {
            four.inner.add(&v);
        }
        // Rank 0.5 · 3 = 1.5: halfway between 2 and 3.
        let median = four
            .estimate(&SketchStatistic::Quantile { q: 0.5 })
            .unwrap();
        assert!((median - 2.5).abs() <= alpha * 2.5, "{median}");
    }

    // The Planner never sends a φ outside [0, 1] (Pass 1 prunes the sketch
    // candidate, `QuantileOutsideUnitInterval`, and the exact plan answers
    // ±Inf), so the kernel refuses one; NaN likewise.
    #[test]
    fn quantile_outside_the_unit_interval_is_an_error() {
        let mut dd = DDSketchAccumulator::new(0.01);
        dd.inner.add(&1.0);
        for q in [-0.1, 1.1, f64::NAN] {
            assert!(
                dd.estimate(&SketchStatistic::Quantile { q }).is_err(),
                "{q}"
            );
        }
    }
}

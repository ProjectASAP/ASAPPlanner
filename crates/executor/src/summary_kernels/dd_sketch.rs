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

    /// Adds one sample; a non-finite or out-of-range value is ignored.
    pub fn update(&mut self, value: f64) {
        self.inner.add(&value);
    }

    /// The lower order statistic at `q`: the bucket representative
    /// `γ^i (1 + α)` of the sample at 0-based rank `⌊q (n − 1)⌋`; `None` when
    /// empty.
    pub fn quantile(&self, q: f64) -> Option<f64> {
        let count = self.inner.get_count();
        if count == 0 {
            return None;
        }
        self.value_at_rank((q * count.saturating_sub(1) as f64).floor() as u64)
    }

    /// The linear interpolation at rank `q (n − 1)` between the two
    /// adjacent order statistics' representatives, as PromQL's `quantile`
    /// interpolates; `None` for `q` outside `[0, 1]`, an empty sketch or a
    /// count past exact Float64 integers. Across opposite signs it need not
    /// keep α relative error.
    pub fn quantile_interpolated(&self, q: f64) -> Option<f64> {
        let count = self.inner.get_count();
        if !(0.0..=1.0).contains(&q) || count == 0 || count > (1u64 << 53) {
            return None;
        }
        let rank = q * (count - 1) as f64;
        let lower = self.value_at_rank(rank.floor() as u64)?;
        let upper = self.value_at_rank(rank.ceil() as u64)?;
        let fraction = rank - rank.floor();
        let result = lower * (1.0 - fraction) + upper * fraction;
        result.is_finite().then_some(result)
    }

    /// The bucket representative of the sample at 0-based rank `rank`,
    /// signed, from the stores sketchlib exposes. sketchlib's own
    /// `get_value_at_quantile` takes the nearest rank `⌈q n⌉` clamped to the
    /// sample range, a different answer this read keeps the executor from
    /// changing.
    pub fn value_at_rank(&self, mut rank: u64) -> Option<f64> {
        let sketch = &self.inner;
        let gamma = (1.0 + sketch.alpha()) / (1.0 - sketch.alpha());
        let representative = |offset: i32, i: usize| {
            gamma.powf((i64::from(offset) + i as i64) as f64) * (1.0 + sketch.alpha())
        };
        let negative = sketch.negative_store_counts();
        for (i, &count) in negative.iter().enumerate().rev() {
            if rank < count {
                return Some(-representative(sketch.negative_store_offset(), i));
            }
            rank -= count;
        }
        if rank < sketch.zero_count() {
            return Some(0.0);
        }
        rank -= sketch.zero_count();
        let positive = sketch.positive_store_counts();
        let mut seen = 0u64;
        for (i, &count) in positive.iter().enumerate() {
            seen = seen.saturating_add(count);
            if seen > rank {
                return Some(representative(sketch.positive_store_offset(), i));
            }
        }
        // Past the last sample: the highest non-empty bucket's representative.
        let last = positive.iter().rposition(|&count| count > 0)?;
        Some(representative(sketch.positive_store_offset(), last))
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
                .quantile(*q)
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
            a.update(f64::from(v));
            b.update(f64::from(v + 50));
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
}

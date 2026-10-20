//! KLL quantile summary over `asap_sketchlib::Kll`.
use crate::{AggregateCore, KernelError};
use asap_sketchlib::Kll;
use planner_types::ir::schema::SketchStatistic;

#[derive(Clone)]
pub struct DatasketchesKLLAccumulator {
    pub inner: Kll,
}

impl DatasketchesKLLAccumulator {
    pub fn new(k: u16) -> Self {
        Self {
            inner: Kll::with_k(i32::from(k)),
        }
    }

    pub fn update(&mut self, value: f64) {
        self.inner.update(&value);
    }

    /// The value at `quantile`; 0 for an empty sketch.
    pub fn get_quantile(&self, quantile: f64) -> f64 {
        if self.is_empty() {
            return 0.0;
        }
        self.inner.quantile(quantile)
    }
}

impl std::fmt::Debug for DatasketchesKLLAccumulator {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DatasketchesKLLAccumulator")
            .field("k", &self.inner.k())
            .field("sketch_n", &self.inner.count())
            .finish()
    }
}

impl AggregateCore for DatasketchesKLLAccumulator {
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
            .ok_or("KLL merges only with KLL")?;
        if self.inner.k() != other.inner.k() {
            return Err(format!(
                "KLL merges only with the same k ({} vs {})",
                self.inner.k(),
                other.inner.k()
            )
            .into());
        }
        let mut merged = self.clone();
        merged.inner.merge(&other.inner);
        Ok(Box::new(merged))
    }

    fn estimate(&self, query: &SketchStatistic) -> Result<f64, KernelError> {
        match query {
            SketchStatistic::Quantile { .. } if self.is_empty() => {
                Err("KLL quantile of an empty population".into())
            }
            SketchStatistic::Quantile { q } if (0.0..=1.0).contains(q) => Ok(self.get_quantile(*q)),
            SketchStatistic::Quantile { .. } => Err("quantile must be in [0, 1]".into()),
            other => Err(format!("KLL does not answer {other:?}").into()),
        }
    }

    fn is_empty(&self) -> bool {
        self.inner.count() == 0
    }

    fn approx_memory_bytes(&self) -> usize {
        std::mem::size_of::<Self>().saturating_add(self.inner.memory_bytes())
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
        let q = SketchStatistic::Quantile { q: 0.5 };
        assert_eq!(merged.estimate(&q).unwrap(), all.estimate(&q).unwrap());
    }

    // KLL answers only quantiles in [0, 1].
    #[test]
    fn rejects_unsupported_or_out_of_range_queries() {
        let kll = DatasketchesKLLAccumulator::new(200);
        assert!(kll.estimate(&SketchStatistic::Quantile { q: 1.5 }).is_err());
        assert!(kll.estimate(&SketchStatistic::Cardinality).is_err());
    }

    // States of different k do not merge.
    #[test]
    fn merge_rejects_a_different_k() {
        let kll = DatasketchesKLLAccumulator::new(200);
        assert!(kll
            .merge_with(&DatasketchesKLLAccumulator::new(100))
            .is_err());
        assert!(kll
            .merge_with(&DatasketchesKLLAccumulator::new(200))
            .is_ok());
    }
}

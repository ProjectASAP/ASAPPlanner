//! KLL quantile summary over `asap_sketchlib::Kll`.
use crate::summary_kernels::traits::{aggregate_core_object_methods, downcast_peer};
use crate::{AggregateCore, KernelError};
use asap_sketchlib::Kll;
use planner_types::summary_type::SketchStatistic;

#[derive(Clone)]
pub struct DatasketchesKLLAccumulator {
    pub(crate) inner: Kll,
}

impl DatasketchesKLLAccumulator {
    pub fn new(k: u16) -> Self {
        Self {
            inner: Kll::with_k(i32::from(k)),
        }
    }

    /// The `asap_sketchlib` sketch, so a deployment can persist it with the
    /// library's codec.
    pub fn sketch(&self) -> &Kll {
        &self.inner
    }

    /// The accumulator restored from a sketch that was read from an
    /// accumulator with [`Self::sketch`] and persisted (for example a
    /// materialized view written to disk).
    pub fn restore_from_disk(mut sketch: Kll) -> Result<Self, KernelError> {
        // A stored sketch takes no more samples: keep just what it answers from.
        sketch.shrink_to_fit();
        Ok(Self { inner: sketch })
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
    aggregate_core_object_methods!();

    fn merge_with(&self, other: &dyn AggregateCore) -> Result<Box<dyn AggregateCore>, KernelError> {
        let other = downcast_peer::<Self>(other, "KLL merges only with KLL")?;
        // sketchlib's merge panics on a different k or m.
        let (k, m) = (self.inner.k(), self.inner.m());
        if (k, m) != (other.inner.k(), other.inner.m()) {
            return Err(format!(
                "KLL merges only with the same k and m ({k}, {m} vs {}, {})",
                other.inner.k(),
                other.inner.m()
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

    /// sketchlib's count of what the sketch holds now (its struct
    /// included); the item buffer grows with the stream.
    fn approx_memory_bytes(&self) -> usize {
        self.inner.memory_bytes()
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

    // The charge is sketchlib's count of what the sketch holds, the struct
    // counted once: it starts small and grows with the stream.
    #[test]
    fn memory_charge_is_the_library_count() {
        let mut kll = DatasketchesKLLAccumulator::new(200);
        assert_eq!(kll.approx_memory_bytes(), kll.inner.memory_bytes());
        let empty = kll.approx_memory_bytes();
        for v in 0..10_000 {
            kll.update(f64::from(v));
        }
        assert_eq!(kll.approx_memory_bytes(), kll.inner.memory_bytes());
        assert!(kll.approx_memory_bytes() > empty);
    }

    // States of different k or m do not merge: an error, not sketchlib's panic.
    #[test]
    fn merge_rejects_a_different_k_or_m() {
        let kll = DatasketchesKLLAccumulator::new(200);
        assert!(kll
            .merge_with(&DatasketchesKLLAccumulator::new(100))
            .is_err());
        assert!(kll
            .merge_with(&DatasketchesKLLAccumulator {
                inner: Kll::new(200, 4),
            })
            .is_err());
        assert!(kll
            .merge_with(&DatasketchesKLLAccumulator::new(200))
            .is_ok());
    }
}

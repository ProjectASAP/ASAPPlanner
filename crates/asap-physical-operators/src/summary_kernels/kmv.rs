//! KMV (k minimum values) distinct-count summary over `asap_sketchlib::KMV`.
//!
//! Every item is hashed with the library's shared `CANONICAL_HASH_SEED`, and
//! a merge keeps the `k` smallest distinct hashes of both states, so merging
//! two states equals building one over the union of their inputs.
use crate::summary_kernels::factory::AccumulatorUpdater;
use crate::{AggregateCore, KernelError, KeyByLabelValues};
use asap_sketchlib::{DataInput, KMV};
use planner_types::post_asap::SketchStatistic;

/// The smallest `k` the estimate `(k - 1) / U(k)` is defined for.
pub const KMV_MIN_K: u32 = 2;

#[derive(Debug, Clone)]
pub struct KmvAccumulator {
    pub inner: KMV,
}

impl KmvAccumulator {
    pub fn new(k: u32) -> Self {
        Self {
            inner: KMV::new(k as usize),
        }
    }

    /// Observe one sample value. Like HLL, a value is hashed by its bits
    /// with `-0.0` folded into `0.0`; NaN is not an item.
    pub fn insert_sample(&mut self, value: f64) {
        if !value.is_nan() {
            let bits = if value == 0.0 { 0 } else { value.to_bits() };
            self.inner.insert(&DataInput::Bytes(&bits.to_le_bytes()));
        }
    }
}

impl AggregateCore for KmvAccumulator {
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
            .ok_or("KMV merges only with KMV")?;
        if self.inner.k != other.inner.k {
            return Err(format!(
                "KMV merges only with the same k ({} vs {})",
                self.inner.k, other.inner.k
            )
            .into());
        }
        let mut merged = self.clone();
        merged.inner.merge(&mut other.inner.clone());
        Ok(Box::new(merged))
    }

    /// Distinct count. A bare `PointCount` over a KMV also reads the distinct count.
    fn estimate(&self, query: &SketchStatistic) -> Result<f64, KernelError> {
        match query {
            // `KMV::estimate` takes `&mut self` without mutating.
            SketchStatistic::Cardinality | SketchStatistic::PointCount { value: None, .. } => {
                Ok(self.inner.clone().estimate())
            }
            other => Err(format!("KMV does not answer {other:?}").into()),
        }
    }

    fn approx_memory_bytes(&self) -> usize {
        // sketchlib reserves all k hash slots at construction.
        std::mem::size_of::<Self>()
            .saturating_add(self.inner.k.saturating_mul(std::mem::size_of::<u64>()))
    }
}

/// A KMV state is its own updater: it observes unkeyed sample values.
impl AccumulatorUpdater for KmvAccumulator {
    fn is_keyed(&self) -> bool {
        false
    }
    fn memory_usage_bytes(&self) -> usize {
        self.approx_memory_bytes()
    }
    fn update_single(&mut self, value: f64, _: i64) {
        self.insert_sample(value);
    }
    fn update_keyed(&mut self, _: &KeyByLabelValues, value: f64, _: i64) {
        self.insert_sample(value);
    }
    fn take_accumulator(&mut self) -> Box<dyn AggregateCore> {
        let result = Box::new(self.clone());
        self.reset();
        result
    }
    fn snapshot_accumulator(&self) -> Box<dyn AggregateCore> {
        Box::new(self.clone())
    }
    fn into_accumulator(self: Box<Self>) -> Box<dyn AggregateCore> {
        self
    }
    fn reset(&mut self) {
        self.inner = KMV::new(self.inner.k);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn build(k: u32, values: impl IntoIterator<Item = u32>) -> KmvAccumulator {
        let mut kmv = KmvAccumulator::new(k);
        for v in values {
            kmv.insert_sample(f64::from(v));
        }
        kmv
    }

    fn cardinality(state: &dyn AggregateCore) -> f64 {
        state.estimate(&SketchStatistic::Cardinality).unwrap()
    }

    // Below k distinct items the k smallest hashes are all the items: exact.
    #[test]
    fn counts_fewer_than_k_distinct_items_exactly() {
        let kmv = build(1024, (0..500).chain(0..500));
        assert_eq!(cardinality(&kmv), 500.0);
        assert_eq!(
            kmv.estimate(&SketchStatistic::PointCount {
                key: planner_types::pre_asap::ColumnRef::SampleValue,
                value: None,
            })
            .unwrap(),
            500.0
        );
    }

    // Above k the estimate is within KMV's relative standard error
    // 1/sqrt(k - 2) times ten (the 99% Chebyshev bound Stage 1 sizes for).
    #[test]
    fn estimates_many_distinct_items_within_its_error() {
        let k = 1024;
        let bound = 10.0 / f64::from(k - 2).sqrt();
        for exact in [10_000u32, 100_000] {
            let estimate = cardinality(&build(k, (0..exact).chain(0..exact / 2)));
            let error = (estimate - f64::from(exact)).abs() / f64::from(exact);
            assert!(error < bound, "{exact}: {estimate} (error {error})");
        }
    }

    // Merging two overlapping states keeps the same k smallest distinct
    // hashes as building one over the union, so both estimate alike.
    #[test]
    fn merge_equals_the_union_build() {
        let k = 256;
        let (a, b) = (build(k, 0..6_000), build(k, 4_000..10_000));
        let union = build(k, 0..10_000);
        let merged = a.merge_with(&b).unwrap();
        let merged = merged.as_any().downcast_ref::<KmvAccumulator>().unwrap();
        let sorted = |s: &KmvAccumulator| {
            let mut hashes: Vec<u64> = s.inner.k_vals.iter().copied().collect();
            hashes.sort_unstable();
            hashes
        };
        assert_eq!(sorted(merged), sorted(&union));
        assert_eq!(cardinality(merged), cardinality(&union));
        // The inputs are unchanged.
        assert_eq!(sorted(&a), sorted(&build(k, 0..6_000)));
    }

    #[test]
    fn rejects_merge_with_a_different_k() {
        assert!(build(16, 0..10).merge_with(&build(32, 0..10)).is_err());
    }

    #[test]
    fn rejects_quantile() {
        assert!(KmvAccumulator::new(16)
            .estimate(&SketchStatistic::Quantile { q: 0.5 })
            .is_err());
    }

    #[test]
    fn negative_zero_is_zero_and_nan_is_not_an_item() {
        let mut kmv = KmvAccumulator::new(16);
        for v in [0.0, -0.0, f64::NAN, 1.0] {
            kmv.update_single(v, 0);
        }
        assert_eq!(cardinality(&kmv), 2.0);
        assert_eq!(cardinality(kmv.take_accumulator().as_ref()), 2.0);
        assert_eq!(cardinality(&kmv), 0.0);
    }

    // sketchlib reserves all k hash slots at construction, so an empty state
    // already holds k * 8 bytes.
    #[test]
    fn memory_counts_the_k_reserved_hashes() {
        let k = 4096;
        let empty = KmvAccumulator::new(k);
        assert!(empty.approx_memory_bytes() >= k as usize * 8);
        assert_eq!(
            empty.approx_memory_bytes(),
            build(k, 0..10).approx_memory_bytes()
        );
    }
}

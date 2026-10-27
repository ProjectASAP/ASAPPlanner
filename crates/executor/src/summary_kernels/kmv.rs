//! KMV (k minimum values) distinct-count summary over `asap_sketchlib::Kmv`.
//!
//! Every item is hashed at the library's `DEFAULT_CANONICAL_SEED_INDEX`, and
//! a merge keeps the `k` smallest distinct hashes of both states, so merging
//! two states equals building one over the union of their inputs.
use crate::summary_kernels::factory::AccumulatorUpdater;
use crate::{AggregateCore, KernelError};
use asap_sketchlib::{DataInput, Kmv};
use planner_types::ir::schema::SketchStatistic;

/// The smallest `k` the estimate `(k - 1) / U(k)` is defined for.
pub const KMV_MIN_K: u32 = 2;

#[derive(Debug, Clone)]
pub struct KmvAccumulator {
    pub inner: Kmv,
}

impl KmvAccumulator {
    pub fn new(k: u32) -> Self {
        Self {
            inner: Kmv::new(k as usize),
        }
    }

    /// Observe one sample value. Like HLL, a value is hashed by its bits
    /// with `-0.0` folded into `0.0`; NaN is not an item.
    pub fn insert_sample(&mut self, value: f64) {
        if !value.is_nan() {
            let bits = if value == 0.0 { 0 } else { value.to_bits() };
            self.inner.update(&DataInput::Bytes(&bits.to_le_bytes()));
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
        merged.inner.merge(&other.inner);
        Ok(Box::new(merged))
    }

    /// Distinct count.
    fn estimate(&self, query: &SketchStatistic) -> Result<f64, KernelError> {
        match query {
            SketchStatistic::Cardinality => Ok(self.inner.estimate()),
            other => Err(format!("KMV does not answer {other:?}").into()),
        }
    }

    fn approx_memory_bytes(&self) -> usize {
        self.inner.memory_bytes()
    }
}

/// A KMV state is its own updater: it observes unkeyed sample values.
impl AccumulatorUpdater for KmvAccumulator {
    fn update_value(&mut self, value: &crate::values::Value, _: i64) -> Result<(), String> {
        use crate::summary_kernels::factory::{distinct_item, DistinctItem};
        match distinct_item(value)? {
            DistinctItem::Sample(value) => self.insert_sample(value),
            DistinctItem::Item(item) => self.inner.update(&item),
            DistinctItem::Skipped => {}
        }
        Ok(())
    }
    fn is_keyed(&self) -> bool {
        false
    }
    fn memory_usage_bytes(&self) -> usize {
        self.approx_memory_bytes()
    }
    fn update_single(&mut self, value: f64, _: i64) -> Result<(), String> {
        self.insert_sample(value);
        Ok(())
    }
    fn update_keyed(&mut self, _: &[String], value: f64, _: i64) -> Result<(), String> {
        self.insert_sample(value);
        Ok(())
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
    fn merge_accumulator(&mut self, other: &dyn AggregateCore) -> Result<(), String> {
        let mut merged = self.merge_with(other).map_err(|error| error.to_string())?;
        std::mem::swap(
            self,
            merged
                .as_any_mut()
                .downcast_mut::<Self>()
                .ok_or("a merge changed the state's type")?,
        );
        Ok(())
    }
    fn reset(&mut self) {
        self.inner = Kmv::new(self.inner.k);
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
    }

    // Above k the estimate is within ten times KMV's relative standard
    // error 1/sqrt(k - 2).
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

    // From k distinct items on, the estimate is `(k - 1) / U`, with `U` the
    // k-th smallest hash mapped into (0, 1]: the estimator the planner's
    // KMV bound is proved for (`accuracy::estimators::cardinality`, #694).
    #[test]
    fn estimates_k_minus_one_over_the_kth_smallest_hash() {
        let k = 256;
        for exact in [k, 5_000] {
            let kmv = build(k, 0..exact);
            let kth = kmv.inner.retained_hashes.iter().copied().max().unwrap();
            let u = (kth as f64 + 1.0) / 18_446_744_073_709_551_616.0;
            assert_eq!(cardinality(&kmv), f64::from(k - 1) / u);
        }
        assert_eq!(cardinality(&build(k, 0..k - 1)), f64::from(k - 1));
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
            let mut hashes: Vec<u64> = s.inner.retained_hashes.iter().copied().collect();
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
            kmv.update_single(v, 0).unwrap();
        }
        assert_eq!(cardinality(&kmv), 2.0);
        assert_eq!(cardinality(kmv.take_accumulator().as_ref()), 2.0);
        assert_eq!(cardinality(&kmv), 0.0);
    }

    // The footprint is the library's: it covers every retained hash and
    // grows as the state fills toward k.
    #[test]
    fn memory_covers_the_retained_hashes() {
        let k = 4096;
        let (empty, full) = (KmvAccumulator::new(k), build(k, 0..20_000));
        assert_eq!(full.inner.retained_hashes.len(), k as usize);
        assert!(full.approx_memory_bytes() >= k as usize * 8);
        assert!(full.approx_memory_bytes() > empty.approx_memory_bytes());
    }

    // An Int64 id and a Utf8 name are items as asap_sketchlib hashes them,
    // so distinct counts of ids need no Float64 column; NULL is no item.
    #[test]
    fn int64_and_utf8_values_are_items() {
        use crate::values::Value;
        let mut ids = KmvAccumulator::new(16);
        for id in [7, 7, -3, 1 << 60, (1 << 60) + 1] {
            ids.update_value(&Value::Int64(id), 0).unwrap();
        }
        ids.update_value(&Value::Null, 0).unwrap();
        assert_eq!(cardinality(&ids), 4.0);
        let mut names = KmvAccumulator::new(16);
        for name in ["a", "b", "a"] {
            names.update_value(&Value::Utf8(name.into()), 0).unwrap();
        }
        assert_eq!(cardinality(&names), 2.0);
        assert!(names.update_value(&Value::Bool(true), 0).is_err());
        // A non-finite sample is refused, as by every numeric kernel.
        assert!(names.update_value(&Value::Float64(f64::NAN), 0).is_err());
        assert!(names
            .update_value(&Value::Float64(f64::INFINITY), 0)
            .is_err());
    }
}

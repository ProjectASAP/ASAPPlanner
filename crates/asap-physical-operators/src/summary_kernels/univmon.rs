//! One frequency state shared by count, distinct, L2 and entropy evaluations.

use crate::AggregateCore;
use asap_sketchlib::sketch_framework::univmon::MAX_LAYER_SIZE;
use asap_sketchlib::{DataInput, UnivMon};
use planner_types::{post_asap::SketchStatistic, pre_asap::ColumnRef};

type Error = Box<dyn std::error::Error + Send + Sync>;

/// Rows beyond the executor's own limit, and shapes `asap_sketchlib`
/// rejects because their hashes would not be independent: more than
/// [`MAX_LAYER_SIZE`] (19) layers, or a layer whose rows read more than
/// one 128-bit hash holds. Each row reads `⌈log2 cols⌉` column bits and one
/// sign bit, so `rows · (⌈log2 cols⌉ + 1) ≤ 128`.
pub(crate) fn check_dimensions(
    heap_size: usize,
    rows: usize,
    cols: usize,
    layers: usize,
) -> Result<(), Error> {
    if heap_size == 0
        || cols == 0
        || !(1..=20).contains(&rows)
        || !(1..=MAX_LAYER_SIZE).contains(&layers)
    {
        return Err("invalid UnivMon dimensions".into());
    }
    let column_bits = cols
        .checked_next_power_of_two()
        .map_or(usize::BITS, usize::trailing_zeros) as usize;
    if rows * (column_bits + 1) > 128 {
        return Err("UnivMon rows and columns exceed one 128-bit hash".into());
    }
    rows.checked_mul(cols)
        .and_then(|n| n.checked_mul(layers))
        .ok_or("UnivMon dimensions overflow")?;
    Ok(())
}

#[derive(Debug, Clone)]
pub struct UnivMonAccumulator {
    inner: UnivMon,
    /// The longest variable-length key the heaps may retain, in bytes: the
    /// memory charge bounds the retained keys by every heap slot holding
    /// one this long, so it never walks the heaps (a build charges memory
    /// after every row).
    max_key_bytes: usize,
}

impl UnivMonAccumulator {
    /// Empty the sketch in place, keeping its shape.
    pub(crate) fn clear(&mut self) {
        self.inner.free();
        self.max_key_bytes = 0;
    }

    pub fn new(heap_size: usize, rows: usize, cols: usize, layers: usize) -> Result<Self, Error> {
        check_dimensions(heap_size, rows, cols, layers)?;
        Ok(Self {
            inner: UnivMon::init_univmon(heap_size, rows, cols, layers),
            max_key_bytes: 0,
        })
    }

    /// Adopt a decoded sketch, e.g. one a deployment restored from its stored
    /// bytes. Rejects dimensions [`Self::new`] rejects and terminal-mode
    /// sketches, which do not accept this accumulator's updates.
    pub fn from_sketch(sketch: UnivMon) -> Result<Self, Error> {
        check_dimensions(
            sketch.heap_size,
            sketch.sketch_row,
            sketch.sketch_col,
            sketch.layer_size,
        )?;
        if !sketch.accepts_standard_updates() {
            return Err("terminal-mode UnivMon state cannot accept standard updates".into());
        }
        // One walk of the decoded heaps, not one per charge.
        let max_key_bytes = (0..sketch.layer_size)
            .flat_map(|layer| sketch.hh_layers[layer].heap())
            .map(|item| match &item.key {
                asap_sketchlib::HeapItem::String(key) => key.capacity(),
                asap_sketchlib::HeapItem::Bytes(key) => key.capacity(),
                _ => 0,
            })
            .max()
            .unwrap_or(0);
        Ok(Self {
            inner: sketch,
            max_key_bytes,
        })
    }

    /// The underlying sketch, so a deployment can encode it with sketchlib's codec.
    pub fn sketch(&self) -> &UnivMon {
        &self.inner
    }

    /// Each non-NaN sample is one occurrence. Signed zero has one identity.
    pub fn insert_sample(&mut self, value: f64) -> Result<(), Error> {
        if value.is_nan() {
            return Ok(());
        }
        self.inner
            .bucket_size
            .checked_add(1)
            .ok_or("UnivMon count overflow")?;
        let bits = if value == 0.0 { 0 } else { value.to_bits() };
        self.inner.insert(&DataInput::U64(bits), 1);
        Ok(())
    }

    /// One occurrence of a typed identity. A finite Float64 is a sample
    /// ([`Self::insert_sample`]); Bool, Int64 and Utf8 keep their type as
    /// type-tagged key bytes, since converting Int64 to Float64 would collapse
    /// neighbouring keys above 2^53. NULL is skipped.
    pub fn insert_value(&mut self, value: &crate::values::Value) -> Result<(), Error> {
        use crate::values::Value;
        match value {
            Value::Null => return Ok(()),
            Value::Float64(number) if number.is_finite() => return self.insert_sample(*number),
            Value::Bool(_) | Value::Int64(_) | Value::Utf8(_) => {}
            _ => return Err("unsupported UnivMon identity type or non-finite sample".into()),
        }
        let key = value.key()?;
        self.inner
            .bucket_size
            .checked_add(1)
            .ok_or("UnivMon count overflow")?;
        self.inner.insert(&DataInput::Bytes(&key), 1);
        self.max_key_bytes = self.max_key_bytes.max(key.len());
        Ok(())
    }

    fn compatible(&self, other: &Self) -> bool {
        (
            self.inner.heap_size,
            self.inner.sketch_row,
            self.inner.sketch_col,
            self.inner.layer_size,
        ) == (
            other.inner.heap_size,
            other.inner.sketch_row,
            other.inner.sketch_col,
            other.inner.layer_size,
        )
    }

    pub fn dimensions(&self) -> (usize, usize, usize, usize) {
        (
            self.inner.heap_size,
            self.inner.sketch_row,
            self.inner.sketch_col,
            self.inner.layer_size,
        )
    }

    pub fn merge_in_place(&mut self, other: &Self) -> Result<(), Error> {
        if !self.compatible(other) {
            return Err("incompatible UnivMon dimensions".into());
        }
        self.inner
            .bucket_size
            .checked_add(other.inner.bucket_size)
            .ok_or("UnivMon count overflow")?;
        self.inner.merge(&other.inner);
        self.max_key_bytes = self.max_key_bytes.max(other.max_key_bytes);
        Ok(())
    }
}

impl AggregateCore for UnivMonAccumulator {
    /// Counters and heap slots of every layer, plus an upper bound on the
    /// variable-length keys the heaps retain: every slot holding the
    /// longest key seen. Constant time, since a build charges memory after
    /// every row.
    fn approx_memory_bytes(&self) -> usize {
        let key_bytes = self
            .inner
            .layer_size
            .saturating_mul(self.inner.heap_size)
            .saturating_mul(self.max_key_bytes);
        key_bytes
            .saturating_add(std::mem::size_of::<Self>())
            .saturating_add(
                self.inner.layer_size.saturating_mul(
                    self.inner
                        .sketch_row
                        .saturating_mul(self.inner.sketch_col)
                        .saturating_mul(16)
                        .saturating_add(self.inner.heap_size.saturating_mul(256)),
                ),
            )
    }
    fn clone_boxed_core(&self) -> Box<dyn AggregateCore> {
        Box::new(self.clone())
    }
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
    fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
        self
    }

    fn merge_with(&self, other: &dyn AggregateCore) -> Result<Box<dyn AggregateCore>, Error> {
        let other = other
            .as_any()
            .downcast_ref::<Self>()
            .ok_or("expected UnivMon state")?;
        let mut merged = self.clone();
        merged.merge_in_place(other)?;
        Ok(Box::new(merged))
    }

    /// Sample count (a bare `PointCount`), distinct count, L2 norm and entropy
    /// of the sample-value frequencies.
    fn estimate(&self, query: &SketchStatistic) -> Result<f64, Error> {
        Ok(match query {
            SketchStatistic::PointCount {
                key: ColumnRef::SampleValue,
                value: None,
            } => self.inner.calc_l1(),
            SketchStatistic::Cardinality => self.inner.calc_card(),
            SketchStatistic::FrequencyL2 => self.inner.calc_l2(),
            SketchStatistic::FrequencyEntropy => self.inner.calc_entropy(),
            other => return Err(format!("UnivMon does not answer {other:?}").into()),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn count() -> SketchStatistic {
        SketchStatistic::PointCount {
            key: ColumnRef::SampleValue,
            value: None,
        }
    }

    // Count, distinct, L2 and entropy evaluations count each non-NaN sample once;
    // signed zero is one identity.
    #[test]
    fn frequency_evaluations() {
        let mut state = UnivMonAccumulator::new(32, 5, 1024, 4).unwrap();
        for value in [0.0, -0.0, 2.0, 2.0, f64::NAN] {
            state.insert_sample(value).unwrap();
        }
        let read = |query| state.estimate(&query).unwrap();
        assert_eq!(read(count()), 4.0);
        assert!((read(SketchStatistic::Cardinality) - 2.0).abs() < 0.01);
        assert!((read(SketchStatistic::FrequencyL2) - 8.0f64.sqrt()).abs() < 0.01);
        assert!((read(SketchStatistic::FrequencyEntropy) - 1.0).abs() < 0.01);
        assert!(state
            .estimate(&SketchStatistic::Quantile { q: 0.5 })
            .is_err());
    }

    // A sketch taken out and adopted back answers the same evaluations.
    #[test]
    fn adopted_sketch_keeps_evaluations() {
        let mut state = UnivMonAccumulator::new(32, 5, 1024, 4).unwrap();
        for value in [1.0, 2.0, 2.0] {
            state.insert_sample(value).unwrap();
        }
        let adopted = UnivMonAccumulator::from_sketch(state.sketch().clone()).unwrap();
        assert_eq!(adopted.dimensions(), state.dimensions());
        for query in [
            count(),
            SketchStatistic::Cardinality,
            SketchStatistic::FrequencyEntropy,
        ] {
            assert_eq!(
                adopted.estimate(&query).unwrap(),
                state.estimate(&query).unwrap()
            );
        }
    }

    // Adoption rejects terminal-mode sketches and dimensions `new` rejects.
    #[test]
    fn adoption_rejects_terminal_or_invalid_sketches() {
        let mut terminal = UnivMon::init_univmon(4, 3, 16, 2);
        terminal.fast_insert(&DataInput::U64(1), 1);
        assert!(UnivMonAccumulator::from_sketch(terminal.clone()).is_err());
        terminal.free();
        assert!(UnivMonAccumulator::from_sketch(terminal).is_ok());
        assert!(UnivMonAccumulator::from_sketch(UnivMon::init_univmon(4, 21, 16, 2)).is_err());
    }

    // Shapes whose hashes are not independent are errors, not library
    // panics: more than 19 layers, or rows · (⌈log2 cols⌉ + 1) > 128.
    #[test]
    fn rejects_shapes_past_the_hash_budget() {
        assert!(UnivMonAccumulator::new(4, 1, 16, 19).is_ok());
        assert!(UnivMonAccumulator::new(4, 1, 16, 20).is_err());
        // 8 · (⌈log2 65536⌉ + 1) = 136; 7 · 17 = 119.
        assert!(UnivMonAccumulator::new(4, 8, 1 << 16, 1).is_err());
        assert!(UnivMonAccumulator::new(4, 7, 1 << 16, 1).is_ok());
        // A non-power-of-two width rounds its bits up: 11 · (11 + 1) = 132.
        assert!(UnivMonAccumulator::new(4, 11, 2047, 1).is_err());
        assert!(UnivMonAccumulator::new(4, 10, 2047, 1).is_ok());
    }

    // Typed keys stay distinct, and merge, when entered as type-tagged bytes:
    // Int64 identities above 2^53 do not collapse.
    #[test]
    fn typed_keys_merge_and_stay_distinct() {
        use crate::values::Value;
        for keys in [
            [
                Value::Utf8("192.0.2.1".into()),
                Value::Utf8("192.0.2.2".into()),
            ],
            [
                Value::Int64(9_007_199_254_740_992),
                Value::Int64(9_007_199_254_740_993),
            ],
            [Value::Bool(false), Value::Bool(true)],
        ] {
            let mut left = UnivMonAccumulator::new(32, 5, 1024, 4).unwrap();
            let mut right = UnivMonAccumulator::new(32, 5, 1024, 4).unwrap();
            for _ in 0..2 {
                left.insert_value(&keys[0]).unwrap();
                right.insert_value(&keys[1]).unwrap();
            }
            left.insert_value(&Value::Null).unwrap();
            left.merge_in_place(&right).unwrap();
            for (statistic, expected) in [
                (count(), 4.0),
                (SketchStatistic::Cardinality, 2.0),
                (SketchStatistic::FrequencyL2, 8.0_f64.sqrt()),
                (SketchStatistic::FrequencyEntropy, 1.0),
            ] {
                let estimate = left.estimate(&statistic).unwrap();
                assert!(
                    (estimate - expected).abs() < 0.01,
                    "{keys:?} {statistic:?}: {estimate}"
                );
            }
        }
    }

    // A non-finite sample or an unsupported identity type is an error.
    #[test]
    fn rejects_non_finite_and_untyped_identities() {
        use crate::values::Value;
        let mut state = UnivMonAccumulator::new(32, 5, 1024, 4).unwrap();
        assert!(state.insert_value(&Value::Float64(f64::INFINITY)).is_err());
        assert!(state.insert_value(&Value::Timestamp(1)).is_err());
    }

    // Retained variable-length identities count toward the state's memory.
    #[test]
    fn memory_accounts_for_string_identities() {
        let mut state = UnivMonAccumulator::new(32, 5, 1024, 4).unwrap();
        let empty = state.approx_memory_bytes();
        state
            .insert_value(&crate::values::Value::Utf8("x".repeat(4096).into()))
            .unwrap();
        assert!(state.approx_memory_bytes() >= empty + 4096);
        state.clear();
        assert_eq!(state.approx_memory_bytes(), empty);
    }

    // A build charges memory after every row, so the charge must not walk
    // the heaps: with keys of one length it stays the same however many
    // keys the heaps hold.
    #[test]
    fn memory_charge_does_not_grow_with_heap_occupancy() {
        let mut state = UnivMonAccumulator::new(512, 3, 64, 4).unwrap();
        let key = |i: i64| crate::values::Value::Utf8(format!("{i:08}").into());
        state.insert_value(&key(0)).unwrap();
        let one = state.approx_memory_bytes();
        for i in 1..2000 {
            state.insert_value(&key(i)).unwrap();
        }
        assert_eq!(state.approx_memory_bytes(), one);
    }

    // Charging memory after every row of a large build stays linear in the
    // rows (it was quadratic: 3 s for this build). Timing-based, so ignored
    // by default; run with `--ignored --release`.
    #[test]
    #[ignore = "timing; run in release"]
    fn per_row_memory_charge_of_a_large_build_is_fast() {
        let mut state = UnivMonAccumulator::new(20_000, 3, 1024, 19).unwrap();
        let start = std::time::Instant::now();
        let mut charged = 0usize;
        for i in 0..40_000i64 {
            state.insert_value(&crate::values::Value::Int64(i)).unwrap();
            charged = charged.max(state.approx_memory_bytes());
        }
        assert!(charged > 0);
        assert!(
            start.elapsed() < std::time::Duration::from_millis(500),
            "{:?}",
            start.elapsed()
        );
    }
}

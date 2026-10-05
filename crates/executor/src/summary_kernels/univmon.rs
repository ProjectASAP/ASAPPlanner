//! One frequency state shared by count, distinct, L2 and entropy evaluations.

use crate::AggregateCore;
use asap_sketchlib::{DataInput, UnivMon};
use planner_types::ir::scalar::ColumnRef;
use planner_types::ir::schema::SketchStatistic;

type Error = Box<dyn std::error::Error + Send + Sync>;

fn check_dimensions(
    heap_size: usize,
    rows: usize,
    cols: usize,
    layers: usize,
) -> Result<(), Error> {
    if heap_size == 0 || cols == 0 || !(1..=20).contains(&rows) || !(1..=64).contains(&layers) {
        return Err("invalid UnivMon dimensions".into());
    }
    rows.checked_mul(cols)
        .and_then(|n| n.checked_mul(layers))
        .ok_or("UnivMon dimensions overflow")?;
    Ok(())
}

#[derive(Debug, Clone)]
pub struct UnivMonAccumulator {
    inner: UnivMon,
}

impl UnivMonAccumulator {
    /// Empty the sketch in place, keeping its shape.
    pub(crate) fn clear(&mut self) {
        self.inner.free();
    }

    pub fn new(heap_size: usize, rows: usize, cols: usize, layers: usize) -> Result<Self, Error> {
        check_dimensions(heap_size, rows, cols, layers)?;
        Ok(Self {
            inner: UnivMon::init_univmon(heap_size, rows, cols, layers),
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
        Ok(Self { inner: sketch })
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

    /// SQL identities are kept in their original type: converting Int64 to
    /// Float64 would collapse neighboring keys above 2^53.
    pub fn insert_value(&mut self, value: &crate::values::Value) -> Result<(), Error> {
        use crate::values::Value;
        match value {
            Value::Null => return Ok(()),
            Value::Float64(number) if number.is_finite() => return self.insert_sample(*number),
            Value::Bool(_) | Value::Int64(_) | Value::Utf8(_) => {}
            _ => return Err("unsupported UnivMon identity type or nonfinite sample".into()),
        }
        let key = value.key()?;
        self.inner
            .bucket_size
            .checked_add(1)
            .ok_or("UnivMon count overflow")?;
        self.inner.insert(&DataInput::Bytes(&key), 1);
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
        Ok(())
    }
}

impl AggregateCore for UnivMonAccumulator {
    fn approx_memory_bytes(&self) -> usize {
        let key_bytes = (0..self.inner.layer_size)
            .flat_map(|layer| self.inner.hh_layers[layer].heap())
            .map(|item| match &item.key {
                asap_sketchlib::HeapItem::String(key) => key.capacity(),
                asap_sketchlib::HeapItem::Bytes(key) => key.capacity(),
                _ => 0,
            })
            .fold(0usize, usize::saturating_add);
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
            // Layer 0 sees the whole stream; its row-median F₂ is the readout
            // the planner certifies (`calc_l2` is the heavy-hitter G-sum).
            SketchStatistic::FrequencyL2 => self.inner.l2_sketch_layers[0].get_l2(),
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
    /// Typed keys remain distinct after merging and restoring persisted native state.
    #[test]
    fn typed_keys_merge_and_roundtrip() {
        use crate::values::Value;
        let mut left = UnivMonAccumulator::new(32, 5, 1024, 4).unwrap();
        let mut right = UnivMonAccumulator::new(32, 5, 1024, 4).unwrap();
        for _ in 0..2 {
            left.insert_value(&Value::Utf8("192.0.2.1".into())).unwrap();
            right
                .insert_value(&Value::Utf8("192.0.2.2".into()))
                .unwrap();
        }
        left.merge_in_place(&right).unwrap();
        let bytes = left.sketch().serialize_to_bytes().unwrap();
        let restored =
            UnivMonAccumulator::from_sketch(UnivMon::deserialize_from_bytes(&bytes).unwrap())
                .unwrap();
        for (statistic, expected) in [
            (SketchStatistic::Cardinality, 2.0),
            (SketchStatistic::FrequencyL2, 8.0_f64.sqrt()),
            (SketchStatistic::FrequencyEntropy, 1.0),
        ] {
            assert!((restored.estimate(&statistic).unwrap() - expected).abs() < 0.01);
        }
    }

    /// L2 reads layer 0's F₂ estimate, and at the planner's (0.01, 0.01)
    /// sizing it is within 1% of a Zipf stream's true L2 norm.
    #[test]
    fn l2_is_layer0_f2_within_the_certified_bound() {
        use asap_logical_optimizer::pass1::replacement::default_size_params;
        use planner_types::ir::operator::agg_intent::default_cardinality;
        use planner_types::ir::schema::{SketchAlgorithm, SketchParams};
        let SketchParams::UnivMon {
            heap_size,
            sketch_rows,
            sketch_cols,
            layers,
        } = default_size_params(SketchAlgorithm::UnivMon, &default_cardinality(), 0.01, 0.01)
        else {
            unreachable!()
        };
        let mut state = UnivMonAccumulator::new(
            heap_size as usize,
            sketch_rows as usize,
            sketch_cols as usize,
            usize::from(layers),
        )
        .unwrap();
        // Zipf(1) frequencies over 5,000 keys: key i occurs ⌊10,000 / i⌋ times.
        let mut f2 = 0.0;
        for key in 1..=5_000u32 {
            let count = 10_000 / key;
            f2 += f64::from(count) * f64::from(count);
            for _ in 0..count {
                state.insert_sample(f64::from(key)).unwrap();
            }
        }
        let l2 = state.estimate(&SketchStatistic::FrequencyL2).unwrap();
        assert_eq!(l2, state.sketch().l2_sketch_layers[0].get_l2());
        assert!(
            (l2 - f2.sqrt()).abs() <= 0.01 * f2.sqrt(),
            "{l2} vs {}",
            f2.sqrt()
        );
    }

    /// Retained variable-length identities contribute to the runtime memory reservation.
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
}

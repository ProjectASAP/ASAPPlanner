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
        std::mem::size_of::<Self>().saturating_add(
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
}

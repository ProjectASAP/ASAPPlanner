//! One frequency state shared by count, distinct, L2 and entropy readouts.

use crate::AggregateCore;
use asap_sketchlib::{DataInput, UnivMon};

type Error = Box<dyn std::error::Error + Send + Sync>;

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
        if heap_size == 0 || cols == 0 || !(1..=20).contains(&rows) || !(1..=64).contains(&layers) {
            return Err("invalid UnivMon dimensions".into());
        }
        rows.checked_mul(cols)
            .and_then(|n| n.checked_mul(layers))
            .ok_or("UnivMon dimensions overflow")?;
        Ok(Self {
            inner: UnivMon::init_univmon(heap_size, rows, cols, layers),
        })
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

    fn merge_with(&self, other: &dyn AggregateCore) -> Result<Box<dyn AggregateCore>, Error> {
        let other = other
            .as_any()
            .downcast_ref::<Self>()
            .ok_or("expected UnivMon state")?;
        let mut merged = self.clone();
        merged.merge_in_place(other)?;
        Ok(Box::new(merged))
    }
}

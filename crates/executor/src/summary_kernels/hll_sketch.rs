//! HyperLogLog distinct-count summary over `asap_sketchlib::HyperLogLogImpl`
//! with the classic estimator.
use crate::{AggregateCore, KernelError};
use asap_sketchlib::sketches::hll::HyperLogLogImpl;
use asap_sketchlib::{Classic, DataInput};
use planner_types::ir::schema::SketchStatistic;

/// The precisions the executor builds.
pub const HLL_PRECISIONS: std::ops::RangeInclusive<u8> = 4..=18;

// sketchlib fixes the precision in the register type and defines P12, P14
// and P16; the executor builds every precision in `HLL_PRECISIONS`.
asap_sketchlib::impl_hll_registers!(HllRegistersP4, 4, 1_usize << 4);
asap_sketchlib::impl_hll_registers!(HllRegistersP5, 5, 1_usize << 5);
asap_sketchlib::impl_hll_registers!(HllRegistersP6, 6, 1_usize << 6);
asap_sketchlib::impl_hll_registers!(HllRegistersP7, 7, 1_usize << 7);
asap_sketchlib::impl_hll_registers!(HllRegistersP8, 8, 1_usize << 8);
asap_sketchlib::impl_hll_registers!(HllRegistersP9, 9, 1_usize << 9);
asap_sketchlib::impl_hll_registers!(HllRegistersP10, 10, 1_usize << 10);
asap_sketchlib::impl_hll_registers!(HllRegistersP11, 11, 1_usize << 11);
asap_sketchlib::impl_hll_registers!(HllRegistersP13, 13, 1_usize << 13);
asap_sketchlib::impl_hll_registers!(HllRegistersP15, 15, 1_usize << 15);
asap_sketchlib::impl_hll_registers!(HllRegistersP17, 17, 1_usize << 17);
asap_sketchlib::impl_hll_registers!(HllRegistersP18, 18, 1_usize << 18);

macro_rules! hll_by_precision {
    ($($variant:ident = $precision:literal => $registers:ty),* $(,)?) => {
        /// A classic-estimator HyperLogLog at one precision.
        #[derive(Debug, Clone)]
        enum Hll {
            $($variant(HyperLogLogImpl<Classic, $registers>),)*
        }

        impl Hll {
            fn new(precision: u8) -> Option<Self> {
                match precision {
                    $($precision => Some(Self::$variant(HyperLogLogImpl::<Classic, $registers>::new())),)*
                    _ => None,
                }
            }
            fn precision(&self) -> u8 {
                match self {
                    $(Self::$variant(_) => $precision,)*
                }
            }
            fn update(&mut self, item: &DataInput) {
                match self {
                    $(Self::$variant(hll) => hll.update(item),)*
                }
            }
            fn estimate(&self) -> usize {
                match self {
                    $(Self::$variant(hll) => hll.estimate(),)*
                }
            }
            /// `None` when the precisions differ.
            fn merge(&mut self, other: &Self) -> Option<()> {
                match (self, other) {
                    $((Self::$variant(hll), Self::$variant(other)) => hll.merge(other),)*
                    _ => return None,
                }
                Some(())
            }
        }
    };
}

hll_by_precision! {
    P4 = 4 => HllRegistersP4,
    P5 = 5 => HllRegistersP5,
    P6 = 6 => HllRegistersP6,
    P7 = 7 => HllRegistersP7,
    P8 = 8 => HllRegistersP8,
    P9 = 9 => HllRegistersP9,
    P10 = 10 => HllRegistersP10,
    P11 = 11 => HllRegistersP11,
    P12 = 12 => asap_sketchlib::HllRegistersP12,
    P13 = 13 => HllRegistersP13,
    P14 = 14 => asap_sketchlib::HllRegistersP14,
    P15 = 15 => HllRegistersP15,
    P16 = 16 => asap_sketchlib::HllRegistersP16,
    P17 = 17 => HllRegistersP17,
    P18 = 18 => HllRegistersP18,
}

#[derive(Debug, Clone)]
pub struct HllSketchAccumulator {
    inner: Hll,
}

impl HllSketchAccumulator {
    /// Fails for a precision outside [`HLL_PRECISIONS`].
    pub fn new(precision: u8) -> Result<Self, KernelError> {
        Ok(Self {
            inner: Hll::new(precision)
                .ok_or_else(|| format!("HLL precision {precision} is outside 4..=18"))?,
        })
    }

    pub fn precision(&self) -> u8 {
        self.inner.precision()
    }

    /// Observes one sample value, hashed by its bits with `-0.0` folded into
    /// `0.0`; NaN is not an item.
    pub fn insert_sample(&mut self, value: f64) {
        if !value.is_nan() {
            let bits = if value == 0.0 { 0 } else { value.to_bits() };
            self.inner.update(&DataInput::Bytes(&bits.to_le_bytes()));
        }
    }

    /// Observes one item given as bytes.
    pub fn insert_bytes(&mut self, item: &[u8]) {
        self.inner.update(&DataInput::Bytes(item));
    }

    /// Empties the sketch, keeping its precision.
    pub fn clear(&mut self) {
        self.inner = Hll::new(self.precision()).expect("a built precision");
    }
}

impl AggregateCore for HllSketchAccumulator {
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
            .ok_or("HLL merges only with HLL")?;
        let mut merged = self.clone();
        merged
            .inner
            .merge(&other.inner)
            .ok_or("HLL merges only with the same precision")?;
        Ok(Box::new(merged))
    }

    /// Distinct count, rounded down to an integer by sketchlib. A bare
    /// `PointCount` over an HLL also reads the distinct count.
    fn estimate(&self, query: &SketchStatistic) -> Result<f64, KernelError> {
        match query {
            SketchStatistic::Cardinality | SketchStatistic::PointCount { value: None, .. } => {
                Ok(self.inner.estimate() as f64)
            }
            other => Err(format!("HLL does not answer {other:?}").into()),
        }
    }

    /// The struct and its `2^precision` one-byte registers.
    fn approx_memory_bytes(&self) -> usize {
        std::mem::size_of::<Self>().saturating_add(1 << self.precision())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Distinct count after merge counts overlapping items once.
    #[test]
    fn merged_cardinality_deduplicates_overlap() {
        let (mut a, mut b) = (
            HllSketchAccumulator::new(12).unwrap(),
            HllSketchAccumulator::new(12).unwrap(),
        );
        for v in 0..1000u32 {
            a.insert_bytes(&v.to_le_bytes());
            b.insert_bytes(&(v + 500).to_le_bytes());
        }
        let merged = a.merge_with(&b).unwrap();
        let estimate = merged.estimate(&SketchStatistic::Cardinality).unwrap();
        assert!((estimate - 1500.0).abs() / 1500.0 < 0.05, "{estimate}");
        assert!(a
            .merge_with(&HllSketchAccumulator::new(13).unwrap())
            .is_err());
    }

    // Every precision in 4..=18 builds, and no other.
    #[test]
    fn builds_exactly_the_supported_precisions() {
        for precision in 0..=20u8 {
            let built = HllSketchAccumulator::new(precision);
            assert_eq!(built.is_ok(), HLL_PRECISIONS.contains(&precision));
            if let Ok(hll) = built {
                assert_eq!(hll.precision(), precision);
            }
        }
    }

    // HLL does not answer quantiles.
    #[test]
    fn rejects_quantile() {
        let hll = HllSketchAccumulator::new(12).unwrap();
        assert!(hll.estimate(&SketchStatistic::Quantile { q: 0.5 }).is_err());
    }
}

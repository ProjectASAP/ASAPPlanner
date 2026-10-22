//! HyperLogLog distinct-count summary over `asap_sketchlib`'s classic
//! `HyperLogLogImpl`, at a precision chosen at run time.
use crate::{AggregateCore, KernelError};
use asap_sketchlib::DataInput;
use planner_types::ir::schema::SketchStatistic;
use registers::Registers;

/// The precisions a state can be built at.
pub const HLL_PRECISIONS: std::ops::RangeInclusive<u8> = 4..=18;

/// `asap_sketchlib` fixes an HLL's precision in its register type: one type
/// per precision in [`HLL_PRECISIONS`], and [`Registers`] holds any of them.
mod registers {
    use asap_sketchlib::sketches::hll::HyperLogLogImpl;
    use asap_sketchlib::{Classic, DataInput};

    macro_rules! precisions {
        ($($variant:ident($storage:ident, $precision:literal)),* $(,)?) => {
            $(asap_sketchlib::impl_hll_registers!($storage, $precision, 1_usize << $precision);)*

            #[derive(Debug, Clone)]
            pub enum Registers {
                $($variant(HyperLogLogImpl<Classic, $storage>),)*
            }

            impl Registers {
                pub fn new(precision: u8) -> Option<Self> {
                    match precision {
                        $($precision => Some(Self::$variant(HyperLogLogImpl::<Classic, $storage>::new())),)*
                        _ => None,
                    }
                }

                pub fn precision(&self) -> u8 {
                    match self {
                        $(Self::$variant(_) => $precision,)*
                    }
                }

                pub fn update(&mut self, item: &DataInput) {
                    match self {
                        $(Self::$variant(hll) => hll.update(item),)*
                    }
                }

                /// False, and `self` unchanged, when the precisions differ.
                pub fn merge(&mut self, other: &Self) -> bool {
                    match (self, other) {
                        $((Self::$variant(hll), Self::$variant(theirs)) => {
                            hll.merge(theirs);
                            true
                        })*
                        _ => false,
                    }
                }

                pub fn estimate(&self) -> usize {
                    match self {
                        $(Self::$variant(hll) => hll.estimate(),)*
                    }
                }

                pub fn registers(&self) -> &[u8] {
                    match self {
                        $(Self::$variant(hll) => hll.registers_as_slice(),)*
                    }
                }
            }
        };
    }

    precisions!(
        P4(HllRegistersP4, 4),
        P5(HllRegistersP5, 5),
        P6(HllRegistersP6, 6),
        P7(HllRegistersP7, 7),
        P8(HllRegistersP8, 8),
        P9(HllRegistersP9, 9),
        P10(HllRegistersP10, 10),
        P11(HllRegistersP11, 11),
        P12(HllRegistersP12, 12),
        P13(HllRegistersP13, 13),
        P14(HllRegistersP14, 14),
        P15(HllRegistersP15, 15),
        P16(HllRegistersP16, 16),
        P17(HllRegistersP17, 17),
        P18(HllRegistersP18, 18),
    );
}

#[derive(Debug, Clone)]
pub struct HllSketchAccumulator {
    inner: Registers,
}

impl HllSketchAccumulator {
    /// Fails for a precision outside [`HLL_PRECISIONS`].
    pub fn new(precision: u8) -> Result<Self, KernelError> {
        Registers::new(precision)
            .map(|inner| Self { inner })
            .ok_or_else(|| format!("HLL precision {precision} is outside 4..=18").into())
    }

    pub fn precision(&self) -> u8 {
        self.inner.precision()
    }

    /// Observe one item, hashed by its bytes.
    pub fn update(&mut self, item: &[u8]) {
        self.inner.update(&DataInput::Bytes(item));
    }

    /// Observe one typed item, hashed as `asap_sketchlib` hashes it.
    pub fn update_item(&mut self, item: &DataInput) {
        self.inner.update(item);
    }

    /// One rank per register.
    pub fn registers(&self) -> &[u8] {
        self.inner.registers()
    }

    /// Empty the state in place, keeping its precision.
    pub fn clear(&mut self) {
        self.inner = Registers::new(self.precision()).expect("a built precision");
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
        if !merged.inner.merge(&other.inner) {
            return Err(format!(
                "HLL merges only with the same precision ({} vs {})",
                self.precision(),
                other.precision()
            )
            .into());
        }
        Ok(Box::new(merged))
    }

    /// Distinct count, rounded down to an integer. A bare `PointCount` over
    /// an HLL also reads the distinct count.
    fn estimate(&self, query: &SketchStatistic) -> Result<f64, KernelError> {
        match query {
            SketchStatistic::Cardinality | SketchStatistic::PointCount { value: None, .. } => {
                Ok(self.inner.estimate() as f64)
            }
            other => Err(format!("HLL does not answer {other:?}").into()),
        }
    }

    fn approx_memory_bytes(&self) -> usize {
        std::mem::size_of::<Self>().saturating_add(self.registers().len())
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
            a.update(&v.to_le_bytes());
            b.update(&(v + 500).to_le_bytes());
        }
        let merged = a.merge_with(&b).unwrap();
        let estimate = merged.estimate(&SketchStatistic::Cardinality).unwrap();
        assert!((estimate - 1500.0).abs() / 1500.0 < 0.05, "{estimate}");
    }

    // HLL does not answer quantiles.
    #[test]
    fn rejects_quantile() {
        let hll = HllSketchAccumulator::new(12).unwrap();
        assert!(hll.estimate(&SketchStatistic::Quantile { q: 0.5 }).is_err());
    }

    // Every precision in 4..=18 builds 2^p registers, others fail, and
    // states of different precisions do not merge.
    #[test]
    fn precision_range_and_mismatched_merge() {
        for precision in HLL_PRECISIONS {
            let hll = HllSketchAccumulator::new(precision).unwrap();
            assert_eq!(hll.precision(), precision);
            assert_eq!(hll.registers().len(), 1 << precision);
        }
        assert!(HllSketchAccumulator::new(3).is_err());
        assert!(HllSketchAccumulator::new(19).is_err());
        let (p12, p14) = (
            HllSketchAccumulator::new(12).unwrap(),
            HllSketchAccumulator::new(14).unwrap(),
        );
        assert!(p12.merge_with(&p14).is_err());
    }

    // The cardinality is an integer: 998 for 1,000 distinct items at p = 12.
    #[test]
    fn cardinality_is_an_integer() {
        let mut hll = HllSketchAccumulator::new(12).unwrap();
        for v in 0..1000u32 {
            hll.update(&v.to_le_bytes());
        }
        let estimate = hll.estimate(&SketchStatistic::Cardinality).unwrap();
        assert_eq!(estimate.fract(), 0.0);
        assert_eq!(estimate, 998.0);
    }
}

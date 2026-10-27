use crate::AggregateCore;
use asap_sketchlib::input::{HydraCounter, HydraQuery};
use asap_sketchlib::{DataInput, Hydra, Kll};

/// The single Hydra key column: a key's whole label tuple is one
/// subpopulation.
const KEY: &str = "key";

/// HydraKLL (shared-grouping quantiles): `asap_sketchlib::Hydra` over KLL cells.
#[derive(Debug, Clone)]
pub struct HydraKllSketchAccumulator {
    pub inner: Hydra,
}

impl HydraKllSketchAccumulator {
    pub fn new(rows: usize, cols: usize, k: u16) -> Self {
        let cell = HydraCounter::Kll(Kll::with_k(i32::from(k)));
        Self {
            inner: Hydra::with_schema(rows, cols, [KEY], cell).expect("a one-column schema"),
        }
    }

    pub fn update(&mut self, key: &[String], value: f64) {
        self.inner
            .update(&[&key.join(";")], &DataInput::F64(value), None)
            .expect("one key per schema column");
    }

    /// The median over rows of each cell's KLL quantile.
    pub fn query_key(&self, key: &[String], quantile: f64) -> f64 {
        self.inner
            .query_key(&[Some(&key.join(";"))], &HydraQuery::Quantile(quantile))
            .expect("KLL cells answer quantiles")
    }
}

impl AggregateCore for HydraKllSketchAccumulator {
    fn clone_boxed_core(&self) -> Box<dyn AggregateCore> {
        Box::new(self.clone())
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
    fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
        self
    }

    fn merge_with(
        &self,
        other: &dyn AggregateCore,
    ) -> Result<Box<dyn AggregateCore>, Box<dyn std::error::Error + Send + Sync>> {
        let hk = other
            .as_any()
            .downcast_ref::<HydraKllSketchAccumulator>()
            .ok_or("Failed to downcast to HydraKllSketchAccumulator")?;

        let mut merged = self.clone();
        merged.inner.merge(&hk.inner)?;
        Ok(Box::new(merged))
    }

    /// The KLL cells' memory, walking every cell.
    fn approx_memory_bytes(&self) -> usize {
        self.inner
            .cells
            .as_slice()
            .iter()
            .map(|cell| match cell {
                HydraCounter::Kll(kll) => kll.memory_bytes(),
                _ => 0,
            })
            .fold(std::mem::size_of::<Self>(), usize::saturating_add)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // A key's quantile reads its own values, and merge keeps both inputs'.
    #[test]
    fn quantiles_per_key_survive_merge() {
        let (mut a, mut b) = (
            HydraKllSketchAccumulator::new(3, 64, 200),
            HydraKllSketchAccumulator::new(3, 64, 200),
        );
        let (x, y) = (["x".to_string()], ["y".to_string()]);
        for v in 1..=50 {
            a.update(&x, f64::from(v));
            b.update(&x, f64::from(v + 50));
            a.update(&y, 1000.0);
        }
        let merged = a.merge_with(&b).unwrap();
        let merged = merged
            .as_any()
            .downcast_ref::<HydraKllSketchAccumulator>()
            .unwrap();
        assert!((merged.query_key(&x, 1.0) - 100.0).abs() <= 1.0);
        assert!(merged.query_key(&x, 0.0) <= 2.0);
        assert!(a
            .merge_with(&HydraKllSketchAccumulator::new(3, 32, 200))
            .is_err());
    }
}

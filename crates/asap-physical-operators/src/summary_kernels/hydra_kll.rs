use crate::{AggregateCore, KeyByLabelValues};
use asap_sketchlib::HydraKllSketch;

/// HydraKLL (shared-grouping quantiles) over `asap_sketchlib::HydraKllSketch`.
#[derive(Debug, Clone)]
pub struct HydraKllSketchAccumulator {
    pub inner: HydraKllSketch,
}

impl HydraKllSketchAccumulator {
    pub fn new(row_num: usize, col_num: usize, k: u16) -> Self {
        Self {
            inner: HydraKllSketch::new(row_num, col_num, k),
        }
    }

    pub fn update(&mut self, key: &KeyByLabelValues, value: f64) {
        self.inner.update(&key.to_semicolon_str(), value);
    }

    pub fn query_key(&self, key: &KeyByLabelValues, quantile: f64) -> f64 {
        self.inner.quantile(&key.to_semicolon_str(), quantile)
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

    fn approx_memory_bytes(&self) -> usize {
        // HydraKLL is a row*col grid of KLL sketches; typical instances
        // are on the order of tens of KiB. 32 KiB is a conservative
        // per-instance default.
        32 * 1024
    }
}

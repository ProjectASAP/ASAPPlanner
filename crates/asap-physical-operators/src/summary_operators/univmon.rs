//! One frequency state shared by count, distinct, L2 and entropy readouts.

use crate::{AggregateCore, AuxStats, KeyByLabelValues, SerializableToSink};
use crate::{AggregationType, Statistic};
use asap_sketchlib::{DataInput, UnivMon};
use serde_json::Value;
use std::collections::HashMap;

type Error = Box<dyn std::error::Error + Send + Sync>;

#[derive(Debug, Clone)]
pub struct UnivMonAccumulator {
    inner: UnivMon,
}

impl UnivMonAccumulator {
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

    pub fn from_bytes(bytes: &[u8]) -> Result<Self, Error> {
        let inner = UnivMon::deserialize_from_bytes(bytes)
            .map_err(|e| format!("invalid UnivMon state: {e}"))?;
        if !inner.accepts_standard_updates() {
            return Err(
                "terminal-mode UnivMon state cannot enter the standard-update accumulator".into(),
            );
        }
        Ok(Self { inner })
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

impl SerializableToSink for UnivMonAccumulator {
    fn serialize_to_json(&self) -> Value {
        serde_json::json!({"count": self.inner.bucket_size})
    }

    fn serialize_to_bytes(&self) -> Vec<u8> {
        self.inner
            .serialize_to_bytes()
            .expect("validated unit-frequency UnivMon state")
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
    fn type_name(&self) -> &'static str {
        "UnivMonAccumulator"
    }
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
    fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
        self
    }
    fn get_accumulator_type(&self) -> AggregationType {
        AggregationType::UnivMon
    }
    fn get_keys(&self) -> Option<Vec<KeyByLabelValues>> {
        None
    }
    fn reset_to_empty(&mut self) {
        self.inner.free();
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

    fn query_statistic(
        &self,
        statistic: Statistic,
        key: &Option<KeyByLabelValues>,
        _: &HashMap<String, String>,
    ) -> Result<f64, Error> {
        if key.is_some() {
            return Err("UnivMon population is selected by the catalog binding".into());
        }
        match statistic {
            Statistic::Count => Ok(self.inner.calc_l1()),
            Statistic::Cardinality => Ok(self.inner.calc_card()),
            Statistic::FrequencyL2 => Ok(self.inner.calc_l2()),
            Statistic::FrequencyEntropy => Ok(self.inner.calc_entropy()),
            _ => Err("unsupported UnivMon readout".into()),
        }
    }

    fn aux_stats(&self) -> AuxStats {
        AuxStats {
            count: Some(self.inner.bucket_size as u64),
            ..AuxStats::empty()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn read(state: &dyn AggregateCore, stat: Statistic) -> f64 {
        state.query_statistic(stat, &None, &HashMap::new()).unwrap()
    }

    /// Duplicate samples affect frequency but not cardinality, including signed zero.
    #[test]
    fn shared_readouts_survive_serialization() {
        let mut state = UnivMonAccumulator::new(32, 5, 1024, 4).unwrap();
        for value in [0.0, -0.0, 2.0, 2.0, f64::NAN] {
            state.insert_sample(value).unwrap();
        }
        let restored = UnivMonAccumulator::from_bytes(&state.serialize_to_bytes()).unwrap();
        for stat in [
            Statistic::Count,
            Statistic::Cardinality,
            Statistic::FrequencyL2,
            Statistic::FrequencyEntropy,
        ] {
            assert_eq!(read(&state, stat), read(&restored, stat));
        }
        assert_eq!(read(&restored, Statistic::Count), 4.0);
        assert!((read(&restored, Statistic::Cardinality) - 2.0).abs() < 0.01);
        assert!((read(&restored, Statistic::FrequencyL2) - 8.0f64.sqrt()).abs() < 0.01);
        assert!((read(&restored, Statistic::FrequencyEntropy) - 1.0).abs() < 0.01);
    }

    /// Terminal-mode serialization is valid sketchlib state but not this accumulator's update domain.
    #[test]
    fn terminal_state_is_rejected_before_ingestion_or_merge() {
        let mut state = UnivMon::init_univmon(4, 3, 16, 2);
        state.fast_insert(&DataInput::U64(1), 1);
        let bytes = state.serialize_to_bytes().unwrap();
        assert!(UnivMonAccumulator::from_bytes(&bytes).is_err());
        state.free();
        assert!(UnivMonAccumulator::from_bytes(&state.serialize_to_bytes().unwrap()).is_ok());
    }

    /// Pane merge preserves overlapping keys and reset removes the previous window.
    #[test]
    fn merge_and_reset_preserve_frequency_semantics() {
        let mut left = UnivMonAccumulator::new(32, 5, 1024, 4).unwrap();
        let mut right = left.clone();
        for value in [1.0, 2.0] {
            left.insert_sample(value).unwrap();
        }
        for value in [2.0, 3.0] {
            right.insert_sample(value).unwrap();
        }
        let merged = left.merge_with(&right).unwrap();
        assert_eq!(read(merged.as_ref(), Statistic::Count), 4.0);
        assert!((read(merged.as_ref(), Statistic::Cardinality) - 3.0).abs() < 0.01);
        left.reset_to_empty();
        assert_eq!(read(&left, Statistic::Count), 0.0);
        assert_eq!(read(&left, Statistic::FrequencyEntropy), 0.0);
        assert!(left
            .merge_with(&UnivMonAccumulator::new(16, 5, 1024, 4).unwrap())
            .is_err());
    }
}

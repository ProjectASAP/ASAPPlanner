//! ASAP type and trait adapter for sketchlib's Float64 weighted frequency kernel.
use crate::{values::Value, Error};
use crate::{AggregateCore, AggregationType, KeyByLabelValues, SerializableToSink, Statistic};
pub use asap_sketchlib::FrequencyAlgorithm;
use asap_sketchlib::{FrequencyIdentity, WeightedFrequency as Kernel, WeightedFrequencyError};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

fn adapt_error(error: WeightedFrequencyError) -> Error {
    match error {
        WeightedFrequencyError::Invalid(message) => Error::Invalid(message),
        WeightedFrequencyError::Update(message) => Error::Operator(message),
    }
}
fn identity(value: &Value) -> Result<FrequencyIdentity, Error> {
    Ok(match value {
        Value::Null => FrequencyIdentity::Null,
        Value::Bool(v) => FrequencyIdentity::Bool(*v),
        Value::Int64(v) => FrequencyIdentity::Int64(*v),
        Value::Float64(v) => FrequencyIdentity::Float64(*v),
        Value::Utf8(v) => FrequencyIdentity::Utf8(v.to_string()),
        _ => {
            return Err(Error::Invalid(
                "unsupported weighted frequency identity".into(),
            ))
        }
    })
}
fn value(identity: FrequencyIdentity) -> Value {
    match identity {
        FrequencyIdentity::Null => Value::Null,
        FrequencyIdentity::Bool(v) => Value::Bool(v),
        FrequencyIdentity::Int64(v) => Value::Int64(v),
        FrequencyIdentity::Float64(v) => Value::Float64(v),
        FrequencyIdentity::Utf8(v) => Value::Utf8(v.into()),
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(transparent)]
pub struct WeightedFrequency {
    inner: Kernel,
}
impl WeightedFrequency {
    pub(crate) fn configuration(
        kind: &planner_types::post_asap::SketchKind,
    ) -> Result<(FrequencyAlgorithm, usize, usize, usize), Error> {
        use planner_types::post_asap::{SketchAlgorithm as A, SketchParams as P};
        let (algorithm, width, depth, capacity) = match (kind.algorithm(), kind.params()) {
            (
                A::CmsWithHeap,
                P::CmsWithHeap {
                    width,
                    depth,
                    heap_size,
                },
            ) => (FrequencyAlgorithm::Cms, *width, *depth, *heap_size),
            (
                A::CountSketchWithHeap,
                P::CountSketchWithHeap {
                    width,
                    depth,
                    heap_size,
                },
            ) if depth % 2 == 1 => (FrequencyAlgorithm::CountSketch, *width, *depth, *heap_size),
            _ => {
                return Err(Error::Invalid(
                    "unsupported weighted frequency family or depth".into(),
                ))
            }
        };
        if width == 0 || depth == 0 || capacity == 0 {
            return Err(Error::Invalid(
                "invalid weighted frequency dimensions".into(),
            ));
        }
        Ok((algorithm, width as usize, depth as usize, capacity as usize))
    }

    pub(crate) fn algorithm(&self) -> FrequencyAlgorithm {
        self.inner.algorithm()
    }
    pub(crate) fn shape(&self) -> (usize, usize, usize) {
        self.inner.shape()
    }
    pub fn new(
        algorithm: FrequencyAlgorithm,
        width: usize,
        depth: usize,
        capacity: usize,
    ) -> Result<Self, Error> {
        Kernel::new(algorithm, width, depth, capacity)
            .map(|inner| Self { inner })
            .map_err(adapt_error)
    }
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, Error> {
        Kernel::from_bytes(bytes)
            .map(|inner| Self { inner })
            .map_err(adapt_error)
    }
    pub fn update(&mut self, values: &[Value], weight: f64) -> Result<(), Error> {
        let values = values.iter().map(identity).collect::<Result<Vec<_>, _>>()?;
        self.inner.update(&values, weight).map_err(adapt_error)
    }
    pub fn rows(&self, n: usize) -> Vec<Vec<Value>> {
        self.inner
            .topk(n)
            .into_iter()
            .map(|(items, score)| {
                let mut row = items.into_iter().map(value).collect::<Vec<_>>();
                row.push(Value::Float64(score));
                row
            })
            .collect()
    }
}
impl SerializableToSink for WeightedFrequency {
    fn serialize_to_json(&self) -> serde_json::Value {
        serde_json::to_value(self).expect("finite validated frequency state")
    }
    fn serialize_to_bytes(&self) -> Vec<u8> {
        self.inner.to_bytes()
    }
}
impl AggregateCore for WeightedFrequency {
    fn clone_boxed_core(&self) -> Box<dyn AggregateCore> {
        Box::new(self.clone())
    }
    fn type_name(&self) -> &'static str {
        "WeightedFrequency"
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
        let other = other
            .as_any()
            .downcast_ref::<Self>()
            .ok_or("weighted frequency state type mismatch")?;
        Ok(Box::new(Self {
            inner: self.inner.merge(&other.inner)?,
        }))
    }
    fn get_accumulator_type(&self) -> AggregationType {
        match self.inner.algorithm() {
            FrequencyAlgorithm::Cms => AggregationType::CountMinSketchWithHeap,
            FrequencyAlgorithm::CountSketch => AggregationType::CountSketchWithHeap,
        }
    }
    fn get_keys(&self) -> Option<Vec<KeyByLabelValues>> {
        None
    }
    fn query_statistic(
        &self,
        _: Statistic,
        _: &Option<KeyByLabelValues>,
        _: &HashMap<String, String>,
    ) -> Result<f64, Box<dyn std::error::Error + Send + Sync>> {
        Err("weighted frequency uses typed row readout".into())
    }
    fn approx_memory_bytes(&self) -> usize {
        self.inner.approx_memory_bytes()
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    // Signed fractional updates and merges retain numeric ranking, not magnitude ranking.
    #[test]
    fn count_sketch_signed_updates_roundtrip_and_merge() {
        let mut left = WeightedFrequency::new(FrequencyAlgorithm::CountSketch, 4096, 5, 8).unwrap();
        left.update(&[Value::Int64(1)], -10.5).unwrap();
        left.update(&[Value::Null], 0.125).unwrap();
        left.update(&[Value::Null], -0.0625).unwrap();
        let mut right =
            WeightedFrequency::new(FrequencyAlgorithm::CountSketch, 4096, 5, 8).unwrap();
        right.update(&[Value::Null], 0.25).unwrap();
        let merged = left.merge_with(&right).unwrap();
        let merged = merged.as_any().downcast_ref::<WeightedFrequency>().unwrap();
        let decoded = WeightedFrequency::from_bytes(&merged.serialize_to_bytes()).unwrap();
        let rows = decoded.rows(2);
        assert!(matches!(rows[0][0], Value::Null));
        assert!(matches!(rows[0][1], Value::Float64(0.3125)));
        assert!(matches!(rows[1][1], Value::Float64(-10.5)));
        assert!(left
            .merge_with(&WeightedFrequency::new(FrequencyAlgorithm::Cms, 4096, 5, 8).unwrap())
            .is_err());
        let before = left.serialize_to_bytes();
        for weight in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
            assert!(left.update(&[Value::Null], weight).is_err());
            assert_eq!(left.serialize_to_bytes(), before);
        }
    }

    // Invalid rates must not mutate state; typed keys cannot collide by formatting.
    #[test]
    fn fractional_updates_typed_identities_and_invalid_weights() {
        let mut state = WeightedFrequency::new(FrequencyAlgorithm::Cms, 4096, 5, 8).unwrap();
        state.update(&[Value::Int64(1)], 0.125).unwrap();
        state.update(&[Value::Int64(1)], 0.125).unwrap();
        state.update(&[Value::Utf8("1".into())], 0.5).unwrap();
        state.update(&[Value::Null], 0.75).unwrap();
        let before = state.serialize_to_bytes();
        let decoded = WeightedFrequency::from_bytes(&before).unwrap();
        assert_eq!(decoded.rows(8).len(), 3);
        assert!(WeightedFrequency::from_bytes(b"old integer state").is_err());
        for weight in [-1.0, f64::INFINITY, f64::NAN] {
            assert!(state.update(&[Value::Null], weight).is_err());
            assert_eq!(state.serialize_to_bytes(), before);
        }
        let rows = state.rows(8);
        assert_eq!(rows.len(), 3);
        assert!(matches!(rows[0][0], Value::Null));
        assert!(matches!(rows[1][0], Value::Utf8(_)));
        assert!(matches!(rows[2][1], Value::Float64(0.25)));
    }
    // Merge uses the same Float64 state representation and rejects other shapes.
    #[test]
    fn compatible_merge_preserves_fractional_weights() {
        let mut left = WeightedFrequency::new(FrequencyAlgorithm::Cms, 4096, 5, 8).unwrap();
        let mut right = left.clone();
        left.update(&[Value::Int64(7)], 0.125).unwrap();
        right.update(&[Value::Int64(7)], 0.25).unwrap();
        let merged = left.merge_with(&right).unwrap();
        let merged = merged.as_any().downcast_ref::<WeightedFrequency>().unwrap();
        assert!(matches!(merged.rows(1)[0][1], Value::Float64(0.375)));
        assert!(left
            .merge_with(&WeightedFrequency::new(FrequencyAlgorithm::Cms, 32, 5, 8).unwrap())
            .is_err());
    }
}

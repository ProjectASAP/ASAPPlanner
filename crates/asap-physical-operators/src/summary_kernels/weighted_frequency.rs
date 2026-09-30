//! ASAP type and trait adapter for sketchlib's Float64 weighted frequency kernel.
use crate::AggregateCore;
use crate::{values::Value, Error};
pub use asap_sketchlib::FrequencyAlgorithm;
use asap_sketchlib::{FrequencyIdentity, WeightedFrequency as Kernel, WeightedFrequencyError};
use serde::{Deserialize, Serialize};

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

    /// Algorithm and `(width, depth, capacity)` shape, so a deployment can check
    /// a decoded state against its declared family.
    pub fn algorithm(&self) -> FrequencyAlgorithm {
        self.inner.algorithm()
    }
    pub fn shape(&self) -> (usize, usize, usize) {
        self.inner.shape()
    }
    /// Sketchlib's versioned `WeightedFrequencyV1` bytes, for deployments that
    /// persist this state.
    pub fn to_bytes(&self) -> Vec<u8> {
        self.inner.to_bytes()
    }
    /// Decode and validate bytes written by [`Self::to_bytes`].
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, Error> {
        Kernel::from_bytes(bytes)
            .map(|inner| Self { inner })
            .map_err(adapt_error)
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
impl AggregateCore for WeightedFrequency {
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
        let other = other
            .as_any()
            .downcast_ref::<Self>()
            .ok_or("weighted frequency state type mismatch")?;
        Ok(Box::new(Self {
            inner: self.inner.merge(&other.inner)?,
        }))
    }
    fn approx_memory_bytes(&self) -> usize {
        self.inner.approx_memory_bytes()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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

    // Stored bytes restore the algorithm, shape and ranked rows; foreign bytes are rejected.
    #[test]
    fn bytes_round_trip() {
        let mut state = WeightedFrequency::new(FrequencyAlgorithm::CountSketch, 64, 3, 4).unwrap();
        state.update(&[Value::Utf8("a".into())], 2.5).unwrap();
        state.update(&[Value::Utf8("b".into())], 1.0).unwrap();
        let restored = WeightedFrequency::from_bytes(&state.to_bytes()).unwrap();
        assert!(matches!(
            restored.algorithm(),
            FrequencyAlgorithm::CountSketch
        ));
        assert_eq!(restored.shape(), (64, 3, 4));
        assert_eq!(
            format!("{:?}", restored.rows(2)),
            format!("{:?}", state.rows(2))
        );
        assert!(WeightedFrequency::from_bytes(b"not a frequency state").is_err());
    }
}

//! Float64 weighted CMS state with typed candidate identities. Each instance
//! represents one partition at one evaluation scope; updates never round rates
//! to integer counts. Candidate membership still requires Planner evidence.
use crate::{values::Value, Error};
use crate::{AggregateCore, AggregationType, KeyByLabelValues, SerializableToSink, Statistic};
use serde::{Deserialize, Serialize};
use std::{
    cmp::Ordering,
    collections::{BinaryHeap, HashMap},
    sync::Arc,
};

#[derive(Clone, Debug, Serialize, Deserialize)]
enum Identity {
    Null,
    Bool(bool),
    Int64(i64),
    Float64(f64),
    Utf8(String),
}
impl Identity {
    fn from_value(value: &Value) -> Result<Self, Error> {
        Ok(match value {
            Value::Null => Self::Null,
            Value::Bool(v) => Self::Bool(*v),
            Value::Int64(v) => Self::Int64(*v),
            Value::Float64(v) if v.is_finite() => Self::Float64(if *v == 0.0 { 0.0 } else { *v }),
            Value::Utf8(v) => Self::Utf8(v.to_string()),
            _ => return Err(Error::Invalid("unsupported weighted CMS identity".into())),
        })
    }
    fn value(&self) -> Value {
        match self {
            Self::Null => Value::Null,
            Self::Bool(v) => Value::Bool(*v),
            Self::Int64(v) => Value::Int64(*v),
            Self::Float64(v) => Value::Float64(*v),
            Self::Utf8(v) => Value::Utf8(Arc::from(v.as_str())),
        }
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
struct Candidate {
    identity: Vec<Identity>,
    key: Vec<u8>,
    score: f64,
}
impl PartialEq for Candidate {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == Ordering::Equal
    }
}
impl Eq for Candidate {}
impl PartialOrd for Candidate {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}
impl Ord for Candidate {
    fn cmp(&self, other: &Self) -> Ordering {
        // The weakest candidate is the root of this bounded min-heap.
        other
            .score
            .total_cmp(&self.score)
            .then_with(|| other.key.cmp(&self.key))
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct WeightedCms {
    width: usize,
    depth: usize,
    capacity: usize,
    cells: Vec<f64>,
    candidates: BinaryHeap<Candidate>,
}
impl WeightedCms {
    pub(crate) fn shape(&self) -> (usize, usize, usize) {
        (self.width, self.depth, self.capacity)
    }
    pub fn new(width: usize, depth: usize, capacity: usize) -> Result<Self, Error> {
        let len = width
            .checked_mul(depth)
            .filter(|_| width > 0 && depth > 0 && capacity > 0)
            .ok_or_else(|| Error::Invalid("invalid weighted CMS dimensions".into()))?;
        let mut cells = Vec::new();
        cells
            .try_reserve_exact(len)
            .map_err(|_| Error::Invalid("weighted CMS allocation failed".into()))?;
        cells.resize(len, 0.0);
        Ok(Self {
            width,
            depth,
            capacity,
            cells,
            candidates: BinaryHeap::new(),
        })
    }
    /// Decode only this kernel's versioned Float64 representation. Integer CMS
    /// wire frames are different representations and are not accepted here.
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, Error> {
        use bincode::Options;
        let bytes = bytes
            .strip_prefix(b"ASAP-WCMS-1\0")
            .ok_or_else(|| Error::Invalid("weighted CMS format/version mismatch".into()))?;
        let mut state: Self = bincode::DefaultOptions::new()
            .with_fixint_encoding()
            .with_limit(bytes.len() as u64)
            .reject_trailing_bytes()
            .deserialize(bytes)
            .map_err(|e| Error::Invalid(e.to_string()))?;
        if state.width == 0
            || state.depth == 0
            || state.capacity == 0
            || state.width.checked_mul(state.depth) != Some(state.cells.len())
            || state.cells.iter().any(|v| !v.is_finite() || *v < 0.0)
            || state.candidates.len() > state.capacity
        {
            return Err(Error::Invalid("invalid weighted CMS state".into()));
        }
        for candidate in &state.candidates {
            if candidate
                .identity
                .iter()
                .any(|v| matches!(v, Identity::Float64(n) if !n.is_finite()))
                || bincode::serialize(&candidate.identity)
                    .map_err(|e| Error::Invalid(e.to_string()))?
                    != candidate.key
            {
                return Err(Error::Invalid("invalid weighted CMS identity".into()));
            }
        }
        state.retain(state.candidates.iter().cloned().collect());
        Ok(state)
    }
    fn indexes(&self, key: &[u8]) -> impl Iterator<Item = usize> + '_ {
        let key = key.to_vec();
        (0..self.depth).map(move |row| {
            row * self.width
                + (xxhash_rust::xxh64::xxh64(&key, row as u64) % self.width as u64) as usize
        })
    }
    fn estimate(&self, key: &[u8]) -> f64 {
        self.indexes(key)
            .map(|i| self.cells[i])
            .fold(f64::INFINITY, f64::min)
    }
    fn retain(&mut self, mut candidates: Vec<Candidate>) {
        candidates.sort_by(|a, b| a.key.cmp(&b.key));
        candidates.dedup_by(|a, b| a.key == b.key);
        self.candidates.clear();
        for mut candidate in candidates {
            candidate.score = self.estimate(&candidate.key);
            self.candidates.push(candidate);
            if self.candidates.len() > self.capacity {
                self.candidates.pop();
            }
        }
    }
    pub fn update(&mut self, values: &[Value], weight: f64) -> Result<(), Error> {
        if !weight.is_finite() || weight < 0.0 {
            return Err(Error::Operator(
                "weighted CMS requires finite nonnegative rates".into(),
            ));
        }
        let identity = values
            .iter()
            .map(Identity::from_value)
            .collect::<Result<Vec<_>, _>>()?;
        let key = bincode::serialize(&identity).map_err(|e| Error::Operator(e.to_string()))?;
        let indexes = self.indexes(&key).collect::<Vec<_>>();
        if indexes
            .iter()
            .any(|&i| !(self.cells[i] + weight).is_finite())
        {
            return Err(Error::Operator("weighted CMS sum overflow".into()));
        }
        for i in indexes {
            self.cells[i] += weight;
        }
        let mut candidates = self.candidates.iter().cloned().collect::<Vec<_>>();
        candidates.push(Candidate {
            identity,
            key,
            score: 0.0,
        });
        self.retain(candidates);
        Ok(())
    }
    pub fn rows(&self, n: usize) -> Vec<Vec<Value>> {
        let mut candidates = self.candidates.iter().collect::<Vec<_>>();
        candidates.sort_by(|a, b| b.score.total_cmp(&a.score).then_with(|| a.key.cmp(&b.key)));
        candidates
            .into_iter()
            .take(n)
            .map(|c| {
                let mut row = c.identity.iter().map(Identity::value).collect::<Vec<_>>();
                row.push(Value::Float64(c.score));
                row
            })
            .collect()
    }
}
impl SerializableToSink for WeightedCms {
    fn serialize_to_json(&self) -> serde_json::Value {
        serde_json::to_value(self).expect("finite validated CMS state")
    }
    fn serialize_to_bytes(&self) -> Vec<u8> {
        let mut bytes = b"ASAP-WCMS-1\0".to_vec();
        bytes.extend(bincode::serialize(self).expect("serializable CMS state"));
        bytes
    }
}
impl AggregateCore for WeightedCms {
    fn clone_boxed_core(&self) -> Box<dyn AggregateCore> {
        Box::new(self.clone())
    }
    fn type_name(&self) -> &'static str {
        "WeightedCms"
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
            .ok_or("weighted CMS state type mismatch")?;
        if (self.width, self.depth, self.capacity) != (other.width, other.depth, other.capacity) {
            return Err("weighted CMS shape mismatch".into());
        }
        let mut result = self.clone();
        for (value, rhs) in result.cells.iter_mut().zip(&other.cells) {
            *value += rhs;
            if !value.is_finite() {
                return Err("weighted CMS merge overflow".into());
            }
        }
        result.retain(
            self.candidates
                .iter()
                .chain(&other.candidates)
                .cloned()
                .collect(),
        );
        Ok(Box::new(result))
    }
    fn get_accumulator_type(&self) -> AggregationType {
        AggregationType::CountMinSketchWithHeap
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
        Err("weighted CMS uses typed row readout".into())
    }
    fn approx_memory_bytes(&self) -> usize {
        std::mem::size_of::<Self>()
            + self.cells.capacity() * 8
            + self
                .candidates
                .iter()
                .map(|c| {
                    std::mem::size_of::<Candidate>()
                        + c.key.capacity()
                        + c.identity
                            .iter()
                            .map(|v| {
                                std::mem::size_of::<Identity>()
                                    + if let Identity::Utf8(s) = v {
                                        s.capacity()
                                    } else {
                                        0
                                    }
                            })
                            .sum::<usize>()
                })
                .sum::<usize>()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    // Invalid rates must not mutate state; typed keys cannot collide by formatting.
    #[test]
    fn fractional_updates_typed_identities_and_invalid_weights() {
        let mut state = WeightedCms::new(4096, 5, 8).unwrap();
        state.update(&[Value::Int64(1)], 0.125).unwrap();
        state.update(&[Value::Int64(1)], 0.125).unwrap();
        state.update(&[Value::Utf8("1".into())], 0.5).unwrap();
        state.update(&[Value::Null], 0.75).unwrap();
        let before = state.serialize_to_bytes();
        let decoded = WeightedCms::from_bytes(&before).unwrap();
        assert_eq!(decoded.rows(8).len(), 3);
        assert!(WeightedCms::from_bytes(b"old integer state").is_err());
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
        let mut left = WeightedCms::new(4096, 5, 8).unwrap();
        let mut right = left.clone();
        left.update(&[Value::Int64(7)], 0.125).unwrap();
        right.update(&[Value::Int64(7)], 0.25).unwrap();
        let merged = left.merge_with(&right).unwrap();
        let merged = merged.as_any().downcast_ref::<WeightedCms>().unwrap();
        assert!(matches!(merged.rows(1)[0][1], Value::Float64(0.375)));
        assert!(left
            .merge_with(&WeightedCms::new(32, 5, 8).unwrap())
            .is_err());
    }
}

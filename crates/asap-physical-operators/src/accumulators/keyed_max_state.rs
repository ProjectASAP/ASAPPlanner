use crate::{
    AggregateCore, AggregationType, KeyByLabelValues, MergeableAccumulator,
    MultipleSubpopulationAggregate, SerializableToSink,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;

use crate::Statistic;

/// Exact per-key maximum over many populations, mergeable by comparison.
///
/// The minimum direction is
/// [`KeyedMinState`](super::keyed_min_state::KeyedMinState),
/// a separate type: these used to be one `MultipleMinMaxAccumulator` whose
/// direction lived in a `sub_type` string that every layer above had to carry
/// alongside the family.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct KeyedMaxState {
    pub values: HashMap<KeyByLabelValues, f64>,
}

impl KeyedMaxState {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn new_with_values(values: HashMap<KeyByLabelValues, f64>) -> Self {
        Self { values }
    }

    pub fn update(&mut self, key: KeyByLabelValues, value: f64) {
        let current = self.values.entry(key).or_insert(f64::NEG_INFINITY);
        if value > *current {
            *current = value;
        }
    }

    pub fn add_value(&mut self, key: KeyByLabelValues, value: f64) {
        self.values.insert(key, value);
    }

    pub fn deserialize_from_json(data: &Value) -> Result<Self, Box<dyn std::error::Error>> {
        let values_data = data["values"]
            .as_object()
            .ok_or("Missing or invalid 'values' field")?;

        let mut values = HashMap::new();
        for (key_str, value) in values_data {
            let key_json: Value = serde_json::from_str(key_str)?;
            let key = KeyByLabelValues::deserialize_from_json(&key_json)?;
            let val = value.as_f64().ok_or("Invalid value")?;
            values.insert(key, val);
        }

        Ok(Self { values })
    }

    pub fn deserialize_from_bytes(buffer: &[u8]) -> Result<Self, Box<dyn std::error::Error>> {
        let mut offset = 0;

        // Read number of entries
        if buffer.len() < 4 {
            return Err("Buffer too short for entry count".into());
        }
        let num_entries = u32::from_le_bytes([
            buffer[offset],
            buffer[offset + 1],
            buffer[offset + 2],
            buffer[offset + 3],
        ]) as usize;
        offset += 4;

        let mut values = HashMap::new();

        for _ in 0..num_entries {
            // Read key length and data
            if buffer.len() < offset + 4 {
                return Err("Buffer too short for key length".into());
            }
            let key_length = u32::from_le_bytes([
                buffer[offset],
                buffer[offset + 1],
                buffer[offset + 2],
                buffer[offset + 3],
            ]) as usize;
            offset += 4;

            if buffer.len() < offset + key_length {
                return Err("Buffer too short for key data".into());
            }
            let key =
                KeyByLabelValues::deserialize_from_bytes(&buffer[offset..offset + key_length])?;
            offset += key_length;

            // Read value
            if buffer.len() < offset + 8 {
                return Err("Buffer too short for value".into());
            }
            let value = f64::from_le_bytes([
                buffer[offset],
                buffer[offset + 1],
                buffer[offset + 2],
                buffer[offset + 3],
                buffer[offset + 4],
                buffer[offset + 5],
                buffer[offset + 6],
                buffer[offset + 7],
            ]);
            offset += 8;

            values.insert(key, value);
        }

        Ok(Self { values })
    }
}

impl SerializableToSink for KeyedMaxState {
    fn serialize_to_json(&self) -> Value {
        let mut values_obj = serde_json::Map::new();
        for (key, value) in &self.values {
            let key_json = key.serialize_to_json();
            let key_str = serde_json::to_string(&key_json).unwrap();
            values_obj.insert(
                key_str,
                Value::Number(serde_json::Number::from_f64(*value).unwrap()),
            );
        }

        serde_json::json!({ "values": values_obj })
    }

    fn serialize_to_bytes(&self) -> Vec<u8> {
        let mut buffer = Vec::new();

        // Write number of entries
        buffer.extend_from_slice(&(self.values.len() as u32).to_le_bytes());

        // Write each key-value pair
        for (key, value) in &self.values {
            let key_bytes = key.serialize_to_bytes();

            // Write key length and data
            buffer.extend_from_slice(&(key_bytes.len() as u32).to_le_bytes());
            buffer.extend_from_slice(&key_bytes);

            // Write value
            buffer.extend_from_slice(&value.to_le_bytes());
        }

        buffer
    }
}

impl AggregateCore for KeyedMaxState {
    fn clone_boxed_core(&self) -> Box<dyn AggregateCore> {
        Box::new(self.clone())
    }

    fn type_name(&self) -> &'static str {
        "KeyedMaxState"
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
        if other.get_accumulator_type() != self.get_accumulator_type() {
            return Err(format!(
                "Cannot merge KeyedMaxState with {}",
                other.get_accumulator_type()
            )
            .into());
        }

        let other_multiple = other
            .as_any()
            .downcast_ref::<KeyedMaxState>()
            .ok_or("Failed to downcast to KeyedMaxState")?;

        let merged = Self::merge_accumulators(vec![self.clone(), other_multiple.clone()])?;

        Ok(Box::new(merged))
    }

    fn get_accumulator_type(&self) -> AggregationType {
        AggregationType::Max
    }

    fn approx_memory_bytes(&self) -> usize {
        const BYTES_PER_ENTRY: usize = 96;
        std::mem::size_of::<Self>() + self.values.len() * BYTES_PER_ENTRY
    }

    fn get_keys(&self) -> Option<Vec<KeyByLabelValues>> {
        Some(self.values.keys().cloned().collect())
    }

    fn query_statistic(
        &self,
        statistic: crate::Statistic,
        key: &Option<KeyByLabelValues>,
        query_kwargs: &std::collections::HashMap<String, String>,
    ) -> Result<f64, Box<dyn std::error::Error + Send + Sync>> {
        use crate::MultipleSubpopulationAggregate;
        let key_val = key.as_ref().ok_or("Key required for KeyedMaxState")?;
        self.query(statistic, key_val, Some(query_kwargs))
    }
}

impl MultipleSubpopulationAggregate for KeyedMaxState {
    fn query(
        &self,
        statistic: Statistic,
        key: &KeyByLabelValues,
        _query_kwargs: Option<&HashMap<String, String>>,
    ) -> Result<f64, Box<dyn std::error::Error + Send + Sync>> {
        match statistic {
            Statistic::Max => self
                .values
                .get(key)
                .copied()
                .ok_or_else(|| format!("Key {key} not found in KeyedMaxState").into()),
            other => Err(format!("Unsupported statistic in KeyedMaxState: {other:?}").into()),
        }
    }

    fn clone_boxed(&self) -> Box<dyn MultipleSubpopulationAggregate> {
        Box::new(self.clone())
    }
}

impl MergeableAccumulator<KeyedMaxState> for KeyedMaxState {
    fn merge_accumulators(
        accumulators: Vec<KeyedMaxState>,
    ) -> Result<KeyedMaxState, Box<dyn std::error::Error + Send + Sync>> {
        if accumulators.is_empty() {
            return Err("No accumulators to merge".into());
        }

        let mut result = KeyedMaxState::new();

        for acc in accumulators {
            for (key, value) in acc.values {
                result.update(key, value);
            }
        }

        Ok(result)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(value: &str) -> KeyByLabelValues {
        KeyByLabelValues::new_with_labels(vec![value.to_string()])
    }

    #[test]
    fn keeps_the_largest_per_key() {
        let mut acc = KeyedMaxState::new();
        acc.update(key("a"), 10.0);
        acc.update(key("a"), 5.0);
        acc.update(key("a"), 15.0);
        acc.update(key("b"), 7.0);

        assert_eq!(acc.query(Statistic::Max, &key("a"), None).unwrap(), 15.0);
        assert_eq!(acc.query(Statistic::Max, &key("b"), None).unwrap(), 7.0);
    }

    #[test]
    fn refuses_the_opposite_statistic_and_unknown_keys() {
        let mut acc = KeyedMaxState::new();
        acc.update(key("a"), 1.0);
        assert!(acc.query(Statistic::Min, &key("a"), None).is_err());
        assert!(acc.query(Statistic::Max, &key("missing"), None).is_err());
    }

    #[test]
    fn merges_per_key() {
        let mut left = KeyedMaxState::new();
        left.update(key("a"), 10.0);
        let mut right = KeyedMaxState::new();
        right.update(key("a"), 5.0);
        right.update(key("b"), 3.0);

        let merged =
            <KeyedMaxState as MergeableAccumulator<KeyedMaxState>>::merge_accumulators(vec![
                left, right,
            ])
            .unwrap();

        assert_eq!(merged.query(Statistic::Max, &key("a"), None).unwrap(), 10.0);
        assert_eq!(merged.query(Statistic::Max, &key("b"), None).unwrap(), 3.0);
    }

    #[test]
    fn refuses_to_merge_with_the_opposite_direction() {
        use super::super::keyed_min_state::KeyedMinState;
        let mine = KeyedMaxState::new();
        let theirs = KeyedMinState::new();
        assert!(mine.merge_with(&theirs).is_err());
    }

    #[test]
    fn round_trips_through_both_serializations() {
        let mut acc = KeyedMaxState::new();
        acc.update(key("a"), 4.0);

        let json = acc.serialize_to_json();
        let from_json = KeyedMaxState::deserialize_from_json(&json).unwrap();
        assert_eq!(
            from_json.query(Statistic::Max, &key("a"), None).unwrap(),
            4.0
        );

        let bytes = acc.serialize_to_bytes();
        let from_bytes = KeyedMaxState::deserialize_from_bytes(&bytes).unwrap();
        assert_eq!(
            from_bytes.query(Statistic::Max, &key("a"), None).unwrap(),
            4.0
        );
    }
}

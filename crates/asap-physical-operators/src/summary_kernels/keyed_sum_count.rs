use crate::{
    AggregateCore, AggregationType, KeyByLabelValues, MergeableAccumulator,
    MultipleSubpopulationAggregate, SerializableToSink,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;

use crate::Statistic;
use planner_types::post_asap::ExactKind;

fn sum_family() -> ExactKind {
    ExactKind::Sum
}

/// Accumulator that maintains separate sum values for multiple keys
/// Allows querying sums for specific label combinations
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KeyedSumCountAccumulator {
    #[serde(default = "sum_family")]
    pub family: ExactKind,
    pub sums: HashMap<KeyByLabelValues, f64>,
    #[serde(default)]
    pub counts: HashMap<KeyByLabelValues, u64>,
}

impl KeyedSumCountAccumulator {
    pub fn new() -> Self {
        Self::for_family(ExactKind::Sum)
    }

    pub fn for_family(family: ExactKind) -> Self {
        assert!(matches!(family, ExactKind::Sum | ExactKind::Count));
        Self {
            family,
            sums: HashMap::new(),
            counts: HashMap::new(),
        }
    }

    pub fn update(&mut self, key: KeyByLabelValues, value: f64) {
        let is_new = !self.sums.contains_key(&key);
        *self.sums.entry(key.clone()).or_insert(0.0) += value;
        if let Some(count) = self.counts.get(&key).copied() {
            if let Some(next) = count.checked_add(1).filter(|next| *next != u64::MAX) {
                self.counts.insert(key, next);
            } else {
                self.counts.remove(&key);
            }
        } else if is_new {
            self.counts.insert(key, 1);
        }
    }

    pub fn add_sum(&mut self, key: KeyByLabelValues, sum: f64) {
        self.counts.remove(&key);
        self.sums.insert(key, sum);
    }

    pub fn deserialize_from_json(data: &Value) -> Result<Self, Box<dyn std::error::Error>> {
        let sums_data = data["sums"]
            .as_object()
            .ok_or("Missing or invalid 'sums' field")?;

        let mut sums = HashMap::new();
        for (key_str, value) in sums_data {
            let key_json: Value = serde_json::from_str(key_str)?;
            let key = KeyByLabelValues::deserialize_from_json(&key_json)?;
            let sum = value.as_f64().ok_or("Invalid sum value")?;
            sums.insert(key, sum);
        }

        let mut counts = HashMap::new();
        if let Some(counts_data) = data.get("counts").and_then(Value::as_object) {
            for (key_str, value) in counts_data {
                let key_json: Value = serde_json::from_str(key_str)?;
                let key = KeyByLabelValues::deserialize_from_json(&key_json)?;
                let count = value.as_u64().ok_or("Invalid count value")?;
                if !sums.contains_key(&key) {
                    return Err("Count key missing from sums".into());
                }
                counts.insert(key, count);
            }
        }
        let family = match data.get("family").and_then(Value::as_str) {
            None | Some("Sum") => ExactKind::Sum,
            Some("Count") => ExactKind::Count,
            _ => return Err("Invalid keyed additive family".into()),
        };
        Ok(Self {
            family,
            sums,
            counts,
        })
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

        let mut sums = HashMap::new();
        let mut keys = Vec::new();

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

            // Read sum value
            if buffer.len() < offset + 8 {
                return Err("Buffer too short for sum value".into());
            }
            let sum = f64::from_le_bytes([
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

            keys.push(key.clone());
            sums.insert(key, sum);
        }
        let remaining = buffer.len() - offset;
        let count_bytes = num_entries
            .checked_mul(8)
            .ok_or("Count section too large")?;
        if remaining != 0 && remaining != count_bytes && remaining != count_bytes + 1 {
            return Err("Invalid count section length".into());
        }
        let mut counts = HashMap::new();
        if count_bytes != 0 && remaining >= count_bytes {
            for key in keys {
                let count = u64::from_le_bytes(buffer[offset..offset + 8].try_into()?);
                offset += 8;
                if count != u64::MAX {
                    counts.insert(key, count);
                }
            }
        }
        let family = if remaining == count_bytes + 1 {
            match buffer[offset] {
                0 => ExactKind::Sum,
                1 => ExactKind::Count,
                _ => return Err("Invalid keyed additive family tag".into()),
            }
        } else {
            ExactKind::Sum
        };
        Ok(Self {
            family,
            sums,
            counts,
        })
    }
}

impl Default for KeyedSumCountAccumulator {
    fn default() -> Self {
        Self::new()
    }
}

impl SerializableToSink for KeyedSumCountAccumulator {
    fn serialize_to_json(&self) -> Value {
        let mut sums_obj = serde_json::Map::new();
        for (key, sum) in &self.sums {
            let key_json = key.serialize_to_json();
            let key_str = serde_json::to_string(&key_json).unwrap();
            sums_obj.insert(
                key_str,
                Value::Number(serde_json::Number::from_f64(*sum).unwrap()),
            );
        }

        let mut counts_obj = serde_json::Map::new();
        for (key, count) in &self.counts {
            let key_str = serde_json::to_string(&key.serialize_to_json()).unwrap();
            counts_obj.insert(key_str, Value::from(*count));
        }

        serde_json::json!({
            "family": if self.family == ExactKind::Count { "Count" } else { "Sum" },
            "sums": sums_obj,
            "counts": counts_obj
        })
    }

    fn serialize_to_bytes(&self) -> Vec<u8> {
        let mut buffer = Vec::new();

        // Write number of entries
        buffer.extend_from_slice(&(self.sums.len() as u32).to_le_bytes());

        // Write each key-value pair
        let mut ordered_keys = Vec::with_capacity(self.sums.len());
        for (key, sum) in &self.sums {
            ordered_keys.push(key);
            let key_bytes = key.serialize_to_bytes();

            // Write key length and data
            buffer.extend_from_slice(&(key_bytes.len() as u32).to_le_bytes());
            buffer.extend_from_slice(&key_bytes);

            // Write sum value
            buffer.extend_from_slice(&sum.to_le_bytes());
        }

        for key in ordered_keys {
            buffer.extend_from_slice(
                &self
                    .counts
                    .get(key)
                    .copied()
                    .unwrap_or(u64::MAX)
                    .to_le_bytes(),
            );
        }

        buffer.push(if self.family == ExactKind::Count {
            1
        } else {
            0
        });

        buffer
    }
}

impl AggregateCore for KeyedSumCountAccumulator {
    fn clone_boxed_core(&self) -> Box<dyn AggregateCore> {
        Box::new(self.clone())
    }

    fn type_name(&self) -> &'static str {
        "KeyedSumCountAccumulator"
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
        // Check if other is also a KeyedSumCountAccumulator
        if other.get_accumulator_type() != self.get_accumulator_type() {
            return Err(format!(
                "Cannot merge KeyedSumCountAccumulator with {}",
                other.get_accumulator_type()
            )
            .into());
        }

        // Downcast to KeyedSumCountAccumulator
        let other_multiple_sum = other
            .as_any()
            .downcast_ref::<KeyedSumCountAccumulator>()
            .ok_or("Failed to downcast to KeyedSumCountAccumulator")?;

        // Use the existing merge_accumulators method
        let merged = Self::merge_accumulators(vec![self.clone(), other_multiple_sum.clone()])?;

        Ok(Box::new(merged))
    }

    fn get_accumulator_type(&self) -> AggregationType {
        if self.family == ExactKind::Count {
            AggregationType::Count
        } else {
            AggregationType::Sum
        }
    }

    fn approx_memory_bytes(&self) -> usize {
        // HashMap<KeyByLabelValues, f64>. Label strings dominate; use a
        // conservative per-entry estimate plus HashMap overhead.
        const BYTES_PER_ENTRY: usize = 112;
        std::mem::size_of::<Self>() + self.sums.len() * BYTES_PER_ENTRY
    }

    fn get_keys(&self) -> Option<Vec<KeyByLabelValues>> {
        Some(self.sums.keys().cloned().collect())
    }

    fn query_statistic(
        &self,
        statistic: crate::Statistic,
        key: &Option<KeyByLabelValues>,
        query_kwargs: &std::collections::HashMap<String, String>,
    ) -> Result<f64, Box<dyn std::error::Error + Send + Sync>> {
        use crate::MultipleSubpopulationAggregate;
        let key_val = key
            .as_ref()
            .ok_or("Key required for KeyedSumCountAccumulator")?;
        self.query(statistic, key_val, Some(query_kwargs))
    }
}

impl MultipleSubpopulationAggregate for KeyedSumCountAccumulator {
    fn query(
        &self,
        statistic: Statistic,
        key: &KeyByLabelValues,
        _query_kwargs: Option<&HashMap<String, String>>,
    ) -> Result<f64, Box<dyn std::error::Error + Send + Sync>> {
        match (&self.family, statistic) {
            (ExactKind::Sum, Statistic::Sum) => self.sums.get(key).copied().ok_or_else(|| {
                "Key not found in KeyedSumCountAccumulator"
                    .to_string()
                    .into()
            }),
            (ExactKind::Count, Statistic::Count) => self
                .counts
                .get(key)
                .map(|count| *count as f64)
                .ok_or_else(|| {
                    "Sample count unavailable in KeyedSumCountAccumulator"
                        .to_string()
                        .into()
                }),
            _ => Err(
                format!("Unsupported statistic in KeyedSumCountAccumulator: {statistic:?}").into(),
            ),
        }
    }

    fn clone_boxed(&self) -> Box<dyn MultipleSubpopulationAggregate> {
        Box::new(self.clone())
    }
}

impl MergeableAccumulator<KeyedSumCountAccumulator> for KeyedSumCountAccumulator {
    fn merge_accumulators(
        accumulators: Vec<KeyedSumCountAccumulator>,
    ) -> Result<KeyedSumCountAccumulator, Box<dyn std::error::Error + Send + Sync>> {
        if accumulators.is_empty() {
            return Err("No accumulators to merge".into());
        }

        let family = accumulators[0].family.clone();
        if accumulators.iter().any(|acc| acc.family != family) {
            return Err("Cannot merge different keyed additive families".into());
        }
        let mut result = KeyedSumCountAccumulator::for_family(family);

        for acc in accumulators {
            for key in acc.sums.keys() {
                match (
                    result.counts.get(key).copied(),
                    acc.counts.get(key).copied(),
                ) {
                    (None, Some(count)) if !result.sums.contains_key(key) => {
                        result.counts.insert(key.clone(), count);
                    }
                    (Some(existing), Some(count)) => {
                        if let Some(total) = existing.checked_add(count) {
                            result.counts.insert(key.clone(), total);
                        } else {
                            result.counts.remove(key);
                        }
                    }
                    _ => {
                        result.counts.remove(key);
                    }
                }
            }
            for (key, sum) in acc.sums {
                *result.sums.entry(key).or_insert(0.0) += sum;
            }
        }

        Ok(result)
    }
}

#[cfg(test)]
mod tests {
    use std::vec;

    use super::*;

    #[test]
    fn test_keyed_sum_count_accumulator_creation() {
        let acc = KeyedSumCountAccumulator::new();
        assert!(acc.sums.is_empty());
    }

    #[test]
    fn test_keyed_sum_count_accumulator_update() {
        let mut acc = KeyedSumCountAccumulator::new();

        let key1 = KeyByLabelValues::new_with_labels(vec!["web".to_string()]);

        let key2 = KeyByLabelValues::new_with_labels(vec!["api".to_string()]);

        acc.update(key1.clone(), 10.0);
        acc.update(key2.clone(), 20.0);
        acc.update(key1.clone(), 5.0); // Should add to existing

        assert_eq!(acc.sums.get(&key1), Some(&15.0));
        assert_eq!(acc.sums.get(&key2), Some(&20.0));
    }

    #[test]
    fn grouped_count_reads_sample_count_and_survives_merge_and_round_trip() {
        let key = KeyByLabelValues::new_with_labels(vec!["web".to_string()]);
        let mut first = KeyedSumCountAccumulator::for_family(ExactKind::Count);
        first.update(key.clone(), 10.0);
        first.update(key.clone(), 20.0);
        let mut second = KeyedSumCountAccumulator::for_family(ExactKind::Count);
        second.update(key.clone(), 7.0);
        let merged = KeyedSumCountAccumulator::merge_accumulators(vec![first, second]).unwrap();
        for acc in [
            merged.clone(),
            KeyedSumCountAccumulator::deserialize_from_json(&merged.serialize_to_json()).unwrap(),
            KeyedSumCountAccumulator::deserialize_from_bytes(&merged.serialize_to_bytes()).unwrap(),
        ] {
            assert_eq!(acc.family, ExactKind::Count);
            assert!(acc.query(Statistic::Sum, &key, None).is_err());
            assert_eq!(acc.query(Statistic::Count, &key, None).unwrap(), 3.0);
        }
    }

    #[test]
    fn keyed_additive_merge_rejects_different_planner_families() {
        assert!(KeyedSumCountAccumulator::merge_accumulators(vec![
            KeyedSumCountAccumulator::for_family(ExactKind::Sum),
            KeyedSumCountAccumulator::for_family(ExactKind::Count),
        ])
        .is_err());
    }

    #[test]
    fn test_keyed_sum_count_accumulator_query() {
        let mut acc = KeyedSumCountAccumulator::new();

        let key = KeyByLabelValues::new_with_labels(vec!["service".to_string()]);

        acc.add_sum(key.clone(), 42.0);

        // Test total queries (querying with the specific key)
        assert_eq!(
            crate::MultipleSubpopulationAggregate::query(&acc, Statistic::Sum, &key, None).unwrap(),
            42.0
        );

        // Test error cases
        assert!(
            crate::MultipleSubpopulationAggregate::query(&acc, Statistic::Min, &key, None).is_err()
        );
    }

    #[test]
    fn test_keyed_sum_count_accumulator_get_keys() {
        let mut acc = KeyedSumCountAccumulator::new();

        let key1 = KeyByLabelValues::new_with_labels(vec!["web".to_string()]);

        let key2 = KeyByLabelValues::new_with_labels(vec!["api".to_string()]);

        acc.add_sum(key1.clone(), 10.0);
        acc.add_sum(key2.clone(), 20.0);

        let keys = crate::AggregateCore::get_keys(&acc).unwrap();
        assert_eq!(keys.len(), 2);
        assert!(keys.contains(&key1));
        assert!(keys.contains(&key2));
    }

    #[test]
    fn test_keyed_sum_count_accumulator_merge() {
        let mut acc1 = KeyedSumCountAccumulator::new();
        let mut acc2 = KeyedSumCountAccumulator::new();

        let key1 = KeyByLabelValues::new_with_labels(vec!["web".to_string()]);

        let key2 = KeyByLabelValues::new_with_labels(vec!["api".to_string()]);

        acc1.add_sum(key1.clone(), 10.0);
        acc1.add_sum(key2.clone(), 20.0);

        acc2.add_sum(key1.clone(), 5.0); // Same key, different accumulator

        let merged = <KeyedSumCountAccumulator as MergeableAccumulator<KeyedSumCountAccumulator>>::merge_accumulators(vec![acc1, acc2]).unwrap();

        assert_eq!(merged.sums.get(&key1), Some(&15.0)); // Should be merged
        assert_eq!(merged.sums.get(&key2), Some(&20.0)); // Should be preserved
    }

    #[test]
    fn test_keyed_sum_count_accumulator_serialization() {
        let mut acc = KeyedSumCountAccumulator::new();

        let key = KeyByLabelValues::new_with_labels(vec!["service".to_string()]);

        acc.add_sum(key.clone(), 42.5);

        // Test JSON serialization
        let json = acc.serialize_to_json();
        let deserialized = KeyedSumCountAccumulator::deserialize_from_json(&json).unwrap();
        assert_eq!(deserialized.sums.get(&key), Some(&42.5));

        // Test byte serialization
        let bytes = acc.serialize_to_bytes();
        let deserialized_bytes = KeyedSumCountAccumulator::deserialize_from_bytes(&bytes).unwrap();
        assert_eq!(deserialized_bytes.sums.get(&key), Some(&42.5));
    }

    #[test]
    fn test_trait_object() {
        let mut acc = KeyedSumCountAccumulator::new();

        let key = KeyByLabelValues::new_with_labels(vec!["web".to_string()]);

        acc.add_sum(key.clone(), 42.0);

        let trait_obj: Box<dyn AggregateCore> = Box::new(acc);

        // Test type name through trait object
        assert_eq!(trait_obj.type_name(), "KeyedSumCountAccumulator");
    }
}

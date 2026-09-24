use crate::{
    AggregateCore, AggregationType, AuxStats, MergeableAccumulator, SerializableToSink,
    SingleSubpopulationAggregate,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;

use crate::Statistic;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SumAccumulator {
    pub sum: f64,
    /// None for scalar-only payloads; a sum does not establish a sample count.
    #[serde(default)]
    pub observation_count: Option<u64>,
}

impl SumAccumulator {
    pub fn new() -> Self {
        Self {
            sum: 0.0,
            observation_count: Some(0),
        }
    }

    pub fn with_sum(sum: f64) -> Self {
        Self {
            sum,
            observation_count: None,
        }
    }

    pub fn update(&mut self, value: f64) {
        self.sum += value;
        self.observation_count = self
            .observation_count
            .and_then(|count| count.checked_add(1));
    }

    pub fn deserialize_from_json(data: &Value) -> Result<Self, Box<dyn std::error::Error>> {
        let sum = data["sum"]
            .as_f64()
            .ok_or("Missing or invalid 'sum' field")?;
        Ok(Self {
            sum,
            observation_count: data.get("observation_count").and_then(Value::as_u64),
        })
    }

    pub fn deserialize_from_bytes(buffer: &[u8]) -> Result<Self, Box<dyn std::error::Error>> {
        match buffer.len() {
            // Legacy Python scalar sums carry no sample-count evidence.
            4 => Ok(Self::with_sum(f32::from_le_bytes(buffer.try_into()?) as f64)),
            // Counted sums use the same fixed layout as the Collector Sum payload.
            16 => Self::from_sum_bytes(buffer),
            len => {
                Err(format!("Invalid persisted Sum payload length: {len} (want 4 or 16)").into())
            }
        }
    }

    /// Decode the fixed Sum payload produced by the first-class Sum
    /// AggregationType path (asap-precompute-go's SumWrapper): float64 sum
    /// (little-endian) followed by uint64 count (little-endian), 16 bytes.
    ///
    /// Sum is an aggregation, NOT a sketch, so this deliberately does NOT
    /// depend on the sketchlib sketch-envelope proto — the payload is a small
    /// self-contained fixed layout. It decodes into the SAME
    /// `AggregationType::Sum` accumulator as a plain-OTLP Sum, so the SumAgg
    /// envelope and a plain Sum land on one identity (`exact_agg:Sum`) with no
    /// new SketchAlgorithm. The supplied observation count is retained for
    /// exact sample-count readouts; scalar-only legacy payloads leave it unknown.
    pub fn from_sum_bytes(buffer: &[u8]) -> Result<Self, Box<dyn std::error::Error>> {
        if buffer.len() < 16 {
            return Err(format!("Sum payload too short: {} bytes (want 16)", buffer.len()).into());
        }
        let sum = f64::from_le_bytes(buffer[0..8].try_into().unwrap());
        let count = u64::from_le_bytes(buffer[8..16].try_into().unwrap());
        Ok(Self {
            sum,
            observation_count: Some(count),
        })
    }
}

impl Default for SumAccumulator {
    fn default() -> Self {
        Self::new()
    }
}

impl SerializableToSink for SumAccumulator {
    fn serialize_to_json(&self) -> Value {
        serde_json::json!({
            "sum": self.sum,
            "observation_count": self.observation_count
        })
    }

    fn serialize_to_bytes(&self) -> Vec<u8> {
        match self.observation_count {
            Some(count) => {
                let mut bytes = Vec::with_capacity(16);
                bytes.extend_from_slice(&self.sum.to_le_bytes());
                bytes.extend_from_slice(&count.to_le_bytes());
                bytes
            }
            None => (self.sum as f32).to_le_bytes().to_vec(),
        }
    }
}

impl AggregateCore for SumAccumulator {
    fn clone_boxed_core(&self) -> Box<dyn AggregateCore> {
        Box::new(self.clone())
    }

    fn type_name(&self) -> &'static str {
        "SumAccumulator"
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
        // Check if other is also a SumAccumulator
        if other.get_accumulator_type() != self.get_accumulator_type() {
            return Err(format!(
                "Cannot merge SumAccumulator with {}",
                other.get_accumulator_type()
            )
            .into());
        }

        // Downcast to SumAccumulator
        let other_sum = other
            .as_any()
            .downcast_ref::<SumAccumulator>()
            .ok_or("Failed to downcast to SumAccumulator")?;

        // Use the existing merge_accumulators method
        let merged = Self::merge_accumulators(vec![self.clone(), other_sum.clone()])?;

        Ok(Box::new(merged))
    }

    fn get_accumulator_type(&self) -> AggregationType {
        AggregationType::Sum
    }

    fn approx_memory_bytes(&self) -> usize {
        // Single f64 + struct overhead.
        std::mem::size_of::<Self>()
    }

    fn aux_stats(&self) -> AuxStats {
        AuxStats {
            sum: Some(self.sum),
            count: self.observation_count,
            ..AuxStats::empty()
        }
    }

    fn get_keys(&self) -> Option<Vec<crate::KeyByLabelValues>> {
        None
    }

    fn query_statistic(
        &self,
        statistic: crate::Statistic,
        _key: &Option<crate::KeyByLabelValues>,
        _query_kwargs: &std::collections::HashMap<String, String>,
    ) -> Result<f64, Box<dyn std::error::Error + Send + Sync>> {
        use crate::SingleSubpopulationAggregate;
        self.query(statistic, None)
    }
}

impl SingleSubpopulationAggregate for SumAccumulator {
    fn query(
        &self,
        statistic: Statistic,
        query_kwargs: Option<&HashMap<String, String>>,
    ) -> Result<f64, Box<dyn std::error::Error + Send + Sync>> {
        // SumAccumulator doesn't use query_kwargs, assert it's None
        if query_kwargs.is_some() {
            return Err("SumAccumulator does not support query parameters".into());
        }

        match statistic {
            Statistic::Sum => Ok(self.sum),
            Statistic::Count => self
                .observation_count
                .map(|count| count as f64)
                .ok_or_else(|| "sample count is unavailable for this Sum payload".into()),
            _ => Err(format!("Unsupported statistic in SumAccumulator: {statistic:?}").into()),
        }
    }

    fn clone_boxed(&self) -> Box<dyn SingleSubpopulationAggregate> {
        Box::new(self.clone())
    }
}

impl MergeableAccumulator<SumAccumulator> for SumAccumulator {
    fn merge_accumulators(
        accumulators: Vec<SumAccumulator>,
    ) -> Result<SumAccumulator, Box<dyn std::error::Error + Send + Sync>> {
        let total_sum = accumulators.iter().map(|acc| acc.sum).sum();
        let observation_count = accumulators
            .iter()
            .try_fold(0u64, |total, acc| total.checked_add(acc.observation_count?));
        Ok(SumAccumulator {
            sum: total_sum,
            observation_count,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Sample counts must survive updates and merges independently of the sum.
    #[test]
    fn observation_count_survives_merge() {
        let mut first = SumAccumulator::new();
        first.update(10.0);
        first.update(20.0);
        let mut second = SumAccumulator::new();
        second.update(100.0);
        let merged = SumAccumulator::merge_accumulators(vec![first, second]).unwrap();
        assert_eq!(merged.sum, 130.0);
        assert_eq!(merged.aux_stats().count, Some(3));
    }

    // A legacy scalar sum has no evidence of how many observations produced it.
    #[test]
    fn legacy_sum_does_not_invent_observation_count() {
        let mut raw = SumAccumulator::new();
        raw.update(10.0);
        let merged =
            SumAccumulator::merge_accumulators(vec![raw, SumAccumulator::with_sum(20.0)]).unwrap();
        assert_eq!(merged.aux_stats().count, None);
    }

    // Persistence retains known counts, including zero and the full u64 range.
    #[test]
    fn counted_sum_binary_round_trip() {
        for count in [0, 3, u64::MAX] {
            let acc = SumAccumulator {
                sum: 1.0000000000001,
                observation_count: Some(count),
            };
            let bytes = acc.serialize_to_bytes();
            assert_eq!(bytes.len(), 16);
            let restored = SumAccumulator::deserialize_from_bytes(&bytes).unwrap();
            assert_eq!(restored.sum, acc.sum);
            assert_eq!(restored.observation_count, Some(count));
        }
    }

    // Existing scalar-only files remain readable without inventing counts.
    #[test]
    fn legacy_binary_sum_has_unknown_count() {
        let bytes = 42.5f32.to_le_bytes();
        let restored = SumAccumulator::deserialize_from_bytes(&bytes).unwrap();
        assert_eq!(restored.sum, 42.5);
        assert_eq!(restored.observation_count, None);
        assert_eq!(restored.serialize_to_bytes(), bytes);
    }

    // Truncated counted payloads must not silently decode as scalar sums.
    #[test]
    fn persisted_sum_rejects_invalid_lengths() {
        for len in [0, 3, 5, 8, 15, 17] {
            assert!(SumAccumulator::deserialize_from_bytes(&vec![0; len]).is_err());
        }
    }

    #[test]
    fn test_sum_accumulator_creation() {
        let acc = SumAccumulator::new();
        assert_eq!(acc.sum, 0.0);

        let acc2 = SumAccumulator::with_sum(42.5);
        assert_eq!(acc2.sum, 42.5);
    }

    #[test]
    fn test_sum_accumulator_update() {
        let mut acc = SumAccumulator::new();
        acc.update(10.0);
        acc.update(20.0);
        assert_eq!(acc.sum, 30.0);
    }

    #[test]
    fn test_sum_accumulator_query() {
        let acc = SumAccumulator::with_sum(42.0);

        assert_eq!(
            crate::SingleSubpopulationAggregate::query(&acc, Statistic::Sum, None).unwrap(),
            42.0
        );
        assert!(crate::SingleSubpopulationAggregate::query(&acc, Statistic::Count, None).is_err());

        assert!(crate::SingleSubpopulationAggregate::query(&acc, Statistic::Min, None).is_err());
        // SumAccumulator is a single subpopulation accumulator, doesn't need key-based queries
        assert_eq!(
            crate::SingleSubpopulationAggregate::query(&acc, Statistic::Sum, None).unwrap(),
            42.0
        );
    }

    #[test]
    fn count_readout_uses_observation_count_not_sum() {
        let mut acc = SumAccumulator::new();
        acc.update(10.0);
        acc.update(20.0);
        assert_eq!(
            crate::SingleSubpopulationAggregate::query(&acc, Statistic::Count, None).unwrap(),
            2.0
        );
    }

    #[test]
    fn test_sum_accumulator_merge() {
        let acc1 = SumAccumulator::with_sum(10.0);
        let acc2 = SumAccumulator::with_sum(20.0);
        let acc3 = SumAccumulator::with_sum(30.0);

        let merged =
            <SumAccumulator as MergeableAccumulator<SumAccumulator>>::merge_accumulators(vec![
                acc1, acc2, acc3,
            ])
            .unwrap();
        assert_eq!(merged.sum, 60.0);
    }

    #[test]
    fn test_sum_accumulator_serialization() {
        let acc = SumAccumulator::with_sum(42.5);

        // Test JSON serialization
        let json = acc.serialize_to_json();
        let deserialized = SumAccumulator::deserialize_from_json(&json).unwrap();
        assert_eq!(acc.sum, deserialized.sum);

        // Test byte serialization
        let bytes = acc.serialize_to_bytes();
        let deserialized_bytes = SumAccumulator::deserialize_from_bytes(&bytes).unwrap();
        assert_eq!(acc.sum, deserialized_bytes.sum);
    }

    #[test]
    fn test_trait_object() {
        let acc: Box<dyn AggregateCore> = Box::new(SumAccumulator::with_sum(42.0));

        assert_eq!(acc.type_name(), "SumAccumulator");
    }

    #[test]
    fn from_sum_bytes_decodes_go_sum_payload() {
        // GOLDEN: the 16-byte payload asap-precompute-go's
        // SumWrapper{10,20,30,40}.Snapshot() emits — float64 sum (LE) followed
        // by uint64 count (LE), sum=100, count=4. Proves the Rust backend
        // decodes the first-class Sum payload the Go agent produces
        // (cross-language wire parity, no sketchlib proto dependency).
        let go_bytes: &[u8] = &[
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x59, 0x40, // 100.0 f64 LE
            0x04, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // 4 u64 LE
        ];
        let acc = SumAccumulator::from_sum_bytes(go_bytes).expect("decode Go Sum payload");
        assert_eq!(acc.sum, 100.0, "decoded Go SumWrapper payload sum");
    }

    #[test]
    fn from_sum_bytes_rejects_short_payload() {
        // A short buffer is rejected (the ingest path then skips the point).
        assert!(SumAccumulator::from_sum_bytes(&[]).is_err());
        assert!(SumAccumulator::from_sum_bytes(&[0u8; 8]).is_err());
    }

    #[test]
    fn aux_stats_exposes_sum_only() {
        let acc = SumAccumulator::with_sum(123.5);
        let aux = acc.aux_stats();
        assert_eq!(aux.sum, Some(123.5));
        assert_eq!(aux.count, None);
        assert_eq!(aux.min, None);
        assert_eq!(aux.max, None);
    }

    #[test]
    fn aux_stats_try_answer_on_sum_statistic() {
        use crate::Statistic;
        let acc = SumAccumulator::with_sum(42.0);
        // Sum statistic is covered by aux without deserialising.
        assert_eq!(acc.aux_stats().try_answer(Statistic::Sum), Some(42.0));
        // Count is not tracked by SumAccumulator.
        assert_eq!(acc.aux_stats().try_answer(Statistic::Count), None);
    }
}

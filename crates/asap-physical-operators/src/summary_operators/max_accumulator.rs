use crate::{
    AggregateCore, AggregationType, AuxStats, MergeableAccumulator, SerializableToSink,
    SingleSubpopulationAggregate,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;

use crate::Statistic;

/// Exact maximum over one population, mergeable by comparison.
///
/// See [`MinAccumulator`](super::min_accumulator::MinAccumulator) for why the
/// two directions are separate types rather than one accumulator carrying a
/// `sub_type` string.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MaxAccumulator {
    pub value: f64,
}

impl Default for MaxAccumulator {
    fn default() -> Self {
        Self::new()
    }
}

impl MaxAccumulator {
    pub fn new() -> Self {
        Self {
            value: f64::NEG_INFINITY,
        }
    }

    pub fn with_value(value: f64) -> Self {
        Self { value }
    }

    pub fn update(&mut self, value: f64) {
        if value > self.value {
            self.value = value;
        }
    }

    pub fn deserialize_from_json(data: &Value) -> Result<Self, Box<dyn std::error::Error>> {
        let value = data["value"]
            .as_f64()
            .ok_or("Missing or invalid 'value' field")?;
        Ok(Self::with_value(value))
    }

    pub fn deserialize_from_bytes(buffer: &[u8]) -> Result<Self, Box<dyn std::error::Error>> {
        if buffer.len() < 8 {
            return Err("Buffer too short".into());
        }
        let value = f64::from_le_bytes([
            buffer[0], buffer[1], buffer[2], buffer[3], buffer[4], buffer[5], buffer[6], buffer[7],
        ]);
        Ok(Self::with_value(value))
    }
}

impl SerializableToSink for MaxAccumulator {
    fn serialize_to_json(&self) -> Value {
        serde_json::json!({ "value": self.value })
    }

    fn serialize_to_bytes(&self) -> Vec<u8> {
        self.value.to_le_bytes().to_vec()
    }
}

impl MergeableAccumulator<MaxAccumulator> for MaxAccumulator {
    fn merge_accumulators(
        accumulators: Vec<MaxAccumulator>,
    ) -> Result<MaxAccumulator, Box<dyn std::error::Error + Send + Sync>> {
        if accumulators.is_empty() {
            return Err("No accumulators to merge".into());
        }
        let mut result = MaxAccumulator::new();
        for acc in accumulators {
            result.update(acc.value);
        }
        Ok(result)
    }
}

impl AggregateCore for MaxAccumulator {
    fn clone_boxed_core(&self) -> Box<dyn AggregateCore> {
        Box::new(self.clone())
    }

    fn type_name(&self) -> &'static str {
        "MaxAccumulator"
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
                "Cannot merge MaxAccumulator with {}",
                other.get_accumulator_type()
            )
            .into());
        }
        let other_max = other
            .as_any()
            .downcast_ref::<MaxAccumulator>()
            .ok_or("Failed to downcast to MaxAccumulator")?;
        let mut merged = self.clone();
        merged.update(other_max.value);
        Ok(Box::new(merged))
    }

    fn get_accumulator_type(&self) -> AggregationType {
        AggregationType::Max
    }

    fn approx_memory_bytes(&self) -> usize {
        std::mem::size_of::<Self>()
    }

    fn aux_stats(&self) -> AuxStats {
        // The sentinel `f64::NEG_INFINITY` from `new()` is surfaced as-is; the
        // query engine already treats it as "no data yet", the same way it
        // does for `query_statistic`.
        AuxStats {
            max: Some(self.value),
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

impl SingleSubpopulationAggregate for MaxAccumulator {
    fn query(
        &self,
        statistic: Statistic,
        query_kwargs: Option<&HashMap<String, String>>,
    ) -> Result<f64, Box<dyn std::error::Error + Send + Sync>> {
        if query_kwargs.is_some() {
            return Err("MaxAccumulator does not support query parameters".into());
        }
        match statistic {
            Statistic::Max => Ok(self.value),
            other => Err(format!("Unsupported statistic in MaxAccumulator: {other:?}").into()),
        }
    }

    fn clone_boxed(&self) -> Box<dyn SingleSubpopulationAggregate> {
        Box::new(self.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keeps_the_largest_update() {
        let mut acc = MaxAccumulator::new();
        acc.update(10.0);
        acc.update(5.0);
        acc.update(15.0);

        assert_eq!(acc.value, 15.0);
        assert_eq!(
            crate::SingleSubpopulationAggregate::query(&acc, Statistic::Max, None).unwrap(),
            15.0
        );
    }

    #[test]
    fn refuses_to_answer_a_minimum_query() {
        let acc = MaxAccumulator::with_value(15.0);
        assert!(crate::SingleSubpopulationAggregate::query(&acc, Statistic::Min, None).is_err());
    }

    #[test]
    fn merges_by_taking_the_largest() {
        let merged =
            <MaxAccumulator as MergeableAccumulator<MaxAccumulator>>::merge_accumulators(vec![
                MaxAccumulator::with_value(10.0),
                MaxAccumulator::with_value(5.0),
                MaxAccumulator::with_value(15.0),
            ])
            .unwrap();
        assert_eq!(merged.value, 15.0);
    }

    #[test]
    fn refuses_to_merge_with_a_minimum() {
        use super::super::min_accumulator::MinAccumulator;
        let max = MaxAccumulator::with_value(15.0);
        let min = MinAccumulator::with_value(5.0);
        assert!(max.merge_with(&min).is_err());
    }

    #[test]
    fn round_trips_through_both_serializations() {
        let acc = MaxAccumulator::with_value(42.5);

        let json = acc.serialize_to_json();
        assert_eq!(
            MaxAccumulator::deserialize_from_json(&json).unwrap().value,
            42.5
        );

        let bytes = acc.serialize_to_bytes();
        assert_eq!(
            MaxAccumulator::deserialize_from_bytes(&bytes)
                .unwrap()
                .value,
            42.5
        );
    }

    #[test]
    fn aux_stats_expose_max_only() {
        let aux = MaxAccumulator::with_value(99.0).aux_stats();
        assert_eq!(aux.max, Some(99.0));
        assert_eq!(aux.min, None);
        assert_eq!(aux.try_answer(Statistic::Max), Some(99.0));
        assert_eq!(aux.try_answer(Statistic::Min), None);
    }
}

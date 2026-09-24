use crate::{
    AggregateCore, AggregationType, AuxStats, MergeableAccumulator, SerializableToSink,
    SingleSubpopulationAggregate,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;

use crate::Statistic;

/// Exact minimum over one population, mergeable by comparison.
///
/// The sibling [`MaxAccumulator`](super::max_accumulator::MaxAccumulator) is a
/// separate type on purpose: these two used to be one `MinMaxAccumulator`
/// whose direction lived in a `sub_type: String`, which meant every layer
/// above -- the wire `aggregationSubType`, the accumulator factory, the
/// summary catalog -- had to carry the direction alongside the family and
/// could silently answer a `min_over_time` read from maximum state.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MinAccumulator {
    pub value: f64,
}

impl Default for MinAccumulator {
    fn default() -> Self {
        Self::new()
    }
}

impl MinAccumulator {
    pub fn new() -> Self {
        Self {
            value: f64::INFINITY,
        }
    }

    pub fn with_value(value: f64) -> Self {
        Self { value }
    }

    pub fn update(&mut self, value: f64) {
        if value < self.value {
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

impl SerializableToSink for MinAccumulator {
    fn serialize_to_json(&self) -> Value {
        serde_json::json!({ "value": self.value })
    }

    fn serialize_to_bytes(&self) -> Vec<u8> {
        self.value.to_le_bytes().to_vec()
    }
}

impl MergeableAccumulator<MinAccumulator> for MinAccumulator {
    fn merge_accumulators(
        accumulators: Vec<MinAccumulator>,
    ) -> Result<MinAccumulator, Box<dyn std::error::Error + Send + Sync>> {
        if accumulators.is_empty() {
            return Err("No accumulators to merge".into());
        }
        let mut result = MinAccumulator::new();
        for acc in accumulators {
            result.update(acc.value);
        }
        Ok(result)
    }
}

impl AggregateCore for MinAccumulator {
    fn clone_boxed_core(&self) -> Box<dyn AggregateCore> {
        Box::new(self.clone())
    }

    fn type_name(&self) -> &'static str {
        "MinAccumulator"
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
                "Cannot merge MinAccumulator with {}",
                other.get_accumulator_type()
            )
            .into());
        }
        let other_min = other
            .as_any()
            .downcast_ref::<MinAccumulator>()
            .ok_or("Failed to downcast to MinAccumulator")?;
        let mut merged = self.clone();
        merged.update(other_min.value);
        Ok(Box::new(merged))
    }

    fn get_accumulator_type(&self) -> AggregationType {
        AggregationType::Min
    }

    fn approx_memory_bytes(&self) -> usize {
        std::mem::size_of::<Self>()
    }

    fn aux_stats(&self) -> AuxStats {
        // The sentinel `f64::INFINITY` from `new()` is surfaced as-is; the
        // query engine already treats it as "no data yet", the same way it
        // does for `query_statistic`.
        AuxStats {
            min: Some(self.value),
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

impl SingleSubpopulationAggregate for MinAccumulator {
    fn query(
        &self,
        statistic: Statistic,
        query_kwargs: Option<&HashMap<String, String>>,
    ) -> Result<f64, Box<dyn std::error::Error + Send + Sync>> {
        if query_kwargs.is_some() {
            return Err("MinAccumulator does not support query parameters".into());
        }
        match statistic {
            Statistic::Min => Ok(self.value),
            other => Err(format!("Unsupported statistic in MinAccumulator: {other:?}").into()),
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
    fn keeps_the_smallest_update() {
        let mut acc = MinAccumulator::new();
        acc.update(10.0);
        acc.update(5.0);
        acc.update(15.0);

        assert_eq!(acc.value, 5.0);
        assert_eq!(
            crate::SingleSubpopulationAggregate::query(&acc, Statistic::Min, None).unwrap(),
            5.0
        );
    }

    #[test]
    fn refuses_to_answer_a_maximum_query() {
        let acc = MinAccumulator::with_value(5.0);
        assert!(crate::SingleSubpopulationAggregate::query(&acc, Statistic::Max, None).is_err());
    }

    #[test]
    fn merges_by_taking_the_smallest() {
        let merged =
            <MinAccumulator as MergeableAccumulator<MinAccumulator>>::merge_accumulators(vec![
                MinAccumulator::with_value(10.0),
                MinAccumulator::with_value(5.0),
                MinAccumulator::with_value(15.0),
            ])
            .unwrap();
        assert_eq!(merged.value, 5.0);
    }

    #[test]
    fn refuses_to_merge_with_a_maximum() {
        use super::super::max_accumulator::MaxAccumulator;
        let min = MinAccumulator::with_value(5.0);
        let max = MaxAccumulator::with_value(15.0);
        assert!(min.merge_with(&max).is_err());
    }

    #[test]
    fn round_trips_through_both_serializations() {
        let acc = MinAccumulator::with_value(42.5);

        let json = acc.serialize_to_json();
        assert_eq!(
            MinAccumulator::deserialize_from_json(&json).unwrap().value,
            42.5
        );

        let bytes = acc.serialize_to_bytes();
        assert_eq!(
            MinAccumulator::deserialize_from_bytes(&bytes)
                .unwrap()
                .value,
            42.5
        );
    }

    #[test]
    fn aux_stats_expose_min_only() {
        let aux = MinAccumulator::with_value(3.5).aux_stats();
        assert_eq!(aux.min, Some(3.5));
        assert_eq!(aux.max, None);
        assert_eq!(aux.count, None);
        assert_eq!(aux.sum, None);
        assert_eq!(aux.try_answer(Statistic::Min), Some(3.5));
        assert_eq!(aux.try_answer(Statistic::Max), None);
    }
}

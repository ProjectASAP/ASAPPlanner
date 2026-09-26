use crate::{
    AggregateCore, AggregationType, Measurement, MergeableAccumulator, SerializableToSink,
    SingleSubpopulationAggregate,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;

use crate::Statistic;

const RESET_AWARE_WIRE_MAGIC: &[u8; 8] = b"ASAPINC2";
const RESET_AWARE_WIRE_EXTENSION_LEN: usize = 8 + 8 + 8;

/// Accumulator for tracking increases in counter metrics
/// Stores the starting and last seen measurements with timestamps
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IncreaseAccumulator {
    pub starting_measurement: Measurement,
    pub starting_timestamp: i64,
    pub last_seen_measurement: Measurement,
    pub last_seen_timestamp: i64,
    /// Sum of monotonic deltas, adding the post-reset value whenever the
    /// counter decreases. This is the reset correction Prometheus applies.
    #[serde(default)]
    pub total_increase: f64,
    #[serde(default)]
    pub sample_count: u64,
}

impl IncreaseAccumulator {
    /// Merge two counter intervals without a temporary collection. Ties retain
    /// the left input, matching the stable ordering of multi-pane merges.
    pub(crate) fn merge_pair(left: &Self, right: &Self) -> Self {
        let (first, second) = if left.starting_timestamp <= right.starting_timestamp {
            (left, right)
        } else {
            (right, left)
        };
        let mut merged = first.clone();
        if second.starting_timestamp > merged.last_seen_timestamp {
            merged.total_increase +=
                if second.starting_measurement.value >= merged.last_seen_measurement.value {
                    second.starting_measurement.value - merged.last_seen_measurement.value
                } else {
                    second.starting_measurement.value
                };
        }
        merged.total_increase += second.total_increase;
        merged.sample_count = merged.sample_count.saturating_add(second.sample_count);
        if second.last_seen_timestamp > merged.last_seen_timestamp {
            merged.last_seen_measurement = second.last_seen_measurement.clone();
            merged.last_seen_timestamp = second.last_seen_timestamp;
        }

        merged
    }

    /// Return the number of bytes occupied by one accumulator at the start of
    /// `buffer`. Old persisted values end after `last_seen_timestamp`; reset-
    /// aware values carry a magic-prefixed extension. The magic makes this
    /// safe when the buffer also contains the next keyed entry.
    pub(crate) fn serialized_len_from_prefix(
        buffer: &[u8],
    ) -> Result<usize, Box<dyn std::error::Error>> {
        if buffer.len() < 4 {
            return Err("Buffer too short for starting measurement length".into());
        }
        let starting_len = u32::from_le_bytes(buffer[0..4].try_into()?) as usize;
        let last_len_offset = 4usize
            .checked_add(starting_len)
            .and_then(|offset| offset.checked_add(8))
            .ok_or("IncreaseAccumulator length overflow")?;
        if buffer.len() < last_len_offset + 4 {
            return Err("Buffer too short for last seen measurement length".into());
        }
        let last_len =
            u32::from_le_bytes(buffer[last_len_offset..last_len_offset + 4].try_into()?) as usize;
        let legacy_len = last_len_offset
            .checked_add(4)
            .and_then(|offset| offset.checked_add(last_len))
            .and_then(|offset| offset.checked_add(8))
            .ok_or("IncreaseAccumulator length overflow")?;
        if buffer.len() < legacy_len {
            return Err("Buffer too short for last seen timestamp".into());
        }
        let has_extension = buffer.len() >= legacy_len + RESET_AWARE_WIRE_EXTENSION_LEN
            && &buffer[legacy_len..legacy_len + RESET_AWARE_WIRE_MAGIC.len()]
                == RESET_AWARE_WIRE_MAGIC;
        Ok(legacy_len
            + if has_extension {
                RESET_AWARE_WIRE_EXTENSION_LEN
            } else {
                0
            })
    }

    pub fn new(
        starting_measurement: Measurement,
        starting_timestamp: i64,
        last_seen_measurement: Measurement,
        last_seen_timestamp: i64,
    ) -> Self {
        let total_increase = if last_seen_timestamp <= starting_timestamp {
            0.0
        } else if last_seen_measurement.value >= starting_measurement.value {
            last_seen_measurement.value - starting_measurement.value
        } else {
            last_seen_measurement.value
        };
        let sample_count = if last_seen_timestamp > starting_timestamp {
            2
        } else {
            1
        };
        Self {
            starting_measurement,
            starting_timestamp,
            last_seen_measurement,
            last_seen_timestamp,
            total_increase,
            sample_count,
        }
    }

    pub fn update(&mut self, measurement: Measurement, timestamp: i64) {
        if timestamp < self.last_seen_timestamp {
            return;
        }
        if timestamp == self.last_seen_timestamp {
            return;
        }
        if measurement.value >= self.last_seen_measurement.value {
            self.total_increase += measurement.value - self.last_seen_measurement.value;
        } else {
            self.total_increase += measurement.value;
        }
        self.last_seen_measurement = measurement;
        self.last_seen_timestamp = timestamp;
        self.sample_count = self.sample_count.saturating_add(1);
    }

    pub fn deserialize_from_json(data: &Value) -> Result<Self, Box<dyn std::error::Error>> {
        let starting_measurement =
            Measurement::deserialize_from_json(&data["starting_measurement"])?;
        let starting_timestamp = data["starting_timestamp"]
            .as_i64()
            .ok_or("Missing or invalid 'starting_timestamp' field")?;
        let last_seen_measurement =
            Measurement::deserialize_from_json(&data["last_seen_measurement"])?;
        let last_seen_timestamp = data["last_seen_timestamp"]
            .as_i64()
            .ok_or("Missing or invalid 'last_seen_timestamp' field")?;

        let mut accumulator = Self::new(
            starting_measurement,
            starting_timestamp,
            last_seen_measurement,
            last_seen_timestamp,
        );
        accumulator.total_increase = data["total_increase"]
            .as_f64()
            .unwrap_or(accumulator.total_increase);
        accumulator.sample_count = data["sample_count"]
            .as_u64()
            .unwrap_or(accumulator.sample_count);
        Ok(accumulator)
    }

    pub fn deserialize_from_bytes(buffer: &[u8]) -> Result<Self, Box<dyn std::error::Error>> {
        let mut offset = 0;

        // Read starting measurement length and data
        if buffer.len() < offset + 4 {
            return Err("Buffer too short for starting measurement length".into());
        }
        let starting_measurement_length = u32::from_le_bytes([
            buffer[offset],
            buffer[offset + 1],
            buffer[offset + 2],
            buffer[offset + 3],
        ]) as usize;
        offset += 4;

        if buffer.len() < offset + starting_measurement_length {
            return Err("Buffer too short for starting measurement".into());
        }
        let starting_measurement = Measurement::deserialize_from_bytes(
            &buffer[offset..offset + starting_measurement_length],
        )?;
        offset += starting_measurement_length;

        // Read starting timestamp
        if buffer.len() < offset + 8 {
            return Err("Buffer too short for starting timestamp".into());
        }
        let starting_timestamp = i64::from_le_bytes([
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

        // Read last seen measurement length and data
        if buffer.len() < offset + 4 {
            return Err("Buffer too short for last seen measurement length".into());
        }
        let last_seen_measurement_length = u32::from_le_bytes([
            buffer[offset],
            buffer[offset + 1],
            buffer[offset + 2],
            buffer[offset + 3],
        ]) as usize;
        offset += 4;

        if buffer.len() < offset + last_seen_measurement_length {
            return Err("Buffer too short for last seen measurement".into());
        }
        let last_seen_measurement = Measurement::deserialize_from_bytes(
            &buffer[offset..offset + last_seen_measurement_length],
        )?;
        offset += last_seen_measurement_length;

        // Read last seen timestamp
        if buffer.len() < offset + 8 {
            return Err("Buffer too short for last seen timestamp".into());
        }
        let last_seen_timestamp = i64::from_le_bytes([
            buffer[offset],
            buffer[offset + 1],
            buffer[offset + 2],
            buffer[offset + 3],
            buffer[offset + 4],
            buffer[offset + 5],
            buffer[offset + 6],
            buffer[offset + 7],
        ]);

        let mut accumulator = Self::new(
            starting_measurement,
            starting_timestamp,
            last_seen_measurement,
            last_seen_timestamp,
        );
        offset += 8;
        if buffer.len() >= offset + RESET_AWARE_WIRE_EXTENSION_LEN
            && &buffer[offset..offset + RESET_AWARE_WIRE_MAGIC.len()] == RESET_AWARE_WIRE_MAGIC
        {
            offset += RESET_AWARE_WIRE_MAGIC.len();
            accumulator.total_increase = f64::from_le_bytes(
                buffer[offset..offset + 8]
                    .try_into()
                    .expect("checked total-increase bytes"),
            );
            offset += 8;
            accumulator.sample_count = u64::from_le_bytes(
                buffer[offset..offset + 8]
                    .try_into()
                    .expect("checked sample-count bytes"),
            );
        }
        Ok(accumulator)
    }
}

impl SerializableToSink for IncreaseAccumulator {
    fn serialize_to_json(&self) -> Value {
        serde_json::json!({
            "starting_measurement": self.starting_measurement.serialize_to_json(),
            "starting_timestamp": self.starting_timestamp,
            "last_seen_measurement": self.last_seen_measurement.serialize_to_json(),
            "last_seen_timestamp": self.last_seen_timestamp,
            "total_increase": self.total_increase,
            "sample_count": self.sample_count,
        })
    }

    fn serialize_to_bytes(&self) -> Vec<u8> {
        let starting_measurement_bytes = self.starting_measurement.serialize_to_bytes();
        let last_seen_measurement_bytes = self.last_seen_measurement.serialize_to_bytes();

        let mut buffer = Vec::new();

        // Starting measurement length and data
        buffer.extend_from_slice(&(starting_measurement_bytes.len() as u32).to_le_bytes());
        buffer.extend_from_slice(&starting_measurement_bytes);

        // Starting timestamp
        buffer.extend_from_slice(&self.starting_timestamp.to_le_bytes());

        // Last seen measurement length and data
        buffer.extend_from_slice(&(last_seen_measurement_bytes.len() as u32).to_le_bytes());
        buffer.extend_from_slice(&last_seen_measurement_bytes);

        // Last seen timestamp
        buffer.extend_from_slice(&self.last_seen_timestamp.to_le_bytes());
        buffer.extend_from_slice(RESET_AWARE_WIRE_MAGIC);
        buffer.extend_from_slice(&self.total_increase.to_le_bytes());
        buffer.extend_from_slice(&self.sample_count.to_le_bytes());

        buffer
    }
}

impl MergeableAccumulator<IncreaseAccumulator> for IncreaseAccumulator {
    fn merge_accumulators(
        accumulators: Vec<IncreaseAccumulator>,
    ) -> Result<IncreaseAccumulator, Box<dyn std::error::Error + Send + Sync>> {
        if accumulators.is_empty() {
            return Err("No accumulators to merge".into());
        }

        let mut accumulators = accumulators;
        accumulators.sort_by_key(|accumulator| accumulator.starting_timestamp);
        let mut result = accumulators[0].clone();

        for acc in &accumulators[1..] {
            result = Self::merge_pair(&result, acc);
        }

        Ok(result)
    }
}

impl AggregateCore for IncreaseAccumulator {
    fn clone_boxed_core(&self) -> Box<dyn AggregateCore> {
        Box::new(self.clone())
    }

    fn type_name(&self) -> &'static str {
        "IncreaseAccumulator"
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
        // Check if other is also an IncreaseAccumulator
        if other.get_accumulator_type() != self.get_accumulator_type() {
            return Err(format!(
                "Cannot merge IncreaseAccumulator with {}",
                other.get_accumulator_type()
            )
            .into());
        }

        // Downcast to IncreaseAccumulator
        let other_increase = other
            .as_any()
            .downcast_ref::<IncreaseAccumulator>()
            .ok_or("Failed to downcast to IncreaseAccumulator")?;

        let merged = Self::merge_pair(self, other_increase);
        Ok(Box::new(merged))
    }

    fn get_accumulator_type(&self) -> AggregationType {
        AggregationType::Increase
    }

    fn approx_memory_bytes(&self) -> usize {
        // Two Measurements + two i64s. Measurements are a few f64 fields.
        std::mem::size_of::<Self>()
    }

    fn get_keys(&self) -> Option<Vec<crate::KeyByLabelValues>> {
        None
    }

    fn query_statistic(
        &self,
        statistic: crate::Statistic,
        _key: &Option<crate::KeyByLabelValues>,
        query_kwargs: &std::collections::HashMap<String, String>,
    ) -> Result<f64, Box<dyn std::error::Error + Send + Sync>> {
        use crate::SingleSubpopulationAggregate;
        self.query(
            statistic,
            (!query_kwargs.is_empty()).then_some(query_kwargs),
        )
    }
}

impl SingleSubpopulationAggregate for IncreaseAccumulator {
    fn query(
        &self,
        statistic: Statistic,
        query_kwargs: Option<&HashMap<String, String>>,
    ) -> Result<f64, Box<dyn std::error::Error + Send + Sync>> {
        match statistic {
            Statistic::Increase => Ok(self.extrapolated_value(query_kwargs, false)?),
            Statistic::Rate => Ok(self.extrapolated_value(query_kwargs, true)?),
            // For instant `sum [by (...)] (counter_metric)` Prometheus
            // sums the latest cumulative value of each matching series.
            // The IncreaseAccumulator already tracks that latest value
            // in `last_seen_measurement`, so per-series Sum is just
            // that scalar; the engine's outer aggregation groups by the
            // `by` labels and adds the per-series totals across keys.
            //
            // See PR #108 audit conclusion (commit 4359e10) and issue
            // ProjectASAP/ASAPCollector#46: pre-fix the ASAP tier ingested
            // counters as IncreaseAccumulator and bare `sum by (...) (<counter>)`
            // capability-missed because this trait did not answer Sum.
            Statistic::Sum => Ok(self.last_seen_measurement.value),
            _ => Err(format!("Unsupported statistic in IncreaseAccumulator: {statistic:?}").into()),
        }
    }

    fn clone_boxed(&self) -> Box<dyn SingleSubpopulationAggregate> {
        Box::new(self.clone())
    }
}

impl IncreaseAccumulator {
    fn extrapolated_value(
        &self,
        query_kwargs: Option<&HashMap<String, String>>,
        is_rate: bool,
    ) -> Result<f64, Box<dyn std::error::Error + Send + Sync>> {
        if self.sample_count < 2 || self.last_seen_timestamp <= self.starting_timestamp {
            return Err("at least two ordered counter samples are required".into());
        }
        let sampled_interval = (self.last_seen_timestamp - self.starting_timestamp) as f64 / 1000.0;
        let Some(kwargs) = query_kwargs else {
            return Ok(if is_rate {
                self.total_increase / sampled_interval
            } else {
                self.total_increase
            });
        };
        let range_start = kwargs
            .get("range_start_ms")
            .ok_or("missing range_start_ms")?
            .parse::<i64>()?;
        let range_end = kwargs
            .get("range_end_ms")
            .ok_or("missing range_end_ms")?
            .parse::<i64>()?;
        if range_end <= range_start {
            return Err("invalid counter evaluation range".into());
        }

        let mut duration_to_start =
            (self.starting_timestamp.saturating_sub(range_start)) as f64 / 1000.0;
        let duration_to_end = (range_end.saturating_sub(self.last_seen_timestamp)) as f64 / 1000.0;
        let average_sample_interval = sampled_interval / (self.sample_count - 1) as f64;
        let extrapolation_threshold = average_sample_interval * 1.1;

        if self.total_increase > 0.0 && self.starting_measurement.value >= 0.0 {
            let duration_to_zero =
                sampled_interval * (self.starting_measurement.value / self.total_increase);
            duration_to_start = duration_to_start.min(duration_to_zero);
        }
        let mut extrapolate_to = sampled_interval;
        extrapolate_to += if duration_to_start < extrapolation_threshold {
            duration_to_start.max(0.0)
        } else {
            average_sample_interval / 2.0
        };
        extrapolate_to += if duration_to_end < extrapolation_threshold {
            duration_to_end.max(0.0)
        } else {
            average_sample_interval / 2.0
        };
        let mut factor = extrapolate_to / sampled_interval;
        if is_rate {
            factor /= (range_end - range_start) as f64 / 1000.0;
        }
        Ok(self.total_increase * factor)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_increase_accumulator_creation() {
        let starting_measurement = Measurement::new(10.0);
        let last_seen_measurement = Measurement::new(25.0);
        let acc = IncreaseAccumulator::new(
            starting_measurement.clone(),
            1000,
            last_seen_measurement.clone(),
            2000,
        );

        assert_eq!(acc.starting_measurement.value, 10.0);
        assert_eq!(acc.starting_timestamp, 1000);
        assert_eq!(acc.last_seen_measurement.value, 25.0);
        assert_eq!(acc.last_seen_timestamp, 2000);
    }

    #[test]
    fn test_increase_accumulator_update() {
        let starting_measurement = Measurement::new(10.0);
        let mut acc = IncreaseAccumulator::new(
            starting_measurement.clone(),
            1000,
            starting_measurement.clone(),
            1000,
        );

        let new_measurement = Measurement::new(25.0);
        acc.update(new_measurement.clone(), 2000);

        assert_eq!(acc.last_seen_measurement.value, 25.0);
        assert_eq!(acc.last_seen_timestamp, 2000);
        assert_eq!(acc.starting_measurement.value, 10.0); // Should remain unchanged
    }

    #[test]
    fn test_increase_accumulator_query() {
        let starting_measurement = Measurement::new(10.0);
        let last_seen_measurement = Measurement::new(25.0);
        let acc = IncreaseAccumulator::new(
            starting_measurement,
            1000,
            last_seen_measurement,
            3000, // 2 second difference
        );

        // Test increase calculation
        assert_eq!(
            crate::SingleSubpopulationAggregate::query(&acc, Statistic::Increase, None).unwrap(),
            15.0
        );

        // Test rate calculation (per second)
        assert_eq!(
            crate::SingleSubpopulationAggregate::query(&acc, Statistic::Rate, None).unwrap(),
            7.5
        ); // 15.0 / 2.0

        // Statistic::Sum returns the latest cumulative counter value,
        // matching Prometheus semantics for instant `sum(<counter>)`.
        // (Issue ProjectASAP/ASAPCollector#46, PR #108 diagnosis.)
        assert_eq!(
            crate::SingleSubpopulationAggregate::query(&acc, Statistic::Sum, None).unwrap(),
            25.0
        );

        // Unsupported statistics still error.
        assert!(crate::SingleSubpopulationAggregate::query(&acc, Statistic::Min, None).is_err());
    }

    #[test]
    fn prometheus_counter_reset_and_boundary_extrapolation() {
        let mut acc = IncreaseAccumulator::new(
            Measurement::new(10.0),
            10_000,
            Measurement::new(10.0),
            10_000,
        );
        acc.update(Measurement::new(20.0), 20_000);
        acc.update(Measurement::new(3.0), 30_000);
        acc.update(Measurement::new(13.0), 50_000);
        assert_eq!(acc.total_increase, 23.0);
        assert_eq!(acc.sample_count, 4);

        let kwargs = HashMap::from([
            ("range_start_ms".into(), "0".into()),
            ("range_end_ms".into(), "60000".into()),
        ]);
        let increase =
            crate::SingleSubpopulationAggregate::query(&acc, Statistic::Increase, Some(&kwargs))
                .unwrap();
        let rate = crate::SingleSubpopulationAggregate::query(&acc, Statistic::Rate, Some(&kwargs))
            .unwrap();
        assert!((increase - 34.5).abs() < 1e-12);
        assert!((rate - 0.575).abs() < 1e-12);
    }

    #[test]
    fn pane_merge_preserves_resets_and_prometheus_extrapolation() {
        let mut left = IncreaseAccumulator::new(
            Measurement::new(10.0),
            10_000,
            Measurement::new(10.0),
            10_000,
        );
        left.update(Measurement::new(20.0), 20_000);
        let mut right =
            IncreaseAccumulator::new(Measurement::new(3.0), 30_000, Measurement::new(3.0), 30_000);
        right.update(Measurement::new(13.0), 50_000);
        let merged = IncreaseAccumulator::merge_accumulators(vec![right, left]).unwrap();
        assert_eq!(merged.total_increase, 23.0);
        assert_eq!(merged.sample_count, 4);
        let kwargs = HashMap::from([
            ("range_start_ms".into(), "0".into()),
            ("range_end_ms".into(), "60000".into()),
        ]);
        assert_eq!(
            crate::SingleSubpopulationAggregate::query(&merged, Statistic::Increase, Some(&kwargs))
                .unwrap(),
            34.5
        );
    }

    #[test]
    fn counter_sds_state_is_constant_size_per_pane() {
        let mut acc = IncreaseAccumulator::new(Measurement::new(0.0), 0, Measurement::new(0.0), 0);
        let initial = acc.serialize_to_bytes().len();
        for second in 1..=86_400 {
            acc.update(Measurement::new(second as f64), second * 1_000);
        }
        assert_eq!(acc.serialize_to_bytes().len(), initial);
        assert_eq!(acc.sample_count, 86_401);
        assert_eq!(
            acc.approx_memory_bytes(),
            std::mem::size_of::<IncreaseAccumulator>()
        );
    }

    #[test]
    fn test_increase_accumulator_sum_is_latest_cumulative_value() {
        // Instant `sum (<counter>)` semantics: the per-series summand is
        // the latest cumulative counter value. Two series with latest
        // values 100 and 50 (started at 10 and 5 respectively) should
        // each report Sum = 100 and Sum = 50 — the engine's `sum by`
        // outer aggregation does the cross-series total.
        let acc_a =
            IncreaseAccumulator::new(Measurement::new(10.0), 1000, Measurement::new(100.0), 2000);
        let acc_b =
            IncreaseAccumulator::new(Measurement::new(5.0), 1000, Measurement::new(50.0), 2000);
        assert_eq!(
            crate::SingleSubpopulationAggregate::query(&acc_a, Statistic::Sum, None).unwrap(),
            100.0
        );
        assert_eq!(
            crate::SingleSubpopulationAggregate::query(&acc_b, Statistic::Sum, None).unwrap(),
            50.0
        );
    }

    #[test]
    fn test_increase_accumulator_merge() {
        let acc1 =
            IncreaseAccumulator::new(Measurement::new(10.0), 1000, Measurement::new(20.0), 2000);
        let acc2 = IncreaseAccumulator::new(
            Measurement::new(5.0),
            500, // Earlier start
            Measurement::new(15.0),
            1500,
        );
        let acc3 = IncreaseAccumulator::new(
            Measurement::new(20.0),
            2000,
            Measurement::new(30.0),
            3000, // Later end
        );

        let merged =
            <IncreaseAccumulator as MergeableAccumulator<IncreaseAccumulator>>::merge_accumulators(
                vec![acc1, acc2, acc3],
            )
            .unwrap();

        // Should use earliest start and latest end
        assert_eq!(merged.starting_measurement.value, 5.0);
        assert_eq!(merged.starting_timestamp, 500);
        assert_eq!(merged.last_seen_measurement.value, 30.0);
        assert_eq!(merged.last_seen_timestamp, 3000);
    }

    #[test]
    fn test_increase_accumulator_serialization() {
        let acc =
            IncreaseAccumulator::new(Measurement::new(10.0), 1000, Measurement::new(25.0), 2000);

        // Test JSON serialization
        let json = acc.serialize_to_json();
        let deserialized = IncreaseAccumulator::deserialize_from_json(&json).unwrap();
        assert_eq!(
            acc.starting_measurement.value,
            deserialized.starting_measurement.value
        );
        assert_eq!(acc.starting_timestamp, deserialized.starting_timestamp);
        assert_eq!(
            acc.last_seen_measurement.value,
            deserialized.last_seen_measurement.value
        );
        assert_eq!(acc.last_seen_timestamp, deserialized.last_seen_timestamp);

        // Test byte serialization
        let bytes = acc.serialize_to_bytes();
        let deserialized_bytes = IncreaseAccumulator::deserialize_from_bytes(&bytes).unwrap();
        assert_eq!(
            acc.starting_measurement.value,
            deserialized_bytes.starting_measurement.value
        );
        assert_eq!(
            acc.starting_timestamp,
            deserialized_bytes.starting_timestamp
        );
        assert_eq!(
            acc.last_seen_measurement.value,
            deserialized_bytes.last_seen_measurement.value
        );
        assert_eq!(
            acc.last_seen_timestamp,
            deserialized_bytes.last_seen_timestamp
        );
        assert_eq!(acc.total_increase, deserialized_bytes.total_increase);
        assert_eq!(acc.sample_count, deserialized_bytes.sample_count);

        let legacy = &bytes[..bytes.len() - RESET_AWARE_WIRE_EXTENSION_LEN];
        let legacy_value = IncreaseAccumulator::deserialize_from_bytes(legacy).unwrap();
        assert_eq!(legacy_value.total_increase, 15.0);
        assert_eq!(legacy_value.sample_count, 2);
    }

    #[test]
    fn test_trait_object() {
        let acc: Box<dyn AggregateCore> = Box::new(IncreaseAccumulator::new(
            Measurement::new(10.0),
            1000,
            Measurement::new(25.0),
            2000,
        ));

        assert_eq!(acc.type_name(), "IncreaseAccumulator");
    }
}

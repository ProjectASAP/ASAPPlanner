use crate::AggregateCore;
use serde::{Deserialize, Serialize};

/// Accumulator for tracking increases in counter metrics
/// Stores the starting and last seen measurements with timestamps
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IncreaseAccumulator {
    pub starting_measurement: f64,
    pub starting_timestamp: i64,
    pub last_seen_measurement: f64,
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
            merged.total_increase += if second.starting_measurement >= merged.last_seen_measurement
            {
                second.starting_measurement - merged.last_seen_measurement
            } else {
                second.starting_measurement
            };
        }
        merged.total_increase += second.total_increase;
        merged.sample_count = merged.sample_count.saturating_add(second.sample_count);
        if second.last_seen_timestamp > merged.last_seen_timestamp {
            merged.last_seen_measurement = second.last_seen_measurement;
            merged.last_seen_timestamp = second.last_seen_timestamp;
        }

        merged
    }

    pub fn new(
        starting_measurement: f64,
        starting_timestamp: i64,
        last_seen_measurement: f64,
        last_seen_timestamp: i64,
    ) -> Self {
        let total_increase = if last_seen_timestamp <= starting_timestamp {
            0.0
        } else if last_seen_measurement >= starting_measurement {
            last_seen_measurement - starting_measurement
        } else {
            last_seen_measurement
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

    pub fn update(&mut self, measurement: f64, timestamp: i64) {
        if timestamp < self.last_seen_timestamp {
            return;
        }
        if timestamp == self.last_seen_timestamp {
            return;
        }
        if measurement >= self.last_seen_measurement {
            self.total_increase += measurement - self.last_seen_measurement;
        } else {
            self.total_increase += measurement;
        }
        self.last_seen_measurement = measurement;
        self.last_seen_timestamp = timestamp;
        self.sample_count = self.sample_count.saturating_add(1);
    }
}

impl AggregateCore for IncreaseAccumulator {
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
        // Downcast to IncreaseAccumulator
        let other_increase = other
            .as_any()
            .downcast_ref::<IncreaseAccumulator>()
            .ok_or("Failed to downcast to IncreaseAccumulator")?;

        let merged = Self::merge_pair(self, other_increase);
        Ok(Box::new(merged))
    }

    fn approx_memory_bytes(&self) -> usize {
        // Two f64 measurements, two i64 timestamps and the running totals.
        std::mem::size_of::<Self>()
    }
}

impl IncreaseAccumulator {
    /// PromQL-style increase or rate, extrapolated to `range_ms` when given.
    pub(crate) fn extrapolated_value(
        &self,
        range_ms: Option<(i64, i64)>,
        is_rate: bool,
    ) -> Result<f64, Box<dyn std::error::Error + Send + Sync>> {
        if self.sample_count < 2 || self.last_seen_timestamp <= self.starting_timestamp {
            return Err("at least two ordered counter samples are required".into());
        }
        let sampled_interval = (self.last_seen_timestamp - self.starting_timestamp) as f64 / 1000.0;
        let Some((range_start, range_end)) = range_ms else {
            return Ok(if is_rate {
                self.total_increase / sampled_interval
            } else {
                self.total_increase
            });
        };
        if range_end <= range_start {
            return Err("invalid counter evaluation range".into());
        }

        let mut duration_to_start =
            (self.starting_timestamp.saturating_sub(range_start)) as f64 / 1000.0;
        let duration_to_end = (range_end.saturating_sub(self.last_seen_timestamp)) as f64 / 1000.0;
        let average_sample_interval = sampled_interval / (self.sample_count - 1) as f64;
        let extrapolation_threshold = average_sample_interval * 1.1;

        if self.total_increase > 0.0 && self.starting_measurement >= 0.0 {
            let duration_to_zero =
                sampled_interval * (self.starting_measurement / self.total_increase);
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
        let starting_measurement = 10.0;
        let last_seen_measurement = 25.0;
        let acc = IncreaseAccumulator::new(starting_measurement, 1000, last_seen_measurement, 2000);

        assert_eq!(acc.starting_measurement, 10.0);
        assert_eq!(acc.starting_timestamp, 1000);
        assert_eq!(acc.last_seen_measurement, 25.0);
        assert_eq!(acc.last_seen_timestamp, 2000);
    }

    #[test]
    fn test_increase_accumulator_update() {
        let starting_measurement = 10.0;
        let mut acc =
            IncreaseAccumulator::new(starting_measurement, 1000, starting_measurement, 1000);

        let new_measurement = 25.0;
        acc.update(new_measurement, 2000);

        assert_eq!(acc.last_seen_measurement, 25.0);
        assert_eq!(acc.last_seen_timestamp, 2000);
        assert_eq!(acc.starting_measurement, 10.0); // Should remain unchanged
    }
}

use crate::summary_kernels::traits::{aggregate_core_object_methods, downcast_peer};
use crate::AggregateCore;
use serde::{Deserialize, Serialize};

/// Accumulator for tracking increases in counter metrics
/// Stores the starting and last seen values with timestamps
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IncreaseAccumulator {
    pub starting_value: f64,
    pub starting_timestamp: i64,
    pub last_seen_value: f64,
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
    /// the left input, matching the stable ordering of multi-tumbling-window merges.
    pub(crate) fn merge_pair(left: &Self, right: &Self) -> Self {
        let (first, second) = if left.starting_timestamp <= right.starting_timestamp {
            (left, right)
        } else {
            (right, left)
        };
        let mut merged = first.clone();
        if second.starting_timestamp > merged.last_seen_timestamp {
            merged.total_increase += if second.starting_value >= merged.last_seen_value {
                second.starting_value - merged.last_seen_value
            } else {
                second.starting_value
            };
        }
        merged.total_increase += second.total_increase;
        merged.sample_count = merged.sample_count.saturating_add(second.sample_count);
        if second.last_seen_timestamp > merged.last_seen_timestamp {
            merged.last_seen_value = second.last_seen_value;
            merged.last_seen_timestamp = second.last_seen_timestamp;
        }

        merged
    }

    pub fn new(
        starting_value: f64,
        starting_timestamp: i64,
        last_seen_value: f64,
        last_seen_timestamp: i64,
    ) -> Self {
        let total_increase = if last_seen_timestamp <= starting_timestamp {
            0.0
        } else if last_seen_value >= starting_value {
            last_seen_value - starting_value
        } else {
            last_seen_value
        };
        let sample_count = if last_seen_timestamp > starting_timestamp {
            2
        } else {
            1
        };
        Self {
            starting_value,
            starting_timestamp,
            last_seen_value,
            last_seen_timestamp,
            total_increase,
            sample_count,
        }
    }

    pub fn update(&mut self, value: f64, timestamp: i64) {
        if timestamp < self.last_seen_timestamp {
            return;
        }
        if timestamp == self.last_seen_timestamp {
            return;
        }
        if value >= self.last_seen_value {
            self.total_increase += value - self.last_seen_value;
        } else {
            self.total_increase += value;
        }
        self.last_seen_value = value;
        self.last_seen_timestamp = timestamp;
        self.sample_count = self.sample_count.saturating_add(1);
    }
}

impl AggregateCore for IncreaseAccumulator {
    aggregate_core_object_methods!();

    fn merge_with(
        &self,
        other: &dyn AggregateCore,
    ) -> Result<Box<dyn AggregateCore>, Box<dyn std::error::Error + Send + Sync>> {
        // Downcast to IncreaseAccumulator
        let other_increase = downcast_peer::<IncreaseAccumulator>(
            other,
            "Failed to downcast to IncreaseAccumulator",
        )?;

        let merged = Self::merge_pair(self, other_increase);
        Ok(Box::new(merged))
    }

    fn approx_memory_bytes(&self) -> usize {
        // Two f64 values, two i64 timestamps and the running totals.
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
        let Some(range_ms) = range_ms else {
            let sampled_interval_seconds =
                (self.last_seen_timestamp - self.starting_timestamp) as f64 / 1000.0;
            return Ok(if is_rate {
                self.total_increase / sampled_interval_seconds
            } else {
                self.total_increase
            });
        };
        if range_ms.1 <= range_ms.0 {
            return Err("invalid counter evaluation range".into());
        }
        extrapolated_rate(
            SampledDelta {
                first_sample: (self.starting_timestamp, self.starting_value),
                last_sample_ms: self.last_seen_timestamp,
                sample_count: self.sample_count,
                delta: self.total_increase,
            },
            range_ms,
            true,
            is_rate,
        )
        .ok_or_else(|| "at least two ordered counter samples are required".into())
    }
}

/// What PromQL's `rate`, `increase` and `delta` read of one series' samples
/// in a range: the first sample `(time_ms, value)`, the last sample's time,
/// the number of samples, and the change from first to last (for a counter,
/// with each reset adding the value before it). Example: samples 1, 2, …,
/// 17 every 15 s from 60 s are `first_sample: (60_000, 1.)`,
/// `last_sample_ms: 300_000`, `sample_count: 17`, `delta: 16.`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct SampledDelta {
    pub(crate) first_sample: (i64, f64),
    pub(crate) last_sample_ms: i64,
    pub(crate) sample_count: u64,
    pub(crate) delta: f64,
}

/// Prometheus `extrapolatedRate` (promql/functions.go, v3.16.0) over float
/// samples: `sampled`'s delta extrapolated to the range `(start_ms,
/// end_ms]`, per second of the range when `is_rate`. A side whose nearest
/// sample lies more than 1.1 average sample intervals from the range bound
/// extrapolates half an interval only; then, for a `counter`, the start
/// extrapolates no further back than where the counter would reach zero.
/// `None` under two samples or with no time between them, as Prometheus
/// emits no sample.
///
/// Example: samples 1, 2, …, 17 every 15 s from 60 s, `increase(x[5m])` at
/// 300 s: 60 s before the first sample exceeds 16.5 s, so the start
/// extrapolates 7.5 s; the zero point lies 15 s back, farther. The result
/// is 16 × (240 + 7.5) / 240 = 16.5.
pub(crate) fn extrapolated_rate(
    sampled: SampledDelta,
    (start_ms, end_ms): (i64, i64),
    counter: bool,
    is_rate: bool,
) -> Option<f64> {
    let SampledDelta {
        first_sample: (first_ms, first_value),
        last_sample_ms,
        sample_count,
        delta,
    } = sampled;
    let sampled_interval_seconds = (last_sample_ms as f64 - first_ms as f64) / 1000.;
    if sample_count < 2 || sampled_interval_seconds <= 0. {
        return None;
    }
    let average_interval_seconds = sampled_interval_seconds / (sample_count - 1) as f64;
    let extrapolation_threshold_seconds = average_interval_seconds * 1.1;
    let mut to_start_seconds = (first_ms as f64 - start_ms as f64) / 1000.;
    let mut to_end_seconds = (end_ms as f64 - last_sample_ms as f64) / 1000.;
    if to_start_seconds >= extrapolation_threshold_seconds {
        to_start_seconds = average_interval_seconds / 2.;
    }
    // After the half-interval cap, as Prometheus 3.x orders it.
    if counter && delta > 0. && first_value >= 0. {
        to_start_seconds = to_start_seconds.min(sampled_interval_seconds * first_value / delta);
    }
    if to_end_seconds >= extrapolation_threshold_seconds {
        to_end_seconds = average_interval_seconds / 2.;
    }
    let mut factor =
        (sampled_interval_seconds + to_start_seconds + to_end_seconds) / sampled_interval_seconds;
    if is_rate {
        factor /= (end_ms as f64 - start_ms as f64) / 1000.;
    }
    Some(delta * factor)
}

/// PromQL `irate` over a series' last two samples `(time_ms, value)`,
/// `previous` older than `last`: the increase per second, the newer value
/// being the increase after a counter reset. `None` when both share a time,
/// as Prometheus `instantValue` emits no sample then. Example: 8 then 3
/// 10 s later is a reset, 3 / 10 = 0.3.
pub(crate) fn instant_rate(previous: (i64, f64), last: (i64, f64)) -> Option<f64> {
    let interval_ms = last.0 - previous.0;
    if interval_ms == 0 {
        return None;
    }
    let increase = if last.1 < previous.1 {
        last.1
    } else {
        last.1 - previous.1
    };
    Some(increase / (interval_ms as f64 / 1000.))
}

#[cfg(test)]
mod tests {
    use super::*;

    // PromQL `irate` of the last two samples: their increase per second, the
    // newer value after a counter reset, and no value when both share a time
    // (Prometheus `instantValue` avoids dividing by zero).
    #[test]
    fn instant_rate_is_promql_irate() {
        assert_eq!(instant_rate((10_000, 5.), (20_000, 8.)), Some(0.3));
        assert_eq!(instant_rate((10_000, 8.), (20_000, 3.)), Some(0.3));
        assert_eq!(instant_rate((20_000, 5.), (20_000, 8.)), None);
    }

    #[test]
    fn test_increase_accumulator_creation() {
        let starting_value = 10.0;
        let last_seen_value = 25.0;
        let acc = IncreaseAccumulator::new(starting_value, 1000, last_seen_value, 2000);

        assert_eq!(acc.starting_value, 10.0);
        assert_eq!(acc.starting_timestamp, 1000);
        assert_eq!(acc.last_seen_value, 25.0);
        assert_eq!(acc.last_seen_timestamp, 2000);
    }

    #[test]
    fn test_increase_accumulator_update() {
        let starting_value = 10.0;
        let mut acc = IncreaseAccumulator::new(starting_value, 1000, starting_value, 1000);

        let new_measurement = 25.0;
        acc.update(new_measurement, 2000);

        assert_eq!(acc.last_seen_value, 25.0);
        assert_eq!(acc.last_seen_timestamp, 2000);
        assert_eq!(acc.starting_value, 10.0); // Should remain unchanged
    }
}

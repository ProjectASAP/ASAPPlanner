//! The order a counter (rate, increase) applies its samples in: each
//! group's in increasing time, with an optional reorder buffer that takes
//! each group's samples in any order within the data's declared
//! `max_out_of_order_ms`.
use super::{operator_error, StateColumnUpdater};
use crate::values::Value;
use crate::Error;

use planner_types::ir::schema::FieldDataType;
use std::collections::BTreeMap;
use std::ops::RangeBounds;

/// A counter's per-group sample order: each group's latest applied sample
/// time, and the samples held for reordering when the data declares a
/// bound on lateness.
#[derive(Default)]
pub(super) struct CounterOrder {
    /// Each group's latest applied sample time, by group: a sample is
    /// applied only after it (series `a` applied 10 s and 12 s: 11 s is
    /// refused).
    pub(super) latest_applied_ms: Vec<Option<i64>>,
    /// The held samples, when the updater reorders
    /// ([`StateColumnUpdater::with_max_out_of_order_ms`]).
    pub(super) reorder_buffer: Option<ReorderBuffer>,
}

/// A counter's samples held until the newest sample seen in their group
/// is more than `max_out_of_order_ms` past them, then applied in time
/// order: with a 5 s bound, group A's sample at 11 s is held until a
/// sample of A after 16 s arrives; samples of other groups do not release
/// it.
pub(super) struct ReorderBuffer {
    max_out_of_order_ms: i64,
    /// Each group's newest sample time seen (held or applied), by group.
    newest_seen_ms: Vec<Option<i64>>,
    /// The held samples' values, by group then time: a group's held
    /// samples are a contiguous range, popped in time order.
    held: BTreeMap<(u32, i64), f64>,
}

/// Memory one held sample takes in a [`ReorderBuffer`]: its key and value.
pub(super) const HELD_SAMPLE_BYTES: usize = std::mem::size_of::<((u32, i64), f64)>();

impl CounterOrder {
    /// Track one more group, with no sample yet.
    pub(super) fn add_group(&mut self) {
        self.latest_applied_ms.push(None);
        if let Some(buffer) = &mut self.reorder_buffer {
            buffer.newest_seen_ms.push(None);
        }
    }

    /// After merging another shard's states: each group's latest applied
    /// (and newest seen) time is the later of the two shards'.
    pub(super) fn merge_latest_applied(&mut self, theirs: &CounterOrder) {
        for (latest, theirs) in self
            .latest_applied_ms
            .iter_mut()
            .zip(&theirs.latest_applied_ms)
        {
            *latest = (*latest).max(*theirs);
        }
        if let (Some(buffer), Some(their_buffer)) =
            (&mut self.reorder_buffer, &theirs.reorder_buffer)
        {
            for (newest, theirs) in buffer
                .newest_seen_ms
                .iter_mut()
                .zip(&their_buffer.newest_seen_ms)
            {
                *newest = (*newest).max(*theirs);
            }
        }
    }
}

impl ReorderBuffer {
    /// An empty buffer for `group_count` groups.
    pub(super) fn new(max_out_of_order_ms: i64, group_count: usize) -> Self {
        Self {
            max_out_of_order_ms,
            newest_seen_ms: vec![None; group_count],
            held: BTreeMap::new(),
        }
    }
}

impl StateColumnUpdater {
    /// Hold counter sample `value` of `group` at `timestamp_ms` in the
    /// reorder buffer, then apply, in time order, every held sample of
    /// `group` more than the bound behind the group's newest sample.
    pub(super) fn hold_and_release(
        &mut self,
        group: u32,
        value: &Value,
        timestamp_ms: i64,
    ) -> Result<(), Error> {
        // Validate now, not when the sample is applied: a held sample is
        // applied by `seal`, which cannot fail, so every held sample must
        // be one its state takes.
        let (Value::Float64(value), FieldDataType::ExactAggregate(kind, _)) = (value, &self.family)
        else {
            return Err(operator_error("summary update requires Float64"));
        };
        super::super::factory::exact_accumulator_input(*kind, *value).map_err(operator_error)?;
        let counter = self.counter.as_mut().expect("only a counter reorders");
        let index = group as usize;
        if counter.latest_applied_ms[index].is_some_and(|prior| timestamp_ms <= prior) {
            return Err(operator_error(
                "counter sample is not after its group's last applied sample \
                 (later than max_out_of_order_ms)",
            ));
        }
        let buffer = counter
            .reorder_buffer
            .as_mut()
            .expect("a reordering updater");
        if buffer.held.contains_key(&(group, timestamp_ms)) {
            return Err(operator_error(
                "counter samples must have distinct timestamps within each group",
            ));
        }
        buffer.held.insert((group, timestamp_ms), *value);
        self.memory_bytes += HELD_SAMPLE_BYTES;
        let newest = &mut buffer.newest_seen_ms[index];
        let newest_ms = newest.map_or(timestamp_ms, |newest| newest.max(timestamp_ms));
        *newest = Some(newest_ms);
        let release_before_ms = newest_ms.saturating_sub(buffer.max_out_of_order_ms);
        self.release_held(group, release_before_ms)
    }

    /// Apply, in time order, every held sample of `group` older than
    /// `before_ms`.
    fn release_held(&mut self, group: u32, before_ms: i64) -> Result<(), Error> {
        while let Some(((_, timestamp_ms), value)) =
            self.pop_first_held((group, i64::MIN)..(group, before_ms))
        {
            self.apply(group as usize, &Value::Float64(value), timestamp_ms)?;
        }
        Ok(())
    }

    /// Apply every held sample, each group's in time order.
    pub(super) fn release_all_held(&mut self) -> Result<(), Error> {
        while let Some(((group, timestamp_ms), value)) = self.pop_first_held(..) {
            self.apply(group as usize, &Value::Float64(value), timestamp_ms)?;
        }
        Ok(())
    }

    /// Remove the first held sample (by group then time) whose key is in
    /// `keys` from the buffer, uncharging it.
    fn pop_first_held(&mut self, keys: impl RangeBounds<(u32, i64)>) -> Option<((u32, i64), f64)> {
        let buffer = self.counter.as_mut()?.reorder_buffer.as_mut()?;
        let (&key, &value) = buffer.held.range(keys).next()?;
        buffer.held.remove(&key);
        self.memory_bytes -= HELD_SAMPLE_BYTES;
        Some((key, value))
    }
}

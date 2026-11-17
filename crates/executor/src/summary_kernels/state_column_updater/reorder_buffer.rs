//! The order a counter (rate, increase, irate) applies its samples in:
//! each group's in increasing time, with an optional reorder buffer that
//! takes each group's samples in any order within the data's declared
//! `max_out_of_order_ms`.
use super::super::exact::ScalarState;
use super::{operator_error, GroupStates, StateColumnUpdater};
use crate::values::Value;
use crate::Error;

use planner_types::ir::schema::FieldDataType;
use std::collections::VecDeque;

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

/// A counter's samples held until the newest sample of their group is at
/// least `max_out_of_order_ms` past them, then applied in time order: with
/// a 5 s bound, group A's sample at 11 s is held until a sample of A at
/// 16 s or later arrives; samples of other groups do not release it. With
/// a 0 bound every sample is applied at once, as without a buffer.
pub(super) struct ReorderBuffer {
    max_out_of_order_ms: i64,
    /// Each group's held samples as (time, value), sorted by time: an
    /// in-order sample is pushed at the back and a released one popped
    /// from the front, both O(1); a late one is inserted by binary search.
    /// The newest held sample, at the back, is the group's newest sample
    /// seen (every held sample is after every applied one).
    held: Vec<VecDeque<(i64, f64)>>,
}

/// Memory one held sample slot takes in a group's queue: its time and
/// value. A queue is charged for its capacity.
pub(super) const HELD_SAMPLE_BYTES: usize = std::mem::size_of::<(i64, f64)>();

/// Memory one group's (empty) queue of held samples takes.
const HELD_QUEUE_BYTES: usize = std::mem::size_of::<VecDeque<(i64, f64)>>();

impl CounterOrder {
    /// Track one more group, with no sample yet; returns the bytes this
    /// adds (its empty held-sample queue, when reordering).
    pub(super) fn add_group(&mut self) -> usize {
        self.latest_applied_ms.push(None);
        match &mut self.reorder_buffer {
            Some(buffer) => {
                buffer.held.push(VecDeque::new());
                HELD_QUEUE_BYTES
            }
            None => 0,
        }
    }

    /// Hold samples within `max_out_of_order_ms` from now on; returns the
    /// bytes the buffer takes (an empty queue per group).
    pub(super) fn reorder_within(&mut self, max_out_of_order_ms: i64) -> usize {
        let group_count = self.latest_applied_ms.len();
        self.reorder_buffer = Some(ReorderBuffer {
            max_out_of_order_ms,
            held: (0..group_count).map(|_| VecDeque::new()).collect(),
        });
        group_count * HELD_QUEUE_BYTES
    }

    /// After merging another shard's states: each group's latest applied
    /// time is the later of the two shards'.
    pub(super) fn merge_latest_applied(&mut self, theirs: &CounterOrder) {
        for (latest, theirs) in self
            .latest_applied_ms
            .iter_mut()
            .zip(&theirs.latest_applied_ms)
        {
            *latest = (*latest).max(*theirs);
        }
    }
}

impl ReorderBuffer {
    /// The bytes the held samples' queues take: their capacity.
    #[cfg(test)]
    pub(super) fn held_bytes(&self) -> usize {
        self.held
            .iter()
            .map(|queue| queue.capacity() * HELD_SAMPLE_BYTES)
            .sum()
    }
}

impl StateColumnUpdater {
    /// Hold counter sample `value` of `group` at `timestamp_ms` in the
    /// reorder buffer, then apply, in time order, every held sample of
    /// `group` at least the bound behind the group's newest sample.
    pub(super) fn hold_and_release(
        &mut self,
        group: u32,
        value: &Value,
        timestamp_ms: i64,
    ) -> Result<(), Error> {
        // Validate now, not when the sample is applied: a held sample is
        // applied by `seal`, which cannot fail, so every held sample must
        // be one its state takes. Released samples skip the check.
        let (Value::Float64(value), FieldDataType::ExactAggregate(kind, _)) = (value, &self.family)
        else {
            return Err(operator_error("summary update requires Float64"));
        };
        super::super::factory::exact_accumulator_input(*kind, *value).map_err(operator_error)?;
        let index = group as usize;
        let Some(CounterOrder {
            latest_applied_ms,
            reorder_buffer: Some(buffer),
        }) = &mut self.counter
        else {
            unreachable!("only a reordering counter holds samples")
        };
        if latest_applied_ms[index].is_some_and(|prior| timestamp_ms <= prior) {
            return Err(operator_error(
                "counter sample is not after its group's last applied sample \
                 (later than max_out_of_order_ms)",
            ));
        }
        let queue = &mut buffer.held[index];
        let capacity = queue.capacity();
        match queue.back() {
            Some(&(newest_ms, _)) if timestamp_ms <= newest_ms => {
                let position = queue.partition_point(|&(held_ms, _)| held_ms < timestamp_ms);
                if queue[position].0 == timestamp_ms {
                    return Err(operator_error(
                        "counter samples must have distinct timestamps within each group",
                    ));
                }
                queue.insert(position, (timestamp_ms, *value));
            }
            _ => queue.push_back((timestamp_ms, *value)),
        }
        self.memory_bytes += (queue.capacity() - capacity) * HELD_SAMPLE_BYTES;
        let GroupStates::Exact { states, .. } = &mut self.states else {
            unreachable!("a counter has exact states")
        };
        let newest_ms = queue.back().expect("a sample was just held").0;
        let release_through_ms = newest_ms.saturating_sub(buffer.max_out_of_order_ms);
        while let Some(&(held_ms, held_value)) = queue.front() {
            if held_ms > release_through_ms {
                break;
            }
            queue.pop_front();
            apply_in_order(states, latest_applied_ms, index, held_ms, held_value);
        }
        Ok(())
    }

    /// Apply every held sample, each group's in time order, and free the
    /// queues' memory.
    pub(super) fn release_all_held(&mut self) {
        let Some(CounterOrder {
            latest_applied_ms,
            reorder_buffer: Some(buffer),
        }) = &mut self.counter
        else {
            return;
        };
        let GroupStates::Exact { states, .. } = &mut self.states else {
            unreachable!("a counter has exact states")
        };
        for (index, queue) in buffer.held.iter_mut().enumerate() {
            self.memory_bytes -= queue.capacity() * HELD_SAMPLE_BYTES;
            for (held_ms, held_value) in std::mem::take(queue) {
                apply_in_order(states, latest_applied_ms, index, held_ms, held_value);
            }
        }
    }
}

/// Apply a held sample, validated when held and after its group's last
/// applied sample, to group `index`'s state.
fn apply_in_order(
    states: &mut [ScalarState],
    latest_applied_ms: &mut [Option<i64>],
    index: usize,
    timestamp_ms: i64,
    value: f64,
) {
    states[index].update(value, timestamp_ms);
    latest_applied_ms[index] = Some(timestamp_ms);
}

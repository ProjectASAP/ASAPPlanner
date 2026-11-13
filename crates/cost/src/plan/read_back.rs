//! How far back from the evaluation time the readers of a time-indexed
//! producer (raw data, a chain, results stored per evaluation time) read its
//! rows, over the shared walk [`asap_types::plan::time_window::windows_read_above`]:
//! used by Stage 3 (over candidates) and [`plan_cost`] (over plan nodes) for
//! retention and for a query-time `Scan`'s read per execution
//! (`docs/design_docs/concepts/memo-groups-properties-requirements.md`,
//! section 5, "Cost and retention").
//!
//! [`plan_cost`]: crate::plan_cost

use asap_types::plan::time_window::{windows_read_above, TimeIndexedRead};

/// The most any reader of `producer` reads back from the evaluation time
/// (`readers_of` gives a node's readers, `read_of` how a reader reads,
/// [`TimeIndexedRead::of`]): each window's `read_back_ms` plus the
/// `span_ms` of the reader at its end (a range query's span, for
/// retention; 0 for one execution's read). `None`, unbounded, when one
/// reads it whole (an `@` anchor, or a reader at the evaluation time over
/// raw data, `reads_raw_data`). 0 when nothing reads it.
///
/// Example: `sum_over_time(m[5m] offset 1h)` reads `m` back 1h + 5m,
/// directly or through a query-time chain of 1-minute windows; through an
/// ingestion-time chain of 1-minute windows, `m` is read back 1 minute. A
/// stored `TimeShift(1h)` read by `TimeRange(5m)` is read back 1h + 5m.
pub fn read_back_ms<N: Copy + Ord, R: IntoIterator<Item = N>>(
    producer: N,
    readers_of: impl Fn(N) -> R,
    read_of: impl Fn(N) -> TimeIndexedRead,
    span_ms: impl Fn(N) -> u64,
    reads_raw_data: bool,
) -> Option<u64> {
    windows_read_above(producer, readers_of, read_of, reads_raw_data)?
        .into_iter()
        .try_fold(0_u64, |most, (reader, window)| {
            Some(most.max(window.read_back_ms().checked_add(span_ms(reader))?))
        })
}

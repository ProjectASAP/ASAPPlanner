//! How far back from the evaluation time the readers of a time-indexed
//! producer (raw data, a chain, results stored per evaluation time) read its
//! rows, over the shared walk
//! [`asap_types::plan::time_window::windows_readers_need`]: used by Stage 3
//! (over candidates) and [`plan_cost`] (over plan nodes) for retention and
//! for a query-time `Scan`'s read per execution
//! (`docs/design_docs/concepts/memo-groups-properties-requirements.md`,
//! section 5, "Cost and retention").
//!
//! [`plan_cost`]: crate::plan_cost

use asap_types::plan::time_window::{windows_readers_need, TimeIndexedPlan};

/// The most any reader of `producer` in `plan` reaches back from the
/// evaluation time ([`windows_readers_need`]): each window's
/// `reaches_back_ms` plus the `span_ms` of the reader at its end (a range
/// query's span, for retention; 0 for one execution's read). `None`,
/// unbounded, when one reads it whole (an `@` anchor, or a reader of only
/// the current time over raw data). 0 when nothing reads it.
///
/// Example: `sum_over_time(m[5m] offset 1h)` reads `m` back 1h + 5m,
/// directly or through a query-time chain of 1-minute windows; through an
/// ingestion-time chain of 1-minute windows, `m` is read back 1 minute. A
/// stored `TimeShift(1h)` read by `TimeRange(5m)` is read back 1h + 5m.
pub fn read_back_ms<P: TimeIndexedPlan>(
    plan: &P,
    producer: P::Node,
    span_ms: impl Fn(P::Node) -> u64,
) -> Option<u64> {
    windows_readers_need(plan, producer)?
        .into_iter()
        .try_fold(0_u64, |most, (reader, window)| {
            Some(most.max(window.reaches_back_ms().checked_add(span_ms(reader))?))
        })
}

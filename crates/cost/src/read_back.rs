//! How far back from the evaluation time the consumers of a time-indexed
//! producer (raw data, a chain, results stored per evaluation time) read its
//! rows: one walk, shared by Stage 3 (over candidates) and [`plan_cost`]
//! (over plan nodes) for retention and for a query-time `Scan`'s read
//! window (concepts doc, section 5, "Cost and retention").
//!
//! [`plan_cost`]: crate::plan_cost

use std::collections::BTreeSet;

use asap_types::ir::{ASAPOp, NonASAPOp, Operator};

/// Over the paths from `consumers` (each a reader of the producer) up
/// through `TimeShift`s without `@` (`consumers_of` gives a node's own
/// readers), the most of the shifts' offsets plus the `TimeRange`'s range
/// plus `span_ms` of that `TimeRange` (a range query's span, for retention;
/// 0 for one execution's read). A tumbling-window chain reads its newest
/// window back. Any other reader reads at the evaluation time (its
/// `span_ms`), except over raw data (`reads_raw_data`), which it reads
/// whole: `None`, unbounded, as under a `TimeShift` anchored with `@`.
///
/// Example: `sum_over_time(m[5m] offset 1h)` reads `m` back 1h + 5m.
pub fn read_back_ms<'a, N: Copy + Ord, C: 'a>(
    consumers: impl IntoIterator<Item = N>,
    operator_of: impl Fn(N) -> &'a Operator<C>,
    consumers_of: impl Fn(N) -> Vec<N>,
    span_ms: impl Fn(N) -> i64,
    reads_raw_data: bool,
) -> Option<u64> {
    let mut need_ms: i64 = 0;
    // (reader, offset of the shifts below it), walked up through shifts.
    let mut pending: Vec<(N, i64)> = consumers.into_iter().map(|c| (c, 0)).collect();
    let mut seen = BTreeSet::new();
    while let Some((consumer, offset_ms)) = pending.pop() {
        if !seen.insert((consumer, offset_ms)) {
            continue;
        }
        let read_ms = match operator_of(consumer) {
            Operator::NonASAP(NonASAPOp::TimeRange { range, .. }) => {
                let range_ms = i64::try_from(range.as_millis()).ok()?;
                offset_ms
                    .checked_add(range_ms)?
                    .checked_add(span_ms(consumer))?
            }
            Operator::NonASAP(NonASAPOp::TimeShift { shift, .. }) if shift.at.is_none() => {
                let offset_ms = offset_ms.checked_add(shift.offset_ms)?;
                pending.extend(
                    consumers_of(consumer)
                        .into_iter()
                        .map(|reader| (reader, offset_ms)),
                );
                continue;
            }
            Operator::NonASAP(NonASAPOp::TimeShift { .. }) => return None,
            // A chain builds its newest window from it.
            Operator::ASAP(ASAPOp::SummaryTumblingWindowChain {
                tumbling_window_width_ms,
                ..
            }) => offset_ms.checked_add(i64::try_from(*tumbling_window_width_ms).ok()?)?,
            _ if reads_raw_data => return None,
            _ => offset_ms.checked_add(span_ms(consumer))?,
        };
        need_ms = need_ms.max(read_ms);
    }
    u64::try_from(need_ms).ok()
}

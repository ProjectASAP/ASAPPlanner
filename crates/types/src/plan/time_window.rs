//! Time windows relative to an evaluation time, and the two walks over a
//! plan that find them, shared by the cost model (retention and a scan's
//! read), Stage 3, the installer's checks and the executor:
//!
//! - up from a time-indexed producer (raw data, a chain, results stored
//!   per evaluation time) through its readers, the window each reads
//!   ([`windows_read_above`]);
//! - down from a node, the windows its rows were read through
//!   ([`windows_below`], [`end_offset_below`]).
//!
//! The walks are generic over the node type `N`: a plan node, a Stage 3
//! candidate or an executor node, with closures giving each node's
//! operator, inputs or readers.
//!
//! Example: `sum_over_time(m[5m] offset 1m)` reads `m` through
//! `TimeShift(1m) -> TimeRange(5m)`: `TimeWindow { end_offset_ms: 60_000,
//! width_ms: 300_000 }`, `(t - 6m, t - 1m]`, read back 360,000 ms.

use serde::{Deserialize, Serialize};

use crate::ir::operator::asap::ASAPOp;
use crate::ir::operator::node::Operator;
use crate::ir::operator::non_asap::NonASAPOp;
use crate::ir::properties::execution_data_state::ExecutionTiming;

/// A time window relative to an evaluation time t: (t - end_offset_ms - width_ms, t - end_offset_ms].
///
/// E.g. a 5m read with offset 1m = `TimeWindow { end_offset_ms: 60_000,
/// width_ms: 300_000 }`; its `read_back_ms` is 360,000.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct TimeWindow {
    pub end_offset_ms: u64,
    pub width_ms: u64,
}

impl TimeWindow {
    /// How far back from t the window reaches: `end_offset_ms + width_ms`
    /// (saturating).
    pub fn read_back_ms(&self) -> u64 {
        self.end_offset_ms.saturating_add(self.width_ms)
    }

    /// The smallest window holding both.
    pub fn union(self, other: Self) -> Self {
        let end_offset_ms = self.end_offset_ms.min(other.end_offset_ms);
        Self {
            end_offset_ms,
            width_ms: self
                .read_back_ms()
                .max(other.read_back_ms())
                .saturating_sub(end_offset_ms),
        }
    }

    /// `(start, end]` of the window at evaluation time `evaluation_time_ms`,
    /// in ms (saturating).
    pub fn bounds_ms(&self, evaluation_time_ms: i64) -> (i64, i64) {
        let millis = |ms: u64| i64::try_from(ms).unwrap_or(i64::MAX);
        let end = evaluation_time_ms.saturating_sub(millis(self.end_offset_ms));
        (end.saturating_sub(millis(self.width_ms)), end)
    }
}

/// How a reader of a time-indexed producer reads its rows, relative to the
/// reader's own evaluation time ([`Self::of`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TimeIndexedRead {
    /// Its own readers read the rows `offset_ms` earlier: a `TimeShift`
    /// without `@`, or, with offset 0, a chain built at query time, which
    /// builds at each evaluation the windows its readers read.
    Through { offset_ms: i64 },
    /// The window ending at its evaluation time, `width_ms` wide: a
    /// `TimeRange`'s range, or a chain built at ingestion time's tumbling
    /// window (it builds its newest window from it).
    Window { width_ms: u64 },
    /// The rows of its own evaluation time: any other operator.
    AtEvaluationTime,
    /// Every row: a `TimeShift` anchored with `@`, which reads at a time
    /// that is not relative to the evaluation time.
    Whole,
}

impl TimeIndexedRead {
    /// How `operator`, run at `timing`, reads its input.
    ///
    /// Example: `TimeRange(5m)` is `Window { width_ms: 300_000 }`; a
    /// query-time chain `Through { offset_ms: 0 }`.
    pub fn of<C>(operator: &Operator<C>, timing: ExecutionTiming) -> Self {
        match operator {
            Operator::NonASAP(NonASAPOp::TimeShift { shift, .. }) => match shift.at {
                None => Self::Through {
                    offset_ms: shift.offset_ms,
                },
                Some(_) => Self::Whole,
            },
            Operator::NonASAP(NonASAPOp::TimeRange { range, .. }) => Self::Window {
                width_ms: u64::try_from(range.as_millis()).unwrap_or(u64::MAX),
            },
            Operator::ASAP(ASAPOp::SummaryTumblingWindowChain {
                tumbling_window_width_ms,
                ..
            }) => match timing {
                ExecutionTiming::IngestionTime => Self::Window {
                    width_ms: *tumbling_window_width_ms,
                },
                ExecutionTiming::QueryTime => Self::Through { offset_ms: 0 },
            },
            _ => Self::AtEvaluationTime,
        }
    }
}

/// The windows the readers of the time-indexed `producer` read it through:
/// walking up from it through `Through` readers (`readers_of` gives a
/// node's readers, `read_of` how a reader reads), each reader that ends a
/// path with the window it reads back from the evaluation time, the
/// offsets of the shifts walked through included: a `Window` reader its
/// window, an `AtEvaluationTime` reader the empty window at its offset.
/// `None`, unbounded, when one reads it `Whole`, an `AtEvaluationTime`
/// reader reads it and `at_evaluation_time_reads_whole` (raw data, which
/// is not stored by evaluation time), or a path's offsets are negative or
/// overflow.
///
/// Example: a `Scan` read by `TimeRange(5m)` and by `TimeShift(1h) ->
/// TimeRange(5m)` is read through `{0, 5m}` and `{1h, 5m}`.
pub fn windows_read_above<N: Copy + Ord, R: IntoIterator<Item = N>>(
    producer: N,
    readers_of: impl Fn(N) -> R,
    read_of: impl Fn(N) -> TimeIndexedRead,
    at_evaluation_time_reads_whole: bool,
) -> Option<Vec<(N, TimeWindow)>> {
    let mut windows = Vec::new();
    // (node whose readers are next, the offsets walked through below them)
    let mut pending: Vec<(N, i64)> = vec![(producer, 0)];
    let mut seen = std::collections::BTreeSet::new();
    while let Some((node, offset_ms)) = pending.pop() {
        if !seen.insert((node, offset_ms)) {
            continue;
        }
        for reader in readers_of(node) {
            let width_ms = match read_of(reader) {
                TimeIndexedRead::Through {
                    offset_ms: shift_ms,
                } => {
                    pending.push((reader, offset_ms.checked_add(shift_ms)?));
                    continue;
                }
                TimeIndexedRead::Window { width_ms } => width_ms,
                TimeIndexedRead::AtEvaluationTime if !at_evaluation_time_reads_whole => 0,
                TimeIndexedRead::AtEvaluationTime | TimeIndexedRead::Whole => return None,
            };
            let end_offset_ms = u64::try_from(offset_ms).ok()?;
            windows.push((
                reader,
                TimeWindow {
                    end_offset_ms,
                    width_ms,
                },
            ));
        }
    }
    Some(windows)
}

/// How far before its evaluation time `node` reads its input: the sum of
/// the offsets of the `TimeShift`s right below it (`input_of` gives a
/// node's only input, `operator_of` its operator). `None` when one has an
/// `@` anchor, or the sum is negative or overflows.
///
/// Example: `TimeRange(5m)` over `TimeShift(1m)` over `TimeShift(1h)`:
/// 3,660,000.
pub fn end_offset_below<'a, N: Copy, C: 'a>(
    node: N,
    input_of: impl Fn(N) -> Option<N>,
    operator_of: impl Fn(N) -> &'a Operator<C>,
) -> Option<u64> {
    let mut end_offset_ms = 0i64;
    let mut current = node;
    while let Some(input) = input_of(current) {
        let Operator::NonASAP(NonASAPOp::TimeShift { shift, .. }) = operator_of(input) else {
            break;
        };
        if shift.at.is_some() {
            return None;
        }
        end_offset_ms = end_offset_ms.checked_add(shift.offset_ms)?;
        current = input;
    }
    u64::try_from(end_offset_ms).ok()
}

/// The windows back from the evaluation time that `node` reads its rows
/// through: each `TimeRange` at or below it, and each
/// `SummaryTumblingWindowChain` at or below it reached without passing
/// one (its tumbling window), each over the shifts right below it
/// ([`end_offset_below`]). `inputs_of` gives a node's inputs and
/// `input_of` its only input. `None` when one is read through an `@`
/// anchor or a negative offset.
///
/// Example: a merge of `TimeRange(5m)` over a scan and of `TimeShift(1h)
/// -> TimeRange(5m)` over it: `{0, 5m}` and `{1h, 5m}`.
pub fn windows_below<'a, N: Copy + Ord + 'a, C: 'a, I: IntoIterator<Item = N>>(
    node: N,
    inputs_of: impl Fn(N) -> I,
    input_of: impl Fn(N) -> Option<N>,
    operator_of: impl Fn(N) -> &'a Operator<C>,
) -> Option<Vec<TimeWindow>> {
    let mut windows = Vec::new();
    let mut seen = std::collections::BTreeSet::new();
    let mut pending = vec![node];
    while let Some(id) = pending.pop() {
        if !seen.insert(id) {
            continue;
        }
        let width_ms = match operator_of(id) {
            Operator::NonASAP(NonASAPOp::TimeRange { range, .. }) => {
                u64::try_from(range.as_millis()).ok()?
            }
            Operator::ASAP(ASAPOp::SummaryTumblingWindowChain {
                tumbling_window_width_ms,
                ..
            }) => *tumbling_window_width_ms,
            _ => {
                pending.extend(inputs_of(id));
                continue;
            }
        };
        windows.push(TimeWindow {
            end_offset_ms: end_offset_below(id, &input_of, &operator_of)?,
            width_ms,
        });
    }
    Some(windows)
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIVE_MINUTES: TimeWindow = TimeWindow {
        end_offset_ms: 0,
        width_ms: 300_000,
    };

    /// `(t - 6m, t - 1m]` and `(t - 5m, t]` are both held by `(t - 6m, t]`.
    #[test]
    fn the_union_of_two_windows_holds_both() {
        let shifted = TimeWindow {
            end_offset_ms: 60_000,
            width_ms: 300_000,
        };
        assert_eq!(shifted.read_back_ms(), 360_000);
        assert_eq!(
            shifted.union(FIVE_MINUTES),
            TimeWindow {
                end_offset_ms: 0,
                width_ms: 360_000
            }
        );
        assert_eq!(shifted.bounds_ms(1_000_000), (640_000, 940_000));
    }

    /// Nodes 0 (a scan) ← 1 `TimeRange(5m)`, and 0 ← 2 `TimeShift(1h)` ← 3
    /// `TimeRange(5m)`; node 4, a reader at the evaluation time, reads 0
    /// too. The scan is read through `{0, 5m}` and `{1h, 5m}`; the reader
    /// at the evaluation time reads raw data whole.
    #[test]
    fn readers_read_a_producer_through_the_shifts_above_it() {
        let readers = |node: u32| -> Vec<u32> {
            match node {
                0 => vec![1, 2],
                2 => vec![3],
                _ => vec![],
            }
        };
        let read_of = |node: u32| match node {
            1 | 3 => TimeIndexedRead::Window { width_ms: 300_000 },
            2 => TimeIndexedRead::Through {
                offset_ms: 3_600_000,
            },
            _ => TimeIndexedRead::AtEvaluationTime,
        };
        let mut windows = windows_read_above(0, readers, read_of, true).unwrap();
        windows.sort();
        assert_eq!(
            windows,
            vec![
                (1, FIVE_MINUTES),
                (
                    3,
                    TimeWindow {
                        end_offset_ms: 3_600_000,
                        width_ms: 300_000
                    }
                )
            ]
        );
        let with_reader_at_evaluation = |node: u32| match node {
            0 => vec![1, 4],
            _ => vec![],
        };
        assert_eq!(
            windows_read_above(0, with_reader_at_evaluation, read_of, true),
            None
        );
        assert_eq!(
            windows_read_above(0, with_reader_at_evaluation, read_of, false)
                .map(|windows| windows.len()),
            Some(2)
        );
    }
}

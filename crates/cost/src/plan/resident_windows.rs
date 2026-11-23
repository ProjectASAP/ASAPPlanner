//! The windows of a stored tumbling-window chain that stay resident in
//! memory when the data plane keeps every window in a file and the
//! recently written or read ones also in a cache of decoded windows
//! (`docs/design_docs/proposals/cost-terms-from-calibration.md`, section
//! 1). A view's resident windows are those of its largest single reader:
//! the windows that reader read within the cache's idle time and the
//! windows written within it, among the windows that reader alone needs
//! retained. `plan_cost` takes the most over a plan's readers, and Stage 3's
//! selection the most over its selected readers, from the same counts.

use crate::plan::kept_results::computed_range;
use crate::AnswerCache;
use asap_types::ir::operator::tumbling_window_chain::cover_windows;
use asap_types::workload::{greatest_common_divisor, QueryWorkloadEntry};

/// Which windows of a chain one read takes, at the time it reads.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum WindowRead {
    /// Every window ending in `(t − offset − range, t − offset]`: a
    /// `TimeRange(range)` over `TimeShift`s of `offset` in all. A range of
    /// 0 (a read of the current time only) takes the newest window ended
    /// by `t − offset`.
    Range { range_ms: u64, offset_ms: u64 },
    /// The windows of level `level` that a cover of `range` over
    /// `level_widths_ms` (widest first) takes at `t` ([`cover_windows`]).
    Cover {
        range_ms: u64,
        level_widths_ms: Vec<u64>,
        level: usize,
    },
    /// Every retained window: a reader whose reads the model does not
    /// follow.
    Every,
}

/// When one reader reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ReadTimes {
    /// A query's requests on the grid `period_ms`, `phase_ms`, each
    /// evaluating `steps` steps `step_ms` apart, the last at the request's
    /// time.
    Query {
        period_ms: u64,
        phase_ms: u64,
        steps: u64,
        step_ms: u64,
    },
    /// An ingestion-time reader refreshed when each of its `width_ms`
    /// windows closes (at multiples of `width_ms`).
    Refresh { width_ms: u64 },
}

impl ReadTimes {
    /// When a query-time reader reached by `entry`'s requests reads: on the
    /// query's evaluation grid, at each request the steps it computes (its
    /// range's, or with an `answer_cache` only its new ones, a fractional
    /// mean rounded up: `computed_range`), one for an instant query.
    pub fn of_query(entry: &QueryWorkloadEntry, answer_cache: Option<AnswerCache>) -> Self {
        let grid = entry.evaluation_grid();
        let range = computed_range(entry, answer_cache);
        ReadTimes::Query {
            period_ms: grid.period_ms,
            phase_ms: grid.phase_ms,
            steps: range.map_or(1, |range| {
                range.mean_evaluations_per_request().ceil() as u64
            }),
            step_ms: range.map_or(0, |range| range.step_ms),
        }
    }
}

/// One reader of a chain: what it reads, and when.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ChainReader {
    pub read: WindowRead,
    pub times: ReadTimes,
}

impl ChainReader {
    /// How far back from its newest read time this reader reads, in ms:
    /// its offset and range (a cover's range) and its request's older
    /// steps. `None` for [`WindowRead::Every`].
    ///
    /// Example: `max_over_time(m[5m] offset 1h)` as a 1 h range at 15 s
    /// steps: 1 h + 5 min + 1 h.
    pub fn reads_back_ms(&self) -> Option<u64> {
        let span_ms = match self.times {
            ReadTimes::Query { steps, step_ms, .. } => steps.saturating_sub(1) * step_ms,
            ReadTimes::Refresh { .. } => 0,
        };
        let window_ms = match &self.read {
            WindowRead::Range {
                range_ms,
                offset_ms,
            } => offset_ms + range_ms,
            WindowRead::Cover { range_ms, .. } => *range_ms,
            WindowRead::Every => return None,
        };
        Some(window_ms + span_ms)
    }

    /// The windows of a chain of `width_ms` this reader alone needs
    /// retained: [`Self::reads_back_ms`] in windows, at least one (the
    /// newest, as `retained_output_count` counts a retention), and one more
    /// for a read of the current time only, which takes the whole window
    /// ended by its time. `None` for [`WindowRead::Every`].
    ///
    /// Example: over 15 s windows, `TimeRange(5m)` 20, a read of the
    /// current time an hour back 241.
    pub fn retained_windows(&self, width_ms: u64) -> Option<u64> {
        let width_ms = width_ms.max(1);
        let window_read_whole = match self.read {
            WindowRead::Range { range_ms: 0, .. } => width_ms,
            _ => 0,
        };
        Some(
            (self.reads_back_ms()? + window_read_whole)
                .max(width_ms)
                .div_ceil(width_ms),
        )
    }
}

/// How many windows of a chain of `width_ms` windows are resident at
/// `now_ms` (a multiple of `width_ms`): those written within the idle time
/// (ending in `(now − idle, now]`) and those `reader` read within it (none
/// without one), of the `retained` newest (ending in
/// `(now − retained · width, now]`).
fn resident_window_count(
    width_ms: u64,
    retained: u64,
    idle_ms: u64,
    now_ms: i64,
    reader: Option<&ChainReader>,
) -> u64 {
    let width = width_ms.max(1) as i64;
    let newest = now_ms.div_euclid(width);
    let oldest = newest - retained as i64 + 1;
    let idle_start = now_ms - idle_ms as i64;
    // Inclusive ranges of window indices (a window ending at `k · width`
    // has index `k`), clipped to the retained ones.
    let mut ranges: Vec<(i64, i64)> = Vec::new();
    let mut add = |first: i64, last: i64| {
        let (first, last) = (first.max(oldest), last.min(newest));
        if first <= last {
            ranges.push((first, last));
        }
    };
    // Every window ending in `(start, end]`.
    let interval = |start: i64, end: i64| (start.div_euclid(width) + 1, end.div_euclid(width));
    // Written within the idle time.
    let (first, last) = interval(idle_start, now_ms);
    add(first, last);
    if let Some(reader) = reader {
        if reader.read == WindowRead::Every {
            add(oldest, newest);
        } else {
            for t in read_times(reader.times, idle_start, now_ms) {
                match &reader.read {
                    WindowRead::Range {
                        range_ms: 0,
                        offset_ms,
                    } => {
                        // The newest window ended by `t − offset`.
                        let index = (t - *offset_ms as i64).div_euclid(width);
                        add(index, index);
                    }
                    WindowRead::Range {
                        range_ms,
                        offset_ms,
                    } => {
                        let end = t - *offset_ms as i64;
                        let (first, last) = interval(end - *range_ms as i64, end);
                        add(first, last);
                    }
                    WindowRead::Cover {
                        range_ms,
                        level_widths_ms,
                        level,
                    } => {
                        // A time off the narrowest level's grid reads no
                        // cover (the executor refuses it).
                        if let Ok(windows) = cover_windows(t - *range_ms as i64, t, level_widths_ms)
                        {
                            for (at, end) in windows {
                                if at == *level {
                                    let index = end.div_euclid(width);
                                    add(index, index);
                                }
                            }
                        }
                    }
                    WindowRead::Every => unreachable!("handled above"),
                }
            }
        }
    }
    ranges.sort_unstable();
    let mut count = 0;
    let mut covered_to = i64::MIN;
    for (first, last) in ranges {
        let first = first.max(covered_to.saturating_add(1));
        if first <= last {
            count += (last - first + 1) as u64;
        }
        covered_to = covered_to.max(last);
    }
    count
}

/// The times in `(idle_start, now]` at which a reader of `times` reads.
fn read_times(times: ReadTimes, idle_start: i64, now_ms: i64) -> Vec<i64> {
    let on_grid = |period_ms: u64, phase_ms: u64| {
        let period = period_ms.max(1) as i64;
        let phase = (phase_ms as i64).rem_euclid(period);
        let mut t = (now_ms - phase).div_euclid(period) * period + phase;
        let mut times = Vec::new();
        while t > idle_start {
            times.push(t);
            t -= period;
        }
        times
    };
    match times {
        ReadTimes::Query {
            period_ms,
            phase_ms,
            steps,
            step_ms,
        } => on_grid(period_ms, phase_ms)
            .into_iter()
            .flat_map(|request| {
                (0..steps.max(1)).map(move |step| request - (step * step_ms) as i64)
            })
            .collect(),
        ReadTimes::Refresh { width_ms } => on_grid(width_ms, 0),
    }
}

/// The resident windows of a chain of `width_ms` windows retaining
/// `retained` over time, written within `idle_ms` and read by `reader`
/// (none without one): their mean and their most, over every `now` on the
/// chain's grid within one period of the reader (the least common multiple
/// of the chain's width, its period and its cover's widths; `now` sampled
/// at most 2,048 times).
fn resident_windows_over_time(
    width_ms: u64,
    retained: u64,
    idle_ms: u64,
    reader: Option<&ChainReader>,
) -> (f64, u64) {
    let width = width_ms.max(1);
    let mut period = width;
    if let Some(reader) = reader {
        let mut lengths = vec![match reader.times {
            ReadTimes::Query { period_ms, .. } => period_ms,
            ReadTimes::Refresh { width_ms } => width_ms,
        }];
        if let WindowRead::Cover {
            level_widths_ms, ..
        } = &reader.read
        {
            lengths.extend(level_widths_ms.iter().copied());
        }
        for length in lengths {
            let length = length.max(1);
            period = period / greatest_common_divisor(period, length) * length;
        }
    }
    const MOST_SAMPLES: u64 = 2_048;
    let positions = (period / width).max(1);
    let stride = positions.div_ceil(MOST_SAMPLES);
    // A `now` far from the epoch, so every read stays at positive times.
    let base = (1_000 * period.max(retained * width)) as i64;
    let (mut total, mut most, mut samples) = (0u64, 0u64, 0u64);
    let mut position = 0;
    while position < positions {
        let now = base + (position * width) as i64;
        let resident = resident_window_count(width, retained, idle_ms, now, reader);
        total += resident;
        most = most.max(resident);
        samples += 1;
        position += stride;
    }
    (total as f64 / samples.max(1) as f64, most)
}

/// The resident windows, mean and most, a chain of `width_ms` windows keeps
/// for `reader` alone under a cache of `idle_ms`: those it read and those
/// written within the idle time, of the windows it alone needs retained
/// ([`ChainReader::retained_windows`]). `None` for [`WindowRead::Every`],
/// which keeps every retained window.
pub fn reader_resident_windows(
    width_ms: u64,
    idle_ms: u64,
    reader: &ChainReader,
) -> Option<(f64, u64)> {
    let retained = reader.retained_windows(width_ms)?;
    Some(resident_windows_over_time(
        width_ms,
        retained,
        idle_ms,
        Some(reader),
    ))
}

/// The resident windows, mean and most, a chain of `width_ms` windows
/// retaining `retained` keeps under a cache of `idle_ms` whatever reads it:
/// the windows written within the idle time.
pub fn written_resident_windows(width_ms: u64, retained: u64, idle_ms: u64) -> (f64, u64) {
    resident_windows_over_time(width_ms, retained, idle_ms, None)
}

/// The resident windows, mean and most, of a chain of `width_ms` windows
/// retaining `retained` read by `readers` under a cache of `idle_ms`: the
/// most of [`written_resident_windows`] and each reader's
/// [`reader_resident_windows`]; every retained window with a
/// [`WindowRead::Every`] reader. Exact for readers that each read up to the
/// newest window (the largest reader's reads contain the others'); below
/// the union of what they read when two read disjoint windows.
pub fn view_resident_windows(
    width_ms: u64,
    retained: u64,
    idle_ms: u64,
    readers: &[ChainReader],
) -> (f64, u64) {
    let mut most = written_resident_windows(width_ms, retained, idle_ms);
    for reader in readers {
        let (mean, peak) = reader_resident_windows(width_ms, idle_ms, reader)
            .unwrap_or((retained as f64, retained));
        most = (most.0.max(mean), most.1.max(peak));
    }
    most
}

#[cfg(test)]
mod tests {
    use super::*;

    const S15: u64 = 15_000;
    const MIN: u64 = 60_000;
    const HOUR: u64 = 3_600_000;

    fn every_15s() -> ReadTimes {
        ReadTimes::Query {
            period_ms: S15,
            phase_ms: 0,
            steps: 1,
            step_ms: 0,
        }
    }

    /// `max_over_time(m[5m] offset 1h)` every 15 s over a 15 s chain: it
    /// alone needs 1 h 5 min retained (260 windows, ending in
    /// `(now − 1 h 5 min, now]`); idle time 60 s. The four evaluations of
    /// the last minute read the windows ending in
    /// `(now − 45 s − 1 h 5 min, now − 1 h]`, of which the 20 ending in
    /// `(now − 1 h 5 min, now − 1 h]` are still retained; the four written in
    /// the last minute end in `(now − 60 s, now]`. 24 resident.
    #[test]
    fn an_offset_reader_keeps_only_its_range_and_the_newest_windows_resident() {
        let reader = ChainReader {
            read: WindowRead::Range {
                range_ms: 5 * MIN,
                offset_ms: HOUR,
            },
            times: every_15s(),
        };
        assert_eq!(reader.retained_windows(S15), Some(260));
        let now = 100 * HOUR as i64;
        assert_eq!(resident_window_count(S15, 260, MIN, now, Some(&reader)), 24);
        assert_eq!(reader_resident_windows(S15, MIN, &reader), Some((24.0, 24)));
    }

    /// `max_over_time(m[1h])` every 15 s: each evaluation reads its whole
    /// hour, so the last minute's four read every one of the 240 windows it
    /// needs retained: all resident.
    #[test]
    fn a_reader_of_its_whole_range_keeps_every_retained_window_resident() {
        let reader = ChainReader {
            read: WindowRead::Range {
                range_ms: HOUR,
                offset_ms: 0,
            },
            times: every_15s(),
        };
        assert_eq!(
            reader_resident_windows(S15, MIN, &reader),
            Some((240.0, 240))
        );
    }

    /// A 1 h cover over 5 min and 15 s windows, every 15 s, at
    /// `now = 100 h + 105 s`, idle time 60 s. The evaluations at now − 45,
    /// − 30, − 15 and now read the 15 s windows at their right edge ending
    /// in `(100 h, t]` (at most 7, ending at 100 h + 15 … + 105 s) and at
    /// their left edge ending in `(t − 1 h, 99 h + 300 s]` (at most 16, from
    /// the earliest `t`). The four written in the last minute are among the
    /// right edge's. 23 resident of the 15 s level's 245.
    #[test]
    fn a_cover_keeps_only_the_edges_of_its_narrow_level_resident() {
        let reader = ChainReader {
            read: WindowRead::Cover {
                range_ms: HOUR,
                level_widths_ms: vec![5 * MIN, S15],
                level: 1,
            },
            times: every_15s(),
        };
        let now = (100 * HOUR + 105_000) as i64;
        assert_eq!(resident_window_count(S15, 245, MIN, now, Some(&reader)), 23);
    }

    /// A 5 min level built from a 15 s chain at each 5 min close reads the
    /// 20 windows of the closing 5 min, all it needs retained; within 60 s
    /// of a close they are resident, then only the four newest: over the 20
    /// positions of `now` in a 5 min period, 4 of them (now at the close and
    /// 15, 30, 45 s after) keep the 20 retained windows, the other 16 keep
    /// 4. Mean (4 · 20 + 16 · 4) / 20 = 7.2, most 20.
    #[test]
    fn a_wider_level_keeps_the_windows_it_built_from_resident_for_the_idle_time() {
        let reader = ChainReader {
            read: WindowRead::Range {
                range_ms: 5 * MIN,
                offset_ms: 0,
            },
            times: ReadTimes::Refresh { width_ms: 5 * MIN },
        };
        assert_eq!(reader_resident_windows(S15, MIN, &reader), Some((7.2, 20)));
    }

    /// A read of the current time only, an hour back
    /// (`TimeShift(1h)` over a chain read at its newest window), every
    /// 15 s: it needs 241 windows retained, ending in
    /// `(now − 1 h − 15 s, now]`. Of the four windows the last minute's
    /// reads took (ended by `t − 1 h`), the one ending at `now − 1 h` is
    /// still retained, besides the four newest: 5 resident.
    #[test]
    fn a_zero_length_read_with_an_offset_keeps_the_window_it_reads_resident() {
        let reader = ChainReader {
            read: WindowRead::Range {
                range_ms: 0,
                offset_ms: HOUR,
            },
            times: every_15s(),
        };
        assert_eq!(reader.retained_windows(S15), Some(241));
        assert_eq!(reader_resident_windows(S15, MIN, &reader), Some((5.0, 5)));
    }

    /// A reader the model does not follow keeps every retained window.
    #[test]
    fn an_unfollowed_reader_keeps_every_window_resident() {
        let reader = ChainReader {
            read: WindowRead::Every,
            times: every_15s(),
        };
        assert_eq!(reader_resident_windows(S15, MIN, &reader), None);
        assert_eq!(
            view_resident_windows(S15, 241, MIN, &[reader]),
            (241.0, 241)
        );
    }

    /// Without readers, the windows written within the idle time.
    #[test]
    fn without_readers_only_the_newest_windows_are_resident() {
        assert_eq!(written_resident_windows(S15, 241, MIN), (4.0, 4));
        assert_eq!(view_resident_windows(S15, 241, MIN, &[]), (4.0, 4));
    }

    /// Two readers, the offset reader above and `max_over_time(m[5m])`
    /// every 15 s (20 windows), over a chain retaining 260: the view keeps
    /// the larger's 24, not their union's 44 (the documented undercount for
    /// disjoint readers).
    #[test]
    fn a_view_keeps_its_largest_readers_windows() {
        let offset = ChainReader {
            read: WindowRead::Range {
                range_ms: 5 * MIN,
                offset_ms: HOUR,
            },
            times: every_15s(),
        };
        let recent = ChainReader {
            read: WindowRead::Range {
                range_ms: 5 * MIN,
                offset_ms: 0,
            },
            times: every_15s(),
        };
        assert_eq!(reader_resident_windows(S15, MIN, &recent), Some((20.0, 20)));
        assert_eq!(
            view_resident_windows(S15, 260, MIN, &[offset, recent]),
            (24.0, 24)
        );
    }
}

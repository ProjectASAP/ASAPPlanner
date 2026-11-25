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
use asap_types::ir::operator::tumbling_window_chain::CoverLevel;
use asap_types::workload::{least_common_multiple, QueryWorkloadEntry};
use std::collections::HashSet;

/// Which windows of a chain one read takes, at the time it reads.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum WindowRead {
    /// Every window ending in `(t − offset − range, t − offset]`: a
    /// `TimeRange(range)` over `TimeShift`s of `offset` in all. A range of
    /// 0 (a read of the current time only) takes the newest window ended
    /// by `t − offset`.
    Range { range_ms: u64, offset_ms: u64 },
    /// The windows a cover takes from one of its levels at `t`
    /// ([`CoverLevel::window_ends_at`]).
    Cover(CoverLevel),
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
    /// Runs per request: a range request's steps, 1 for an instant query or
    /// a refresh.
    pub fn runs_per_request(&self) -> f64 {
        match *self {
            ReadTimes::Query { steps, .. } => steps.max(1) as f64,
            ReadTimes::Refresh { .. } => 1.0,
        }
    }

    /// Requests per second: one per period of a query's grid, one per
    /// window close of a refresh.
    pub fn requests_per_second(&self) -> f64 {
        let period_ms = match *self {
            ReadTimes::Query { period_ms, .. } => period_ms,
            ReadTimes::Refresh { width_ms } => width_ms,
        };
        1_000.0 / period_ms.max(1) as f64
    }

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
            WindowRead::Cover(cover) => cover.offset_ms + cover.range_ms,
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
    // Written within the idle time.
    let (first, last) = windows_ending_in(idle_start, now_ms, width);
    add(first, last);
    if let Some(reader) = reader {
        if reader.read == WindowRead::Every {
            add(oldest, newest);
        } else {
            for t in read_times(reader.times, idle_start, now_ms) {
                for (first, last) in windows_read_at(&reader.read, t, width) {
                    add(first, last);
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

/// Every window ending in `(start, end]`, by index (a window ending at
/// `k · width` has index `k`), as an inclusive range.
fn windows_ending_in(start: i64, end: i64, width: i64) -> (i64, i64) {
    (start.div_euclid(width) + 1, end.div_euclid(width))
}

/// The windows, by index (a window ending at `k · width` has index `k`), as
/// inclusive ranges, that `read` takes at time `t`; none for
/// [`WindowRead::Every`].
fn windows_read_at(read: &WindowRead, t: i64, width: i64) -> Vec<(i64, i64)> {
    match read {
        WindowRead::Range {
            range_ms: 0,
            offset_ms,
        } => {
            // The newest window ended by `t − offset`.
            let index = (t - *offset_ms as i64).div_euclid(width);
            vec![(index, index)]
        }
        WindowRead::Range {
            range_ms,
            offset_ms,
        } => {
            let end = t - *offset_ms as i64;
            vec![windows_ending_in(end - *range_ms as i64, end, width)]
        }
        // A time off the narrowest level's grid reads no cover (the
        // executor refuses it).
        WindowRead::Cover(cover) => cover
            .window_ends_at(t)
            .map(|ends| {
                ends.into_iter()
                    .map(|end| {
                        let index = end.div_euclid(width);
                        (index, index)
                    })
                    .collect()
            })
            .unwrap_or_default(),
        WindowRead::Every => Vec::new(),
    }
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
        if let WindowRead::Cover(cover) = &reader.read {
            lengths.extend(cover.level_widths_ms.iter().copied());
        }
        for length in lengths {
            let length = length.max(1);
            period = least_common_multiple(period, length);
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

/// What `reader` reads of a chain of `width_ms` windows under a cache of
/// `idle_ms`, per second: the windows each request reads (its steps'
/// windows, each decoded once), and of them those read from disk: neither
/// written within the idle time before the request nor read by one of the
/// reader's requests within it. Averaged over the requests of one period
/// of the reader (the least common multiple of its period, the chain's
/// width and its cover's widths; at most 2,048 requests). `None` for
/// [`WindowRead::Every`], whose reads the model does not follow.
///
/// Example: `max_over_time(m[1h])` every 5 min over 5 min windows, idle
/// 60 s: each request reads 12 windows, all from disk: 12 ÷ 300 s each.
pub fn reader_window_reads_per_second(
    width_ms: u64,
    idle_ms: u64,
    reader: &ChainReader,
) -> Option<WindowReadsPerSecond> {
    if reader.read == WindowRead::Every {
        return None;
    }
    let width = width_ms.max(1);
    let (request_period_ms, steps, step_ms) = match reader.times {
        ReadTimes::Query {
            period_ms,
            steps,
            step_ms,
            ..
        } => (period_ms.max(1), steps.max(1), step_ms),
        ReadTimes::Refresh { width_ms } => (width_ms.max(1), 1, 0),
    };
    let phase_ms = match reader.times {
        ReadTimes::Query { phase_ms, .. } => phase_ms % request_period_ms,
        ReadTimes::Refresh { .. } => 0,
    };
    let mut period = least_common_multiple(request_period_ms, width);
    if let WindowRead::Cover(cover) = &reader.read {
        for &length in &cover.level_widths_ms {
            let length = length.max(1);
            period = least_common_multiple(period, length);
        }
    }
    let width = width as i64;
    // The windows one request at `request` reads.
    let request_windows = |request: i64| -> HashSet<i64> {
        (0..steps)
            .flat_map(|step| {
                windows_read_at(&reader.read, request - (step * step_ms) as i64, width)
            })
            .flat_map(|(first, last)| first..=last)
            .collect()
    };
    const MAX_SAMPLED_REQUESTS: u64 = 2_048;
    let requests = (period / request_period_ms).max(1);
    let stride = requests.div_ceil(MAX_SAMPLED_REQUESTS);
    // A request time far from the epoch, on the reader's grid, so every
    // read stays at positive times.
    let far_ms = 1_000 * period.max(reader.reads_back_ms().unwrap_or(0) + width as u64);
    let start_ms = (far_ms.div_ceil(period) * period + phase_ms) as i64;
    let (mut read, mut from_disk, mut sampled) = (0u64, 0u64, 0u64);
    let mut request_index = 0;
    while request_index < requests {
        let request = start_ms + (request_index * request_period_ms) as i64;
        let idle_start = request - idle_ms as i64;
        let windows = request_windows(request);
        // Read by an earlier request within the idle time.
        let mut earlier: HashSet<i64> = HashSet::new();
        let mut before = request - request_period_ms as i64;
        while before > idle_start {
            earlier.extend(request_windows(before));
            before -= request_period_ms as i64;
        }
        // Written within the idle time: ending in `(idle_start, request]`.
        let written = (idle_start.div_euclid(width) + 1)..=request.div_euclid(width);
        read += windows.len() as u64;
        from_disk += windows
            .iter()
            .filter(|index| !written.contains(index) && !earlier.contains(index))
            .count() as u64;
        sampled += 1;
        request_index += stride;
    }
    let requests_per_second = reader.times.requests_per_second();
    let per_request = |count: u64| count as f64 / sampled.max(1) as f64;
    Some(WindowReadsPerSecond {
        windows_read_per_second: per_request(read) * requests_per_second,
        windows_read_from_disk_per_second: per_request(from_disk) * requests_per_second,
    })
}

/// What one reader reads of a chain per second
/// ([`reader_window_reads_per_second`]).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct WindowReadsPerSecond {
    /// Windows read per second, each request's once.
    pub windows_read_per_second: f64,
    /// Of them, read from disk: not in the cache when read.
    pub windows_read_from_disk_per_second: f64,
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
            read: WindowRead::Cover(CoverLevel {
                range_ms: HOUR,
                offset_ms: 0,
                level_widths_ms: vec![5 * MIN, S15],
                level: 1,
            }),
            times: every_15s(),
        };
        let now = (100 * HOUR + 105_000) as i64;
        assert_eq!(resident_window_count(S15, 245, MIN, now, Some(&reader)), 23);
    }

    /// The same cover an hour back (`[1h] offset 1h`), at the same `now`: it
    /// alone needs 2 h retained, 480 windows, ending in
    /// `(98 h + 105 s, now]`. The evaluations at now − 45 … now read the 15 s
    /// windows at the right edge of `(t − 2 h, t − 1 h]`, ending in
    /// `(99 h, t − 1 h]` (at most 7), and at its left edge, ending in
    /// `(t − 2 h, 98 h + 300 s]`, of which the 13 ending after 98 h + 105 s
    /// are still retained. The four written in the last minute are an hour
    /// newer than any of them: 7 + 13 + 4 = 24 resident.
    #[test]
    fn a_cover_under_an_offset_keeps_the_edges_an_hour_back_and_the_newest_resident() {
        let reader = ChainReader {
            read: WindowRead::Cover(CoverLevel {
                range_ms: HOUR,
                offset_ms: HOUR,
                level_widths_ms: vec![5 * MIN, S15],
                level: 1,
            }),
            times: every_15s(),
        };
        assert_eq!(reader.retained_windows(S15), Some(480));
        let now = (100 * HOUR + 105_000) as i64;
        assert_eq!(resident_window_count(S15, 480, MIN, now, Some(&reader)), 24);
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

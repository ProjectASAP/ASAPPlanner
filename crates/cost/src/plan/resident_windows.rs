//! The windows of a stored tumbling-window chain that stay resident in
//! memory when the data plane keeps a view's windows in a cache of
//! recently read windows and the rest in files
//! (`docs/design_docs/proposals/cost-terms-from-calibration.md`, section
//! 1): a window is resident while it was written or read within the
//! cache's idle time; the others are on disk.

use std::collections::BTreeSet;

use asap_types::ir::operator::tumbling_window_chain::cover_windows;
use asap_types::workload::greatest_common_divisor;

/// Which windows of a chain one read takes, at the time it reads.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum WindowRead {
    /// Every window ending in `(t − offset − range, t − offset]`: a
    /// `TimeRange(range)` over `TimeShift`s of `offset` in all.
    Range { range_ms: u64, offset_ms: u64 },
    /// The windows of level `level` that a cover of `range` over
    /// `level_widths_ms` (widest first) takes at `t`
    /// ([`cover_windows`]).
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
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum ReadTimes {
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

/// One reader of a chain: what it reads, and when.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ChainReader {
    pub(crate) read: WindowRead,
    pub(crate) times: ReadTimes,
}

/// The ends of the windows of a chain of `width_ms` windows resident at
/// `now_ms` (a multiple of `width_ms`): those written within the idle time
/// (ending in `(now − idle, now]`) and those any reader read within it, of
/// the `retained` newest (ending in `(now − retained · width, now]`).
pub(crate) fn resident_window_ends(
    width_ms: u64,
    retained: u64,
    idle_ms: u64,
    now_ms: i64,
    readers: &[ChainReader],
) -> BTreeSet<i64> {
    let width = width_ms.max(1) as i64;
    let oldest_kept = now_ms - retained as i64 * width;
    let idle_start = now_ms - idle_ms as i64;
    let mut ends = BTreeSet::new();
    // Every window end in `(start, end]` on the chain's grid.
    let add_interval = |ends: &mut BTreeSet<i64>, start: i64, end: i64| {
        let first = start.max(oldest_kept).div_euclid(width) * width + width;
        let mut window_end = first;
        while window_end <= end.min(now_ms) {
            ends.insert(window_end);
            window_end += width;
        }
    };
    // Written within the idle time.
    add_interval(&mut ends, idle_start, now_ms);
    for reader in readers {
        if reader.read == WindowRead::Every {
            add_interval(&mut ends, oldest_kept, now_ms);
            continue;
        }
        for t in read_times(reader.times, idle_start, now_ms) {
            match &reader.read {
                WindowRead::Range {
                    range_ms,
                    offset_ms,
                } => {
                    let end = t - *offset_ms as i64;
                    add_interval(&mut ends, end - *range_ms as i64, end);
                }
                WindowRead::Cover {
                    range_ms,
                    level_widths_ms,
                    level,
                } => {
                    // A time off the narrowest level's grid reads no cover
                    // (the executor refuses it).
                    if let Ok(windows) = cover_windows(t - *range_ms as i64, t, level_widths_ms) {
                        ends.extend(
                            windows
                                .into_iter()
                                .filter(|(at, end)| at == level && *end > oldest_kept)
                                .map(|(_, end)| end),
                        );
                    }
                }
                WindowRead::Every => unreachable!("handled above"),
            }
        }
    }
    ends
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

/// The resident windows of a chain over time: their mean and their most,
/// over every `now` on the chain's grid within one period of its readers.
pub(crate) fn resident_windows(
    width_ms: u64,
    retained: u64,
    idle_ms: u64,
    readers: &[ChainReader],
) -> (f64, u64) {
    let width = width_ms.max(1);
    // One period of what the readers do: the least common multiple of the
    // chain's width, their periods and their covers' widest windows,
    // bounded so a long period is sampled, not walked.
    let mut period = width;
    for reader in readers {
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
        let resident = resident_window_ends(width, retained, idle_ms, now, readers).len() as u64;
        total += resident;
        most = most.max(resident);
        samples += 1;
        position += stride;
    }
    (total as f64 / samples.max(1) as f64, most)
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
            step_ms: S15,
        }
    }

    /// `max_over_time(m[5m] offset 1h)` every 15 s over a 15 s chain
    /// retained 1 h 5 min (260 windows, ending in `(now − 1 h 5 min, now]`),
    /// idle time 60 s: the four evaluations of the last minute read the
    /// windows ending in `(now − 45 s − 1 h 5 min, now − 1 h]`, of which the
    /// 20 ending in `(now − 1 h 5 min, now − 1 h]` are still retained; the
    /// four written in the last minute end in `(now − 60 s, now]`. 24
    /// resident.
    #[test]
    fn an_offset_reader_keeps_only_its_range_and_the_newest_windows_resident() {
        let reader = ChainReader {
            read: WindowRead::Range {
                range_ms: 5 * MIN,
                offset_ms: HOUR,
            },
            times: every_15s(),
        };
        let now = 100 * HOUR as i64;
        let resident = resident_window_ends(S15, 260, MIN, now, std::slice::from_ref(&reader));
        assert_eq!(resident.len(), 24);
        assert_eq!(resident_windows(S15, 260, MIN, &[reader]), (24.0, 24));
    }

    /// `max_over_time(m[1h])` every 15 s: each evaluation reads its whole
    /// hour, so the last minute's four read every one of 243 windows
    /// (1 h + 45 s), capped at the 241 retained: all resident.
    #[test]
    fn a_reader_of_its_whole_range_keeps_every_retained_window_resident() {
        let reader = ChainReader {
            read: WindowRead::Range {
                range_ms: HOUR,
                offset_ms: 0,
            },
            times: every_15s(),
        };
        assert_eq!(resident_windows(S15, 241, MIN, &[reader]), (241.0, 241));
    }

    /// A 1 h cover over 5 min and 15 s windows, every 15 s, at
    /// `now = 100 h + 105 s`, idle time 60 s. The evaluations at now − 45,
    /// − 30, − 15 and now read the 15 s windows at their right edge ending
    /// in `(100 h, t]` (at most 7, ending at 100 h + 15 … + 105 s) and at
    /// their left edge ending in `(t − 1 h, 99 h + 300 s]` (at most 16, from
    /// the earliest `t`). The four written in the last minute are among the
    /// right edge's. 23 resident of the 15 s level's 241.
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
        assert_eq!(
            resident_window_ends(S15, 245, MIN, now, &[reader]).len(),
            23
        );
    }

    /// A 5 min level built from a 15 s chain at each 5 min close reads the
    /// 20 windows of the closing 5 min; within 60 s of a close they are
    /// resident, then only the four newest: over the 20 positions of
    /// `now` in a 5 min period, 4 of them (now at the close and 15, 30, 45 s
    /// after) keep the read windows (20, 20 + 1, 20 + 2, 20 + 3 of them
    /// with the newest), the other 16 keep 4. Mean (20 + 21 + 22 + 23 + 16
    /// · 4) / 20 = 7.5, most 23.
    #[test]
    fn a_wider_level_keeps_the_windows_it_built_from_resident_for_the_idle_time() {
        let reader = ChainReader {
            read: WindowRead::Range {
                range_ms: 5 * MIN,
                offset_ms: 0,
            },
            times: ReadTimes::Refresh { width_ms: 5 * MIN },
        };
        assert_eq!(resident_windows(S15, 241, MIN, &[reader]), (7.5, 23));
    }

    /// A reader the model does not follow keeps every retained window.
    #[test]
    fn an_unfollowed_reader_keeps_every_window_resident() {
        let reader = ChainReader {
            read: WindowRead::Every,
            times: every_15s(),
        };
        assert_eq!(resident_windows(S15, 241, MIN, &[reader]), (241.0, 241));
    }

    /// Without readers, the windows written within the idle time.
    #[test]
    fn without_readers_only_the_newest_windows_are_resident() {
        assert_eq!(resident_windows(S15, 241, MIN, &[]), (4.0, 4));
    }
}

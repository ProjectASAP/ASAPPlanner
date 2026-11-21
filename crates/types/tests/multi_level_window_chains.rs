//! The windows a `TimeRangeCover` selects (design doc
//! `multi-level-window-chains.md`, section 3).
use asap_types::ir::operator::tumbling_window_chain::{
    cover_windows, mean_cover_windows, mean_cover_windows_per_level,
};

const SECOND_MS: i64 = 1_000;
const MINUTE_MS: i64 = 60 * SECOND_MS;
const HOUR_MS: i64 = 60 * MINUTE_MS;

/// T1. For three level sets, every evaluation time on the 15 s grid over a
/// day and ranges of 1 h, 57.5 min (not a multiple of 5 min) and 30 min:
/// each window lies inside `(T - W, T]`, the windows are disjoint, and
/// together they are the whole range.
#[test]
fn cover_windows_tile_the_range() {
    let level_sets: [&[u64]; 3] = [
        &[300_000, 15_000],
        &[300_000, 60_000, 15_000],
        &[900_000, 300_000, 60_000, 15_000],
    ];
    for levels in level_sets {
        for range_ms in [HOUR_MS, 57 * MINUTE_MS + 30 * SECOND_MS, 30 * MINUTE_MS] {
            let mut t = 100 * HOUR_MS;
            while t < 124 * HOUR_MS {
                let mut windows: Vec<(i64, i64)> = cover_windows(t - range_ms, t, levels)
                    .into_iter()
                    .map(|(level, end)| (end - levels[level] as i64, end))
                    .collect();
                windows.sort();
                let mut covered_to = t - range_ms;
                for (start, end) in &windows {
                    assert_eq!(
                        *start, covered_to,
                        "{levels:?} {range_ms} at {t}: {windows:?}"
                    );
                    covered_to = *end;
                }
                assert_eq!(covered_to, t, "{levels:?} {range_ms} at {t}");
                t += 15 * SECOND_MS;
            }
        }
    }
}

/// T2. The design's counts: at T = 100 h + 105 s over 5 min and 15 s, 13
/// windows of 15 s, 11 of 5 min and 7 of 15 s, 31 in all; the means over
/// 15 s steps for a 1 h range: 30.05, 18.05 and 12.02 merges per step
/// against 240.
#[test]
fn cover_counts_match_the_design_table() {
    let t = 100 * HOUR_MS + 105 * SECOND_MS;
    let windows = cover_windows(t - HOUR_MS, t, &[300_000, 15_000]);
    let wide = windows.iter().filter(|(level, _)| *level == 0).count();
    let narrow_old_edge = windows
        .iter()
        .filter(|(level, end)| *level == 1 && *end <= 99 * HOUR_MS + 300 * SECOND_MS)
        .count();
    let narrow_new_edge = windows
        .iter()
        .filter(|(level, end)| *level == 1 && *end > 100 * HOUR_MS)
        .count();
    assert_eq!((narrow_old_edge, wide, narrow_new_edge), (13, 11, 7));
    assert_eq!(windows.len(), 31);
    let mean = |levels: &[u64]| mean_cover_windows(HOUR_MS as u64, 0, 15_000, levels);
    assert!((mean(&[300_000, 15_000]) - 30.05).abs() < 1e-9);
    assert!((mean(&[300_000, 60_000, 15_000]) - 18.05).abs() < 1e-9);
    assert!((mean(&[900_000, 300_000, 60_000, 15_000]) - 721.0 / 60.0).abs() < 1e-9);
    assert_eq!(mean(&[15_000]), 240.0);
    let per_level = mean_cover_windows_per_level(HOUR_MS as u64, 0, 15_000, &[300_000, 15_000]);
    assert!((per_level[0] - 11.05).abs() < 1e-9 && (per_level[1] - 19.0).abs() < 1e-9);
}

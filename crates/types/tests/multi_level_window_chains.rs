//! The windows a `TimeRangeCover` selects (design doc
//! `multi-level-window-chains.md`, section 3).
use asap_types::ir::operator::tumbling_window_chain::{
    cover_windows, mean_cover_windows_per_level, CoverOffTheGrid,
};
use asap_types::stages::EvaluationGrid;

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
                    .unwrap()
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
    let windows = cover_windows(t - HOUR_MS, t, &[300_000, 15_000]).unwrap();
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
    let every_15_s = EvaluationGrid {
        period_ms: 15_000,
        phase_ms: 0,
    };
    let mean = |levels: &[u64]| -> f64 {
        mean_cover_windows_per_level(HOUR_MS as u64, every_15_s, levels)
            .iter()
            .sum()
    };
    assert!((mean(&[300_000, 15_000]) - 30.05).abs() < 1e-9);
    assert!((mean(&[300_000, 60_000, 15_000]) - 18.05).abs() < 1e-9);
    assert!((mean(&[900_000, 300_000, 60_000, 15_000]) - 721.0 / 60.0).abs() < 1e-9);
    assert_eq!(mean(&[15_000]), 240.0);
    let per_level = mean_cover_windows_per_level(HOUR_MS as u64, every_15_s, &[300_000, 15_000]);
    assert!((per_level[0] - 11.05).abs() < 1e-9 && (per_level[1] - 19.0).abs() < 1e-9);
}

/// An interval off the narrowest level's grid has no exact cover: refused,
/// not answered from part of it. `(T - 1h, T]` at T = 100 h + 7 s over
/// 5 min and 15 s.
#[test]
fn an_interval_off_the_narrowest_grid_is_refused() {
    let t = 100 * HOUR_MS + 7 * SECOND_MS;
    assert_eq!(
        cover_windows(t - HOUR_MS, t, &[300_000, 15_000]),
        Err(CoverOffTheGrid {
            start_ms: t - HOUR_MS,
            end_ms: t,
            narrowest_ms: 15_000,
        })
    );
}

/// The closed-form means equal the windows counted at every evaluation
/// time of the grid, for ranges at least twice and less than twice the
/// widest width, grids of several periods and phases.
#[test]
fn closed_form_means_equal_counted_means() {
    let cases: [(u64, &[u64]); 5] = [
        (3_600_000, &[300_000, 15_000]),
        (3_600_000, &[900_000, 300_000, 60_000, 15_000]),
        (300_000, &[60_000, 10_000]),
        (60_000, &[60_000, 10_000]),
        (3_450_000, &[300_000, 15_000]),
    ];
    for (range_ms, levels) in cases {
        let narrowest = *levels.last().unwrap();
        for period_ms in [narrowest, 4 * narrowest, 6 * narrowest] {
            for phase_ms in [0, narrowest] {
                let grid = EvaluationGrid {
                    period_ms,
                    phase_ms: phase_ms % period_ms,
                };
                let closed = mean_cover_windows_per_level(range_ms, grid, levels);
                let evaluations = 2 * levels[0] / narrowest * period_ms / narrowest;
                let mut counted = vec![0.0; levels.len()];
                for k in 0..evaluations {
                    let t = (1_000 * HOUR_MS as u64 + grid.phase_ms + k * period_ms) as i64;
                    for (level, _) in cover_windows(t - range_ms as i64, t, levels).unwrap() {
                        counted[level] += 1.0;
                    }
                }
                for (level, closed) in closed.iter().enumerate() {
                    let counted = counted[level] / evaluations as f64;
                    assert!(
                        (closed - counted).abs() < 1e-9,
                        "{range_ms} {levels:?} {grid:?} level {level}: {closed} vs {counted}"
                    );
                }
            }
        }
    }
}
